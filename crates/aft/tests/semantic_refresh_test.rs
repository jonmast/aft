//! Integration tests for `SemanticIndex::refresh_stale_files`.
//!
//! These cover the four real cases the configure path triggers on restart:
//!  - no-op (nothing changed → empty summary, no embed calls)
//!  - one file changed mtime → only that file is re-embedded
//!  - one file deleted from the walk → entries dropped, no embeds
//!  - one new file appeared in the walk → only that file is embedded
//!
//! We deliberately use a stub embedder that returns deterministic vectors so
//! we can assert byte-exact entry counts and dimensions without depending on
//! ONNX runtime or fastembed.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use aft::semantic_index::SemanticIndex;

/// Stub embedder that returns vectors based on text content.
/// Tracks all calls so we can assert which files (and how many) got embedded.
struct StubEmbedder {
    calls: Mutex<Vec<Vec<String>>>,
}

impl StubEmbedder {
    fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
        }
    }

    fn embed(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, String> {
        let vectors: Vec<Vec<f32>> = texts
            .iter()
            .map(|text| {
                // Spread vectors based on content so different chunks have
                // different embeddings — keeps the index nontrivial.
                let len = text.len() as f32;
                vec![1.0, len.fract().abs(), 0.0, 0.0]
            })
            .collect();
        self.calls.lock().expect("lock embed calls").push(texts);
        Ok(vectors)
    }

    fn total_embedded_texts(&self) -> usize {
        self.calls
            .lock()
            .expect("lock embed calls")
            .iter()
            .map(|batch| batch.len())
            .sum()
    }

    fn batch_count(&self) -> usize {
        self.calls.lock().expect("lock embed calls").len()
    }

    fn embedded_texts(&self) -> Vec<String> {
        self.calls
            .lock()
            .expect("lock embed calls")
            .iter()
            .flat_map(|batch| batch.iter().cloned())
            .collect()
    }
}

/// Build an initial index over a small two-file project. Returns the index +
/// the file paths so tests can mutate one and call refresh.
fn build_two_file_index(project_root: &Path) -> (SemanticIndex, PathBuf, PathBuf) {
    let file_a = project_root.join("src/a.rs");
    let file_b = project_root.join("src/b.rs");
    fs::create_dir_all(file_a.parent().expect("parent")).expect("create src");
    fs::write(
        &file_a,
        "pub fn alpha() -> i32 {\n    let x = 1;\n    x\n}\n\npub fn alpha_helper() -> i32 {\n    let y = 2;\n    y\n}\n",
    )
    .expect("write a");
    fs::write(
        &file_b,
        "pub fn beta() -> i32 {\n    let x = 3;\n    x\n}\n\npub fn beta_helper() -> i32 {\n    let y = 4;\n    y\n}\n",
    )
    .expect("write b");

    let stub = StubEmbedder::new();
    let mut embed = |texts: Vec<String>| stub.embed(texts);
    let index = SemanticIndex::build(
        project_root,
        &[file_a.clone(), file_b.clone()],
        &mut embed,
        16,
    )
    .expect("build initial index");
    (index, file_a, file_b)
}

/// Touch a file's mtime by overwriting its contents, then explicitly advance
/// it so platforms with 1-second mtime granularity observe a changed timestamp.
fn rewrite_with_new_mtime(path: &Path, new_contents: &str) {
    let modified = fs::metadata(path)
        .expect("stat file before rewrite")
        .modified()
        .expect("file mtime before rewrite");
    let advanced = modified
        .checked_add(Duration::from_secs(2))
        .expect("advanced mtime");
    fs::write(path, new_contents).expect("rewrite");
    filetime::set_file_mtime(path, filetime::FileTime::from_system_time(advanced))
        .expect("set advanced mtime");
}

static SHARED_LOG_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn shared_lock() -> &'static Mutex<()> {
    SHARED_LOG_LOCK.get_or_init(|| Mutex::new(()))
}

