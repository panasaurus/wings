mod policy;
mod protocol;

use crate::{Config, Engine, State};
use anyhow::{Context, Result, ensure};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::{
    ambient_authority,
    fs::{Dir, OpenOptions, OpenOptionsExt},
};
use policy::IdlePolicy;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    io::{Read, Write},
    net::{IpAddr, SocketAddr},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{Mutex, Notify, Semaphore},
    time::{sleep, timeout},
};

#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Route {
    pub server: String,
    pub egg: String,
    pub listen: SocketAddr,
    pub idle_timeout_seconds: u64,
    pub startup_grace_seconds: u64,
    pub poll_interval_seconds: u64,
    pub startup_timeout_seconds: u64,
    pub stop_timeout_seconds: u64,
    pub connection_limit: usize,
}
impl Default for Route {
    fn default() -> Self {
        Self {
            server: String::new(),
            egg: String::new(),
            listen: "127.0.0.1:25565".parse().unwrap(),
            idle_timeout_seconds: 600,
            startup_grace_seconds: 120,
            poll_interval_seconds: 15,
            startup_timeout_seconds: 120,
            stop_timeout_seconds: 120,
            connection_limit: 128,
        }
    }
}
impl Route {
    pub fn validate_settings(&self, settings: &Value) -> Result<()> {
        ensure!(
            settings["uuid"] == self.server
                && settings["egg"]["id"].as_str() == Some(self.egg.as_str()),
            "Hibernation requires the configured Minecraft Java egg UUID"
        );
        let allocation = &settings["allocations"]["default"];
        let ip: IpAddr = allocation["ip"]
            .as_str()
            .context("Missing primary allocation IP")?
            .parse()?;
        ensure!(
            ip == self.listen.ip()
                && allocation["port"].as_u64() == Some(u64::from(self.listen.port())),
            "Hibernation listener must exactly match the primary allocation; update configuration and restart the daemon"
        );
        let ports = settings["allocations"]["mappings"][ip.to_string()]
            .as_array()
            .context("Missing primary allocation mapping")?;
        ensure!(
            ports
                .iter()
                .any(|port| port.as_u64() == Some(u64::from(self.listen.port()))),
            "Primary allocation is not mapped"
        );
        Ok(())
    }
}

pub fn validate_config(config: &Config) -> Result<()> {
    ensure!(
        config.hibernation.len() <= 64,
        "At most 64 hibernation listeners are supported"
    );
    let mut ids = HashSet::new();
    let mut addresses = HashSet::new();
    for route in &config.hibernation {
        Engine::name(&route.server)?;
        ensure!(
            uuid::Uuid::parse_str(&route.egg)?.to_string() == route.egg,
            "Use a canonical Minecraft egg UUID"
        );
        ensure!(ids.insert(&route.server), "Duplicate hibernation server");
        ensure!(
            addresses.insert(route.listen),
            "Duplicate hibernation listener"
        );
        ensure!(
            route.listen.port() > 0 && route.listen.port() != config.api.port,
            "Invalid or conflicting hibernation port"
        );
        ensure!(
            route.listen.is_ipv4(),
            "Minecraft hibernation currently requires an IPv4 allocation"
        );
        ensure!(
            (1..=86400).contains(&route.idle_timeout_seconds),
            "Idle timeout must be 1–86400 seconds"
        );
        ensure!(
            (1..=3600).contains(&route.startup_grace_seconds),
            "Startup grace must be 1–3600 seconds"
        );
        ensure!(
            (1..=60).contains(&route.poll_interval_seconds),
            "Poll interval must be 1–60 seconds"
        );
        ensure!(
            (1..=600).contains(&route.startup_timeout_seconds)
                && (1..=600).contains(&route.stop_timeout_seconds),
            "Startup/stop timeouts must be 1–600 seconds"
        );
        ensure!(
            (1..=1024).contains(&route.connection_limit),
            "Connection limit must be 1–1024"
        );
        for other in &config.hibernation {
            if route.server != other.server && route.listen.port() == other.listen.port() {
                ensure!(
                    !route.listen.ip().is_unspecified() && !other.listen.ip().is_unspecified(),
                    "Overlapping hibernation listeners"
                );
            }
        }
    }
    Ok(())
}

pub fn route_for<'a>(config: &'a Config, id: &str) -> Option<&'a Route> {
    config.hibernation.iter().find(|route| route.server == id)
}

