use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::AftError;
use sha2::{Digest, Sha256};

const MAX_UNDO_DEPTH: usize = 20;

/// Current on-disk backup metadata schema version.
///
/// Bump this when the `meta.json` shape changes. Readers check the field and
/// refuse or migrate older versions instead of misinterpreting them.
const SCHEMA_VERSION: u32 = 3;

/// A single backup entry for a file.
#[derive(Debug, Clone)]
pub struct BackupEntry {
    pub backup_id: String,
    pub content: String,
    pub timestamp: u64,
    pub description: String,
    pub op_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RestoredOperation {
    pub op_id: String,
    pub restored: Vec<RestoredFile>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct RestoredFile {
    pub path: PathBuf,
    pub backup_id: String,
}

/// Per-(session, file) undo store with optional disk persistence.
///
/// Introduced alongside project-shared bridges (issue #14): one bridge can now
/// serve many OpenCode sessions in the same project, so undo history must be
/// partitioned by session to keep session A's edits invisible to session B.
///
/// The 20-entry cap is enforced **per (session, file)** deliberately — a global
/// per-file LRU would re-couple sessions and let one busy session evict
/// another's history.
///
/// Disk layout (schema v2):
///   `<storage_dir>/backups/<session_hash>/session.json` — session metadata
///   `<storage_dir>/backups/<session_hash>/<path_hash>/meta.json` — file path + count + session
///   `<storage_dir>/backups/<session_hash>/<path_hash>/0.bak` … `19.bak` — snapshots
///
/// Legacy layouts from before sessionization (flat `<path_hash>/` directly under
/// `backups/`) are migrated on first `set_storage_dir` call into the default
/// session namespace.
#[derive(Debug)]
pub struct BackupStore {
    /// session -> path -> entry stack
    entries: HashMap<String, HashMap<PathBuf, Vec<BackupEntry>>>,
    /// session -> path -> disk metadata
    disk_index: HashMap<String, HashMap<PathBuf, DiskMeta>>,
    /// session -> metadata
    session_meta: HashMap<String, SessionMeta>,
    counter: AtomicU64,
    storage_dir: Option<PathBuf>,
}

#[derive(Debug, Clone)]
struct DiskMeta {
    dir: PathBuf,
    count: usize,
}

#[derive(Debug, Clone, Default)]
struct SessionMeta {
    /// Unix timestamp of last read/write activity in this session namespace.
    /// Maintained in-memory now, reserved for future inactivity-TTL cleanup.
    last_accessed: u64,
}

impl BackupStore {
    pub fn new() -> Self {
        BackupStore {
            entries: HashMap::new(),
            disk_index: HashMap::new(),
            session_meta: HashMap::new(),
            counter: AtomicU64::new(0),
            storage_dir: None,
        }
    }

    /// Set storage directory for disk persistence (called during configure).
    ///
    /// Loads the disk index for all session namespaces, removes stale session
    /// directories, and migrates any legacy pre-session (flat) layout into the
    /// default namespace.
    pub fn set_storage_dir(&mut self, dir: PathBuf, ttl_hours: u32) {
        self.storage_dir = Some(dir);
        self.gc_stale_sessions(ttl_hours);
        self.migrate_legacy_layout_if_needed();
        self.load_disk_index();
    }

    /// Snapshot the current contents of `path` under the given session namespace.
    pub fn snapshot(
        &mut self,
        session: &str,
        path: &Path,
        description: &str,
    ) -> Result<String, AftError> {
        self.snapshot_with_op(session, path, description, None)
    }

    /// Snapshot the current contents of `path` under the given session namespace,
    /// optionally tagging it with an operation id shared by all files touched by
    /// one mutating tool call.
    pub fn snapshot_with_op(
        &mut self,
        session: &str,
        path: &Path,
        description: &str,
        op_id: Option<&str>,
    ) -> Result<String, AftError> {
        let content = std::fs::read_to_string(path).map_err(|_| AftError::FileNotFound {
            path: path.display().to_string(),
        })?;

        let key = canonicalize_key(path);
        let id = self.next_id();
        let entry = BackupEntry {
            backup_id: id.clone(),
            content,
            timestamp: current_timestamp(),
            description: description.to_string(),
            op_id: op_id.map(str::to_string),
        };

        let session_entries = self.entries.entry(session.to_string()).or_default();
        let stack = session_entries.entry(key.clone()).or_default();
        if stack.len() >= MAX_UNDO_DEPTH {
            stack.remove(0);
        }
        stack.push(entry);

        // Persist to disk
        let stack_clone = stack.clone();
        self.write_snapshot_to_disk(session, &key, &stack_clone);
        self.touch_session(session);

        Ok(id)
    }

    /// Restore every top-of-stack backup entry belonging to the most recent
    /// operation in this session.
    pub fn restore_last_operation(&mut self, session: &str) -> Result<RestoredOperation, AftError> {
        let disk_keys: Vec<PathBuf> = self
            .disk_index
            .get(session)
            .map(|files| files.keys().cloned().collect())
            .unwrap_or_default();
        for key in disk_keys {
            self.load_from_disk_if_needed(session, &key);
        }

        let mut latest: Option<((u64, u64), String)> = None;
        if let Some(files) = self.entries.get(session) {
            for stack in files.values() {
                for entry in stack {
                    if let Some(op_id) = &entry.op_id {
                        let seq = backup_sequence(&entry.backup_id).unwrap_or(0);
                        let order = (entry.timestamp, seq);
                        if latest
                            .as_ref()
                            .map_or(true, |(latest_order, _)| order > *latest_order)
                        {
                            latest = Some((order, op_id.clone()));
                        }
                    }
                }
            }
        }

        let Some((_, op_id)) = latest else {
            return Err(AftError::NoUndoHistory {
                path: "operation".to_string(),
            });
        };

        let keys_to_restore: Vec<PathBuf> = self
            .entries
            .get(session)
            .map(|files| {
                files
                    .iter()
                    .filter_map(|(key, stack)| {
                        stack.last().and_then(|entry| {
                            (entry.op_id.as_deref() == Some(op_id.as_str())).then(|| key.clone())
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        if keys_to_restore.is_empty() {
            return Err(AftError::NoUndoHistory {
                path: "operation".to_string(),
            });
        }

        let mut restored = Vec::new();
        let mut warnings = Vec::new();
        for key in keys_to_restore {
            let warning = self.check_external_modification(session, &key, &key);
            let (entry, _) = self.do_restore(session, &key, &key)?;
            if let Some(warning) = warning {
                warnings.push(format!("{}: {}", key.display(), warning));
            }
            restored.push(RestoredFile {
                path: key,
                backup_id: entry.backup_id,
            });
        }
        self.touch_session(session);

        Ok(RestoredOperation {
            op_id,
            restored,
            warnings,
        })
    }

    /// Pop the most recent backup for `(session, path)` and restore the file.
    /// Returns `(entry, optional_warning)`.
    pub fn restore_latest(
        &mut self,
        session: &str,
        path: &Path,
    ) -> Result<(BackupEntry, Option<String>), AftError> {
        let key = canonicalize_key(path);

        // Try memory first
        let in_memory = self
            .entries
            .get(session)
            .and_then(|s| s.get(&key))
            .map_or(false, |s| !s.is_empty());
        if in_memory {
            let result = self.do_restore(session, &key, path);
            if result.is_ok() {
                self.touch_session(session);
            }
            return result;
        }

        // Try disk fallback
        if self.load_from_disk_if_needed(session, &key) {
            // Check for external modification
            let warning = self.check_external_modification(session, &key, path);
            let (entry, _) = self.do_restore(session, &key, path)?;
            self.touch_session(session);
            return Ok((entry, warning));
        }

        Err(AftError::NoUndoHistory {
            path: path.display().to_string(),
        })
    }

    /// Return the backup history for `(session, path)` (oldest first).
    pub fn history(&self, session: &str, path: &Path) -> Vec<BackupEntry> {
        let key = canonicalize_key(path);
        self.entries
            .get(session)
            .and_then(|s| s.get(&key))
            .cloned()
            .unwrap_or_default()
    }

    /// Return the number of on-disk backup entries for `(session, file)`.
    pub fn disk_history_count(&self, session: &str, path: &Path) -> usize {
        let key = canonicalize_key(path);
        self.disk_index
            .get(session)
            .and_then(|s| s.get(&key))
            .map(|m| m.count)
            .unwrap_or(0)
    }

    /// Return all files that have at least one backup entry in this session
    /// (memory + disk). Other sessions' files are not visible.
    pub fn tracked_files(&self, session: &str) -> Vec<PathBuf> {
        let mut files: std::collections::HashSet<PathBuf> = self
            .entries
            .get(session)
            .map(|s| s.keys().cloned().collect())
            .unwrap_or_default();
        if let Some(disk) = self.disk_index.get(session) {
            for key in disk.keys() {
                files.insert(key.clone());
            }
        }
        files.into_iter().collect()
    }

    /// Return all session namespaces that currently have any backup state
    /// (memory or disk). Exposed for `/aft-status` aggregate reporting.
    pub fn sessions_with_backups(&self) -> Vec<String> {
        let mut sessions: std::collections::HashSet<String> =
            self.entries.keys().cloned().collect();
        for s in self.disk_index.keys() {
            sessions.insert(s.clone());
        }
        sessions.into_iter().collect()
    }

    /// Total on-disk bytes across all sessions (best-effort, reads metadata only).
    /// Used by `/aft-status` to surface storage footprint.
    pub fn total_disk_bytes(&self) -> u64 {
        let mut total = 0u64;
        for session_dirs in self.disk_index.values() {
            for meta in session_dirs.values() {
                if let Ok(read_dir) = std::fs::read_dir(&meta.dir) {
                    for entry in read_dir.flatten() {
                        if let Ok(m) = entry.metadata() {
                            if m.is_file() {
                                total += m.len();
                            }
                        }
                    }
                }
            }
        }
        total
    }

    fn next_id(&self) -> String {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        format!("backup-{}", n)
    }

    fn touch_session(&mut self, session: &str) {
        let now = current_timestamp();
        self.session_meta
            .entry(session.to_string())
            .or_default()
            .last_accessed = now;
        self.write_session_marker(session, now);
    }

    // ---- Internal helpers ----

    fn do_restore(
        &mut self,
        session: &str,
        key: &Path,
        path: &Path,
    ) -> Result<(BackupEntry, Option<String>), AftError> {
        let session_entries =
            self.entries
                .get_mut(session)
                .ok_or_else(|| AftError::NoUndoHistory {
                    path: path.display().to_string(),
                })?;
        let stack = session_entries
            .get_mut(key)
            .ok_or_else(|| AftError::NoUndoHistory {
                path: path.display().to_string(),
            })?;

        let entry = stack
            .last()
            .cloned()
            .ok_or_else(|| AftError::NoUndoHistory {
                path: path.display().to_string(),
            })?;

        std::fs::write(path, &entry.content).map_err(|e| AftError::IoError {
            path: path.display().to_string(),
            message: e.to_string(),
        })?;

        stack.pop();
        if stack.is_empty() {
            session_entries.remove(key);
            // Also prune the session map when its last file is gone.
            if session_entries.is_empty() {
                self.entries.remove(session);
            }
            self.remove_disk_backups(session, key);
        } else {
            let stack_clone = self
                .entries
                .get(session)
                .and_then(|s| s.get(key))
                .cloned()
                .unwrap_or_default();
            self.write_snapshot_to_disk(session, key, &stack_clone);
        }

        Ok((entry, None))
    }

    fn check_external_modification(
        &self,
        session: &str,
        key: &Path,
        path: &Path,
    ) -> Option<String> {
        if let (Some(stack), Ok(current)) = (
            self.entries.get(session).and_then(|s| s.get(key)),
            std::fs::read_to_string(path),
        ) {
            if let Some(latest) = stack.last() {
                if latest.content != current {
                    return Some("file was modified externally since last backup".to_string());
                }
            }
        }
        None
    }

    // ---- Disk persistence ----

    fn backups_dir(&self) -> Option<PathBuf> {
        self.storage_dir.as_ref().map(|d| d.join("backups"))
    }

    fn session_dir(&self, session: &str) -> Option<PathBuf> {
        self.backups_dir()
            .map(|d| d.join(Self::session_hash(session)))
    }

    fn session_hash(session: &str) -> String {
        hash_session(session)
    }

    fn path_hash(key: &Path) -> String {
        // v0.16.0 intentionally switched from DefaultHasher to SHA-256 for
        // stable on-disk names. Existing DefaultHasher backup directories are
        // not migrated: backups are short-lived/session-scoped, so one-time
        // loss of pre-upgrade undo history is acceptable.
        stable_hash_16(key.to_string_lossy().as_bytes())
    }

    fn write_session_marker(&self, session: &str, last_accessed: u64) {
        let Some(session_dir) = self.session_dir(session) else {
            return;
        };
        if let Err(e) = std::fs::create_dir_all(&session_dir) {
            crate::slog_warn!("failed to create session dir: {}", e);
            return;
        }
        let marker = session_dir.join("session.json");
        let json = serde_json::json!({
            "schema_version": SCHEMA_VERSION,
            "session_id": session,
            "last_accessed": last_accessed,
        });
        if let Ok(s) = serde_json::to_string_pretty(&json) {
            let tmp = session_dir.join("session.json.tmp");
            if std::fs::write(&tmp, s).is_ok() {
                let _ = std::fs::rename(&tmp, marker);
            }
        }
    }

    fn gc_stale_sessions(&mut self, ttl_hours: u32) {
        let backups_dir = match self.backups_dir() {
            Some(d) if d.exists() => d,
            _ => return,
        };
        let ttl_secs = u64::from(if ttl_hours == 0 { 72 } else { ttl_hours }) * 60 * 60;
        let cutoff = current_timestamp().saturating_sub(ttl_secs);
        let entries = match std::fs::read_dir(&backups_dir) {
            Ok(entries) => entries,
            Err(_) => return,
        };

        for entry in entries.flatten() {
            let session_dir = entry.path();
            if !session_dir.is_dir() || session_dir.join("meta.json").exists() {
                continue;
            }
            let Some(last_accessed) = Self::read_session_last_accessed(&session_dir) else {
                continue;
            };
            if last_accessed >= cutoff {
                continue;
            }
            if let Err(e) = std::fs::remove_dir_all(&session_dir) {
                crate::slog_warn!(
                    "failed to remove stale backup session {}: {}",
                    session_dir.display(),
                    e
                );
            } else {
                crate::slog_warn!(
                    "removed stale backup session {} (last_accessed={})",
                    session_dir.display(),
                    last_accessed
                );
            }
        }
    }

    /// One-time migration: move pre-session flat layout into the default
    /// session namespace. Called from `set_storage_dir` so existing backups
    /// survive the upgrade.
    ///
    /// Detection: any directory directly under `backups/` that contains a
    /// `meta.json` (as opposed to a `session.json` marker or subdirectories)
    /// is treated as a legacy entry.
    fn migrate_legacy_layout_if_needed(&mut self) {
        let backups_dir = match self.backups_dir() {
            Some(d) if d.exists() => d,
            _ => return,
        };
        let default_session_dir =
            backups_dir.join(Self::session_hash(crate::protocol::DEFAULT_SESSION_ID));

        let entries = match std::fs::read_dir(&backups_dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        let mut migrated = 0usize;
        for entry in entries.flatten() {
            let entry_path = entry.path();
            // Skip non-directories and already-sessionized layouts.
            if !entry_path.is_dir() {
                continue;
            }
            if entry_path == default_session_dir {
                continue;
            }
            let meta_path = entry_path.join("meta.json");
            if !meta_path.exists() {
                continue; // Already a session-hash dir (contains per-path subdirs), skip
            }
            // This is a legacy flat-layout path-hash directory. Move it under
            // the default session namespace.
            if let Err(e) = std::fs::create_dir_all(&default_session_dir) {
                crate::slog_warn!("failed to create default session dir: {}", e);
                return;
            }
            let leaf = match entry_path.file_name() {
                Some(n) => n,
                None => continue,
            };
            let target = default_session_dir.join(leaf);
            if target.exists() {
                // Already migrated on a prior run that was interrupted —
                // leave both and let the regular load pick up the target.
                continue;
            }
            match std::fs::rename(&entry_path, &target) {
                Ok(()) => {
                    // Bump meta.json to include session_id + schema_version.
                    Self::upgrade_meta_file(
                        &target.join("meta.json"),
                        crate::protocol::DEFAULT_SESSION_ID,
                    );
                    migrated += 1;
                }
                Err(e) => {
                    crate::slog_warn!(
                        "failed to migrate legacy backup {}: {}",
                        entry_path.display(),
                        e
                    );
                }
            }
        }
        if migrated > 0 {
            crate::slog_info!(
                "migrated {} legacy backup entries into default session namespace",
                migrated
            );
            // Write a session.json marker so future scans don't re-migrate.
            let marker = default_session_dir.join("session.json");
            let json = serde_json::json!({
                "schema_version": SCHEMA_VERSION,
                "session_id": crate::protocol::DEFAULT_SESSION_ID,
                "last_accessed": current_timestamp(),
            });
            if let Ok(s) = serde_json::to_string_pretty(&json) {
                let _ = std::fs::write(&marker, s);
            }
        }
    }

    fn upgrade_meta_file(meta_path: &Path, session_id: &str) {
        let content = match std::fs::read_to_string(meta_path) {
            Ok(c) => c,
            Err(_) => return,
        };
        let mut parsed: serde_json::Value = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(_) => return,
        };
        if let Some(obj) = parsed.as_object_mut() {
            let count = obj.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
            obj.insert(
                "schema_version".to_string(),
                serde_json::json!(SCHEMA_VERSION),
            );
            obj.insert("session_id".to_string(), serde_json::json!(session_id));
            obj.entry("entries").or_insert_with(|| {
                serde_json::Value::Array(
                    (0..count)
                        .map(|i| {
                            serde_json::json!({
                                "backup_id": format!("disk-{}", i),
                                "timestamp": 0,
                                "description": "restored from disk",
                                "op_id": null,
                            })
                        })
                        .collect(),
                )
            });
        }
        if let Ok(s) = serde_json::to_string_pretty(&parsed) {
            let tmp = meta_path.with_extension("json.tmp");
            if std::fs::write(&tmp, &s).is_ok() {
                let _ = std::fs::rename(&tmp, meta_path);
            }
        }
    }

    fn load_disk_index(&mut self) {
        let backups_dir = match self.backups_dir() {
            Some(d) if d.exists() => d,
            _ => return,
        };
        let session_dirs = match std::fs::read_dir(&backups_dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        let mut total_entries = 0usize;
        for session_entry in session_dirs.flatten() {
            let session_dir = session_entry.path();
            if !session_dir.is_dir() {
                continue;
            }
            // Recover the session_id from session.json if present, otherwise skip
            // (can't invert the hash to recover the original).
            let session_id = Self::read_session_marker(&session_dir)
                .unwrap_or_else(|| crate::protocol::DEFAULT_SESSION_ID.to_string());

            let path_dirs = match std::fs::read_dir(&session_dir) {
                Ok(e) => e,
                Err(_) => continue,
            };
            let per_session = self.disk_index.entry(session_id.clone()).or_default();
            for path_entry in path_dirs.flatten() {
                let path_dir = path_entry.path();
                if !path_dir.is_dir() {
                    continue;
                }
                let meta_path = path_dir.join("meta.json");
                if let Ok(content) = std::fs::read_to_string(&meta_path) {
                    if let Ok(meta) = serde_json::from_str::<serde_json::Value>(&content) {
                        if let (Some(path_str), Some(count)) = (
                            meta.get("path").and_then(|v| v.as_str()),
                            meta.get("count").and_then(|v| v.as_u64()),
                        ) {
                            per_session.insert(
                                PathBuf::from(path_str),
                                DiskMeta {
                                    dir: path_dir.clone(),
                                    count: count as usize,
                                },
                            );
                            total_entries += 1;
                        }
                    }
                }
            }
        }
        if total_entries > 0 {
            crate::slog_info!(
                "loaded {} backup entries across {} session(s) from disk",
                total_entries,
                self.disk_index.len()
            );
        }
    }

    fn read_session_marker(session_dir: &Path) -> Option<String> {
        let marker = session_dir.join("session.json");
        let content = std::fs::read_to_string(&marker).ok()?;
        let parsed: serde_json::Value = serde_json::from_str(&content).ok()?;
        parsed
            .get("session_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    }

    fn read_session_last_accessed(session_dir: &Path) -> Option<u64> {
        let marker = session_dir.join("session.json");
        let content = std::fs::read_to_string(&marker).ok()?;
        let parsed: serde_json::Value = serde_json::from_str(&content).ok()?;
        parsed.get("last_accessed").and_then(|v| v.as_u64())
    }

    fn load_from_disk_if_needed(&mut self, session: &str, key: &Path) -> bool {
        let meta = match self
            .disk_index
            .get(session)
            .and_then(|s| s.get(key))
            .cloned()
        {
            Some(m) if m.count > 0 => m,
            _ => return false,
        };

        let mut entries = Vec::new();
        let entry_meta = std::fs::read_to_string(meta.dir.join("meta.json"))
            .ok()
            .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
            .and_then(|meta| meta.get("entries").and_then(|v| v.as_array()).cloned())
            .unwrap_or_default();

        for i in 0..meta.count {
            let bak_path = meta.dir.join(format!("{}.bak", i));
            if let Ok(content) = std::fs::read_to_string(&bak_path) {
                let meta = entry_meta.get(i);
                entries.push(BackupEntry {
                    backup_id: meta
                        .and_then(|m| m.get("backup_id"))
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("disk-{}", i)),
                    content,
                    timestamp: meta
                        .and_then(|m| m.get("timestamp"))
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0),
                    description: meta
                        .and_then(|m| m.get("description"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("restored from disk")
                        .to_string(),
                    op_id: meta
                        .and_then(|m| m.get("op_id"))
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                });
            }
        }

        if entries.is_empty() {
            return false;
        }

        self.entries
            .entry(session.to_string())
            .or_default()
            .insert(key.to_path_buf(), entries);
        true
    }

    fn write_snapshot_to_disk(&mut self, session: &str, key: &Path, stack: &[BackupEntry]) {
        let session_dir = match self.session_dir(session) {
            Some(d) => d,
            None => return,
        };

        // Ensure session dir + marker exist.
        if let Err(e) = std::fs::create_dir_all(&session_dir) {
            crate::slog_warn!("failed to create session dir: {}", e);
            return;
        }
        let marker = session_dir.join("session.json");
        if !marker.exists() {
            let json = serde_json::json!({
                "schema_version": SCHEMA_VERSION,
                "session_id": session,
                "last_accessed": current_timestamp(),
            });
            if let Ok(s) = serde_json::to_string_pretty(&json) {
                let _ = std::fs::write(&marker, s);
            }
        }

        let hash = Self::path_hash(key);
        let dir = session_dir.join(&hash);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            crate::slog_warn!("failed to create backup dir: {}", e);
            return;
        }

        for (i, entry) in stack.iter().enumerate() {
            let bak_path = dir.join(format!("{}.bak", i));
            let tmp_path = dir.join(format!("{}.bak.tmp", i));
            if std::fs::write(&tmp_path, &entry.content).is_ok() {
                let _ = std::fs::rename(&tmp_path, &bak_path);
            }
        }

        // Clean up extra .bak files if stack shrank.
        for i in stack.len()..MAX_UNDO_DEPTH {
            let old = dir.join(format!("{}.bak", i));
            if old.exists() {
                let _ = std::fs::remove_file(&old);
            }
        }

        let entries: Vec<serde_json::Value> = stack
            .iter()
            .map(|entry| {
                serde_json::json!({
                    "backup_id": entry.backup_id,
                    "timestamp": entry.timestamp,
                    "description": entry.description,
                    "op_id": entry.op_id,
                })
            })
            .collect();
        let meta = serde_json::json!({
            "schema_version": SCHEMA_VERSION,
            "session_id": session,
            "path": key.display().to_string(),
            "count": stack.len(),
            "entries": entries,
        });
        let meta_path = dir.join("meta.json");
        let meta_tmp = dir.join("meta.json.tmp");
        if let Ok(content) = serde_json::to_string_pretty(&meta) {
            if std::fs::write(&meta_tmp, &content).is_ok() {
                let _ = std::fs::rename(&meta_tmp, &meta_path);
            }
        }

        // Keep the in-memory disk_index in sync so tracked_files() and
        // disk_history_count() immediately reflect what we just wrote.
        self.disk_index
            .entry(session.to_string())
            .or_default()
            .insert(
                key.to_path_buf(),
                DiskMeta {
                    dir,
                    count: stack.len(),
                },
            );
    }

    fn remove_disk_backups(&mut self, session: &str, key: &Path) {
        let removed = self.disk_index.get_mut(session).and_then(|s| s.remove(key));
        if let Some(meta) = removed {
            let _ = std::fs::remove_dir_all(&meta.dir);
        } else if let Some(session_dir) = self.session_dir(session) {
            let hash = Self::path_hash(key);
            let dir = session_dir.join(&hash);
            if dir.exists() {
                let _ = std::fs::remove_dir_all(&dir);
            }
        }

        // If this session has no more disk entries, drop the map slot (session
        // dir itself is kept so the marker survives future sessions).
        let empty = self
            .disk_index
            .get(session)
            .map(|s| s.is_empty())
            .unwrap_or(false);
        if empty {
            self.disk_index.remove(session);
        }
    }
}

pub fn hash_session(session: &str) -> String {
    stable_hash_16(session.as_bytes())
}

pub fn new_op_id() -> String {
    let mut bytes = [0u8; 4];
    if getrandom::fill(&mut bytes).is_err() {
        bytes = current_timestamp().to_le_bytes()[..4]
            .try_into()
            .unwrap_or([0; 4]);
    }
    let rand = u32::from_le_bytes(bytes);
    format!("op-{}-{:08x}", current_timestamp() * 1000, rand)
}

fn canonicalize_key(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|err| {
        log::debug!(
            "backup canonicalize_key fallback for {}: {}",
            path.display(),
            err
        );
        path.to_path_buf()
    })
}

fn current_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn stable_hash_16(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest[..8]
        .iter()
        .map(|byte| format!("{:02x}", byte))
        .collect()
}

fn backup_sequence(backup_id: &str) -> Option<u64> {
    backup_id
        .strip_prefix("backup-")
        .or_else(|| backup_id.strip_prefix("disk-"))
        .and_then(|s| s.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::DEFAULT_SESSION_ID;
    use std::fs;

    fn temp_file(name: &str, content: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("aft_backup_tests");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn snapshot_and_restore_round_trip() {
        let path = temp_file("round_trip.txt", "original");
        let mut store = BackupStore::new();

        let id = store
            .snapshot(DEFAULT_SESSION_ID, &path, "before edit")
            .unwrap();
        assert!(id.starts_with("backup-"));

        fs::write(&path, "modified").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "modified");

        let (entry, _) = store.restore_latest(DEFAULT_SESSION_ID, &path).unwrap();
        assert_eq!(entry.content, "original");
        assert_eq!(fs::read_to_string(&path).unwrap(), "original");
    }

    #[test]
    fn multiple_snapshots_preserve_order() {
        let path = temp_file("order.txt", "v1");
        let mut store = BackupStore::new();

        store.snapshot(DEFAULT_SESSION_ID, &path, "first").unwrap();
        fs::write(&path, "v2").unwrap();
        store.snapshot(DEFAULT_SESSION_ID, &path, "second").unwrap();
        fs::write(&path, "v3").unwrap();
        store.snapshot(DEFAULT_SESSION_ID, &path, "third").unwrap();

        let history = store.history(DEFAULT_SESSION_ID, &path);
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].content, "v1");
        assert_eq!(history[1].content, "v2");
        assert_eq!(history[2].content, "v3");
    }

    #[test]
    fn restore_pops_from_stack() {
        let path = temp_file("pop.txt", "v1");
        let mut store = BackupStore::new();

        store.snapshot(DEFAULT_SESSION_ID, &path, "first").unwrap();
        fs::write(&path, "v2").unwrap();
        store.snapshot(DEFAULT_SESSION_ID, &path, "second").unwrap();

        let (entry, _) = store.restore_latest(DEFAULT_SESSION_ID, &path).unwrap();
        assert_eq!(entry.description, "second");
        assert_eq!(entry.content, "v2");

        let history = store.history(DEFAULT_SESSION_ID, &path);
        assert_eq!(history.len(), 1);
    }

    #[test]
    fn empty_history_returns_empty_vec() {
        let store = BackupStore::new();
        let path = Path::new("/tmp/aft_backup_tests/nonexistent_history.txt");
        assert!(store.history(DEFAULT_SESSION_ID, path).is_empty());
    }

    #[test]
    fn snapshot_nonexistent_file_returns_error() {
        let mut store = BackupStore::new();
        let path = Path::new("/tmp/aft_backup_tests/absolutely_does_not_exist.txt");
        assert!(store.snapshot(DEFAULT_SESSION_ID, path, "test").is_err());
    }

    #[test]
    fn tracked_files_lists_snapshotted_paths() {
        let path1 = temp_file("tracked1.txt", "a");
        let path2 = temp_file("tracked2.txt", "b");
        let mut store = BackupStore::new();

        store.snapshot(DEFAULT_SESSION_ID, &path1, "snap1").unwrap();
        store.snapshot(DEFAULT_SESSION_ID, &path2, "snap2").unwrap();
        assert_eq!(store.tracked_files(DEFAULT_SESSION_ID).len(), 2);
    }

    #[test]
    fn sessions_are_isolated() {
        let path = temp_file("isolated.txt", "original");
        let mut store = BackupStore::new();

        store.snapshot("session_a", &path, "a's snapshot").unwrap();

        // Session B sees no history for this file.
        assert!(store.history("session_b", &path).is_empty());
        assert_eq!(store.tracked_files("session_b").len(), 0);

        // Session B's restore_latest fails with NoUndoHistory.
        let err = store.restore_latest("session_b", &path);
        assert!(matches!(err, Err(AftError::NoUndoHistory { .. })));

        // Session A still sees its own snapshot.
        assert_eq!(store.history("session_a", &path).len(), 1);
        assert_eq!(store.tracked_files("session_a").len(), 1);
    }

    #[test]
    fn per_session_per_file_cap_is_independent() {
        // Two sessions fill up their own stacks independently; hitting the cap
        // in session A does not evict anything from session B.
        let path = temp_file("cap_indep.txt", "v0");
        let mut store = BackupStore::new();

        for i in 0..(MAX_UNDO_DEPTH + 5) {
            fs::write(&path, format!("a{}", i)).unwrap();
            store.snapshot("session_a", &path, "a").unwrap();
        }
        fs::write(&path, "b_initial").unwrap();
        store.snapshot("session_b", &path, "b").unwrap();

        // Session A should be capped at MAX_UNDO_DEPTH.
        assert_eq!(store.history("session_a", &path).len(), MAX_UNDO_DEPTH);
        // Session B should still have its single entry.
        assert_eq!(store.history("session_b", &path).len(), 1);
    }

    #[test]
    fn sessions_with_backups_lists_all_namespaces() {
        let path_a = temp_file("sessions_list_a.txt", "a");
        let path_b = temp_file("sessions_list_b.txt", "b");
        let mut store = BackupStore::new();

        store.snapshot("alice", &path_a, "from alice").unwrap();
        store.snapshot("bob", &path_b, "from bob").unwrap();

        let sessions = store.sessions_with_backups();
        assert_eq!(sessions.len(), 2);
        assert!(sessions.iter().any(|s| s == "alice"));
        assert!(sessions.iter().any(|s| s == "bob"));
    }

    #[test]
    fn disk_persistence_survives_reload() {
        let dir = std::env::temp_dir().join("aft_backup_disk_test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let file_path = temp_file("disk_persist.txt", "original");

        // Create store with storage, snapshot under default session, drop.
        {
            let mut store = BackupStore::new();
            store.set_storage_dir(dir.clone(), 72);
            store
                .snapshot(DEFAULT_SESSION_ID, &file_path, "before edit")
                .unwrap();
        }

        // Modify the file externally.
        fs::write(&file_path, "externally modified").unwrap();

        // Create new store, load from disk, restore.
        let mut store2 = BackupStore::new();
        store2.set_storage_dir(dir.clone(), 72);

        let (entry, warning) = store2
            .restore_latest(DEFAULT_SESSION_ID, &file_path)
            .unwrap();
        assert_eq!(entry.content, "original");
        assert!(warning.is_some()); // modified externally
        assert_eq!(fs::read_to_string(&file_path).unwrap(), "original");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_flat_layout_migrates_to_default_session() {
        // Simulate a pre-session on-disk layout (schema v1) and verify it's
        // moved under the default session namespace on set_storage_dir.
        let dir = std::env::temp_dir().join("aft_backup_migration_test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let backups = dir.join("backups");
        fs::create_dir_all(&backups).unwrap();

        // Fake legacy entry for some path hash.
        let legacy_hash = "deadbeefcafebabe";
        let legacy_dir = backups.join(legacy_hash);
        fs::create_dir_all(&legacy_dir).unwrap();
        fs::write(legacy_dir.join("0.bak"), "original content").unwrap();
        let legacy_meta = serde_json::json!({
            "path": "/tmp/migrated_file.txt",
            "count": 1,
        });
        fs::write(
            legacy_dir.join("meta.json"),
            serde_json::to_string_pretty(&legacy_meta).unwrap(),
        )
        .unwrap();

        // Run migration.
        let mut store = BackupStore::new();
        store.set_storage_dir(dir.clone(), 72);

        // After migration, the legacy dir should be gone from the top level,
        // and the entry should now live under the default-session hash dir.
        let default_session_dir = backups.join(BackupStore::session_hash(DEFAULT_SESSION_ID));
        assert!(default_session_dir.exists());
        assert!(default_session_dir.join(legacy_hash).exists());
        assert!(!backups.join(legacy_hash).exists());

        // The upgraded meta.json should now include session_id + schema_version.
        let meta_content =
            fs::read_to_string(default_session_dir.join(legacy_hash).join("meta.json")).unwrap();
        let meta: serde_json::Value = serde_json::from_str(&meta_content).unwrap();
        assert_eq!(meta["session_id"], DEFAULT_SESSION_ID);
        assert_eq!(meta["schema_version"], SCHEMA_VERSION);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_storage_dir_removes_stale_backup_sessions() {
        let dir = std::env::temp_dir().join("aft_backup_gc_test");
        let _ = fs::remove_dir_all(&dir);
        let backups = dir.join("backups");
        fs::create_dir_all(&backups).unwrap();

        let stale_session_dir = backups.join("stale-session");
        fs::create_dir_all(&stale_session_dir).unwrap();
        let stale_marker = serde_json::json!({
            "schema_version": SCHEMA_VERSION,
            "session_id": "stale",
            "last_accessed": 1,
        });
        fs::write(
            stale_session_dir.join("session.json"),
            serde_json::to_string_pretty(&stale_marker).unwrap(),
        )
        .unwrap();

        let mut store = BackupStore::new();
        store.set_storage_dir(dir.clone(), 1);

        assert!(!stale_session_dir.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn restore_last_operation_restores_all_top_entries_for_same_op() {
        let path_a = temp_file("op_restore_a.txt", "a1");
        let path_b = temp_file("op_restore_b.txt", "b1");
        let mut store = BackupStore::new();
        let op_id = "op-test-00000001";

        store
            .snapshot_with_op(DEFAULT_SESSION_ID, &path_a, "a", Some(op_id))
            .unwrap();
        store
            .snapshot_with_op(DEFAULT_SESSION_ID, &path_b, "b", Some(op_id))
            .unwrap();
        fs::write(&path_a, "a2").unwrap();
        fs::write(&path_b, "b2").unwrap();

        let restored = store.restore_last_operation(DEFAULT_SESSION_ID).unwrap();
        assert_eq!(restored.op_id, op_id);
        assert_eq!(restored.restored.len(), 2);
        assert_eq!(fs::read_to_string(&path_a).unwrap(), "a1");
        assert_eq!(fs::read_to_string(&path_b).unwrap(), "b1");
    }

    #[test]
    fn restore_last_operation_restores_only_most_recent_op() {
        let path_a = temp_file("op_recent_a.txt", "a1");
        let path_b = temp_file("op_recent_b.txt", "b1");
        let mut store = BackupStore::new();

        store
            .snapshot_with_op(DEFAULT_SESSION_ID, &path_a, "older", Some("op-older"))
            .unwrap();
        store
            .snapshot_with_op(DEFAULT_SESSION_ID, &path_b, "newer", Some("op-newer"))
            .unwrap();
        fs::write(&path_a, "a2").unwrap();
        fs::write(&path_b, "b2").unwrap();

        let restored = store.restore_last_operation(DEFAULT_SESSION_ID).unwrap();
        assert_eq!(restored.op_id, "op-newer");
        assert_eq!(restored.restored.len(), 1);
        assert_eq!(fs::read_to_string(&path_a).unwrap(), "a2");
        assert_eq!(fs::read_to_string(&path_b).unwrap(), "b1");
    }

    #[test]
    fn restore_last_operation_ignores_legacy_entries_without_op_id() {
        let path = temp_file("op_legacy_none.txt", "v1");
        let mut store = BackupStore::new();

        store.snapshot(DEFAULT_SESSION_ID, &path, "legacy").unwrap();
        fs::write(&path, "v2").unwrap();

        let err = store.restore_last_operation(DEFAULT_SESSION_ID);
        assert!(matches!(err, Err(AftError::NoUndoHistory { .. })));
        assert_eq!(fs::read_to_string(&path).unwrap(), "v2");
    }

    #[test]
    fn schema_v2_meta_loads_with_none_op_id_and_persists_as_v3() {
        let dir = std::env::temp_dir().join("aft_backup_v2_to_v3_test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let file_path = temp_file("v2_to_v3.txt", "original");
        let key = canonicalize_key(&file_path);
        let session_dir = dir
            .join("backups")
            .join(BackupStore::session_hash(DEFAULT_SESSION_ID));
        let path_dir = session_dir.join(BackupStore::path_hash(&key));
        fs::create_dir_all(&path_dir).unwrap();
        fs::write(path_dir.join("0.bak"), "original").unwrap();
        fs::write(
            session_dir.join("session.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "schema_version": 2,
                "session_id": DEFAULT_SESSION_ID,
                "last_accessed": current_timestamp(),
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(
            path_dir.join("meta.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "schema_version": 2,
                "session_id": DEFAULT_SESSION_ID,
                "path": key.display().to_string(),
                "count": 1,
            }))
            .unwrap(),
        )
        .unwrap();

        let mut store = BackupStore::new();
        store.set_storage_dir(dir.clone(), 72);
        assert!(store.load_from_disk_if_needed(DEFAULT_SESSION_ID, &key));
        let history = store.history(DEFAULT_SESSION_ID, &file_path);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].op_id, None);

        fs::write(&file_path, "second").unwrap();
        store
            .snapshot_with_op(DEFAULT_SESSION_ID, &file_path, "second", Some("op-v3"))
            .unwrap();
        let written: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(path_dir.join("meta.json")).unwrap()).unwrap();
        assert_eq!(written["schema_version"], SCHEMA_VERSION);
        assert_eq!(written["entries"][0]["op_id"], serde_json::Value::Null);
        assert_eq!(written["entries"][1]["op_id"], "op-v3");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn per_file_restore_latest_still_works_with_op_ids() {
        let path = temp_file("op_per_file.txt", "v1");
        let mut store = BackupStore::new();

        store
            .snapshot_with_op(DEFAULT_SESSION_ID, &path, "op", Some("op-file"))
            .unwrap();
        fs::write(&path, "v2").unwrap();

        let (entry, _) = store.restore_latest(DEFAULT_SESSION_ID, &path).unwrap();
        assert_eq!(entry.op_id.as_deref(), Some("op-file"));
        assert_eq!(fs::read_to_string(&path).unwrap(), "v1");
    }
}
