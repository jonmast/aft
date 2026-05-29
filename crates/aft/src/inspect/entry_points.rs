use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::job::normalize_path;

const JS_MODULE_EXTENSIONS: &[&str] = &["ts", "tsx", "js", "jsx", "mts", "cts", "mjs", "cjs"];

#[derive(Debug, Clone, Default)]
pub(crate) struct EntryPointSet {
    entry_points: BTreeSet<PathBuf>,
    public_api_files: BTreeSet<PathBuf>,
    warnings: Vec<String>,
}

impl EntryPointSet {
    pub(crate) fn is_entry_point(&self, file: &Path) -> bool {
        contains_path(&self.entry_points, file)
    }

    pub(crate) fn is_public_api_file(&self, file: &Path) -> bool {
        contains_path(&self.public_api_files, file)
    }

    pub(crate) fn public_api_files_relative(&self, project_root: &Path) -> BTreeSet<String> {
        self.public_api_files
            .iter()
            .map(|file| relative_path(project_root, file))
            .collect()
    }

    pub(crate) fn warnings(&self) -> &[String] {
        &self.warnings
    }

    fn insert_entry_point(&mut self, path: &Path) {
        let path = snapshot_path(path);
        self.entry_points.insert(path.clone());
        self.public_api_files.insert(path);
    }

    fn warn(&mut self, message: String) {
        self.warnings.push(message);
    }
}

#[derive(Debug, Default)]
struct ManifestPaths {
    cargo_tomls: Vec<PathBuf>,
    package_jsons: Vec<PathBuf>,
}

pub(crate) fn resolve_entry_points(project_root: &Path) -> EntryPointSet {
    let project_root = snapshot_path(project_root);
    let mut manifests = ManifestPaths::default();
    collect_manifests(&project_root, &mut manifests);

    let manifest_found = !manifests.cargo_tomls.is_empty() || !manifests.package_jsons.is_empty();
    let mut entry_points = EntryPointSet::default();

    for manifest in manifests.cargo_tomls {
        collect_cargo_manifest_entry_points(&manifest, &mut entry_points);
    }
    for manifest in manifests.package_jsons {
        collect_package_manifest_entry_points(&manifest, &mut entry_points);
    }

    if !manifest_found {
        collect_fallback_entry_points(&project_root, &mut entry_points);
    }

    entry_points
}

fn collect_manifests(project_root: &Path, manifests: &mut ManifestPaths) {
    let mut stack = vec![project_root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        let mut entries = entries.filter_map(Result::ok).collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.path());

        for entry in entries {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };

            if file_type.is_dir() {
                if !should_skip_manifest_dir(&path) {
                    stack.push(path);
                }
                continue;
            }

            if !file_type.is_file() {
                continue;
            }

            match path.file_name().and_then(|name| name.to_str()) {
                Some("Cargo.toml") => manifests.cargo_tomls.push(path),
                Some("package.json") => manifests.package_jsons.push(path),
                _ => {}
            }
        }
    }

    manifests.cargo_tomls.sort();
    manifests.cargo_tomls.dedup();
    manifests.package_jsons.sort();
    manifests.package_jsons.dedup();
}

fn should_skip_manifest_dir(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some(".git" | "node_modules" | "target" | ".aft-test-storage" | ".aft-cache")
    )
}

fn collect_cargo_manifest_entry_points(manifest: &Path, entry_points: &mut EntryPointSet) {
    let package_dir = manifest.parent().unwrap_or_else(|| Path::new("."));
    let source = match fs::read_to_string(manifest) {
        Ok(source) => source,
        Err(error) => {
            entry_points.warn(format!("failed to read {}: {error}", manifest.display()));
            return;
        }
    };
    let value = match source.parse::<toml::Value>() {
        Ok(value) => value,
        Err(error) => {
            entry_points.warn(format!("failed to parse {}: {error}", manifest.display()));
            return;
        }
    };

    let has_package = value.get("package").is_some_and(toml::Value::is_table);
    collect_cargo_lib_target(package_dir, &value, has_package, entry_points);
    collect_cargo_bin_targets(package_dir, &value, has_package, entry_points);
}

fn collect_cargo_lib_target(
    package_dir: &Path,
    value: &toml::Value,
    has_package: bool,
    entry_points: &mut EntryPointSet,
) {
    if let Some(lib) = value.get("lib") {
        if let Some(path) = lib.get("path").and_then(toml::Value::as_str) {
            insert_existing_entry_point(entry_points, &package_dir.join(path));
        } else {
            insert_existing_entry_point(entry_points, &package_dir.join("src/lib.rs"));
        }
        return;
    }

    if has_package && package_autodiscovery_enabled(value, "autolib") {
        insert_existing_entry_point(entry_points, &package_dir.join("src/lib.rs"));
    }
}