pub fn publish_address(
    config: &Config,
    settings: &Value,
    ip: &str,
    port: u16,
    protocol: &str,
) -> Result<String> {
    let id = settings["uuid"].as_str().context("Missing UUID")?;
    if let Some(route) = route_for(config, id) {
        route.validate_settings(settings)?;
        if port == route.listen.port()
            && ip.parse::<IpAddr>()? == route.listen.ip()
            && protocol == "tcp"
        {
            return Ok(format!("127.0.0.1::{port}/tcp"));
        }
        ensure!(
            port != route.listen.port() || protocol != "tcp",
            "Primary port cannot be exposed through another allocation"
        );
    }
    Ok(format!("{ip}:{port}:{port}/{protocol}"))
}

pub fn network_matches(config: &Config, settings: &Value, container: &Value) -> Result<bool> {
    let id = settings["uuid"].as_str().context("Missing UUID")?;
    let label = &container["Config"]["Labels"]["dev.panasaurus.hibernation"];
    if let Some(route) = route_for(config, id) {
        route.validate_settings(settings)?;
        let key = format!("{}/tcp", route.listen.port());
        let bindings = container["HostConfig"]["PortBindings"][key].as_array();
        return Ok(label.as_str() == Some(route.listen.to_string().as_str())
            && bindings
                .is_some_and(|items| items.len() == 1 && items[0]["HostIp"] == "127.0.0.1"));
    }
    Ok(label.is_null())
}

fn backend(route: &Route, container: &Value) -> Result<SocketAddr> {
    ensure!(
        container["Config"]["Labels"]["dev.panasaurus.hibernation"] == route.listen.to_string(),
        "Recreate this container offline to enable the private Minecraft backend"
    );
    let key = format!("{}/tcp", route.listen.port());
    let ports = container["NetworkSettings"]["Ports"][key]
        .as_array()
        .context("Private backend is not published")?;
    ensure!(
        ports.len() == 1 && ports[0]["HostIp"] == "127.0.0.1",
        "Refusing a non-private Minecraft backend"
    );
    let port: u16 = ports[0]["HostPort"]
        .as_str()
        .context("Missing backend port")?
        .parse()?;
    ensure!(
        port > 0 && port != route.listen.port(),
        "Invalid backend port"
    );
    Ok(SocketAddr::from(([127, 0, 0, 1], port)))
}

