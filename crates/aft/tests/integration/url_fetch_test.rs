use super::helpers::AftProcess;
use aft::url_fetch::{
    cache_content_path_for_url, cache_meta_path_for_url, fetch_url_to_cache,
    is_private_ip_for_test, UrlFetchOptions,
};
use serde_json::json;
use std::fs;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

struct MockServer {
    addr: SocketAddr,
    _join: thread::JoinHandle<()>,
}

impl MockServer {
    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.addr.port())
    }
}

fn spawn_mock_server<F>(max_requests: usize, handler: F) -> MockServer
where
    F: Fn(String, &mut TcpStream) + Send + Sync + 'static,
{
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind mock server");
    let addr = listener.local_addr().expect("mock server addr");
    let handler = Arc::new(handler);
    let join = thread::spawn(move || {
        for stream in listener.incoming().take(max_requests).flatten() {
            let handler = Arc::clone(&handler);
            let mut stream = stream;
            let path = read_request_path(&mut stream);
            handler(path, &mut stream);
        }
    });
    MockServer { addr, _join: join }
}

fn read_request_path(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set read timeout");
    let mut request = Vec::new();
    let mut buf = [0u8; 512];
    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
        let n = stream.read(&mut buf).expect("read request");
        if n == 0 {
            break;
        }
        request.extend_from_slice(&buf[..n]);
    }
    let text = String::from_utf8_lossy(&request);
    text.lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/")
        .to_string()
}

fn write_response(stream: &mut TcpStream, status: &str, content_type: &str, body: &[u8]) {
    write!(
        stream,
        "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    )
    .expect("write response headers");
    stream.write_all(body).expect("write response body");
    stream.flush().expect("flush response");
}

fn configure_with_storage(project: &Path, storage: &Path) -> AftProcess {
    let mut aft = AftProcess::spawn();
    let resp = aft.send(
        &json!({
            "id": "cfg",
            "command": "configure",
            "harness": "opencode",
            "project_root": project,
            "storage_dir": storage,
        })
        .to_string(),
    );
    assert_eq!(resp["success"], true, "configure failed: {resp:?}");
    aft
}

#[test]
fn private_ip_blocked_at_fetch_time() {
    let project = TempDir::new().unwrap();
    let storage = TempDir::new().unwrap();
    let mut aft = configure_with_storage(project.path(), storage.path());

    let resp = aft.send(
        &json!({
            "id": "private",
            "command": "outline",
            "file": "http://127.0.0.1/foo",
        })
        .to_string(),
    );

    assert_eq!(resp["success"], false, "private URL should fail: {resp:?}");
    assert!(
        resp["message"]
            .as_str()
            .unwrap_or_default()
            .contains("Blocked private URL host"),
        "unexpected error: {resp:?}"
    );
    assert!(aft.shutdown().success());
}

#[test]
fn cache_hit_revalidates_ssrf() {
    let project = TempDir::new().unwrap();
    let storage = TempDir::new().unwrap();
    let server = spawn_mock_server(1, |_path, stream| {
        write_response(stream, "200 OK", "text/markdown", b"# Cached\n");
    });
    let url = server.url("/doc.md");
    let mut aft = configure_with_storage(project.path(), storage.path());

    let first = aft.send(
        &json!({
            "id": "prime",
            "command": "outline",
            "file": url,
            "allow_private": true,
        })
        .to_string(),
    );
    assert_eq!(first["success"], true, "prime should succeed: {first:?}");

    let second = aft.send(
        &json!({
            "id": "revalidate",
            "command": "outline",
            "file": url,
        })
        .to_string(),
    );
    assert_eq!(
        second["success"], false,
        "cache hit must revalidate SSRF: {second:?}"
    );
    assert!(second["message"]
        .as_str()
        .unwrap_or_default()
        .contains("Blocked private URL host"));
    assert!(aft.shutdown().success());
}

#[test]
fn body_read_stall_aborts_within_timeout() {
    let project = TempDir::new().unwrap();
    let storage = TempDir::new().unwrap();
    let server = spawn_mock_server(1, |_path, stream| {
        write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-type: text/markdown\r\ncontent-length: 5\r\nconnection: keep-alive\r\n\r\n"
        )
        .expect("write stall headers");
        stream.flush().expect("flush stall headers");
        thread::sleep(Duration::from_secs(30));
    });
    let mut aft = configure_with_storage(project.path(), storage.path());
    let started = Instant::now();

    let resp = aft.send(
        &json!({
            "id": "stall",
            "command": "outline",
            "file": server.url("/stall.md"),
            "allow_private": true,
        })
        .to_string(),
    );

    assert_eq!(resp["success"], false, "stall should fail: {resp:?}");
    assert!(resp["message"]
        .as_str()
        .unwrap_or_default()
        .contains("Body read stalled"));
    assert!(
        started.elapsed() < Duration::from_secs(22),
        "stall timeout took too long: {:?}",
        started.elapsed()
    );
    assert!(aft.shutdown().success());
}

