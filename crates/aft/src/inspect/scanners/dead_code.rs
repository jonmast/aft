use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::time::{Instant, UNIX_EPOCH};

use rayon::prelude::*;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::cache_freshness::{self, FileFreshness};
use crate::inspect::{
    CallgraphOutboundCall, CallgraphSnapshot, FileContribution, InspectCategory, InspectJob,
    InspectResult, InspectScanSuccess,
};

const MAX_DRILL_DOWN_ITEMS: usize = 100;
const JS_MODULE_EXTENSIONS: &[&str] = &["ts", "tsx", "js", "jsx", "mts", "cts", "mjs", "cjs"];

type ExportNode = (String, String);

pub fn run_dead_code_scan(job: &InspectJob) -> InspectResult {
    let started = Instant::now();

    let Some(snapshot) = job.callgraph_snapshot.as_deref() else {
        let success = InspectScanSuccess {
            scanned_files: job.scope_files.clone(),
            contributions: Vec::new(),
            aggregate: callgraph_unavailable_aggregate(job.scope_files.len()),
        };
        return InspectResult::success(job, success, started.elapsed());
    };

    let entry_point_files = snapshot
        .entry_points
        .iter()
        .map(|file| relative_path(&job.project_root, file))
        .collect::<BTreeSet<_>>();
    let (exported_symbols_by_file, files_by_exported_symbol) =
        exported_symbol_indexes(job, snapshot);

    let contributions = job
        .scope_files
        .par_iter()
        .map(|file| {
            gather_file_contribution(
                job,
                snapshot,
                file,
                &exported_symbols_by_file,
                &files_by_exported_symbol,
                &entry_point_files,
            )
        })
        .collect::<Vec<_>>();

    let public_api_files = collect_public_api_files(&job.project_root);
    let aggregate = aggregate_dead_code_contributions(&contributions, &public_api_files);
    let success = InspectScanSuccess {
        scanned_files: job.scope_files.clone(),
        contributions,
        aggregate,
    };

    InspectResult::success(job, success, started.elapsed())
}

fn exported_symbol_indexes(
    job: &InspectJob,
    snapshot: &CallgraphSnapshot,
) -> (
    BTreeMap<String, BTreeSet<String>>,
    BTreeMap<String, BTreeSet<String>>,
) {
    let mut exported_symbols_by_file: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut files_by_exported_symbol: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

    for export in &snapshot.exported_symbols {
        let file = relative_path(&job.project_root, &export.file);
        exported_symbols_by_file
            .entry(file.clone())
            .or_default()
            .insert(export.symbol.clone());
        files_by_exported_symbol
            .entry(export.symbol.clone())
            .or_default()
            .insert(file);
    }

    (exported_symbols_by_file, files_by_exported_symbol)
}

fn gather_file_contribution(
    job: &InspectJob,
    snapshot: &CallgraphSnapshot,
    file: &Path,
    exported_symbols_by_file: &BTreeMap<String, BTreeSet<String>>,
    files_by_exported_symbol: &BTreeMap<String, BTreeSet<String>>,
    entry_point_files: &BTreeSet<String>,
) -> FileContribution {
    let file_name = relative_path(&job.project_root, file);
    let is_entry_point_file = entry_point_files.contains(&file_name);
    let exports = snapshot
        .exported_symbols
        .iter()
        .filter(|export| same_file(&job.project_root, &export.file, file))
        .map(|export| {
            json!({
                "symbol": export.symbol,
                "kind": export.kind,
                "line": export.line,
                "is_entry_point": is_entry_point_file,
            })
        })
        .collect::<Vec<_>>();

    let mut internal_calls = snapshot
        .outbound_calls
        .iter()
        .filter(|call| same_file(&job.project_root, &call.caller_file, file))
        .filter_map(|call| {
            project_internal_call(
                &job.project_root,
                call,
                &file_name,
                exported_symbols_by_file,
                files_by_exported_symbol,
            )
        })
        .collect::<Vec<_>>();
    internal_calls.sort_by(|left, right| {
        left.file
            .cmp(&right.file)
            .then_with(|| left.symbol.cmp(&right.symbol))
            .then_with(|| left.line.cmp(&right.line))
    });
    internal_calls.dedup_by(|left, right| {
        left.file == right.file && left.symbol == right.symbol && left.line == right.line
    });

    FileContribution::new(
        InspectCategory::DeadCode,
        file.to_path_buf(),
        collect_freshness(file),
        json!({
            "file": file_name,
            "exports": exports,
            "internal_calls": internal_calls
                .into_iter()
                .map(|call| json!({
                    "file": call.file,
                    "symbol": call.symbol,
                    "line": call.line,
                }))
                .collect::<Vec<_>>(),
        }),
    )
}

