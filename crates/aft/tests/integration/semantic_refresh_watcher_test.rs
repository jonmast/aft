use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

use crate::helpers::AftProcess;

struct MockEmbeddingServer {
    base_url: String,
    addr: SocketAddr,
    running: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl MockEmbeddingServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind embedding server");
        listener
            .set_nonblocking(true)
            .expect("set embedding server nonblocking");
        let addr = listener.local_addr().expect("embedding server addr");
        let running = Arc::new(AtomicBool::new(true));
        let running_for_thread = Arc::clone(&running);
        let handle = thread::spawn(move || {
            while running_for_thread.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = handle_embedding_request(&mut stream);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            base_url: format!("http://{addr}"),
            addr,
            running,
            handle: Some(handle),
        }
    }
}

impl Drop for MockEmbeddingServer {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
        if let Some(handle) = self.handle.take() {
            handle.join().expect("embedding server thread");
        }
    }
}

fn handle_embedding_request(stream: &mut TcpStream) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut header_end = None;
    let mut content_length = 0usize;

    loop {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if header_end.is_none() {
            if let Some(pos) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
                header_end = Some(pos + 4);
                for line in String::from_utf8_lossy(&buf[..pos + 4]).lines() {
                    let Some((name, value)) = line.split_once(':') else {
                        continue;
                    };
                    if name.eq_ignore_ascii_case("content-length") {
                        content_length = value.trim().parse::<usize>().unwrap_or(0);
                    }
                }
            }
        }
        if let Some(end) = header_end {
            if buf.len() >= end + content_length {
                break;
            }
        }
    }

    let body = header_end
        .and_then(|end| buf.get(end..end + content_length))
        .and_then(|bytes| serde_json::from_slice::<Value>(bytes).ok())
        .unwrap_or_else(|| json!({ "input": [] }));
    let inputs = match &body["input"] {
        Value::Array(values) => values
            .iter()
            .filter_map(|value| value.as_str().map(str::to_string))
            .collect::<Vec<_>>(),
        Value::String(value) => vec![value.clone()],
        _ => Vec::new(),
    };

    if inputs
        .iter()
        .any(|input| input.to_ascii_lowercase().contains("after edit refreshed"))
    {
        thread::sleep(Duration::from_millis(500));
    }

    let data = inputs
        .iter()
        .enumerate()
        .map(|(index, input)| json!({ "embedding": embedding_for(input), "index": index }))
        .collect::<Vec<_>>();
    let body = json!({ "data": data }).to_string();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(response.as_bytes())
}

fn embedding_for(text: &str) -> Vec<f32> {
    let lower = text.to_ascii_lowercase();
    if lower.contains("alpha_anchor") || lower.contains("alpha anchor") {
        vec![1.0, 0.0, 0.0]
    } else if lower.contains("edited_refresh_marker") || lower.contains("edited refresh marker") {
        vec![0.0, 1.0, 0.0]
    } else {
        vec![0.0, 0.0, 1.0]
    }
}

fn setup_project(files: &[(&str, &str)]) -> tempfile::TempDir {
    let temp_dir = tempfile::tempdir().expect("create project dir");
    for (relative_path, content) in files {
        let path = temp_dir.path().join(relative_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent dirs");
        }
        fs::write(path, content).expect("write fixture");
    }
    temp_dir
}

fn send(aft: &mut AftProcess, request: Value) -> Value {
    aft.send(&serde_json::to_string(&request).expect("serialize request"))
}

fn configure_semantic_openai(
    aft: &mut AftProcess,
    root: &Path,
    storage_dir: &Path,
    base_url: &str,
) -> Value {
    send(
        aft,
        json!({
            "id": "cfg-semantic-refresh",
            "command": "configure",
            "harness": "opencode",
            "project_root": root.display().to_string(),
            "search_index": false,
            "semantic_search": true,
            "storage_dir": storage_dir.display().to_string(),
            "semantic": {
                "backend": "openai_compatible",
                "model": "test-embedding",
                "base_url": base_url,
                "timeout_ms": 5_000,
                "max_batch_size": 64,
            },
        }),
    )
}

