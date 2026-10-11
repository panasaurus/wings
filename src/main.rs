mod api;
mod config;
mod engine;
mod files;
mod hibernation;
mod storage;
mod websocket;

use anyhow::{Context, Result, ensure};
use config::Config;
use engine::Engine;
use serde_json::Value;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Weak, atomic::AtomicBool},
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, RwLock, Semaphore};

#[derive(Default)]
pub struct ConsoleSnapshot {
    collected_at: Option<Instant>,
    usage: Option<Value>,
    lines: Vec<String>,
}

pub struct Server {
    settings: RwLock<Value>,
    operation: Arc<Mutex<()>>,
    installing: AtomicBool,
    console_slots: Arc<Semaphore>,
    console_users: Mutex<HashMap<String, Weak<Semaphore>>>,
    console_snapshot: Mutex<ConsoleSnapshot>,
}

pub struct State {
    config: RwLock<Config>,
    engine: Engine,
    client: reqwest::Client,
    servers: RwLock<HashMap<String, Arc<Server>>>,
    revoked: RwLock<HashMap<String, u64>>,
    booted_at: u64,
    console_slots: Arc<Semaphore>,
    hibernation: RwLock<HashMap<String, Arc<hibernation::Runtime>>>,
    wake_slots: Arc<Semaphore>,
    proxy_slots: Arc<Semaphore>,
}

impl State {
    async fn remote(&self, path: &str) -> Result<Value> {
        let config = self.config.read().await;
        let request = self
            .client
            .get(format!(
                "{}/api/remote/{path}",
                config.remote.trim_end_matches('/')
            ))
            .bearer_auth(format!("{}.{}", config.token_id, config.token));
        drop(config);
        Ok(request.send().await?.error_for_status()?.json().await?)
    }

    async fn remote_servers(&self) -> Result<HashMap<String, Value>> {
        let mut servers = HashMap::new();
        let mut page = 1;
        loop {
            let response = self
                .remote(&format!("servers?per_page=50&page={page}"))
                .await?;
            for item in response["data"]
                .as_array()
                .context("Panel server list is malformed")?
            {
                let id = item["uuid"].as_str().context("Missing server UUID")?;
                Engine::name(id)?;
                ensure!(item["settings"]["uuid"] == id, "Panel UUID mismatch");
                servers.insert(id.to_owned(), item["settings"].clone());
            }
            if page >= response["meta"]["last_page"].as_u64().unwrap_or(1) {
                break;
            }
            page += 1;
        }
        Ok(servers)
    }

    async fn server(&self, id: &str) -> Result<Arc<Server>> {
        self.servers
            .read()
            .await
            .get(id)
            .cloned()
            .context("Server is not assigned to this node")
    }

    async fn console_snapshot(&self, id: &str) -> Result<(Option<Value>, Vec<String>)> {
        let server = self.server(id).await?;
        let mut snapshot = server.console_snapshot.lock().await;
        if snapshot
            .collected_at
            .is_none_or(|time| time.elapsed() >= Duration::from_secs(3))
        {
            snapshot.usage = self.engine.resources(id).await.ok();
            if let Some(usage) = &mut snapshot.usage {
                usage["hibernation"] = hibernation::summary(self, id).await;
            }
            if let Ok(lines) = self.engine.logs(id).await {
                snapshot.lines = lines;
            }
            snapshot.collected_at = Some(Instant::now());
        }
        Ok((snapshot.usage.clone(), snapshot.lines.clone()))
    }

    async fn installation_callback(&self, id: &str, success: bool, reinstall: bool) -> Result<()> {
        let config = self.config.read().await;
        let request = self
            .client
            .post(format!(
                "{}/api/remote/servers/{id}/install",
                config.remote.trim_end_matches('/')
            ))
            .bearer_auth(format!("{}.{}", config.token_id, config.token))
            .json(&serde_json::json!({"successful":success,"reinstall":reinstall}));
        drop(config);
        request.send().await?.error_for_status()?;
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().init();
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|arg| arg == "--version") {
        println!("Panasaurus Wings Rust {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let path = args
        .iter()
        .position(|arg| arg == "--config")
        .and_then(|index| args.get(index + 1))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("config.yml"));
    let config = Config::load(&path)?;
    tokio::fs::create_dir_all(&config.system.data).await?;
    let engine = Engine {
        config: config.clone(),
    };
    engine
        .run(&engine::strings(&["info", "--format", "{{json .}}"]))
        .await
        .context("Docker is unavailable")?;
    let state = Arc::new(State {
        config: RwLock::new(config.clone()),
        engine,
        client: reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?,
        servers: RwLock::new(HashMap::new()),
        revoked: RwLock::new(HashMap::new()),
        booted_at: chrono::Utc::now().timestamp() as u64,
        console_slots: Arc::new(Semaphore::new(128)),
        hibernation: RwLock::new(HashMap::new()),
        wake_slots: Arc::new(Semaphore::new(4)),
        proxy_slots: Arc::new(Semaphore::new(1024)),
    });
    let assigned = state
        .remote_servers()
        .await
        .context("Cannot authenticate to the panel or load node servers")?;
    let count = assigned.len();
    for (id, settings) in assigned {
        state.servers.write().await.insert(
            id,
            Arc::new(Server {
                settings: RwLock::new(settings),
                operation: Arc::new(Mutex::new(())),
                installing: AtomicBool::new(false),
                console_slots: Arc::new(Semaphore::new(16)),
                console_users: Mutex::new(HashMap::new()),
                console_snapshot: Mutex::new(ConsoleSnapshot::default()),
            }),
        );
    }
    hibernation::start(state.clone()).await?;
    let listener =
        tokio::net::TcpListener::bind((config.api.host.as_str(), config.api.port)).await?;
    tracing::info!(address = %listener.local_addr()?, servers = count, "Panasaurus Rust node is ready");
    axum::serve(listener, api::router(state)?)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
