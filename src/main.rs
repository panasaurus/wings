mod api;
mod archives;
mod backup;
mod config;
mod engine;
mod file_api;
mod file_history;
mod file_search;
mod files;
mod hibernation;
mod lifecycle;
mod network;
mod openapi;
mod process;
mod sandbox;
mod service;
mod sftp;
mod sftp_extensions;
mod ssh_cli;
mod storage;
mod system_stats;
mod tokens;
mod transfers;
mod upgrade;
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
}

pub struct Server {
    settings: RwLock<Value>,
    operation: Arc<Mutex<()>>,
    installing: AtomicBool,
    console_slots: Arc<Semaphore>,
    console_users: Mutex<HashMap<String, Weak<Semaphore>>>,
    console_snapshot: Mutex<ConsoleSnapshot>,
    events: tokio::sync::broadcast::Sender<Value>,
    lifecycle: Mutex<lifecycle::Lifecycle>,
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
    config_path: PathBuf,
    audit_slots: Arc<Semaphore>,
    jobs: Arc<Semaphore>,
    used_tokens: Mutex<HashMap<String, u64>>,
    downloads: Mutex<HashMap<String, file_api::DownloadJob>>,
    transfers: Mutex<HashMap<String, tokio_util::sync::CancellationToken>>,
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
                let mut settings = item["settings"].clone();
                settings["_process_configuration"] = item["process_configuration"].clone();
                servers.insert(id.to_owned(), settings);
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

    async fn console_snapshot(&self, id: &str) -> Result<Option<Value>> {
        let server = self.server(id).await?;
        let mut snapshot = server.console_snapshot.lock().await;
        if snapshot
            .collected_at
            .is_none_or(|time| time.elapsed() >= Duration::from_secs(3))
        {
            snapshot.usage = self.engine.resources(id).await.ok();
            if let Some(usage) = &mut snapshot.usage {
                usage["hibernation"] = hibernation::summary(self, id).await;
                usage["state"] = serde_json::json!(lifecycle::status(self, id).await?);
            }
            snapshot.collected_at = Some(Instant::now());
        }
        Ok(snapshot.usage.clone())
    }

    async fn installation_callback(&self, id: &str, success: bool, reinstall: bool) -> Result<()> {
        self.remote_request(
            reqwest::Method::POST,
            &format!("servers/{id}/install"),
            Some(serde_json::json!({"successful":success,"reinstall":reinstall})),
        )
        .await?;
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().init();
    let _ = rustls_acme::rustls::crypto::aws_lc_rs::default_provider().install_default();
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|arg| arg == "--version") {
        println!("Panasaurus Wings Rust {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if service::cli(&args).await? {
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
        config_path: path.clone(),
        audit_slots: Arc::new(Semaphore::new(8)),
        jobs: Arc::new(Semaphore::new(config.limits.jobs)),
        used_tokens: Mutex::new(HashMap::new()),
        downloads: Mutex::new(HashMap::new()),
        transfers: Mutex::new(HashMap::new()),
    });
    let assigned = state
        .remote_servers()
        .await
        .context("Cannot authenticate to the panel or load node servers")?;
    for (id, settings) in assigned {
        state
            .servers
            .write()
            .await
            .insert(id, Arc::new(Server::new(settings)));
    }
    sandbox::private_directory(&config.private_root())?;
    hibernation::start(state.clone()).await?;
    lifecycle::start(state.clone()).await?;
    sftp::start(state.clone()).await?;
    service::serve(state).await
}

impl Server {
    fn new(settings: Value) -> Self {
        Self {
            settings: RwLock::new(settings),
            operation: Arc::new(Mutex::new(())),
            installing: AtomicBool::new(false),
            console_slots: Arc::new(Semaphore::new(16)),
            console_users: Mutex::new(HashMap::new()),
            console_snapshot: Mutex::new(ConsoleSnapshot::default()),
            events: tokio::sync::broadcast::channel(128).0,
            lifecycle: Mutex::new(lifecycle::Lifecycle::default()),
        }
    }
    fn emit(&self, event: &str, args: Vec<String>) {
        let _ = self
            .events
            .send(serde_json::json!({"event":event,"args":args}));
    }
}
impl State {
    async fn remote_request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value> {
        let config = self.config.read().await;
        let mut request = self
            .client
            .request(
                method,
                format!("{}/api/remote/{path}", config.remote.trim_end_matches('/')),
            )
            .bearer_auth(format!("{}.{}", config.token_id, config.token));
        drop(config);
        if let Some(body) = body {
            request = request.json(&body)
        }
        let mut last = None;
        for attempt in 0..3 {
            let response = request
                .try_clone()
                .context("Cannot retry request")?
                .send()
                .await;
            match response {
                Ok(r) if r.status().is_success() => {
                    let b = r.bytes().await?;
                    return if b.is_empty() {
                        Ok(Value::Null)
                    } else {
                        Ok(serde_json::from_slice(&b)?)
                    };
                }
                Ok(r) if r.status().is_client_error() => {
                    anyhow::bail!("Panel rejected callback: {}", r.status())
                }
                Ok(r) => last = Some(anyhow::anyhow!("Panel callback failed: {}", r.status())),
                Err(e) => last = Some(e.into()),
            }
            tokio::time::sleep(Duration::from_millis(250 * (attempt + 1))).await;
        }
        Err(last.context("Panel callback failed")?)
    }
    async fn audit(
        self: &Arc<Self>,
        event: &str,
        server: &str,
        user: &str,
        ip: &str,
        metadata: Value,
    ) {
        let body = serde_json::json!({"data":[{"event":event,"server":server,"user":if user.is_empty(){Value::Null}else{json_value(user)},"ip":if ip.is_empty(){Value::Null}else{json_value(ip)},"timestamp":chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs,true),"metadata":metadata}]});
        let Ok(permit) = self.audit_slots.clone().try_acquire_owned() else {
            tracing::warn!("Activity delivery queue is full");
            return;
        };
        let state = self.clone();
        tokio::spawn(async move {
            let _permit = permit;
            if state
                .remote_request(reqwest::Method::POST, "activity", Some(body))
                .await
                .is_err()
            {
                tracing::warn!("Activity delivery failed");
            }
        });
    }
}

fn json_value(v: &str) -> Value {
    Value::String(v.to_owned())
}