fn status(aft: &mut AftProcess) -> Value {
    send(
        aft,
        json!({
            "id": "status",
            "command": "status",
        }),
    )
}

fn wait_for_semantic_status<F>(aft: &mut AftProcess, label: &str, predicate: F) -> Value
where
    F: Fn(&Value) -> bool,
{
    let mut last_response = None;
    for _ in 0..150 {
        let response = status(aft);
        assert_eq!(
            response["success"], true,
            "status should succeed while waiting for {label}: {response:?}"
        );
        if predicate(&response) {
            return response;
        }
        last_response = Some(response);
        thread::sleep(Duration::from_millis(100));
    }

    panic!(
        "semantic status did not become {label} in time; last response: {:?}",
        last_response
    );
}

#[test]
fn semantic_refresh_watcher_reindexes_modified_file_and_clears_refreshing() {
    let project = setup_project(&[
        (
            "src/a.rs",
            "pub fn alpha_anchor() -> &'static str {\n    \"alpha anchor\"\n}\n",
        ),
        (
            "src/b.rs",
            "pub fn edited_refresh_marker() -> &'static str {\n    \"before edit\"\n}\n",
        ),
        (
            "src/c.rs",
            "pub fn gamma_helper() -> &'static str {\n    \"gamma\"\n}\n",
        ),
    ]);
    let storage = tempfile::tempdir().expect("create storage dir");
    let server = MockEmbeddingServer::start();
    let mut aft = AftProcess::spawn();

    let configure =
        configure_semantic_openai(&mut aft, project.path(), storage.path(), &server.base_url);
    assert_eq!(
        configure["success"], true,
        "configure should succeed: {configure:?}"
    );

    let ready = wait_for_semantic_status(&mut aft, "initial ready", |response| {
        response["semantic_index"]["status"] == "ready"
            && response["semantic_index"]["entries"].as_u64().unwrap_or(0) >= 3
            && response["semantic_index"]["refreshing_count"] == 0
    });
    assert_eq!(ready["semantic_index"]["refreshing_count"], 0);

    let edited_file = project.path().join("src/b.rs");
    fs::write(
        &edited_file,
        "pub fn edited_refresh_marker() -> &'static str {\n    \"after edit refreshed content\"\n}\n",
    )
    .expect("edit watched file");

    let refreshing = wait_for_semantic_status(&mut aft, "watcher refreshing", |response| {
        response["semantic_index"]["status"] == "ready"
            && response["semantic_index"]["refreshing_count"] == 1
    });
    assert_eq!(refreshing["semantic_index"]["refreshing_count"], 1);

    let refreshed = wait_for_semantic_status(&mut aft, "refresh completed", |response| {
        response["semantic_index"]["status"] == "ready"
            && response["semantic_index"]["refreshing_count"] == 0
    });
    assert_eq!(refreshed["semantic_index"]["refreshing_count"], 0);

    let search = send(
        &mut aft,
        json!({
            "id": "semantic-refresh-search",
            "command": "semantic_search",
            "query": "edited refresh marker",
            "hint": "semantic",
            "top_k": 5,
        }),
    );
    assert_eq!(
        search["success"], true,
        "semantic search should succeed: {search:?}"
    );
    assert_eq!(search["status"], "ready");
    let results = search["results"].as_array().expect("results array");
    let edited_result = results
        .iter()
        .find(|result| {
            result["file"]
                .as_str()
                .is_some_and(|file| file.ends_with("src/b.rs"))
        })
        .unwrap_or_else(|| panic!("expected refreshed src/b.rs result, got {results:?}"));
    assert!(
        edited_result["snippet"]
            .as_str()
            .is_some_and(|snippet| snippet.contains("after edit refreshed content")),
        "expected refreshed snippet, got {edited_result:?}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}