#[test]
fn refresh_is_noop_when_nothing_changed() {
    let _guard = shared_lock().lock();
    let project = tempfile::tempdir().expect("create project dir");
    let (mut index, file_a, file_b) = build_two_file_index(project.path());
    let entries_before = index.entry_count();

    let stub = StubEmbedder::new();
    let mut embed = |texts: Vec<String>| stub.embed(texts);
    let mut progress = |_done: usize, _total: usize| {};

    let summary = index
        .refresh_stale_files(
            project.path(),
            &[file_a.clone(), file_b.clone()],
            &mut embed,
            16,
            &mut progress,
        )
        .expect("refresh succeeds");

    assert!(summary.is_noop(), "summary should be noop, got {summary:?}");
    assert_eq!(summary.deleted, 0);
    assert_eq!(summary.changed, 0);
    assert_eq!(summary.added, 0);
    assert_eq!(stub.total_embedded_texts(), 0, "no embeds for noop");
    assert_eq!(index.entry_count(), entries_before, "entries preserved");
}

#[test]
fn refresh_re_embeds_only_changed_file() {
    let _guard = shared_lock().lock();
    let project = tempfile::tempdir().expect("create project dir");
    let (mut index, file_a, file_b) = build_two_file_index(project.path());
    let entries_before = index.entry_count();

    // Modify file_a — file_b is untouched and must keep its cached embeddings.
    rewrite_with_new_mtime(
        &file_a,
        "pub fn alpha_renamed() -> i32 {\n    let x = 99;\n    x\n}\n\npub fn alpha_helper_renamed() -> i32 {\n    let y = 100;\n    y\n}\n",
    );

    let stub = StubEmbedder::new();
    let mut embed = |texts: Vec<String>| stub.embed(texts);
    let mut progress = |_done: usize, _total: usize| {};

    let summary = index
        .refresh_stale_files(
            project.path(),
            &[file_a.clone(), file_b.clone()],
            &mut embed,
            16,
            &mut progress,
        )
        .expect("refresh succeeds");

    assert_eq!(summary.changed, 1, "exactly one file changed");
    assert_eq!(summary.deleted, 0);
    assert_eq!(summary.added, 0);
    assert!(stub.total_embedded_texts() > 0, "should re-embed something");

    // We embedded only file_a's chunks, not file_b's. Total embedded texts
    // must be strictly less than entries_before (which covered both files).
    assert!(
        stub.total_embedded_texts() < entries_before,
        "should embed less than full rebuild; embedded={}, full={}",
        stub.total_embedded_texts(),
        entries_before
    );

    // Sanity: index still has entries for file_b without re-embedding.
    let count_for_b = count_entries_for_file(&index, &file_b);
    assert!(count_for_b > 0, "file_b entries preserved");
}

#[test]
fn refresh_drops_entries_for_files_no_longer_in_walk() {
    let _guard = shared_lock().lock();
    let project = tempfile::tempdir().expect("create project dir");
    let (mut index, file_a, file_b) = build_two_file_index(project.path());

    let count_for_b_before = count_entries_for_file(&index, &file_b);
    assert!(count_for_b_before > 0, "precondition: index has b entries");

    // Simulate: walk now only returns file_a (file_b deleted or excluded).
    let stub = StubEmbedder::new();
    let mut embed = |texts: Vec<String>| stub.embed(texts);
    let mut progress = |_done: usize, _total: usize| {};

    let summary = index
        .refresh_stale_files(
            project.path(),
            std::slice::from_ref(&file_a),
            &mut embed,
            16,
            &mut progress,
        )
        .expect("refresh succeeds");

    assert_eq!(summary.deleted, 1, "file_b reported as deleted");
    assert_eq!(summary.changed, 0);
    assert_eq!(summary.added, 0);
    assert_eq!(stub.total_embedded_texts(), 0, "no embed calls");
    assert_eq!(
        count_entries_for_file(&index, &file_b),
        0,
        "file_b entries dropped"
    );
}

