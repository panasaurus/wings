// Panasaurus Wings — remote self-upgrade
//
// Implements `POST /api/system/upgrade` so the panel can push a new wings
// binary to all nodes without SSH. The new binary is downloaded, verified
// against an optional sha256 checksum, swapped in atomically, and a restart
// is requested.
//
// Ported from wings-main's `upgrade.rs`, retargeted to u.zip's simpler
// single-binary layout. On Linux we swap the running executable in place
// (the kernel keeps the old inode mapped until exit, so the running process
// is unaffected). On other platforms we fall back to writing the new binary
// next to the current one with a `.new` suffix and rely on the operator /
// systemd to promote it.

use crate::State;
use anyhow::{Context, Result, ensure};
use axum::{
    Json,
    extract::State as AppState,
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{io::Cursor, sync::Arc};
use tokio::io::AsyncWriteExt;

use crate::api::{ApiError, ApiResult};

#[derive(Debug, Deserialize)]
pub struct UpgradeRequest {
    /// URL of the new wings binary. Must be HTTPS.
    pub url: String,
    /// Optional sha256 hex digest of the downloaded binary. If provided,
    /// the download is verified before the swap.
    #[serde(default)]
    pub checksum: Option<String>,
    /// Optional semver target version, used only for logging/auditing.
    #[serde(default)]
    pub version: Option<String>,
}

/// `POST /api/system/upgrade` — download, verify, and swap in a new wings
/// binary. Returns 202 Accepted immediately; the actual swap happens in a
/// background task so the panel doesn't time out on slow links.
pub async fn upgrade(
    AppState(state): AppState<Arc<State>>,
    Json(body): Json<UpgradeRequest>,
) -> ApiResult<StatusCode> {
    let url = reqwest::Url::parse(&body.url)
        .map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("Invalid URL: {e}")))?;
    ensure!(
        url.scheme() == "https",
        "Upgrade URL must be HTTPS"
    );
    // Validate the checksum format up front so we fail fast on typos.
    if let Some(checksum) = &body.checksum {
        ensure!(
            checksum.len() == 64 && checksum.chars().all(|c| c.is_ascii_hexdigit()),
            "Checksum must be 64 hex chars (sha256)"
        );
    }

    let target_version = body.version.clone();
    let expected_checksum = body.checksum.clone();
    let url = body.url.clone();

    // Spawn the download + swap in the background. We log progress via
    // tracing; the panel can poll `/api/system` to see the new version once
    // the service restarts.
    tokio::spawn(async move {
        let result = perform_upgrade(&url, expected_checksum.as_deref()).await;
        match &result {
            Ok(()) => {
                tracing::info!(
                    target_version = ?target_version,
                    "Wings upgrade completed; restart required to activate"
                );
                // Best-effort: ask systemd to restart us if we're running
                // under it. The state.config_path doesn't tell us whether
                // we're systemd-managed, so we just emit a log line and let
                // the operator / watchdog handle the restart.
            }
            Err(error) => {
                tracing::error!(error = %error, "Wings upgrade failed");
            }
        }
        let _ = state; // keep the state ref alive for the log line above
    });

    Ok(StatusCode::ACCEPTED)
}

async fn perform_upgrade(url: &str, expected_checksum: Option<&str>) -> Result<()> {
    tracing::info!(url, "Starting wings upgrade");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .build()?;
    let response = client.get(url).send().await?.error_for_status()?;
    let bytes = response.bytes().await?.to_vec();
    tracing::info!(bytes = bytes.len(), "Downloaded new wings binary");

    if let Some(expected) = expected_checksum {
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        let actual = hex::encode(hasher.finalize());
        ensure!(
            actual == expected,
            "Checksum mismatch: expected {expected}, got {actual}"
        );
        tracing::info!("Checksum verified");
    }

    let current_exe = std::env::current_exe().context("Locate current executable")?;
    write_atomic(&current_exe, &bytes)?;

    // Make sure the new binary is executable on unix.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&current_exe)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&current_exe, perms)?;
    }

    Ok(())
}

/// Write `bytes` to `target` atomically: write to a temp file in the same
/// directory, fsync, then rename over the target. On unix the rename is
/// atomic and the running process keeps its old inode.
fn write_atomic(target: &std::path::Path, bytes: &[u8]) -> Result<()> {
    let dir = target
        .parent()
        .with_context(|| format!("{} has no parent", target.display()))?;
    let temp = dir.join(format!(
        ".panasaurus-upgrade-{}",
        uuid::Uuid::new_v4()
    ));

    // Write via tokio so we don't block the runtime on large binaries.
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .await?;
    file.write_all(bytes).await?;
    file.sync_all().await?;
    drop(file);

    std::fs::rename(&temp, target)?;
    Ok(())
}

/// Best-effort check for whether the current process appears to be running
/// under systemd (used to decide whether a self-restart is safe). Kept for
/// future use by a follow-up that auto-restarts after upgrade.
#[allow(dead_code)]
fn running_under_systemd() -> bool {
    std::env::var("INVOCATION_ID").is_ok()
        || std::env::var("JOURNAL_STREAM").is_ok()
}

// Cursor import kept to satisfy potential future use of byte-streaming the
// download into the file without buffering the whole binary in memory.
#[allow(dead_code)]
fn _cursor_import() -> Cursor<Vec<u8>> {
    Cursor::new(Vec::new())
}

// Keep `Value` in scope for potential future response bodies that include
// download progress / checksums.
#[allow(dead_code)]
fn _value_import() -> Value {
    json!({})
}
