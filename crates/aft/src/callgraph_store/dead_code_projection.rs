use rusqlite::{Connection, OpenFlags};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::inspect::job::{CallgraphExport, CallgraphOutboundCall, CallgraphSnapshot};
use crate::inspect::scanners::DEFAULT_EXPORT_MARKER_KIND;

use super::{database_ready, CallGraphStoreError, Result, BACKEND_TREESITTER};

pub fn project_dead_code_snapshot(db_path: &Path) -> Result<CallgraphSnapshot> {
    if !db_path.is_file() {
        return Err(CallGraphStoreError::Unavailable(format!(
            "database does not exist: {}",
            db_path.display()
        )));
    }

    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(Duration::from_millis(5_000))?;
    if !database_ready(&conn).unwrap_or(false) {
        return Err(CallGraphStoreError::Unavailable(
            "database is missing, stale, or mid-build".to_string(),
        ));
    }

    let project_root = project_root_from_backend_state(&conn)?;
    let files = project_files_from_store(&conn, &project_root)?;
    let exported_symbols = exported_symbols_from_store(&conn, &project_root)?;
    let outbound_calls = outbound_calls_from_store(&conn, &project_root)?;
    let entry_points = entry_points_for_files(&project_root, &files);

    Ok(CallgraphSnapshot {
        generated_at: Some(SystemTime::now()),
        files,
        exported_symbols,
        outbound_calls,
        entry_points,
    })
}