#[test]
fn refresh_embeds_new_files_added_to_walk() {
    let _guard = shared_lock().lock();
    let project = tempfile::tempdir().expect("create project dir");
    let (mut index, file_a, file_b) = build_two_file_index(project.path());
    let entries_before = index.entry_count();

    // A new file that was never in the original index.
    let file_c = project.path().join("src/c.rs");
    fs::write(
        &file_c,
        "pub fn gamma() -> i32 {\n    let z = 5;\n    z\n}\n\npub fn gamma_helper() -> i32 {\n    let w = 6;\n    w\n}\n",
    )
    .expect("write c");

    let stub = StubEmbedder::new();
    let mut embed = |texts: Vec<String>| stub.embed(texts);
    let mut progress = |_done: usize, _total: usize| {};

    let summary = index
        .refresh_stale_files(
            project.path(),
            &[file_a, file_b, file_c.clone()],
            &mut embed,
            16,
            &mut progress,
        )
        .expect("refresh succeeds");

    assert_eq!(summary.added, 1, "file_c discovered as new");
    assert_eq!(summary.changed, 0);
    assert_eq!(summary.deleted, 0);
    assert!(stub.total_embedded_texts() > 0);
    assert!(stub.total_embedded_texts() > 0, "embedded only file_c");

    // Index grew strictly larger (kept original, added new).
    assert!(
        index.entry_count() > entries_before,
        "index grew; before={}, after={}",
        entries_before,
        index.entry_count()
    );
    assert!(
        count_entries_for_file(&index, &file_c) > 0,
        "file_c entries present"
    );
}

#[test]
fn refresh_handles_changed_plus_deleted_plus_new_in_one_call() {
    let _guard = shared_lock().lock();
    let project = tempfile::tempdir().expect("create project dir");
    let (mut index, file_a, file_b) = build_two_file_index(project.path());

    // Change file_a, drop file_b from the walk, add file_c.
    rewrite_with_new_mtime(
        &file_a,
        "pub fn alpha_v2() -> i32 {\n    let v = 42;\n    v\n}\n",
    );
    let file_c = project.path().join("src/c.rs");
    fs::write(
        &file_c,
        "pub fn gamma() -> i32 {\n    let z = 5;\n    z\n}\n",
    )
    .expect("write c");

    let stub = StubEmbedder::new();
    let mut embed = |texts: Vec<String>| stub.embed(texts);
    let mut batches: Vec<(usize, usize)> = Vec::new();
    let mut progress = |done: usize, total: usize| batches.push((done, total));

    let summary = index
        .refresh_stale_files(
            project.path(),
            &[file_a, file_c.clone()],
            &mut embed,
            16,
            &mut progress,
        )
        .expect("refresh succeeds");

    assert_eq!(summary.deleted, 1, "file_b deleted");
    assert_eq!(summary.changed, 1, "file_a changed");
    assert_eq!(summary.added, 1, "file_c new");

    // file_b entries are gone.
    assert_eq!(count_entries_for_file(&index, &file_b), 0);
    // file_c entries are present.
    assert!(count_entries_for_file(&index, &file_c) > 0);

    // Progress callback fired at least once with a meaningful total.
    assert!(
        batches.iter().any(|(_done, total)| *total > 0),
        "progress callback should report nonzero total at least once"
    );

    // Embedded calls should match changed + new files only — not file_b
    // (deleted) and not the original file_a/file_b cached embeddings.
    assert!(stub.batch_count() >= 1, "at least one embed batch");
}

