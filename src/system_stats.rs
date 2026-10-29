// Panasaurus Wings — system-wide stats and log endpoints
//
// Adds three endpoints ported from wings-main:
//   GET /api/stats            — aggregated CPU/memory/disk across all servers
//   GET /api/system/logs      — list wings log files
//   GET /api/system/logs/{file} — read a specific wings log file
//
// These let panel operators inspect node health and diagnose problems
// without SSH-ing into the box.

use crate::State;
use axum::{
    Json,
    extract::{Path, State as AppState},
    http::StatusCode,
};
use serde_json::{Value, json};
use std::sync::Arc;

use crate::api::{ApiError, ApiResult};

/// Aggregated node-wide stats. Pulls per-server utilization from each
/// server's cached console snapshot (so it's cheap — no per-request Docker
/// round-trips) and combines with host-level disk space.
pub async fn stats(AppState(state): AppState<Arc<State>>) -> ApiResult<Json<Value>> {
    let ids: Vec<String> = state.servers.read().await.keys().cloned().collect();
    let mut servers = Vec::with_capacity(ids.len());
    let mut total_cpu = 0u64;
    let mut total_mem = 0u64;
    let mut total_disk = 0u64;

    for id in &ids {
        if let Ok(Some(usage)) = state.console_snapshot(id).await {
            let cpu = usage["cpu_absolute"].as_u64().unwrap_or(0);
            let mem = usage["memory_bytes"].as_u64().unwrap_or(0);
            let disk = usage["disk_bytes"].as_u64().unwrap_or(0);
            let state_str = usage["state"].as_str().unwrap_or("offline").to_owned();
            total_cpu = total_cpu.saturating_add(cpu);
            total_mem = total_mem.saturating_add(mem);
            total_disk = total_disk.saturating_add(disk);
            servers.push(json!({
                "uuid": id,
                "state": state_str,
                "cpu": cpu,
                "memory": mem,
                "disk": disk,
            }));
        }
    }

    let (disk_total, disk_free) = disk_space(&state.engine.config.system.data);
    let info = state
        .engine
        .json(&crate::engine::strings(&["info", "--format", "{{json .}}"]))
        .await
        .unwrap_or(Value::Null);

    Ok(Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "booted_at": state.booted_at,
        "uptime_seconds": chrono::Utc::now().timestamp().saturating_sub(state.booted_at as i64),
        "servers": {
            "total": ids.len(),
            "list": servers,
        },
        "aggregated": {
            "cpu_percent": total_cpu,
            "memory_bytes": total_mem,
            "disk_bytes": total_disk,
        },
        "host": {
            "cpu_threads": info["NCPU"].as_u64().unwrap_or(0),
            "memory_bytes": info["MemTotal"].as_u64().unwrap_or(0),
            "disk_bytes": {
                "total": disk_total,
                "used": disk_total.saturating_sub(disk_free),
                "free": disk_free,
            },
        },
    })))
}

/// List wings log files in the configured log directory. Returns each file's
/// name, size, and last-modified timestamp so the panel can render a picker.
pub async fn list_logs(AppState(state): AppState<Arc<State>>) -> ApiResult<Json<Value>> {
    let log_dir = log_directory(&state);
    let mut entries = Vec::new();
    if let Ok(mut read_dir) = tokio::fs::read_dir(&log_dir).await {
        while let Ok(Some(entry)) = read_dir.next_entry().await {
            let name = entry.file_name().to_string_lossy().to_string();
            // Only surface log-looking files; hide dotfiles and the private
            // node directory.
            if name.starts_with('.') {
                continue;
            }
            if let Ok(metadata) = entry.metadata().await {
                if !metadata.is_file() {
                    continue;
                }
                let modified = metadata
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                entries.push(json!({
                    "name": name,
                    "size": metadata.len(),
                    "modified": modified,
                }));
            }
        }
    }
    entries.sort_by(|a, b| {
        b["modified"]
            .as_u64()
            .unwrap_or(0)
            .cmp(&a["modified"].as_u64().unwrap_or(0))
    });
    Ok(Json(json!({"data": entries})))
}

/// Read a single wings log file. The file name is validated to prevent path
/// traversal (must be a bare filename, no slashes or `..`).
pub async fn read_log(
    AppState(state): AppState<Arc<State>>,
    Path(file): Path<String>,
) -> ApiResult<Json<Value>> {
    if file.contains('/')
        || file.contains('\\')
        || file.contains('\0')
        || file == "."
        || file == ".."
        || file.starts_with('.')
    {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "Invalid log file name.".into(),
        ));
    }
    let path = log_directory(&state).join(&file);
    match tokio::fs::read_to_string(&path).await {
        Ok(contents) => {
            // Truncate very large logs to keep the response manageable.
            let truncated = contents.len() > 1024 * 1024;
            let body = if truncated {
                contents.split_off(contents.len() - 1024 * 1024)
            } else {
                contents
            };
            Ok(Json(json!({
                "file": file,
                "truncated": truncated,
                "contents": body,
            })))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(ApiError(
            StatusCode::NOT_FOUND,
            "Log file not found.".into(),
        )),
        Err(e) => Err(ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to read log file: {e}"),
        )),
    }
}

/// Resolve where wings log files live. We follow the tracing-appender
/// convention of writing to `<private_root>/logs`, falling back to the
/// private root itself if that directory doesn't exist.
fn log_directory(state: &State) -> std::path::PathBuf {
    let private = state.engine.config.private_root();
    let logs = private.join("logs");
    if logs.is_dir() {
        logs
    } else {
        private
    }
}

/// Disk space helper, mirroring the one in `api::disk_space` so we don't
/// need to make that private fn public.
fn disk_space(path: &std::path::Path) -> (u64, u64) {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        if let Ok(c_path) = CString::new(path.as_os_str().as_bytes()) {
            let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
            if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } == 0 {
                let total = (stat.f_blocks as u64).saturating_mul(stat.f_frsize as u64);
                let free = (stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64);
                return (total, free);
            }
        }
    }
    let _ = path;
    (0, 0)
}
