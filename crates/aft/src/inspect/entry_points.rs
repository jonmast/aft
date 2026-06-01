use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::job::normalize_path;

const JS_MODULE_EXTENSIONS: &[&str] = &["ts", "tsx", "js", "jsx", "mts", "cts", "mjs", "cjs"];

#[derive(Debug, Clone, Default)]
pub(crate) struct EntryPointSet {
    liveness_root_files: BTreeSet<PathBuf>,
    public_api_files: BTreeSet<PathBuf>,
    warnings: Vec<String>,
}

impl EntryPointSet {
    pub(crate) fn is_entry_point(&self, file: &Path) -> bool {
        self.is_liveness_root_file(file)
    }

    pub(crate) fn is_liveness_root_file(&self, file: &Path) -> bool {
        contains_path(&self.liveness_root_files, file)
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

    fn insert_liveness_root(&mut self, path: &Path) {
        self.liveness_root_files.insert(snapshot_path(path));
    }

    fn insert_public_api_file(&mut self, path: &Path) {
        let path = snapshot_path(path);
        self.liveness_root_files.insert(path.clone());
        self.public_api_files.insert(path);
    }

    fn warn(&mut self, message: String) {
        self.warnings.push(message);
    }

    fn has_liveness_roots(&self) -> bool {
        !self.liveness_root_files.is_empty()
    }
}

#[derive(Debug, Default)]
struct ManifestPaths {
    cargo_tomls: Vec<PathBuf>,
    package_jsons: Vec<PathBuf>,
}

#[derive(Debug, Clone, Copy)]
enum EntryPointKind {
    LivenessRoot,
    PublicApi,
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

    // A manifest can be present without declaring any runnable or public root
    // (for example a private package.json or a virtual Cargo workspace). In
    // that case, keep the conventional fallback so liveness analysis never
    // starts from an empty root set solely because a manifest exists.
    if !manifest_found || !entry_points.has_liveness_roots() {
        collect_fallback_entry_points(&project_root, &mut entry_points);
    }

    entry_points
}

pub(crate) fn collect_entry_point_manifests(project_root: &Path) -> Vec<PathBuf> {
    let project_root = snapshot_path(project_root);
    let mut manifests = ManifestPaths::default();
    collect_manifests(&project_root, &mut manifests);

    let mut paths = manifests.cargo_tomls;
    paths.extend(manifests.package_jsons);
    paths.sort();
    paths.dedup();
    paths
}

fn collect_manifests(project_root: &Path, manifests: &mut ManifestPaths) {
    for path in entry_point_walk_files(project_root) {
        match path.file_name().and_then(|name| name.to_str()) {
            Some("Cargo.toml") => manifests.cargo_tomls.push(path),
            Some("package.json") => manifests.package_jsons.push(path),
            _ => {}
        }
    }

    manifests.cargo_tomls.sort();
    manifests.cargo_tomls.dedup();
    manifests.package_jsons.sort();
    manifests.package_jsons.dedup();
}

fn entry_point_walk_files(project_root: &Path) -> Vec<PathBuf> {
    let mut builder = ignore::WalkBuilder::new(project_root);
    builder
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .require_git(false)
        .add_custom_ignore_filename(".aftignore")
        .filter_entry(|entry| {
            !entry
                .file_type()
                .is_some_and(|file_type| file_type.is_dir())
                || !should_skip_manifest_dir(entry.path())
        });

    let mut files = builder
        .build()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_type()
                .is_some_and(|file_type| file_type.is_file())
        })
        .map(|entry| entry.into_path())
        .collect::<Vec<_>>();
    files.sort();
    files
}

fn should_skip_manifest_dir(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some(
            ".git"
                | "node_modules"
                | "target"
                | "venv"
                | ".venv"
                | "__pycache__"
                | ".tox"
                | "dist"
                | "build"
                | ".aft-test-storage"
                | ".aft-cache"
        )
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
            insert_existing_entry_point(
                entry_points,
                &package_dir.join(path),
                EntryPointKind::PublicApi,
            );
        } else {
            insert_existing_entry_point(
                entry_points,
                &package_dir.join("src/lib.rs"),
                EntryPointKind::PublicApi,
            );
        }
        return;
    }

    if has_package && package_autodiscovery_enabled(value, "autolib") {
        insert_existing_entry_point(
            entry_points,
            &package_dir.join("src/lib.rs"),
            EntryPointKind::PublicApi,
        );
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
                insert_existing_entry_point(
                    entry_points,
                    &package_dir.join(path),
                    EntryPointKind::LivenessRoot,
                );
            } else if let Some(name) = bin.get("name").and_then(toml::Value::as_str) {
                let named_bin = package_dir.join("src/bin").join(format!("{name}.rs"));
                if named_bin.is_file() {
                    insert_existing_entry_point(
                        entry_points,
                        &named_bin,
                        EntryPointKind::LivenessRoot,
                    );
                } else {
                    insert_existing_entry_point(
                        entry_points,
                        &package_dir.join("src/main.rs"),
                        EntryPointKind::LivenessRoot,
                    );
                }
            } else {
                insert_existing_entry_point(
                    entry_points,
                    &package_dir.join("src/main.rs"),
                    EntryPointKind::LivenessRoot,
                );
            }
        }
    }

    if has_package && package_autodiscovery_enabled(value, "autobins") {
        insert_existing_entry_point(
            entry_points,
            &package_dir.join("src/main.rs"),
            EntryPointKind::LivenessRoot,
        );
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
            insert_existing_entry_point(entry_points, &path, EntryPointKind::LivenessRoot);
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

    let mut public_entries = BTreeSet::new();
    if let Some(main) = value.get("main").and_then(Value::as_str) {
        public_entries.insert(main.to_string());
    }
    if let Some(module) = value.get("module").and_then(Value::as_str) {
        public_entries.insert(module.to_string());
    }
    if let Some(exports) = value.get("exports") {
        collect_json_entry_strings(exports, &mut public_entries);
    }

    let mut bin_entries = BTreeSet::new();
    if let Some(bin) = value.get("bin") {
        collect_json_entry_strings(bin, &mut bin_entries);
    }

    for entry in public_entries {
        insert_package_entry(package_dir, &entry, entry_points, EntryPointKind::PublicApi);
    }
    for entry in bin_entries {
        insert_package_entry(
            package_dir,
            &entry,
            entry_points,
            EntryPointKind::LivenessRoot,
        );
    }

    if let Some(scripts) = value.get("scripts").and_then(Value::as_object) {
        for command in scripts.values().filter_map(Value::as_str) {
            collect_script_entry_points(package_dir, command, entry_points);
        }
    }
}