pub(crate) fn callgraph_unavailable_aggregate(scanned_files: usize) -> serde_json::Value {
    json!({
        "count": 0,
        "items": [],
        "by_language": {},
        "drill_down_capped": false,
        "callgraph_available": false,
        "scanned_files": scanned_files,
        "notes": ["callgraph_unavailable"],
    })
}

pub(crate) fn aggregate_dead_code_contributions(
    contributions: &[FileContribution],
    public_api_files: &BTreeSet<String>,
) -> serde_json::Value {
    aggregate_dead_code_contributions_with_limit(
        contributions,
        public_api_files,
        Some(MAX_DRILL_DOWN_ITEMS),
    )
}

pub(crate) fn aggregate_dead_code_contributions_with_limit(
    contributions: &[FileContribution],
    public_api_files: &BTreeSet<String>,
    drill_down_limit: Option<usize>,
) -> serde_json::Value {
    let parsed = contributions
        .iter()
        .filter_map(|contribution| {
            serde_json::from_value::<DeadCodeContribution>(contribution.contribution.clone()).ok()
        })
        .collect::<Vec<_>>();

    let export_nodes = export_nodes(&parsed);
    let edges_by_source = edges_by_source_export(&parsed, &export_nodes);
    let reachable = reachable_exports(&parsed, &edges_by_source);

    let mut by_language: BTreeMap<String, usize> = BTreeMap::new();
    let mut dead_items = Vec::new();
    for contribution in &parsed {
        let is_public_api_file = public_api_files.contains(&contribution.file);
        for export in &contribution.exports {
            let node = (contribution.file.clone(), export.symbol.clone());
            if reachable.contains(&node) || is_public_api_file {
                continue;
            }

            *by_language
                .entry(language_for_file(&contribution.file).to_string())
                .or_default() += 1;
            dead_items.push(json!({
                "file": contribution.file,
                "symbol": export.symbol,
                "kind": export.kind,
                "line": export.line,
            }));
        }
    }

    let count = dead_items.len();
    let drill_down_capped = drill_down_limit.is_some_and(|limit| count > limit);
    if let Some(limit) = drill_down_limit {
        dead_items.truncate(limit);
    }

    json!({
        "count": count,
        "items": dead_items,
        "by_language": by_language,
        "drill_down_capped": drill_down_capped,
        "callgraph_available": true,
        "scanned_files": contributions.len(),
    })
}

fn export_nodes(contributions: &[DeadCodeContribution]) -> BTreeSet<ExportNode> {
    contributions
        .iter()
        .flat_map(|contribution| {
            contribution
                .exports
                .iter()
                .map(|export| (contribution.file.clone(), export.symbol.clone()))
        })
        .collect()
}

fn edges_by_source_export(
    contributions: &[DeadCodeContribution],
    export_nodes: &BTreeSet<ExportNode>,
) -> BTreeMap<ExportNode, BTreeSet<ExportNode>> {
    let mut edges: BTreeMap<ExportNode, BTreeSet<ExportNode>> = BTreeMap::new();

    for contribution in contributions {
        for call in &contribution.internal_calls {
            let target = (call.file.clone(), call.symbol.clone());
            if !export_nodes.contains(&target) {
                continue;
            }

            if let Some(source) = source_export_for_call(contribution, call.line)
                .or_else(|| single_entry_point_export(contribution))
            {
                let source = (contribution.file.clone(), source.symbol.clone());
                if export_nodes.contains(&source) {
                    edges.entry(source).or_default().insert(target);
                }
            }
        }
    }

    edges
}

fn source_export_for_call(
    contribution: &DeadCodeContribution,
    line: u32,
) -> Option<&ExportContribution> {
    contribution
        .exports
        .iter()
        .filter(|export| export.line <= line)
        .max_by_key(|export| export.line)
}

fn single_entry_point_export(contribution: &DeadCodeContribution) -> Option<&ExportContribution> {
    let mut entry_exports = contribution
        .exports
        .iter()
        .filter(|export| export.is_entry_point);
    let first = entry_exports.next()?;
    if entry_exports.next().is_none() {
        Some(first)
    } else {
        None
    }
}

fn reachable_exports(
    contributions: &[DeadCodeContribution],
    edges_by_source: &BTreeMap<ExportNode, BTreeSet<ExportNode>>,
) -> BTreeSet<ExportNode> {
    let mut reachable = BTreeSet::new();
    let mut queue = VecDeque::new();

    for contribution in contributions {
        for export in &contribution.exports {
            if export.is_entry_point {
                queue.push_back((contribution.file.clone(), export.symbol.clone()));
            }
        }
    }

    while let Some(node) = queue.pop_front() {
        if !reachable.insert(node.clone()) {
            continue;
        }
        if let Some(targets) = edges_by_source.get(&node) {
            for target in targets {
                if !reachable.contains(target) {
                    queue.push_back(target.clone());
                }
            }
        }
    }

    reachable
}