fn collect_cargo_bin_targets(
    package_dir: &Path,
    value: &toml::Value,
    has_package: bool,
    entry_points: &mut EntryPointSet,
) {
    if let Some(bins) = value.get("bin").and_then(toml::Value::as_array) {
        for bin in bins {
            if let Some(path) = bin.get("path").and_then(toml::Value::as_str) {
                insert_existing_entry_point(entry_points, &package_dir.join(path));
            } else if let Some(name) = bin.get("name").and_then(toml::Value::as_str) {
                let named_bin = package_dir.join("src/bin").join(format!("{name}.rs"));
                if named_bin.is_file() {
                    insert_existing_entry_point(entry_points, &named_bin);
                } else {
                    insert_existing_entry_point(entry_points, &package_dir.join("src/main.rs"));
                }
            } else {
                insert_existing_entry_point(entry_points, &package_dir.join("src/main.rs"));
            }
        }
    }

    if has_package && package_autodiscovery_enabled(value, "autobins") {
        insert_existing_entry_point(entry_points, &package_dir.join("src/main.rs"));
        collect_cargo_src_bin_targets(package_dir, entry_points);
    }
}

fn package_autodiscovery_enabled(value: &toml::Value, key: &str) -> bool {
    value
        .get("package")
        .and_then(|package| package.get(key))
        .and_then(toml::Value::as_bool)
        .unwrap_or(true)
}

fn collect_cargo_src_bin_targets(package_dir: &Path, entry_points: &mut EntryPointSet) {
    let src_bin = package_dir.join("src/bin");
    let Ok(entries) = fs::read_dir(src_bin) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) == Some("rs") && path.is_file()
        {
            insert_existing_entry_point(entry_points, &path);
        }
    }
}

fn collect_package_manifest_entry_points(manifest: &Path, entry_points: &mut EntryPointSet) {
    let package_dir = manifest.parent().unwrap_or_else(|| Path::new("."));
    let source = match fs::read_to_string(manifest) {
        Ok(source) => source,
        Err(error) => {
            entry_points.warn(format!("failed to read {}: {error}", manifest.display()));
            return;
        }
    };
    let value = match serde_json::from_str::<Value>(&source) {
        Ok(value) => value,
        Err(error) => {
            entry_points.warn(format!("failed to parse {}: {error}", manifest.display()));
            return;
        }
    };

    let mut entries = BTreeSet::new();
    if let Some(main) = value.get("main").and_then(Value::as_str) {
        entries.insert(main.to_string());
    }
    if let Some(module) = value.get("module").and_then(Value::as_str) {
        entries.insert(module.to_string());
    }
    if let Some(exports) = value.get("exports") {
        collect_json_entry_strings(exports, &mut entries);
    }
    if let Some(bin) = value.get("bin") {
        collect_json_entry_strings(bin, &mut entries);
    }

    for entry in entries {
        insert_package_entry(package_dir, &entry, entry_points);
    }
}

fn collect_json_entry_strings(value: &Value, entries: &mut BTreeSet<String>) {
    match value {
        Value::String(entry) => {
            entries.insert(entry.clone());
        }
        Value::Array(values) => {
            for value in values {
                collect_json_entry_strings(value, entries);
            }
        }
        Value::Object(map) => {
            for value in map.values() {
                collect_json_entry_strings(value, entries);
            }
        }
        _ => {}
    }
}

fn insert_package_entry(package_dir: &Path, entry: &str, entry_points: &mut EntryPointSet) {
    let entry = entry.trim();
    if entry.is_empty() || entry.starts_with('#') || entry.contains('*') {
        return;
    }

    if let Some(path) = resolve_package_entry(package_dir, entry) {
        entry_points.insert_entry_point(&path);
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

fn collect_fallback_entry_points(project_root: &Path, entry_points: &mut EntryPointSet) {
    let mut stack = vec![project_root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };

            if file_type.is_dir() {
                if !should_skip_manifest_dir(&path) {
                    stack.push(path);
                }
                continue;
            }

            if file_type.is_file() && is_conventional_entry_point(project_root, &path) {
                entry_points.insert_entry_point(&path);
            }
        }
    }
}

fn is_conventional_entry_point(project_root: &Path, file: &Path) -> bool {
    let relative = file.strip_prefix(project_root).unwrap_or(file);
    let relative_display = relative.to_string_lossy().replace('\\', "/");
    if relative_display.starts_with("bin/") || relative_display.contains("/bin/") {
        return true;
    }

    let Some(file_name) = relative.file_name().and_then(|name| name.to_str()) else {
        return false;
    };

    matches!(
        file_name,
        "main.rs"
            | "lib.rs"
            | "main.ts"
            | "main.tsx"
            | "main.js"
            | "main.jsx"
            | "main.py"
            | "main.go"
            | "index.ts"
            | "index.tsx"
            | "index.js"
            | "index.jsx"
    )
}

fn insert_existing_entry_point(entry_points: &mut EntryPointSet, path: &Path) {
    if path.is_file() {
        entry_points.insert_entry_point(path);
    }
}

fn contains_path(paths: &BTreeSet<PathBuf>, file: &Path) -> bool {
    let snapshot = snapshot_path(file);
    paths.contains(&snapshot) || paths.contains(&normalize_path(file))
}

fn snapshot_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| normalize_path(path))
}

fn relative_path(project_root: &Path, path: &Path) -> String {
    let project_root = snapshot_path(project_root);
    let path = snapshot_path(path);
    path.strip_prefix(&project_root)
        .unwrap_or(path.as_path())
        .to_string_lossy()
        .replace('\\', "/")
}