/// Extract local source files referenced by an npm `scripts` command (e.g.
/// `bun run benchmarks/src/runner.ts`, `node scripts/build.mjs`, `tsx x.ts`).
/// These are runnable roots — their reachable code is live even though nothing
/// imports them — so harness/script code is not reported dead. Conservative:
/// only tokens that resolve to an existing local source file are added; flags,
/// bare binaries, config files, and node_modules refs are ignored.
fn collect_script_entry_points(
    package_dir: &Path,
    command: &str,
    entry_points: &mut EntryPointSet,
) {
    let is_separator =
        |c: char| c.is_whitespace() || matches!(c, '&' | '|' | ';' | '(' | ')' | '<' | '>' | ',');
    for token in command.split(is_separator) {
        let token = token
            .trim()
            .trim_matches(|c| c == '"' || c == '\'' || c == '`');
        if token.is_empty() || token.starts_with('-') || token.contains("node_modules") {
            continue;
        }
        let is_source = Path::new(token)
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| JS_MODULE_EXTENSIONS.contains(&extension));
        if !is_source {
            continue;
        }
        let candidate = normalize_path(&package_dir.join(token));
        if candidate.is_file() {
            insert_resolved_entry_point(entry_points, &candidate, EntryPointKind::LivenessRoot);
        }
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

fn insert_package_entry(
    package_dir: &Path,
    entry: &str,
    entry_points: &mut EntryPointSet,
    kind: EntryPointKind,
) {
    let entry = entry.trim();
    if entry.is_empty() || entry.starts_with('#') || entry.contains('*') {
        return;
    }

    if let Some(path) = resolve_package_entry(package_dir, entry) {
        insert_resolved_entry_point(entry_points, &path, kind);
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
    for path in entry_point_walk_files(project_root) {
        if let Some(kind) = conventional_entry_point_kind(project_root, &path) {
            insert_resolved_entry_point(entry_points, &path, kind);
        }
    }
}

fn conventional_entry_point_kind(project_root: &Path, file: &Path) -> Option<EntryPointKind> {
    let relative = file.strip_prefix(project_root).unwrap_or(file);
    let relative_display = relative.to_string_lossy().replace('\\', "/");
    if relative_display.starts_with("bin/") || relative_display.contains("/bin/") {
        return Some(EntryPointKind::LivenessRoot);
    }

    let file_name = relative.file_name().and_then(|name| name.to_str())?;
    match file_name {
        "lib.rs" | "index.ts" | "index.tsx" | "index.js" | "index.jsx" => {
            Some(EntryPointKind::PublicApi)
        }
        "main.rs" | "main.ts" | "main.tsx" | "main.js" | "main.jsx" | "main.py" | "main.go" => {
            Some(EntryPointKind::LivenessRoot)
        }
        _ => None,
    }
}

fn insert_existing_entry_point(
    entry_points: &mut EntryPointSet,
    path: &Path,
    kind: EntryPointKind,
) {
    if path.is_file() {
        insert_resolved_entry_point(entry_points, path, kind);
    }
}

fn insert_resolved_entry_point(
    entry_points: &mut EntryPointSet,
    path: &Path,
    kind: EntryPointKind,
) {
    match kind {
        EntryPointKind::LivenessRoot => entry_points.insert_liveness_root(path),
        EntryPointKind::PublicApi => entry_points.insert_public_api_file(path),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_scripts_seed_source_files_as_liveness_roots() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("scripts")).unwrap();
        std::fs::write(root.join("src/runner.ts"), "export const x = 1;\n").unwrap();
        std::fs::write(root.join("scripts/build.mjs"), "console.log(1)\n").unwrap();
        // A private package (no main/module/exports/bin) with only scripts.
        std::fs::write(
            root.join("package.json"),
            r#"{
                "name": "x",
                "private": true,
                "scripts": {
                    "bench": "bun run src/runner.ts",
                    "build": "node scripts/build.mjs --flag",
                    "lint": "biome check .",
                    "missing": "bun run src/does-not-exist.ts"
                }
            }"#,
        )
        .unwrap();

        let entry_points = resolve_entry_points(root);
        // Real script targets become liveness roots.
        assert!(entry_points.is_liveness_root_file(&root.join("src/runner.ts")));
        assert!(entry_points.is_liveness_root_file(&root.join("scripts/build.mjs")));
        // Non-file tokens (binaries, flags, ".") and missing files are ignored.
        assert!(!entry_points.is_liveness_root_file(&root.join("src/does-not-exist.ts")));
        // Script roots are liveness roots, not public-API surfaces.
        assert!(!entry_points.is_public_api_file(&root.join("src/runner.ts")));
    }
}