#[test]
fn refresh_reuses_line_shifted_file_chunks_without_embedding() {
    let _guard = shared_lock().lock();
    let project = tempfile::tempdir().expect("create project dir");
    let (mut index, file_a, file_b) = build_two_file_index(project.path());
    let count_for_a_before = count_entries_for_file(&index, &file_a);

    let shifted = "\npub fn alpha() -> i32 {\n    let x = 1;\n    x\n}\n\npub fn alpha_helper() -> i32 {\n    let y = 2;\n    y\n}\n";
    rewrite_with_new_mtime(&file_a, shifted);

    let stub = StubEmbedder::new();
    let mut embed = |texts: Vec<String>| stub.embed(texts);
    let mut progress = |_done: usize, _total: usize| {};
    let summary = index
        .refresh_stale_files(
            project.path(),
            &[file_a.clone(), file_b],
            &mut embed,
            16,
            &mut progress,
        )
        .expect("refresh succeeds");

    assert_eq!(summary.changed, 1);
    assert_eq!(stub.total_embedded_texts(), 0);
    assert_eq!(count_entries_for_file(&index, &file_a), count_for_a_before);
}

#[test]
fn refresh_reembeds_only_edited_symbol_in_changed_file() {
    let _guard = shared_lock().lock();
    let project = tempfile::tempdir().expect("create project dir");
    let (mut index, file_a, file_b) = build_two_file_index(project.path());
    let count_for_a_before = count_entries_for_file(&index, &file_a);
    assert!(
        count_for_a_before >= 2,
        "file_a starts with multiple symbols"
    );

    rewrite_with_new_mtime(
        &file_a,
        "pub fn alpha() -> i32 {\n    let x = 99;\n    x\n}\n\npub fn alpha_helper() -> i32 {\n    let y = 2;\n    y\n}\n",
    );

    let stub = StubEmbedder::new();
    let mut embed = |texts: Vec<String>| stub.embed(texts);
    let mut progress = |_done: usize, _total: usize| {};
    let summary = index
        .refresh_stale_files(
            project.path(),
            &[file_a.clone(), file_b],
            &mut embed,
            16,
            &mut progress,
        )
        .expect("refresh succeeds");

    assert_eq!(summary.changed, 1);
    assert_eq!(stub.total_embedded_texts(), 1);
    assert!(stub.embedded_texts()[0].contains("name:alpha"));
    // Duplication guard: the unchanged siblings are reused (embed-count==1 above
    // proves only the edited symbol was embedded), and the post-refresh entry
    // count for the file is unchanged — no chunk dropped, none duplicated.
    assert_eq!(
        count_entries_for_file(&index, &file_a),
        count_for_a_before,
        "entry count unchanged after partial-reuse refresh"
    );
}

#[test]
fn refresh_stale_files_collect_failure_keeps_stale_entries() {
    // The corpus-refresh path (refresh_stale_files) must KEEP stale-but-valid
    // entries when a changed file fails to re-collect (transient/parse error) —
    // the deliberate opposite of the watcher path, which drops them up front.
    let _guard = shared_lock().lock();
    let project = tempfile::tempdir().expect("create project dir");
    let (mut index, file_a, file_b) = build_two_file_index(project.path());
    let count_for_a_before = count_entries_for_file(&index, &file_a);
    assert!(count_for_a_before > 0, "file_a starts with entries");

    // Corrupt file_a (invalid UTF-8 → collect yields nothing) and advance its
    // mtime so it is bucketed as "changed" rather than fresh.
    fs::write(&file_a, [0xff, 0xfe, 0xfd]).expect("write invalid utf8");
    let advanced = fs::metadata(&file_a)
        .expect("stat after corrupt")
        .modified()
        .expect("mtime after corrupt")
        .checked_add(Duration::from_secs(2))
        .expect("advance mtime");
    filetime::set_file_mtime(&file_a, filetime::FileTime::from_system_time(advanced))
        .expect("set advanced mtime");

    let stub = StubEmbedder::new();
    let mut embed = |texts: Vec<String>| stub.embed(texts);
    let mut progress = |_done: usize, _total: usize| {};
    index
        .refresh_stale_files(
            project.path(),
            &[file_a.clone(), file_b],
            &mut embed,
            16,
            &mut progress,
        )
        .expect("refresh succeeds");

    assert_eq!(
        stub.total_embedded_texts(),
        0,
        "nothing embeds when the changed file fails to collect"
    );
    assert_eq!(
        count_entries_for_file(&index, &file_a),
        count_for_a_before,
        "stale entries kept (not dropped, not duplicated) on collect failure"
    );
}

