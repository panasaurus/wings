// Panasaurus Wings — file revision history
//
// Keeps compressed snapshots of files as they're written through the wings
// file API, so users can browse and restore prior revisions from the panel.
// Ported from wings-main's file_history subsystem, simplified to a
// snapshot-on-write model that fits u.zip's single-crate structure.
//
// Storage layout:
//   <private_root>/file_history/<server_uuid>/<file_hash>/<revision_id>.zst
//
// Each revision is zstd-compressed at level 19 (configurable). The most
// recent N revisions per file are kept; older ones are pruned on a
// maintenance interval.

use crate::State;
use anyhow::{Context, Result, ensure};
use axum::{
    Json,
    extract::{Path, Query, State as AppState},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::api::{ApiError, ApiResult};

/// Take a snapshot of `file_path` (relative to the server root) for the
/// given server. Called from the write path after a successful write.
/// Failures are logged but never propagated — file history is best-effort
/// and must not break the write.
pub fn snapshot(
    state: &State,
    server_id: &str,
    server_root: &Path,
    file_path: &str,
) {
    if let Err(error) = try_snapshot(state, server_id, server_root, file_path) {
        tracing::warn!(server = server_id, file = file_path, error = %error, "file_history snapshot failed");
    }
}

fn try_snapshot(
    state: &State,
    server_id: &str,
    server_root: &Path,
    file_path: &str,
) -> Result<()> {
    let config = state.engine.config.clone();
    if !config.system.file_history.enabled {
        return Ok(());
    }
    // Reject obviously-large files — the cost of snapshotting them on every
    // write would dominate.
    let abs = server_root.join(file_path);
    let metadata = std::fs::metadata(&abs)?;
    if metadata.len() > config.system.file_history.file_size_cap {
        return Ok(());
    }
    let bytes = std::fs::read(&abs)?;
    let hash = file_hash(file_path);
    let dir = history_dir(state, server_id, &hash);
    std::fs::create_dir_all(&dir)?;

    let revision_id = format!(
        "{}-{}",
        now_millis(),
        short_hash(&Sha256::digest(&bytes))
    );
    let revision_path = dir.join(format!("{revision_id}.zst"));
    let compressed = zstd::encode_all(
        &bytes[..],
        config.system.file_history.zstd_level as i32,
    )?;
    std::fs::write(&revision_path, compressed)?;

    prune(&dir, config.system.file_history.keep_chains)?;
    enforce_budget(state, server_id, config.system.file_history.per_server_disk_budget)?;
    Ok(())
}

/// `GET /api/servers/{id}/files/revisions?file=<path>` — list prior
/// revisions of a file, newest first.
pub async fn list_revisions(
    AppState(state): AppState<Arc<State>>,
    Path(id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    state.server(&id).await.map_err(|_| {
        ApiError(StatusCode::NOT_FOUND, "Server not found.".into())
    })?;
    let file = query
        .get("file")
        .context("file parameter is required")
        .map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let hash = file_hash(file);
    let dir = history_dir(&state, &id, &hash);
    let mut revisions = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let Some((ts, _)) = name.split_once('-') else {
                continue;
            };
            let ts: u64 = ts.parse().unwrap_or(0);
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            revisions.push(json!({
                "id": name.trim_end_matches(".zst"),
                "timestamp": ts,
                "size": size,
            }));
        }
    }
    revisions.sort_by(|a, b| {
        b["timestamp"]
            .as_u64()
            .unwrap_or(0)
            .cmp(&a["timestamp"].as_u64().unwrap_or(0))
    });
    Ok(Json(json!({"file": file, "revisions": revisions})))
}

/// `GET /api/servers/{id}/files/revisions/{revision}?file=<path>` — fetch
/// the raw contents of a specific revision (decompressed).
pub async fn get_revision(
    AppState(state): AppState<Arc<State>>,
    Path((id, revision)): Path<(String, String)>,
    Query(query): Query<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    state.server(&id).await.map_err(|_| {
        ApiError(StatusCode::NOT_FOUND, "Server not found.".into())
    })?;
    // Validate revision id — only digits, dashes, and hex chars allowed.
    ensure!(
        revision
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-'),
        "Invalid revision id"
    );
    let file = query
        .get("file")
        .context("file parameter is required")
        .map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let hash = file_hash(file);
    let path = history_dir(&state, &id, &hash).join(format!("{revision}.zst"));
    let compressed = std::fs::read(&path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => ApiError(
            StatusCode::NOT_FOUND,
            "Revision not found.".into(),
        ),
        _ => ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to read revision: {e}"),
        ),
    })?;
    let bytes = zstd::decode_all(&compressed[..])
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("Decompress: {e}")))?;
    // Return as base64 so binary files survive the JSON round-trip.
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(Json(json!({
        "file": file,
        "revision": revision,
        "size": bytes.len(),
        "contents_base64": b64,
    })))
}

fn history_dir(state: &State, server_id: &str, file_hash: &str) -> PathBuf {
    state
        .engine
        .config
        .private_root()
        .join("file_history")
        .join(server_id)
        .join(file_hash)
}

fn file_hash(path: &str) -> String {
    short_hash(&Sha256::digest(path.as_bytes()))
}

fn short_hash(digest: &[u8]) -> String {
    hex::encode(&digest[..16])
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Keep only the most recent `keep` revisions in `dir`.
fn prune(dir: &Path, keep: usize) -> Result<()> {
    let mut entries: Vec<(u64, PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        let Some((ts, _)) = name.split_once('-') else {
            continue;
        };
        let ts: u64 = ts.parse().unwrap_or(0);
        entries.push((ts, entry.path()));
    }
    entries.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, path) in entries.iter().skip(keep) {
        let _ = std::fs::remove_file(path);
    }
    Ok(())
}

/// Enforce a per-server disk budget by deleting the oldest revisions across
/// all files until usage drops under `budget`.
fn enforce_budget(state: &State, server_id: &str, budget: u64) -> Result<()> {
    let root = state
        .engine
        .config
        .private_root()
        .join("file_history")
        .join(server_id);
    if !root.exists() {
        return Ok(());
    }
    let mut entries: Vec<(u64, PathBuf)> = Vec::new();
    let mut total: u64 = 0;
    for entry in walkdir::WalkDir::new(&root).into_iter().flatten() {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path().to_path_buf();
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        total = total.saturating_add(size);
        let name = entry.file_name().to_string_lossy().to_string();
        let ts = name
            .split_once('-')
            .and_then(|(t, _)| t.parse::<u64>().ok())
            .unwrap_or(0);
        entries.push((ts, path));
    }
    if total <= budget {
        return Ok(());
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    for (_, path) in entries {
        if total <= budget {
            break;
        }
        if let Ok(metadata) = std::fs::metadata(&path) {
            let size = metadata.len();
            if std::fs::remove_file(&path).is_ok() {
                total = total.saturating_sub(size);
            }
        }
    }
    Ok(())
}