fn project_internal_call(
    project_root: &Path,
    call: &CallgraphOutboundCall,
    caller_file: &str,
    exported_symbols_by_file: &BTreeMap<String, BTreeSet<String>>,
    files_by_exported_symbol: &BTreeMap<String, BTreeSet<String>>,
) -> Option<InternalCall> {
    let target = parse_target(project_root, &call.target);
    let symbol = target.symbol?;
    let file = match target.file {
        Some(file) => {
            if exported_symbols_by_file
                .get(&file)
                .is_some_and(|symbols| symbols.contains(&symbol))
            {
                file
            } else {
                return None;
            }
        }
        None => resolve_unqualified_target(
            caller_file,
            &symbol,
            exported_symbols_by_file,
            files_by_exported_symbol,
        )?,
    };

    Some(InternalCall {
        file,
        symbol,
        line: call.line,
    })
}

fn resolve_unqualified_target(
    caller_file: &str,
    symbol: &str,
    exported_symbols_by_file: &BTreeMap<String, BTreeSet<String>>,
    files_by_exported_symbol: &BTreeMap<String, BTreeSet<String>>,
) -> Option<String> {
    if exported_symbols_by_file
        .get(caller_file)
        .is_some_and(|symbols| symbols.contains(symbol))
    {
        return Some(caller_file.to_string());
    }

    let files = files_by_exported_symbol.get(symbol)?;
    if files.len() == 1 {
        files.iter().next().cloned()
    } else {
        None
    }
}

fn parse_target(project_root: &Path, target: &str) -> ParsedTarget {
    let trimmed = target.trim();
    if trimmed.is_empty() {
        return ParsedTarget {
            file: None,
            symbol: None,
        };
    }

    if let Some((file, symbol)) = trimmed.rsplit_once("::") {
        return ParsedTarget {
            file: Some(relative_path(project_root, Path::new(file))),
            symbol: clean_symbol(symbol),
        };
    }

    if let Some((file, symbol)) = trimmed.rsplit_once('#') {
        return ParsedTarget {
            file: Some(relative_path(project_root, Path::new(file))),
            symbol: clean_symbol(symbol),
        };
    }

    ParsedTarget {
        file: None,
        symbol: clean_symbol(trimmed),
    }
}