struct RecordStore {
    directory: Dir,
    name: String,
    listen: String,
}
#[derive(Serialize, Deserialize)]
struct Record {
    sleeping: bool,
    listen: String,
}
impl RecordStore {
    fn open(root: &Path, route: &Route) -> Result<Self> {
        let parent = Dir::open_ambient_dir(root, ambient_authority())?;
        match parent.create_dir(".hibernation") {
            Ok(()) => (),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(error) => return Err(error.into()),
        }
        let directory = parent.open_dir_nofollow(".hibernation")?;
        use std::os::unix::fs::PermissionsExt;
        directory.set_permissions(
            ".",
            cap_std::fs::Permissions::from_std(std::fs::Permissions::from_mode(0o700)),
        )?;
        Ok(Self {
            directory,
            name: format!("{}.json", route.server),
            listen: route.listen.to_string(),
        })
    }
    fn load(&self) -> Result<bool> {
        let mut options = OpenOptions::new();
        options
            .read(true)
            .follow(FollowSymlinks::No)
            .custom_flags(libc::O_NONBLOCK);
        let file = match self.directory.open_with(&self.name, &options) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        ensure!(
            file.metadata()?.is_file() && file.metadata()?.len() <= 256,
            "Invalid hibernation record"
        );
        let mut bytes = Vec::new();
        file.take(257).read_to_end(&mut bytes)?;
        let record: Record = serde_json::from_slice(&bytes)?;
        Ok(record.sleeping && record.listen == self.listen)
    }
    fn save(&self, sleeping: bool) -> Result<()> {
        let temp = format!("{}.tmp", uuid::Uuid::new_v4());
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .mode(0o600)
            .follow(FollowSymlinks::No);
        let result = (|| -> Result<()> {
            let mut file = self.directory.open_with(&temp, &options)?;
            file.write_all(&serde_json::to_vec(&Record {
                sleeping,
                listen: self.listen.clone(),
            })?)?;
            file.sync_all()?;
            self.directory.rename(&temp, &self.directory, &self.name)?;
            self.directory.open(".")?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = self.directory.remove_file(&temp);
        }
        result
    }
}

struct Activity {
    sleeping: bool,
    policy: IdlePolicy,
    cached: Option<Value>,
    players: Option<u64>,
    error: Option<&'static str>,
    retry_after: Option<Instant>,
}
pub struct Runtime {
    route: Route,
    activity: Mutex<Activity>,
    store: RecordStore,
    connections: Arc<Semaphore>,
    joining: AtomicUsize,
    removed: AtomicBool,
    shutdown: Notify,
}
struct Joining(Arc<Runtime>);
impl Joining {
    fn new(runtime: Arc<Runtime>) -> Self {
        runtime.joining.fetch_add(1, Ordering::SeqCst);
        Self(runtime)
    }
}
impl Drop for Joining {
    fn drop(&mut self) {
        self.0.joining.fetch_sub(1, Ordering::SeqCst);
    }
}

pub async fn start(state: Arc<State>) -> Result<()> {
    let mut listeners = Vec::new();
    for route in &state.engine.config.hibernation {
        let Ok(server) = state.server(&route.server).await else {
            tracing::warn!(server=%route.server, "Skipping hibernation listener for an unassigned server");
            continue;
        };
        if let Err(error) = route.validate_settings(&*server.settings.read().await) {
            tracing::error!(server=%route.server,error=%error,"Hibernation configuration must be updated before this server can start");
            continue;
        }
        let store = RecordStore::open(&state.engine.config.system.data, route)?;
        let sleeping = store.load()?;
        let listener = TcpListener::bind(route.listen).await.context("Cannot bind Minecraft allocation; stop/recreate the old container offline before enabling hibernation")?;
        let runtime = Arc::new(Runtime {
            route: route.clone(),
            store,
            connections: Arc::new(Semaphore::new(route.connection_limit)),
            joining: AtomicUsize::new(0),
            removed: AtomicBool::new(false),
            shutdown: Notify::new(),
            activity: Mutex::new(Activity {
                sleeping,
                policy: IdlePolicy::new(
                    Instant::now(),
                    Duration::from_secs(route.idle_timeout_seconds),
                    Duration::from_secs(route.startup_grace_seconds),
                ),
                cached: None,
                players: None,
                error: None,
                retry_after: None,
            }),
        });
        state
            .hibernation
            .write()
            .await
            .insert(route.server.clone(), runtime.clone());
        listeners.push((listener, runtime));
    }
    for (listener, runtime) in listeners {
        let state_clone = state.clone();
        let runtime_clone = runtime.clone();
        tokio::spawn(async move {
            monitor(state_clone, runtime_clone).await;
        });
        let state_clone = state.clone();
        tokio::spawn(async move {
            loop {
                if runtime.removed.load(Ordering::SeqCst) {
                    break;
                }
                let accepted = tokio::select! { result=listener.accept() => result, _=runtime.shutdown.notified() => break };
                let Ok((stream, _)) = accepted else {
                    break;
                };
                let Ok(global) = state_clone.proxy_slots.clone().try_acquire_owned() else {
                    continue;
                };
                let Ok(local) = runtime.connections.clone().try_acquire_owned() else {
                    continue;
                };
                let state = state_clone.clone();
                let runtime = runtime.clone();
                tokio::spawn(async move {
                    let _permits = (global, local);
                    let _ = serve(state, runtime, stream).await;
                });
            }
        });
    }
    Ok(())
}

pub async fn summary(state: &State, id: &str) -> Value {
    let Some(runtime) = state.hibernation.read().await.get(id).cloned() else {
        return json!({"enabled":false});
    };
    let activity = runtime.activity.lock().await;
    json!({"enabled":true,"sleeping":activity.sleeping,"players":activity.players,"idle_timeout_seconds":runtime.route.idle_timeout_seconds,"error":activity.error})
}

pub async fn manual_power(state: &State, id: &str, action: &str) -> Result<()> {
    if let Some(runtime) = state.hibernation.read().await.get(id).cloned() {
        let mut activity = runtime.activity.lock().await;
        runtime.store.save(false)?;
        activity.sleeping = false;
        activity.cached = None;
        activity.players = None;
        activity.error = None;
        activity.retry_after = None;
        if matches!(action, "start" | "restart") {
            activity.policy.started(Instant::now());
        } else {
            activity.policy.joined(Instant::now());
        }
    }
    Ok(())
}
pub async fn remove(state: &State, id: &str) {
    if let Some(runtime) = state.hibernation.write().await.remove(id) {
        runtime.removed.store(true, Ordering::SeqCst);
        runtime.shutdown.notify_waiters();
    }
}

async fn serve(state: Arc<State>, runtime: Arc<Runtime>, mut stream: TcpStream) -> Result<()> {
    let (frame, hello) = timeout(Duration::from_secs(5), async {
        let frame = protocol::read_frame(&mut stream, protocol::HANDSHAKE_LIMIT).await?;
        let hello = protocol::handshake(&frame)?;
        Ok::<_, anyhow::Error>((frame, hello))
    })
    .await
    .context("Handshake timed out")??;
    if hello.next_state == 1 {
        let activity = runtime.activity.lock().await;
        let value = if activity.sleeping {
            protocol::unavailable(
                activity.cached.as_ref(),
                hello.protocol,
                "Sleeping — join to wake",
            )
        } else {
            activity.cached.clone().unwrap_or_else(|| {
                protocol::unavailable(None, hello.protocol, "Server offline or starting")
            })
        };
        drop(activity);
        return timeout(
            Duration::from_secs(5),
            protocol::respond(&mut stream, &value),
        )
        .await
        .context("Status client timed out")?;
    }
    let login = timeout(
        Duration::from_secs(5),
        protocol::read_frame(&mut stream, protocol::HANDSHAKE_LIMIT),
    )
    .await
    .context("Login Start timed out")??;
    protocol::login_start(&login)?;
    let _joining = Joining::new(runtime.clone());
    let address = ready(&state, &runtime).await?;
    let mut upstream = timeout(Duration::from_secs(3), TcpStream::connect(address)).await??;
    protocol::write_frame(&mut upstream, &frame).await?;
    protocol::write_frame(&mut upstream, &login).await?;
    tokio::select! {
        result=tokio::io::copy_bidirectional(&mut stream,&mut upstream) => { result?; },
        _=runtime.shutdown.notified() => (),
    }
    Ok(())
}

async fn ready(state: &State, runtime: &Runtime) -> Result<SocketAddr> {
    ensure!(!runtime.removed.load(Ordering::SeqCst), "Server removed");
    let server = state.server(&runtime.route.server).await?;
    let _operation = timeout(
        Duration::from_secs(runtime.route.startup_timeout_seconds),
        server.operation.lock(),
    )
    .await
    .context("Server operation busy")?;
    ensure!(
        !runtime.removed.load(Ordering::SeqCst) && !server.installing.load(Ordering::SeqCst),
        "Server unavailable"
    );
    let id = &runtime.route.server;
    let (sleeping, retry) = {
        let mut activity = runtime.activity.lock().await;
        activity.policy.joined(Instant::now());
        (activity.sleeping, activity.retry_after)
    };
    ensure!(
        retry.is_none_or(|time| Instant::now() >= time),
        "Startup cooldown; retry later"
    );
    if sleeping {
        let _wake = state
            .wake_slots
            .clone()
            .try_acquire_owned()
            .context("Node startup limit reached; retry later")?;
        let result = timeout(
            Duration::from_secs(runtime.route.startup_timeout_seconds),
            async {
                let settings = state
                    .remote_servers()
                    .await?
                    .remove(id)
                    .context("Server no longer assigned to this node")?;
                runtime.route.validate_settings(&settings)?;
                ensure!(
                    !settings["suspended"].as_bool().unwrap_or(false),
                    "Server suspended"
                );
                crate::api::reconcile_settings(state, &server, id, settings.clone(), true)
                    .await
                    .map_err(|error| anyhow::anyhow!(error.1))?;
                state.engine.power(&settings, "start").await?;
                let deadline =
                    Instant::now() + Duration::from_secs(runtime.route.startup_timeout_seconds);
                loop {
                    let container = state
                        .engine
                        .inspect(id)
                        .await?
                        .context("Minecraft container missing")?;
                    let address = backend(&runtime.route, &container)?;
                    if let Ok(status) = protocol::query(address).await {
                        runtime.store.save(false)?;
                        let mut activity = runtime.activity.lock().await;
                        activity.sleeping = false;
                        activity.cached = Some(status.clone());
                        activity.players = status["players"]["online"].as_u64();
                        activity.policy.started(Instant::now());
                        activity.error = None;
                        activity.retry_after = None;
                        tracing::info!(server=%id,"Minecraft woke from hibernation");
                        return Ok(address);
                    }
                    ensure!(
                        Instant::now() < deadline && container["State"]["Running"] == true,
                        "Minecraft startup failed or timed out"
                    );
                    sleep(Duration::from_secs(1)).await;
                }
            },
        )
        .await
        .context("Minecraft wake timed out")
        .and_then(|result| result);
        if result.is_err() {
            let mut activity = runtime.activity.lock().await;
            activity.retry_after = Some(Instant::now() + Duration::from_secs(60));
            activity.error =
                Some("Automatic startup failed; retry after 60 seconds or start from the panel");
        }
        return result;
    }
    ensure!(
        !server.settings.read().await["suspended"]
            .as_bool()
            .unwrap_or(false),
        "Server suspended"
    );
    let container = state
        .engine
        .inspect(id)
        .await?
        .context("Server offline; start it from the panel")?;
    ensure!(
        container["State"]["Running"] == true,
        "Server manually stopped; start it from the panel"
    );
    backend(&runtime.route, &container)
}

async fn monitor(state: Arc<State>, runtime: Arc<Runtime>) {
    let mut interval =
        tokio::time::interval(Duration::from_secs(runtime.route.poll_interval_seconds));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! { _=interval.tick() => (), _=runtime.shutdown.notified() => break }
        if runtime.removed.load(Ordering::SeqCst) {
            break;
        }
        if let Err(error) = observe(&state, &runtime).await {
            let mut activity = runtime.activity.lock().await;
            activity.policy.observe(Instant::now(), None, 0);
            activity.players = None;
            activity.cached = None;
            activity.error =
                Some("Player count or container state unavailable; automatic sleep skipped");
            tracing::warn!(server=%runtime.route.server,error=%error,"Hibernation observation failed; preserving server state");
        }
    }
}