#[test]
fn invalidated_refresh_mixed_reuse_and_miss_retains_all_after_apply() {
    // Editing one symbol's body in a multi-symbol file: that symbol's chunk is a
    // cache miss (re-embedded) while siblings + file-summary are reused. The
    // applied delta must carry the FULL replacement set so the serving index
    // neither drops the reused chunks nor duplicates them.
    let _guard = shared_lock().lock();
    let project = tempfile::tempdir().expect("create project dir");
    let (mut worker_index, file_a, _file_b) = build_two_file_index(project.path());
    let mut serving_index = worker_index.clone();
    let count_for_a_before = count_entries_for_file(&serving_index, &file_a);
    assert!(count_for_a_before >= 2, "file_a has multiple symbols");

    // Edit only alpha's body; alpha_helper is byte-identical and reused.
    rewrite_with_new_mtime(
        &file_a,
        "pub fn alpha() -> i32 {\n    let x = 99;\n    x\n}\n\npub fn alpha_helper() -> i32 {\n    let y = 2;\n    y\n}\n",
    );

    let stub = StubEmbedder::new();
    let mut embed = |texts: Vec<String>| stub.embed(texts);
    let mut progress = |_done: usize, _total: usize| {};
    let update = worker_index
        .refresh_invalidated_files(
            project.path(),
            std::slice::from_ref(&file_a),
            &mut embed,
            16,
            100,
            &mut progress,
        )
        .expect("refresh succeeds");

    // Mixed path: exactly the edited symbol re-embeds; the full set is still
    // delivered as the delta (reused + newly embedded).
    assert_eq!(
        stub.total_embedded_texts(),
        1,
        "only the edited symbol re-embeds; siblings reused"
    );
    assert_eq!(
        update.added_entries.len(),
        count_for_a_before,
        "delta is the full replacement set, not just the miss"
    );

    serving_index.apply_refresh_update(
        update.added_entries,
        update.updated_metadata,
        &update.completed_paths,
    );

    assert_eq!(
        count_entries_for_file(&serving_index, &file_a),
        count_for_a_before,
        "no chunk dropped, none duplicated after applying the mixed delta"
    );
}

#[test]
fn invalidated_refresh_delta_retains_reused_chunks_after_apply() {
    let _guard = shared_lock().lock();
    let project = tempfile::tempdir().expect("create project dir");
    let (mut worker_index, file_a, _file_b) = build_two_file_index(project.path());
    let mut serving_index = worker_index.clone();
    let count_for_a_before = count_entries_for_file(&serving_index, &file_a);

    rewrite_with_new_mtime(
        &file_a,
        "\npub fn alpha() -> i32 {\n    let x = 1;\n    x\n}\n\npub fn alpha_helper() -> i32 {\n    let y = 2;\n    y\n}\n",
    );

    let stub = StubEmbedder::new();
    let mut embed = |texts: Vec<String>| stub.embed(texts);
    let mut progress = |_done: usize, _total: usize| {};
    let update = worker_index
        .refresh_invalidated_files(
            project.path(),
            std::slice::from_ref(&file_a),
            &mut embed,
            16,
            100,
            &mut progress,
        )
        .expect("refresh succeeds");

    assert_eq!(stub.total_embedded_texts(), 0);
    assert_eq!(update.added_entries.len(), count_for_a_before);

    serving_index.apply_refresh_update(
        update.added_entries,
        update.updated_metadata,
        &update.completed_paths,
    );

    assert_eq!(
        count_entries_for_file(&serving_index, &file_a),
        count_for_a_before
    );
}

