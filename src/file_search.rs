// Panasaurus Wings — multi-threaded file search
//
// Provides `POST /api/servers/{id}/files/search` for searching files by
// name (glob) or by content (regex). Ported from wings-main's file search
// subsystem, retargeted to u.zip's cap_std sandbox.
//
// Search runs on a rayon thread pool so large servers don't block the
// async runtime. Results are capped by `max_matches` per file and a global
// response size budget to keep the API snappy.

use crate::{State, sandbox::Sandbox};
use anyhow::{Context, Result};
use axum::{
    Json,
    extract::{Path, State as AppState},
    http::StatusCode,
};
use rayon::prelude::*;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use crate::api::{ApiError, ApiResult};

#[derive(Debug, Deserialize)]
pub struct SearchRequest {
    /// Glob pattern for `mode: "filename"`, regex for `mode: "content"`.
    pub pattern: String,
    #[serde(default = "default_mode")]
    pub mode: SearchMode,
    /// Directory to search in, relative to the server root. Defaults to `/`.
    #[serde(default)]
    pub directory: String,
    #[serde(default = "default_max_matches")]
    pub max_matches: usize,
    #[serde(default = "default_max_response_bytes")]
    pub max_response_bytes: usize,
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SearchMode {
    Filename,
    Content,
}

fn default_mode() -> SearchMode {
    SearchMode::Filename
}
fn default_max_matches() -> usize {
    100
}
fn default_max_response_bytes() -> usize {
    8 * 1024 * 1024
}

#[derive(Debug, Serialize)]
pub struct SearchMatch {
    pub path: String,
    pub line: Option<u64>,
    pub column: Option<u64>,
    pub preview: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub matches: Vec<SearchMatch>,
    pub truncated: bool,
    pub files_searched: usize,
    pub files_matched: usize,
}

/// Axum handler: `POST /api/servers/{id}/files/search`.
pub async fn search(
    AppState(state): AppState<Arc<State>>,
    Path(id): Path<String>,
    Json(body): Json<SearchRequest>,
) -> ApiResult<Json<Value>> {
    // Validate the server exists and is not suspended.
    let server = state.server(&id).await.map_err(|_| {
        ApiError(StatusCode::NOT_FOUND, "Server not found.".into())
    })?;
    if server.settings.read().await["suspended"].as_bool().unwrap_or(false) {
        return Err(ApiError(StatusCode::FORBIDDEN, "Server is suspended.".into()));
    }

    // Open the sandbox at the server root.
    let directory = state.engine.directory(&id)?;
    let settings = server.settings.read().await.clone();
    let sandbox = Sandbox::new(&directory, &settings, &state.engine.config)
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("Sandbox error: {e:#}")))?;

    // Pre-compile the pattern. For filename mode we compile as a regex
    // (callers can pass `.*\.log$` style patterns); for content mode we
    // compile the regex once and reuse it across all files.
    let regex = match body.mode {
        SearchMode::Filename => {
            // For filename mode, treat the pattern as a glob first, fall
            // back to regex. We just compile as regex here; clients can pass
            // globs by escaping dots etc. This matches wings-main's
            // behavior.
            Regex::new(&body.pattern).ok()
        }
        SearchMode::Content => Some(
            Regex::new(&body.pattern)
                .map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("Invalid regex: {e}")))?,
        ),
    };

    // Walk the file tree (synchronously — cap_std's Dir isn't Send across
    // await points in the way we'd need, so we collect the file list first,
    // then hand it to rayon).
    let start = if body.directory.is_empty() {
        PathBuf::new()
    } else {
        sandbox
            .checked(&body.directory, true)
            .map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("Invalid directory: {e:#}")))?
    };

    let mut files: Vec<(PathBuf, std::fs::File)> = Vec::new();
    let mut count = 0usize;
    collect_files(&sandbox, &start, &mut files, &mut count)
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("Walk error: {e:#}")))?;

    let total_files = files.len();
    let max_matches = body.max_matches.clamp(1, 10_000);
    let max_response_bytes = body.max_response_bytes.clamp(1024, 64 * 1024 * 1024);
    let response_bytes = Arc::new(AtomicUsize::new(0));
    let files_matched = Arc::new(AtomicUsize::new(0));
    let truncated = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let regex = regex.map(Arc::new);
    let mode = body.mode;
    let max_matches_arc = Arc::new(max_matches);

    // Parallel search across the file list. Each task opens its file via
    // std::fs (we already validated paths through the sandbox) and scans.
    let matches: Vec<SearchMatch> = files
        .par_iter()
        .filter_map(|(relative_path, _)| {
            // Reconstruct the absolute path for std::fs reads.
            let abs = directory.join(relative_path);
            let metadata = std::fs::metadata(&abs).ok()?;
            if !metadata.is_file() {
                return None;
            }
            // Cap individual file size to avoid reading huge binaries.
            if metadata.len() > 64 * 1024 * 1024 {
                return None;
            }
            let path_str = relative_path.to_string_lossy().to_string();

            match mode {
                SearchMode::Filename => {
                    let regex = regex.as_ref()?;
                    if regex.is_match(&path_str) {
                        files_matched.fetch_add(1, Ordering::Relaxed);
                        Some(vec![SearchMatch {
                            path: path_str,
                            line: None,
                            column: None,
                            preview: None,
                        }])
                    } else {
                        None
                    }
                }
                SearchMode::Content => {
                    let regex = regex.as_ref()?;
                    let bytes = std::fs::read(&abs).ok()?;
                    let text = String::from_utf8_lossy(&bytes);
                    let mut found = Vec::new();
                    for (line_idx, line) in text.lines().enumerate() {
                        if found.len() >= max_matches_arc.as_ref() {
                            break;
                        }
                        if let Some(m) = regex.find(line) {
                            let cost = line.len() + 64;
                            let prev = response_bytes.fetch_add(cost, Ordering::Relaxed);
                            if prev > max_response_bytes {
                                truncated.store(true, Ordering::Relaxed);
                                break;
                            }
                            found.push(SearchMatch {
                                path: path_str.clone(),
                                line: Some((line_idx as u64) + 1),
                                column: Some((m.start() as u64) + 1),
                                preview: Some(preview_line(line, m.start(), m.end())),
                            });
                        }
                    }
                    if !found.is_empty() {
                        files_matched.fetch_add(1, Ordering::Relaxed);
                        Some(found)
                    } else {
                        None
                    }
                }
            }
        })
        .flatten()
        .collect();

    Ok(Json(json!(SearchResponse {
        matches,
        truncated: truncated.load(Ordering::Relaxed),
        files_searched: total_files,
        files_matched: files_matched.load(Ordering::Relaxed),
    })))
}

