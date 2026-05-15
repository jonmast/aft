//! Handler for the `delete_file` command: remove a file with backup.

use std::path::Path;

use lsp_types::FileChangeType;

use crate::context::AppContext;
use crate::edit;
use crate::protocol::{RawRequest, Response};

/// Handle a `delete_file` request.
///
/// Params:
///   - `file` (string, required) — path to the file to delete
///
/// Returns: `{ file, deleted, backup_id }`
pub fn handle_delete_file(req: &RawRequest, ctx: &AppContext) -> Response {
    let op_id = crate::backup::new_op_id();
    if let Some(files) = req.params.get("files").and_then(|v| v.as_array()) {
        let mut deleted = Vec::new();
        let mut skipped = Vec::new();
        for value in files {
            let Some(file) = value.as_str() else {
                skipped.push(serde_json::json!({"file": value, "reason": "not a string"}));
                continue;
            };
            match delete_one(req, ctx, file, &op_id) {
                Ok(backup_id) => deleted.push(serde_json::json!({
                    "file": file,
                    "backup_id": backup_id,
                })),
                Err(resp) => skipped.push(serde_json::json!({
                    "file": file,
                    "reason": resp.data.get("message").and_then(|v| v.as_str()).unwrap_or("delete failed"),
                })),
            }
        }
        return Response::success(
            &req.id,
            serde_json::json!({
                "complete": skipped.is_empty(),
                "deleted": deleted,
                "skipped_files": skipped,
            }),
        );
    }

    let file = match req.params.get("file").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "delete_file: missing required param 'file'",
            );
        }
    };

    match delete_one(req, ctx, file, &op_id) {
        Ok(backup_id) => {
            let mut result = serde_json::json!({
                "file": file,
                "deleted": true,
            });

            if let Some(ref id) = backup_id {
                result["backup_id"] = serde_json::json!(id);
            }

            Response::success(&req.id, result)
        }
        Err(resp) => resp,
    }
}

fn delete_one(
    req: &RawRequest,
    ctx: &AppContext,
    file: &str,
    op_id: &str,
) -> Result<Option<String>, Response> {
    let path = match ctx.validate_path(&req.id, Path::new(file)) {
        Ok(path) => path,
        Err(resp) => return Err(resp),
    };

    if !path.exists() {
        return Err(Response::error(
            &req.id,
            "file_not_found",
            format!("delete_file: file not found: {}", file),
        ));
    }

    if path.is_dir() {
        return Err(Response::error(
            &req.id,
            "invalid_request",
            format!("delete_file: '{}' is a directory, not a file", file),
        ));
    }

    // Backup before deletion
    let backup_id = match edit::auto_backup(
        ctx,
        req.session(),
        &path,
        "delete_file: pre-delete backup",
        Some(op_id),
    ) {
        Ok(id) => id,
        Err(e) => {
            return Err(Response::error(&req.id, e.code(), e.to_string()));
        }
    };

    // Delete the file
    if let Err(e) = std::fs::remove_file(&path) {
        return Err(Response::error(
            &req.id,
            "io_error",
            format!("delete_file: failed to delete: {}", e),
        ));
    }

    ctx.lsp_notify_watched_config_file(path.as_path(), FileChangeType::DELETED);

    log::debug!("delete_file: {}", file);

    Ok(backup_id)
}