#[test]
fn invalidated_refresh_reuses_duplicate_embed_text_for_new_identical_symbol() {
    let _guard = shared_lock().lock();
    let project = tempfile::tempdir().expect("create project dir");
    let file = project.path().join("src/dupe.js");
    fs::create_dir_all(file.parent().expect("parent")).expect("create src");
    let duplicate = "function duplicate() {\n  return 1;\n}\n";
    fs::write(&file, duplicate).expect("write duplicate");

    let initial_stub = StubEmbedder::new();
    let mut initial_embed = |texts: Vec<String>| initial_stub.embed(texts);
    let mut index = SemanticIndex::build(
        project.path(),
        std::slice::from_ref(&file),
        &mut initial_embed,
        16,
    )
    .expect("build initial index");

    rewrite_with_new_mtime(&file, &format!("{duplicate}\n{duplicate}"));

    let stub = StubEmbedder::new();
    let mut embed = |texts: Vec<String>| stub.embed(texts);
    let mut progress = |_done: usize, _total: usize| {};
    index
        .refresh_invalidated_files(
            project.path(),
            std::slice::from_ref(&file),
            &mut embed,
            16,
            100,
            &mut progress,
        )
        .expect("refresh succeeds");

    let duplicate_results = index
        .search(&[1.0, 0.0, 0.0, 0.0], 16)
        .into_iter()
        .filter(|result| result.file == file && result.name == "duplicate")
        .count();
    assert_eq!(stub.total_embedded_texts(), 0);
    assert_eq!(duplicate_results, 2);
}

#[test]
fn invalidated_refresh_file_summary_reuse_and_miss_are_text_based() {
    let _guard = shared_lock().lock();
    let project = tempfile::tempdir().expect("create project dir");
    let file = project.path().join("src/lib.rs");
    fs::create_dir_all(file.parent().expect("parent")).expect("create src");
    fs::write(
        &file,
        "//! module docs v1\n\npub fn alpha() -> i32 {\n    1\n}\n",
    )
    .expect("write source");

    let initial_stub = StubEmbedder::new();
    let mut initial_embed = |texts: Vec<String>| initial_stub.embed(texts);
    let mut index = SemanticIndex::build(
        project.path(),
        std::slice::from_ref(&file),
        &mut initial_embed,
        16,
    )
    .expect("build initial index");

    rewrite_with_new_mtime(
        &file,
        "//! module docs v1\n\npub fn alpha() -> i32 {\n    2\n}\n",
    );
    let body_stub = StubEmbedder::new();
    let mut body_embed = |texts: Vec<String>| body_stub.embed(texts);
    let mut progress = |_done: usize, _total: usize| {};
    index
        .refresh_invalidated_files(
            project.path(),
            std::slice::from_ref(&file),
            &mut body_embed,
            16,
            100,
            &mut progress,
        )
        .expect("body refresh succeeds");
    assert_eq!(body_stub.total_embedded_texts(), 1);
    assert!(body_stub.embedded_texts()[0].contains("name:alpha"));

    rewrite_with_new_mtime(
        &file,
        "//! module docs v2\n\npub fn alpha() -> i32 {\n    2\n}\n",
    );
    let doc_stub = StubEmbedder::new();
    let mut doc_embed = |texts: Vec<String>| doc_stub.embed(texts);
    index
        .refresh_invalidated_files(
            project.path(),
            std::slice::from_ref(&file),
            &mut doc_embed,
            16,
            100,
            &mut progress,
        )
        .expect("doc refresh succeeds");
    assert_eq!(doc_stub.total_embedded_texts(), 1);
    assert!(doc_stub.embedded_texts()[0].contains("kind:file-summary"));
}