fn project_root_from_backend_state(conn: &Connection) -> Result<PathBuf> {
    let mut statement = conn.prepare(
        "SELECT DISTINCT workspace_root
         FROM backend_file_state
         WHERE backend = ?1
         ORDER BY workspace_root",
    )?;
    let roots = statement
        .query_map([BACKEND_TREESITTER], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    match roots.as_slice() {
        [root] => Ok(PathBuf::from(root)),
        [] => Err(CallGraphStoreError::Unavailable(
            "database has no workspace root rows".to_string(),
        )),
        _ => Err(CallGraphStoreError::Unavailable(format!(
            "database has multiple workspace roots: {}",
            roots.join(", ")
        ))),
    }
}

fn project_files_from_store(conn: &Connection, project_root: &Path) -> Result<Vec<PathBuf>> {
    let mut statement = conn.prepare("SELECT path FROM files ORDER BY path")?;
    let files = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .map(|path| {
            path.map(|path| canonicalize_for_snapshot(&absolute_store_path(project_root, &path)))
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(files)
}

fn exported_symbols_from_store(
    conn: &Connection,
    project_root: &Path,
) -> Result<Vec<CallgraphExport>> {
    let mut statement = conn.prepare(
        "SELECT file_path, name, kind, start_line, exported, is_default_export
         FROM nodes
         WHERE exported != 0 OR is_default_export != 0
         ORDER BY file_path, start_line, name, kind, id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(ExportRow {
            file_path: row.get(0)?,
            name: row.get(1)?,
            kind: row.get(2)?,
            line: (row.get::<_, i64>(3)?.max(0) as u32).saturating_add(1),
            exported: row.get::<_, i64>(4)? != 0,
            is_default_export: row.get::<_, i64>(5)? != 0,
        })
    })?;

    let mut exports = Vec::new();
    for row in rows {
        let row = row?;
        let file = canonicalize_for_snapshot(&absolute_store_path(project_root, &row.file_path));
        if row.exported {
            exports.push(CallgraphExport {
                file: file.clone(),
                symbol: row.name.clone(),
                kind: row.kind,
                line: row.line,
            });
        }
        if row.is_default_export {
            exports.push(CallgraphExport {
                file,
                symbol: row.name,
                kind: DEFAULT_EXPORT_MARKER_KIND.to_string(),
                line: row.line,
            });
        }
    }
    Ok(exports)
}

fn outbound_calls_from_store(
    conn: &Connection,
    project_root: &Path,
) -> Result<Vec<CallgraphOutboundCall>> {
    let mut statement = conn.prepare(
        "SELECT r.ref_id,
                r.caller_file,
                n.scoped_name,
                r.short_name,
                r.full_ref,
                r.status,
                COALESCE(r.target_file, e.target_file),
                COALESCE(r.target_symbol, e.target_symbol),
                r.line
         FROM refs r
         LEFT JOIN nodes n ON n.id = r.caller_node
         LEFT JOIN edges e ON e.ref_id = r.ref_id AND e.kind = 'call'
         WHERE r.kind = 'call'
         ORDER BY r.caller_file, n.scoped_name, r.line, r.byte_start, r.byte_end, r.ref_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(OutboundRow {
            ref_id: row.get(0)?,
            caller_file: row.get(1)?,
            caller_symbol: row.get(2)?,
            short_name: row.get(3)?,
            full_ref: row.get(4)?,
            status: row.get(5)?,
            target_file: row.get(6)?,
            target_symbol: row.get(7)?,
            line: row.get::<_, i64>(8)? as u32,
        })
    })?;

    let mut calls = Vec::new();
    for row in rows {
        let row = row?;
        let caller_file =
            canonicalize_for_snapshot(&absolute_store_path(project_root, &row.caller_file));
        let caller_symbol = caller_symbol_from_row(&row)?;
        let short_name = row
            .short_name
            .as_deref()
            .or(row.full_ref.as_deref())
            .unwrap_or_default();
        let mut target = if is_resolved_status(&row.status) {
            match (row.target_file.as_deref(), row.target_symbol.as_deref()) {
                (Some(target_file), Some(target_symbol)) => {
                    let target_file =
                        canonicalize_for_snapshot(&absolute_store_path(project_root, target_file));
                    format!("{}::{target_symbol}", target_file.display())
                }
                _ => short_name.to_string(),
            }
        } else {
            short_name.to_string()
        };

        if row
            .full_ref
            .as_deref()
            .is_some_and(|full_ref| is_method_dispatch_callee(full_ref, short_name))
        {
            target.push(crate::inspect::job::DISPATCHED_CALLEE_SEPARATOR);
            target.push_str(row.full_ref.as_deref().unwrap_or_default());
        }

        calls.push(CallgraphOutboundCall {
            caller_file,
            caller_symbol,
            target,
            line: row.line,
        });
    }
    Ok(calls)
}

fn entry_points_for_files(project_root: &Path, files: &[PathBuf]) -> BTreeSet<PathBuf> {
    let resolved_entry_points = crate::inspect::resolve_entry_points(project_root);
    files
        .iter()
        .filter(|file| resolved_entry_points.is_entry_point(file))
        .cloned()
        .collect()
}

fn caller_symbol_from_row(row: &OutboundRow) -> Result<String> {
    row.caller_symbol.clone().ok_or_else(|| {
        CallGraphStoreError::Unavailable(format!(
            "call ref {} is missing caller symbol",
            row.ref_id
        ))
    })
}

fn is_resolved_status(status: &str) -> bool {
    matches!(status, "resolved" | "resolved_local")
}

fn is_method_dispatch_callee(full_callee: &str, callee_name: &str) -> bool {
    let full_callee = full_callee.trim();
    if !full_callee.contains('.') || full_callee == callee_name.trim() {
        return false;
    }

    full_callee
        .rsplit('.')
        .next()
        .map(|segment| segment.trim().trim_start_matches('?') == callee_name.trim())
        .unwrap_or(false)
}

fn absolute_store_path(project_root: &Path, store_path: &str) -> PathBuf {
    let path = Path::new(store_path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        project_root.join(path)
    }
}

fn canonicalize_for_snapshot(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| crate::inspect::job::normalize_path(path))
}

#[derive(Debug)]
struct ExportRow {
    file_path: String,
    name: String,
    kind: String,
    line: u32,
    exported: bool,
    is_default_export: bool,
}

#[derive(Debug)]
struct OutboundRow {
    ref_id: String,
    caller_file: String,
    caller_symbol: Option<String>,
    short_name: Option<String>,
    full_ref: Option<String>,
    status: String,
    target_file: Option<String>,
    target_symbol: Option<String>,
    line: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send<T: Send>() {}

    #[test]
    fn projection_result_is_send() {
        assert_send::<Result<CallgraphSnapshot>>();
    }
}
