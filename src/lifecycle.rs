use crate::{Server, State, engine::strings};
use anyhow::{Result, ensure};
use serde_json::json;
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
#[derive(Default)]
pub struct Lifecycle {
    pub desired: bool,
    pub status: String,
    pub last_crash: Option<Instant>,
    pub transferring: bool,
    pub observed: bool,
    pub log_started: bool,
}
pub async fn status(state: &State, id: &str) -> Result<String> {
    let server = state.server(id).await?;
    let actual = state.engine.state(id).await?;
    let life = server.lifecycle.lock().await;
    Ok(if actual == "offline" {
        "offline".into()
    } else if matches!(life.status.as_str(), "starting" | "stopping") {
        life.status.clone()
    } else {
        "running".into()
    })
}
pub async fn stop(state: &State, server: &Server, id: &str, force: bool) -> Result<()> {
    {
        let mut life = server.lifecycle.lock().await;
        life.desired = false;
        life.status = "stopping".into();
    }
    server.emit("status", vec!["stopping".into()]);
    if state.engine.state(id).await? == "offline" {
        server.lifecycle.lock().await.status = "offline".into();
        return Ok(());
    }
    if force {
        state
            .engine
            .run(&strings(&["kill", &crate::engine::Engine::name(id)?]))
            .await?;
        return Ok(());
    }
    let settings = server.settings.read().await.clone();
    let stop = &settings["_process_configuration"]["stop"];
    let value = stop["value"].as_str().unwrap_or("");
    match stop["type"].as_str() {
        Some("command") if !value.is_empty() => state.engine.send_command(id, value).await?,
        Some("signal") => {
            let signal = match value.to_uppercase().as_str() {
                "C" | "SIGINT" => "SIGINT",
                "SIGTERM" => "SIGTERM",
                "SIGKILL" => "SIGKILL",
                "SIGQUIT" => "SIGQUIT",
                "SIGHUP" => "SIGHUP",
                "SIGABRT" => "SIGABRT",
                _ => anyhow::bail!("Unsupported egg stop signal"),
            };
            state
                .engine
                .run(&strings(&[
                    "kill",
                    "--signal",
                    signal,
                    &crate::engine::Engine::name(id)?,
                ]))
                .await?;
        }
        _ => {
            state
                .engine
                .run(&strings(&[
                    "kill",
                    "--signal",
                    "SIGTERM",
                    &crate::engine::Engine::name(id)?,
                ]))
                .await?;
        }
    }
    let timeout = state.config.read().await.recovery.stop_timeout_seconds;
    let deadline = Instant::now() + Duration::from_secs(timeout);
    while state.engine.state(id).await? != "offline" {
        ensure!(
            Instant::now() < deadline,
            "Server did not stop gracefully; use Kill only if necessary"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    server.lifecycle.lock().await.status = "offline".into();
    server.emit("status", vec!["offline".into()]);
    Ok(())
}
pub async fn begin(state: &State, server: &Server, id: &str) -> Result<()> {
    let actual = state.engine.state(id).await?;
    let mut life = server.lifecycle.lock().await;
    ensure!(!life.transferring, "Server is being transferred");
    life.desired = true;
    if actual != "running" {
        life.status = "starting".into();
        server.emit("status", vec!["starting".into()]);
    }
    Ok(())
}
pub async fn start(state: Arc<State>) -> Result<()> {
    let path = state.engine.config.private_root().join("states.json");
    let states: HashMap<String, bool> = if path.exists() {
        let bytes = std::fs::read(&path)?;
        ensure!(bytes.len() <= 1024 * 1024, "State journal too large");
        serde_json::from_slice(&bytes)?
    } else {
        HashMap::new()
    };
    for (id, server) in state.servers.read().await.iter() {
        if state.engine.cleanup_installer(id).await? {
            let _ = state.installation_callback(id, false, true).await;
        }
        let actual = state.engine.state(id).await?;
        let mut life = server.lifecycle.lock().await;
        life.desired = actual == "running" || states.get(id).copied().unwrap_or(false);
        life.status = actual;
    }

    state
        .remote_request(reqwest::Method::POST, "servers/reset", Some(json!({})))
        .await?;
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        let mut journal_tick = 0u64;
        loop {
            interval.tick().await;
            journal_tick += 1;
            let servers: Vec<_> = state
                .servers
                .read()
                .await
                .iter()
                .map(|(id, s)| (id.clone(), s.clone()))
                .collect();
            for (id, server) in servers {
                if !server.lifecycle.lock().await.log_started {
                    server.lifecycle.lock().await.log_started = true;
                    spawn_logs(state.clone(), id.clone(), server.clone());
                }
                if server.installing.load(std::sync::atomic::Ordering::SeqCst) {
                    continue;
                }
                let Ok(container) = state.engine.inspect(&id).await else {
                    continue;
                };
                let running = container
                    .as_ref()
                    .is_some_and(|c| c["State"]["Running"] == true);
                let config = state.config.read().await.recovery.clone();
                let sleeping = crate::hibernation::summary(&state, &id).await["sleeping"] == true;
                let needs_readiness =
                    server.settings.read().await["_process_configuration"]["startup"]["done"]
                        .as_array()
                        .is_some_and(|v| !v.is_empty());
                let mut life = server.lifecycle.lock().await;
                if running {
                    life.observed = true;
                    if life.status.is_empty()
                        || life.status == "offline"
                        || life.status == "starting" && !needs_readiness
                    {
                        life.status = "running".into();
                        server.emit("status", vec!["running".into()]);
                    }
                    continue;
                }
                let previous = life.status.clone();
                life.status = "offline".into();
                if previous != "offline" {
                    server.emit("status", vec!["offline".into()]);
                }
                if !life.desired || life.transferring || !config.enabled {
                    continue;
                }
                if sleeping {
                    continue;
                }
                let crashed = container.as_ref().is_some_and(|c| {
                    c["State"]["OOMKilled"] == true
                        || c["State"]["ExitCode"].as_i64().unwrap_or(0) != 0
                        || config.detect_clean_exit
                });
                let boot_recovery = !life.observed && config.enabled;
                if !(boot_recovery || (config.restart_crashed && crashed)) {
                    life.desired = false;
                    continue;
                }
                if life.last_crash.is_some_and(|t| {
                    t.elapsed() < Duration::from_secs(config.crash_cooldown_seconds)
                }) {
                    life.desired = false;
                    server.emit(
                        "daemon message",
                        vec![
                            "Automatic restart stopped because the server crashed repeatedly."
                                .into(),
                        ],
                    );
                    continue;
                }
                let Ok(lock) = server.operation.clone().try_lock_owned() else {
                    continue;
                };
                life.last_crash = Some(Instant::now());
                life.observed = true;
                drop(life);
                let state = state.clone();
                tokio::spawn(async move {
                    let _lock = lock;
                    let result = async {
                        let settings = crate::process::refresh(&state, &server, &id).await?;
                        ensure!(
                            !settings["suspended"].as_bool().unwrap_or(false),
                            "Server suspended"
                        );
                        begin(&state, &server, &id).await?;
                        state.engine.power(&settings, "start").await?;
                        Ok::<(), anyhow::Error>(())
                    }
                    .await;
                    if let Err(error) = result {
                        server.lifecycle.lock().await.desired = false;
                        server.emit("daemon message",vec!["Automatic recovery failed; check the node logs and start manually.".into()]);
                        tracing::warn!(server=%id,error=%error,"Automatic server recovery failed");
                    }
                });
            }
            if journal_tick.is_multiple_of(5) {
                let result = persist(&state).await;
                if result.is_err() {
                    tracing::warn!("Could not persist server recovery state");
                }
            }
        }
    });
    Ok(())
}
fn spawn_logs(state: Arc<State>, id: String, server: Arc<Server>) {
    tokio::spawn(async move {
        let mut previous_start = String::new();
        let mut since = String::new();
        loop {
            if state.server(&id).await.is_err() {
                return;
            }
            let container = state.engine.inspect(&id).await.ok().flatten();
            if let Some(container) = container.filter(|c| c["State"]["Running"] == true) {
                let started = container["State"]["StartedAt"].as_str().unwrap_or("");
                if chrono::DateTime::parse_from_rfc3339(started).is_ok() {
                    if started != previous_start {
                        previous_start = started.to_owned();
                        since = started.to_owned();
                    }
                    let mut command = state.engine.command();
                    command
                        .args([
                            "logs",
                            "--follow",
                            "--since",
                            &since,
                            &crate::engine::Engine::name(&id).unwrap(),
                        ])
                        .stdout(std::process::Stdio::piped())
                        .stderr(std::process::Stdio::piped());
                    if let Ok(mut child) = command.spawn()
                        && let (Some(out), Some(err)) = (child.stdout.take(), child.stderr.take())
                    {
                        tokio::join!(
                            forward(
                                state.clone(),
                                id.clone(),
                                server.clone(),
                                out,
                                "console output"
                            ),
                            forward(
                                state.clone(),
                                id.clone(),
                                server.clone(),
                                err,
                                "console output"
                            )
                        );
                        let _ = child.wait().await;
                        since = chrono::Utc::now().to_rfc3339();
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });
}
pub async fn forward<R: tokio::io::AsyncRead + Unpin>(
    state: Arc<State>,
    id: String,
    server: Arc<Server>,
    reader: R,
    event: &str,
) {
    use futures_util::StreamExt;
    use tokio_util::codec::{FramedRead, LinesCodec};
    let mut install_log = if event == "install output" {
        let path = state.engine.config.private_root().join("install-logs");
        if crate::sandbox::private_directory(&path).is_ok() {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(path.join(format!("{id}.log")))
                .ok()
        } else {
            None
        }
    } else {
        None
    };
    let mut lines = FramedRead::new(reader, LinesCodec::new_with_max_length(65536));
    while let Some(line) = lines.next().await {
        let Ok(line) = line else {
            server.emit(
                "daemon message",
                vec!["An oversized console line was discarded.".into()],
            );
            continue;
        };
        if event == "console output" && server.lifecycle.lock().await.status == "starting" {
            let startup = server.settings.read().await["_process_configuration"]["startup"].clone();
            if crate::process::ready(&line, &json!({"startup":startup})) {
                let mut life = server.lifecycle.lock().await;
                if life.status == "starting" {
                    life.status = "running".into();
                    server.emit("status", vec!["running".into()]);
                }
            }
        }
        if let Some(log) = &mut install_log {
            use std::io::Write;
            if log.metadata().is_ok_and(|m| m.len() < 10 * 1024 * 1024) {
                let _ = writeln!(log, "{line}");
            }
        }
        server.emit(event, vec![line]);
        if state.server(&id).await.is_err() {
            break;
        }
    }
}

pub async fn persist(state: &State) -> Result<()> {
    let mut desired = HashMap::new();
    for (id, server) in state.servers.read().await.iter() {
        desired.insert(id.clone(), server.lifecycle.lock().await.desired);
    }
    use std::io::Write;
    let root = state.engine.config.private_root();
    let mut output = tempfile::NamedTempFile::new_in(&root)?;
    output.write_all(&serde_json::to_vec(&desired)?)?;
    output.as_file().sync_all()?;
    output.persist(root.join("states.json"))?;
    Ok(())
}