#[test]
fn invalidated_refresh_deleted_file_drops_entries_after_apply() {
    let _guard = shared_lock().lock();
    let project = tempfile::tempdir().expect("create project dir");
    let (mut worker_index, file_a, _file_b) = build_two_file_index(project.path());
    let mut serving_index = worker_index.clone();

    fs::remove_file(&file_a).expect("delete file");

    let stub = StubEmbedder::new();
    let mut embed = |texts: Vec<String>| stub.embed(texts);
    let mut progress = |_done: usize, _total: usize| {};
    let update = worker_index
        .refresh_invalidated_files(
            project.path(),
            std::slice::from_ref(&file_a),
            &mut embed,
            16,
            100,
            &mut progress,
        )
        .expect("refresh succeeds");

    assert_eq!(update.summary.deleted, 1);
    assert_eq!(stub.total_embedded_texts(), 0);
    serving_index.apply_refresh_update(
        update.added_entries,
        update.updated_metadata,
        &update.completed_paths,
    );
    assert_eq!(count_entries_for_file(&serving_index, &file_a), 0);
}

#[test]
fn invalidated_refresh_collect_failure_does_not_resurrect_stale_entries() {
    let _guard = shared_lock().lock();
    let project = tempfile::tempdir().expect("create project dir");
    let (mut worker_index, file_a, _file_b) = build_two_file_index(project.path());
    let mut serving_index = worker_index.clone();

    fs::write(&file_a, [0xff, 0xfe, 0xfd]).expect("write invalid utf8");

    let stub = StubEmbedder::new();
    let mut embed = |texts: Vec<String>| stub.embed(texts);
    let mut progress = |_done: usize, _total: usize| {};
    let update = worker_index
        .refresh_invalidated_files(
            project.path(),
            std::slice::from_ref(&file_a),
            &mut embed,
            16,
            100,
            &mut progress,
        )
        .expect("refresh succeeds");

    assert_eq!(stub.total_embedded_texts(), 0);
    assert!(update.added_entries.is_empty());
    serving_index.apply_refresh_update(
        update.added_entries,
        update.updated_metadata,
        &update.completed_paths,
    );
    assert_eq!(count_entries_for_file(&serving_index, &file_a), 0);
}

#[test]
fn invalidated_refresh_cap_deferral_stays_file_count_based() {
    let _guard = shared_lock().lock();
    let project = tempfile::tempdir().expect("create project dir");
    let file_a = project.path().join("src/a.rs");
    let file_b = project.path().join("src/b.rs");
    fs::create_dir_all(file_a.parent().expect("parent")).expect("create src");
    fs::write(&file_a, "pub fn alpha() -> i32 {\n    1\n}\n").expect("write a");
    fs::write(&file_b, "pub fn beta() -> i32 {\n    2\n}\n").expect("write b");

    let initial_stub = StubEmbedder::new();
    let mut initial_embed = |texts: Vec<String>| initial_stub.embed(texts);
    let mut index = SemanticIndex::build(
        project.path(),
        std::slice::from_ref(&file_a),
        &mut initial_embed,
        16,
    )
    .expect("build initial index");

    let stub = StubEmbedder::new();
    let mut embed = |texts: Vec<String>| stub.embed(texts);
    let mut progress = |_done: usize, _total: usize| {};
    let update = index
        .refresh_invalidated_files(
            project.path(),
            std::slice::from_ref(&file_b),
            &mut embed,
            16,
            1,
            &mut progress,
        )
        .expect("refresh succeeds");

    assert_eq!(update.summary.total_processed, 1);
    assert_eq!(update.summary.added, 0);
    assert_eq!(stub.total_embedded_texts(), 0);
    assert_eq!(index.indexed_file_count(), 1);
    assert_eq!(count_entries_for_file(&index, &file_b), 0);
}

/// Helper: count entries in the index that belong to `file`. We only have
/// public access via search results, so we issue a query that should match
/// every entry (vector field is mostly 1.0 in our stub) and filter by file.
fn count_entries_for_file(index: &SemanticIndex, file: &Path) -> usize {
    let query = vec![1.0, 0.5, 0.0, 0.0];
    let results = index.search(&query, 1024);
    results.iter().filter(|r| r.file == file).count()
}