fn clean_symbol(symbol: &str) -> Option<String> {
    let trimmed = symbol.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

pub(crate) fn collect_public_api_files(project_root: &Path) -> BTreeSet<String> {
    let mut files = BTreeSet::new();
    collect_package_public_api(project_root, project_root, &mut files);

    let package_json = project_root.join("package.json");
    let Ok(bytes) = std::fs::read(&package_json) else {
        return files;
    };
    let Ok(package) = serde_json::from_slice::<Value>(&bytes) else {
        return files;
    };

    for workspace in workspace_dirs(project_root, &package) {
        collect_package_public_api(project_root, &workspace, &mut files);
    }

    files
}

fn collect_package_public_api(
    project_root: &Path,
    package_dir: &Path,
    files: &mut BTreeSet<String>,
) {
    let package_json = package_dir.join("package.json");
    let Ok(bytes) = std::fs::read(package_json) else {
        return;
    };
    let Ok(package) = serde_json::from_slice::<Value>(&bytes) else {
        return;
    };

    if let Some(main) = package.get("main").and_then(Value::as_str) {
        insert_public_api_path(project_root, package_dir, main, files);
    }
    if let Some(module) = package.get("module").and_then(Value::as_str) {
        insert_public_api_path(project_root, package_dir, module, files);
    }
    if let Some(exports) = package.get("exports") {
        collect_export_values(project_root, package_dir, exports, files);
    }
}

fn collect_export_values(
    project_root: &Path,
    package_dir: &Path,
    value: &Value,
    files: &mut BTreeSet<String>,
) {
    match value {
        Value::String(path) => insert_public_api_path(project_root, package_dir, path, files),
        Value::Array(values) => {
            for value in values {
                collect_export_values(project_root, package_dir, value, files);
            }
        }
        Value::Object(map) => {
            for value in map.values() {
                collect_export_values(project_root, package_dir, value, files);
            }
        }
        _ => {}
    }
}

fn insert_public_api_path(
    project_root: &Path,
    package_dir: &Path,
    value: &str,
    files: &mut BTreeSet<String>,
) {
    if value.starts_with('#') || value.contains('*') {
        return;
    }

    let trimmed = value.trim();
    if trimmed.is_empty() {
        return;
    }

    if let Some(path) = resolve_package_entry(package_dir, trimmed) {
        files.insert(relative_path(project_root, &path));
    }
}

fn resolve_package_entry(package_dir: &Path, entry: &str) -> Option<PathBuf> {
    if entry.starts_with("node:") || entry.contains("://") {
        return None;
    }

    let entry_path = if is_relative_module(entry) {
        package_dir.join(entry)
    } else {
        package_dir.join(entry.trim_start_matches('/'))
    };

    candidate_paths(&entry_path)
        .into_iter()
        .map(|candidate| normalize_path(&candidate))
        .find(|candidate| candidate.is_file())
}

fn candidate_paths(base: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    candidates.push(base.to_path_buf());

    if base.extension().is_none() {
        for extension in JS_MODULE_EXTENSIONS {
            candidates.push(base.with_extension(extension));
        }
    }

    for extension in JS_MODULE_EXTENSIONS {
        candidates.push(base.join(format!("index.{extension}")));
    }

    candidates
}

fn is_relative_module(module_path: &str) -> bool {
    module_path.starts_with("./")
        || module_path.starts_with("../")
        || module_path == "."
        || module_path == ".."
}

fn workspace_dirs(project_root: &Path, package: &Value) -> Vec<PathBuf> {
    let Some(workspaces) = package.get("workspaces") else {
        return Vec::new();
    };

    let patterns = match workspaces {
        Value::Array(values) => values.iter().filter_map(Value::as_str).collect(),
        Value::Object(map) => map
            .get("packages")
            .and_then(Value::as_array)
            .map(|values| values.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default(),
        _ => Vec::new(),
    };

    let mut dirs = Vec::new();
    for pattern in patterns {
        let pattern = pattern.trim_end_matches('/');
        if let Some(prefix) = pattern.strip_suffix("/*") {
            let parent = project_root.join(prefix);
            let Ok(entries) = std::fs::read_dir(parent) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.join("package.json").is_file() {
                    dirs.push(path);
                }
            }
        } else {
            let path = project_root.join(pattern);
            if path.join("package.json").is_file() {
                dirs.push(path);
            }
        }
    }
    dirs
}

fn language_for_file(file: &str) -> &'static str {
    let extension = Path::new(file)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
        .unwrap_or_default();

    match extension.as_str() {
        "rs" => "rust",
        "ts" | "tsx" | "mts" | "cts" => "typescript",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "py" => "python",
        "go" => "go",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => "cpp",
        "zig" => "zig",
        "cs" => "csharp",
        "sh" | "bash" | "zsh" | "fish" => "bash",
        "html" | "htm" => "html",
        "md" | "markdown" => "markdown",
        "sol" => "solidity",
        "vue" => "vue",
        "json" => "json",
        "scala" => "scala",
        "java" => "java",
        "rb" => "ruby",
        "kt" | "kts" => "kotlin",
        "swift" => "swift",
        "php" => "php",
        "lua" => "lua",
        "pl" | "pm" => "perl",
        _ => "unknown",
    }
}

fn collect_freshness(file: &Path) -> FileFreshness {
    cache_freshness::collect(file).unwrap_or_else(|_| FileFreshness {
        mtime: UNIX_EPOCH,
        size: 0,
        content_hash: cache_freshness::zero_hash(),
    })
}

fn same_file(project_root: &Path, left: &Path, right: &Path) -> bool {
    normalize_absolute(project_root, left) == normalize_absolute(project_root, right)
}

fn relative_path(project_root: &Path, path: &Path) -> String {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        project_root.join(path)
    };
    let normalized = normalize_path(&absolute);
    normalized
        .strip_prefix(&normalize_path(project_root))
        .unwrap_or(normalized.as_path())
        .to_string_lossy()
        .replace('\\', "/")
}

fn normalize_absolute(project_root: &Path, path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        project_root.join(path)
    };
    normalize_path(&absolute)
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push(component.as_os_str());
                }
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

#[derive(Debug, Clone, Deserialize)]
struct DeadCodeContribution {
    file: String,
    exports: Vec<ExportContribution>,
    internal_calls: Vec<InternalCallContribution>,
}

#[derive(Debug, Clone, Deserialize)]
struct ExportContribution {
    symbol: String,
    kind: String,
    line: u32,
    #[serde(default)]
    is_entry_point: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct InternalCallContribution {
    file: String,
    symbol: String,
    line: u32,
}

#[derive(Debug, Clone)]
struct InternalCall {
    file: String,
    symbol: String,
    line: u32,
}

#[derive(Debug, Clone)]
struct ParsedTarget {
    file: Option<String>,
    symbol: Option<String>,
}