#[test]
fn redirect_revalidates_each_hop() {
    let storage = TempDir::new().unwrap();
    let server = spawn_mock_server(1, move |_path, stream| {
        write!(
            stream,
            "HTTP/1.1 302 Found\r\nlocation: http://127.0.0.1:{}/private\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            stream.local_addr().unwrap().port()
        )
        .expect("write redirect");
        stream.flush().expect("flush redirect");
    });
    let url = "http://public.test/start";

    let err = fetch_url_to_cache(
        url,
        storage.path(),
        UrlFetchOptions {
            public_host_overrides: vec![(
                "public.test".to_string(),
                vec![IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))],
            )],
            connect_overrides: vec![("public.test".to_string(), server.addr)],
            ..UrlFetchOptions::default()
        },
    )
    .expect_err("redirect to private URL must fail");

    assert!(
        err.to_string().contains("Blocked private URL host"),
        "unexpected error: {err}"
    );
}

#[test]
fn unsupported_content_type_rejected() {
    let project = TempDir::new().unwrap();
    let storage = TempDir::new().unwrap();
    let server = spawn_mock_server(1, |_path, stream| {
        write_response(stream, "200 OK", "application/pdf", b"%PDF");
    });
    let mut aft = configure_with_storage(project.path(), storage.path());

    let resp = aft.send(
        &json!({
            "id": "pdf",
            "command": "outline",
            "file": server.url("/file.pdf"),
            "allow_private": true,
        })
        .to_string(),
    );

    assert_eq!(resp["success"], false, "PDF should fail: {resp:?}");
    let message = resp["message"].as_str().unwrap_or_default();
    assert!(message.contains("Unsupported content type"), "{message}");
    assert!(message.contains("Supported:"), "{message}");
    assert!(aft.shutdown().success());
}

#[test]
fn json_outline_works() {
    let project = TempDir::new().unwrap();
    let storage = TempDir::new().unwrap();
    let server = spawn_mock_server(1, |_path, stream| {
        write_response(
            stream,
            "200 OK",
            "application/json; charset=utf-8",
            br#"{"name":"aft","version":1,"nested":{"ok":true}}"#,
        );
    });
    let url = server.url("/package.json");
    let mut aft = configure_with_storage(project.path(), storage.path());

    let resp = aft.send(
        &json!({
            "id": "json",
            "command": "outline",
            "file": url,
            "allow_private": true,
        })
        .to_string(),
    );

    assert_eq!(
        resp["success"], true,
        "JSON outline should succeed: {resp:?}"
    );
    let text = resp["text"].as_str().expect("outline text");
    assert!(text.contains("name"), "{text}");
    assert!(text.contains("version"), "{text}");
    assert!(text.contains("nested"), "{text}");
    assert!(
        cache_content_path_for_url(storage.path(), &url, ".json").exists(),
        "JSON response should be cached with .json extension"
    );
    assert!(aft.shutdown().success());
}

#[test]
fn body_size_cap() {
    let project = TempDir::new().unwrap();
    let storage = TempDir::new().unwrap();
    let server = spawn_mock_server(1, |_path, stream| {
        write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-type: text/markdown\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            11 * 1024 * 1024
        )
        .expect("write oversized headers");
        stream.flush().expect("flush oversized headers");
    });
    let mut aft = configure_with_storage(project.path(), storage.path());

    let resp = aft.send(
        &json!({
            "id": "large",
            "command": "outline",
            "file": server.url("/large.md"),
            "allow_private": true,
        })
        .to_string(),
    );

    assert_eq!(resp["success"], false, "oversized should fail: {resp:?}");
    assert!(resp["message"]
        .as_str()
        .unwrap_or_default()
        .contains("Response too large"));
    assert!(aft.shutdown().success());
}