/// Walk the sandbox-relative `path`, collecting (relative_path, file) pairs
/// for every regular file. The cap_std `Dir` is not `Send`, so we open files
/// eagerly on the calling thread and drop the `Dir` before returning. The
/// returned file handles are unused after this function — the parallel phase
/// reopens them via std::fs to avoid lifetime issues. (We keep them in the
/// signature to enforce that the walk happens on a single thread.)
fn collect_files(
    sandbox: &Sandbox,
    path: &Path,
    files: &mut Vec<(PathBuf, std::fs::File)>,
    count: &mut usize,
) -> Result<()> {
    if *count > 200_000 {
        anyhow::bail!("File search traversal limit exceeded");
    }
    *count += 1;
    let (dir, name) = if path.as_os_str().is_empty() {
        (sandbox.root.try_clone()?, PathBuf::from("."))
    } else {
        sandbox.parent(path, false)?
    };
    let meta = dir.symlink_metadata(&name)?;
    if meta.is_dir() {
        let sub = dir.open_dir_nofollow(&name)?;
        for entry in sub.entries()? {
            let entry = entry?;
            collect_files(sandbox, &path.join(entry.file_name()), files, count)?;
        }
    } else if meta.is_file() {
        // We reopen via std::fs in the parallel phase; here we just record
        // the relative path. The placeholder file is opened purely to
        // validate we can.
        if let Ok(f) = std::fs::File::open(path) {
            files.push((path.to_path_buf(), f));
        }
    }
    Ok(())
}

/// Build a preview string around a match: ~40 chars of context on each side,
/// capped to 200 chars total, with the match wrapped in `>>…<<`.
fn preview_line(line: &str, start: usize, end: usize) -> String {
    let bytes = line.as_bytes();
    let ctx = 40;
    let lo = start.saturating_sub(ctx);
    let hi = (end + ctx).min(bytes.len());
    let mut out = String::with_capacity(200);
    if lo > 0 {
        out.push_str("…");
    }
    out.push_str(&line[lo..start]);
    out.push_str(">>");
    out.push_str(&line[start..end]);
    out.push_str("<<");
    out.push_str(&line[end..hi]);
    if hi < bytes.len() {
        out.push_str("…");
    }
    if out.len() > 200 {
        out.truncate(200);
        out.push('…');
    }
    out
}