async fn observe(state: &State, runtime: &Runtime) -> Result<()> {
    let id = &runtime.route.server;
    let server = state.server(id).await?;
    let Ok(_operation) = server.operation.try_lock() else {
        return Ok(());
    };
    if server.installing.load(Ordering::SeqCst) {
        return Ok(());
    }
    runtime
        .route
        .validate_settings(&*server.settings.read().await)?;
    let container = state.engine.inspect(id).await?;
    if container
        .as_ref()
        .is_none_or(|value| value["State"]["Running"] != true)
    {
        let mut activity = runtime.activity.lock().await;
        activity.players = None;
        activity.cached = None;
        activity.policy.observe(Instant::now(), None, 0);
        return Ok(());
    }
    let container = container.unwrap();
    let address = backend(&runtime.route, &container)?;
    let status = protocol::query(address).await?;
    let players = status["players"]["online"].as_u64();
    let mut activity = runtime.activity.lock().await;
    if activity.sleeping {
        runtime.store.save(false)?;
        activity.sleeping = false;
        activity.policy.started(Instant::now());
    }
    activity.cached = Some(status);
    activity.players = players;
    activity.error = None;
    if !activity.policy.observe(
        Instant::now(),
        players,
        runtime.joining.load(Ordering::SeqCst),
    ) {
        return Ok(());
    }
    if activity
        .retry_after
        .is_some_and(|time| Instant::now() < time)
    {
        return Ok(());
    }
    drop(activity);
    let settings = state
        .remote_servers()
        .await?
        .remove(id)
        .context("Server no longer assigned to this node")?;
    runtime.route.validate_settings(&settings)?;
    ensure!(
        !settings["suspended"].as_bool().unwrap_or(false),
        "Server suspended"
    );
    let fresh = protocol::query(address).await?;
    if fresh["players"]["online"].as_u64() != Some(0) || runtime.joining.load(Ordering::SeqCst) > 0
    {
        return Ok(());
    }
    runtime.store.save(true)?;
    runtime.activity.lock().await.sleeping = true;
    let stopped = async {
        state.engine.send_command(id, "stop").await?;
        let deadline = Instant::now() + Duration::from_secs(runtime.route.stop_timeout_seconds);
        loop {
            if state.engine.state(id).await? == "offline" {
                return Ok::<_, anyhow::Error>(());
            }
            ensure!(
                Instant::now() < deadline,
                "Minecraft did not finish saving; refusing to force kill"
            );
            sleep(Duration::from_secs(1)).await;
        }
    }
    .await;
    let mut activity = runtime.activity.lock().await;
    activity.cached = None;
    activity.players = None;
    if let Err(error) = stopped {
        runtime.store.save(false)?;
        activity.sleeping = false;
        activity.policy.joined(Instant::now());
        activity.retry_after = Some(Instant::now() + Duration::from_secs(60));
        return Err(error);
    }
    tracing::info!(server=%id,"Empty Minecraft server saved and hibernated");
    Ok(())
}