#[test]
fn cache_writes_atomically() {
    let storage = TempDir::new().unwrap();
    let server = spawn_mock_server(1, |_path, stream| {
        write_response(stream, "200 OK", "application/json", br#"{"atomic":true}"#);
    });
    let url = server.url("/atomic.json");
    let expected_final = cache_content_path_for_url(storage.path(), &url, ".json");
    let saw_tmp_before_rename = Arc::new(AtomicBool::new(false));
    let saw_tmp_before_rename_for_hook = Arc::clone(&saw_tmp_before_rename);
    let expected_final_for_hook = expected_final.clone();

    let cached = fetch_url_to_cache(
        &url,
        storage.path(),
        UrlFetchOptions {
            allow_private: true,
            atomic_write_observer: Some(Arc::new(move |tmp, final_path| {
                if final_path == expected_final_for_hook.as_path() {
                    let name = tmp
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or_default();
                    assert!(
                        name.contains(".tmp-"),
                        "temp name should contain .tmp-: {name}"
                    );
                    assert!(tmp.exists(), "temp file should exist before rename");
                    assert!(
                        !final_path.exists(),
                        "final path must not be visible before atomic rename"
                    );
                    saw_tmp_before_rename_for_hook.store(true, Ordering::SeqCst);
                }
            })),
            ..UrlFetchOptions::default()
        },
    )
    .expect("fetch should succeed");

    assert_eq!(cached, expected_final);
    assert!(saw_tmp_before_rename.load(Ordering::SeqCst));
    assert_eq!(
        fs::read_to_string(expected_final).unwrap(),
        r#"{"atomic":true}"#
    );
}

#[test]
fn ipv4_mapped_ipv6_ssrf_blocked() {
    for ip in [
        "::ffff:127.0.0.1",
        "::127.0.0.1",
        "::1",
        "::",
        "fe80::1",
        "fc00::1",
        "fd00::1",
        "ff00::1",
    ] {
        let parsed = ip.parse::<IpAddr>().expect("parse IPv6 test address");
        assert!(is_private_ip_for_test(parsed), "{ip} should be private");
    }

    let project = TempDir::new().unwrap();
    let storage = TempDir::new().unwrap();
    let mut aft = configure_with_storage(project.path(), storage.path());
    for host in ["[::ffff:127.0.0.1]", "[::1]", "[fe80::1]", "[fc00::1]"] {
        let resp = aft.send(
            &json!({
                "id": format!("block-{host}"),
                "command": "outline",
                "file": format!("http://{host}/doc.md"),
            })
            .to_string(),
        );
        assert_eq!(resp["success"], false, "{host} should fail: {resp:?}");
        assert!(
            resp["message"]
                .as_str()
                .unwrap_or_default()
                .contains("Blocked private URL host"),
            "unexpected error for {host}: {resp:?}"
        );
    }
    assert!(aft.shutdown().success());
}

#[test]
fn rfc6598_and_rfc2544_ranges_blocked() {
    // RFC 6598 Shared Address Space (CGNAT) 100.64.0.0/10 and RFC 2544
    // benchmark subnet 198.18.0.0/15 are non-routable and must be treated as
    // private to block SSRF, including via IPv4-mapped IPv6.
    for ip in [
        "100.64.0.1",
        "100.64.1.1",
        "100.100.50.1",
        "100.127.255.255",
        "198.18.0.1",
        "198.19.255.255",
        "::ffff:100.64.0.1",
        "::ffff:198.18.0.1",
    ] {
        let parsed = ip.parse::<IpAddr>().expect("parse test address");
        assert!(is_private_ip_for_test(parsed), "{ip} should be private");
    }

    // Boundary check: addresses just outside the reserved ranges stay public.
    for ip in [
        "100.63.255.255", // just below 100.64.0.0/10
        "100.128.0.1",    // just above 100.127.255.255
        "198.17.255.255", // just below 198.18.0.0/15
        "198.20.0.1",     // just above 198.19.255.255
        "8.8.8.8",        // canonical public
    ] {
        let parsed = ip.parse::<IpAddr>().expect("parse test address");
        assert!(!is_private_ip_for_test(parsed), "{ip} should be public");
    }
}

#[test]
fn concurrent_fetches_do_not_collide() {
    let storage = TempDir::new().unwrap();
    let server = spawn_mock_server(2, |_path, stream| {
        write_response(stream, "200 OK", "application/json", br#"{"same":true}"#);
    });
    let url = server.url("/same.json");
    let storage_a = storage.path().to_path_buf();
    let storage_b = storage.path().to_path_buf();
    let url_a = url.clone();
    let url_b = url.clone();

    let first = thread::spawn(move || {
        fetch_url_to_cache(
            &url_a,
            &storage_a,
            UrlFetchOptions {
                allow_private: true,
                ..UrlFetchOptions::default()
            },
        )
    });
    let second = thread::spawn(move || {
        fetch_url_to_cache(
            &url_b,
            &storage_b,
            UrlFetchOptions {
                allow_private: true,
                ..UrlFetchOptions::default()
            },
        )
    });

    let path_a = first.join().unwrap().expect("first fetch");
    let path_b = second.join().unwrap().expect("second fetch");
    assert_eq!(path_a, path_b);
    assert!(cache_content_path_for_url(storage.path(), &url, ".json").exists());
    assert!(cache_meta_path_for_url(storage.path(), &url).exists());

    let cache_dir = storage.path().join("url_cache");
    let entries: Vec<String> = fs::read_dir(cache_dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(
        entries.iter().filter(|name| name.contains(".tmp-")).count(),
        0,
        "temp files should be cleaned up: {entries:?}"
    );
    assert_eq!(
        entries.len(),
        2,
        "one content file and one meta file should remain: {entries:?}"
    );
}

#[test]
fn transient_connect_failure_retries_then_succeeds() {
    // First connect: TCP listener accepts the connection but drops it without
    // writing any response. reqwest surfaces that as `is_request()` (the body
    // never read a status line), which is_transient_reqwest_error classifies
    // as transient. Second connect: write a real 200 response. Without the
    // retry the outer call would fail; with it the second attempt wins.
    let attempt = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let attempt_for_handler = Arc::clone(&attempt);
    let server = spawn_mock_server(2, move |_path, stream| {
        let n = attempt_for_handler.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            // Drop without writing anything — peer closed during request.
            let _ = stream.shutdown(std::net::Shutdown::Both);
        } else {
            write_response(stream, "200 OK", "text/markdown", b"# Retried OK\n");
        }
    });
    let url = server.url("/doc.md");
    let storage = TempDir::new().unwrap();

    let start = Instant::now();
    let result = fetch_url_to_cache(
        &url,
        storage.path(),
        UrlFetchOptions {
            allow_private: true,
            ..UrlFetchOptions::default()
        },
    );
    let elapsed = start.elapsed();

    let path = result.expect("retry should make the fetch succeed");
    let body = fs::read_to_string(path).unwrap();
    assert!(body.contains("Retried OK"));
    assert_eq!(
        attempt.load(Ordering::SeqCst),
        2,
        "server should see exactly two connect attempts"
    );
    assert!(
        elapsed >= Duration::from_millis(150),
        "first retry should sleep at least one short backoff before reconnecting (elapsed = {elapsed:?})"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "retry budget shouldn't blow up foreground latency (elapsed = {elapsed:?})"
    );
}

#[test]
fn http_error_status_is_not_retried() {
    // The server *answers* with HTTP 404 on the first request. reqwest treats
    // that as a successful response (status() carries 404), so the caller
    // surfaces "HTTP 404" without re-hammering the server. If the retry loop
    // wrongly retried, the mock would be reached more than once.
    let attempt = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let attempt_for_handler = Arc::clone(&attempt);
    let server = spawn_mock_server(3, move |_path, stream| {
        attempt_for_handler.fetch_add(1, Ordering::SeqCst);
        write_response(stream, "404 Not Found", "text/plain", b"nope\n");
    });
    let url = server.url("/missing.md");
    let storage = TempDir::new().unwrap();

    let result = fetch_url_to_cache(
        &url,
        storage.path(),
        UrlFetchOptions {
            allow_private: true,
            ..UrlFetchOptions::default()
        },
    );
    let err = result.expect_err("404 must surface as error");
    assert!(
        err.to_string().contains("HTTP 404"),
        "unexpected error: {err}"
    );
    assert_eq!(
        attempt.load(Ordering::SeqCst),
        1,
        "HTTP error status must NOT trigger a retry"
    );
}
