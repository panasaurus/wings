use crate::{
    Server, State,
    engine::{Engine, strings},
    files, websocket,
};
use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, Request, State as AppState},
    http::{HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use subtle::ConstantTimeEq;
use tower_http::cors::CorsLayer;

pub struct ApiError(pub StatusCode, pub String);

impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> Self {
        tracing::error!(error = %error, "Node operation failed");
        Self(
            StatusCode::INTERNAL_SERVER_ERROR,
            "The node operation failed. Check the node logs.".into(),
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error":self.1}))).into_response()
    }
}

pub type ApiResult<T> = std::result::Result<T, ApiError>;

pub fn router(state: Arc<State>) -> Result<Router> {
    let config = state.engine.config.clone();
    let mut origins: Vec<HeaderValue> = config
        .allowed_origins
        .iter()
        .map(|value| value.parse())
        .collect::<std::result::Result<_, _>>()?;
    let remote: HeaderValue = config.remote.trim_end_matches('/').parse()?;
    if !origins.contains(&remote) {
        origins.push(remote);
    }
    let cors = CorsLayer::new()
        .allow_origin(origins)
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::DELETE,
            Method::PUT,
            Method::OPTIONS,
        ])
        .allow_headers([header::CONTENT_TYPE, header::AUTHORIZATION]);
    let protected = Router::new()
        .route("/api/system", get(system))
        .route("/api/update", post(update_config))
        .route("/api/servers", get(servers).post(create))
        .route("/api/servers/{id}", get(details).delete(delete))
        .route("/api/servers/{id}/logs", get(logs))
        .route("/api/servers/{id}/files/contents", get(files::contents))
        .route("/api/servers/{id}/files/list-directory", get(files::list))
        .route("/api/servers/{id}/files/write", post(files::write))
        .route("/api/servers/{id}/files/create", post(files::create))
        .route("/api/servers/{id}/files/update", post(files::update))
        .route("/api/servers/{id}/files/delete", post(files::delete))
        .route("/api/servers/{id}/power", post(power))
        .route("/api/servers/{id}/commands", post(commands))
        .route("/api/servers/{id}/sync", post(sync))
        .route("/api/servers/{id}/install", post(reinstall))
        .route("/api/servers/{id}/reinstall", post(reinstall))
        .route("/api/servers/{id}/ws/deny", post(revoke))
        .route("/api/deauthorize-user", post(deauthorize))
        .fallback(unsupported)
        .route_layer(middleware::from_fn_with_state(state.clone(), authorize));
    Ok(Router::new()
        .route("/api/servers/{id}/ws", get(websocket::upgrade))
        .merge(protected)
        .layer(DefaultBodyLimit::max(1024 * 1024))
        .layer(cors)
        .with_state(state))
}

async fn authorize(
    AppState(state): AppState<Arc<State>>,
    request: Request,
    next: Next,
) -> Response {
    let provided = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "));
    let config = state.config.read().await;
    if !provided.is_some_and(|token| bool::from(token.as_bytes().ct_eq(config.token.as_bytes()))) {
        return ApiError(
            StatusCode::UNAUTHORIZED,
            "A valid node token is required.".into(),
        )
        .into_response();
    }
    drop(config);
    next.run(request).await
}

async fn unsupported() -> ApiError {
    ApiError(
        StatusCode::NOT_IMPLEMENTED,
        "This operation is not implemented.".into(),
    )
}

#[derive(Deserialize)]
struct ConfigUpdate {
    #[serde(default)]
    token_id: Option<String>,
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    remote: Option<String>,
    #[serde(default)]
    jwt_audience: Option<String>,
    #[serde(default)]
    allowed_origins: Option<Vec<String>>,
}

async fn update_config(
    AppState(state): AppState<Arc<State>>,
    Json(body): Json<ConfigUpdate>,
) -> ApiResult<StatusCode> {
    let mut config = state.config.write().await;
    if let Some(token_id) = body.token_id {
        if !token_id.trim().is_empty() {
            config.token_id = token_id;
        }
    }
    if let Some(token) = body.token {
        if token.len() >= 32 {
            config.token = token;
        }
    }
    if let Some(remote) = body.remote {
        if reqwest::Url::parse(&remote).is_ok() {
            config.remote = remote;
        }
    }
    if let Some(jwt_audience) = body.jwt_audience {
        if !jwt_audience.trim().is_empty() {
            config.jwt_audience = jwt_audience;
        }
    }
    if let Some(allowed_origins) = body.allowed_origins {
        config.allowed_origins = allowed_origins;
    }
    Ok(StatusCode::NO_CONTENT)
}

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

async fn system(
    AppState(state): AppState<Arc<State>>,
    Query(query): Query<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let info = state
        .engine
        .json(&strings(&["info", "--format", "{{json .}}"]))
        .await?;
    let version = state
        .engine
        .json(&strings(&["version", "--format", "{{json .Server}}"]))
        .await?;
    let architecture = info["Architecture"]
        .as_str()
        .unwrap_or(std::env::consts::ARCH);
    let kernel = info["KernelVersion"].as_str().unwrap_or("unknown");
    let (disk_total, disk_free) = disk_space(&state.engine.config.system.data);
    if query.get("v").is_some_and(|v| v == "2") {
        Ok(Json(
            json!({"version":env!("CARGO_PKG_VERSION"),"implementation":"panasaurus-wings-rust",
            "docker":{"version":version["Version"],"cgroups":{"driver":info["CgroupDriver"],"version":info["CgroupVersion"]},
                "containers":{"total":info["Containers"],"running":info["ContainersRunning"],"paused":info["ContainersPaused"],"stopped":info["ContainersStopped"]},
                "storage":{"driver":info["Driver"],"filesystem":""},"runc":{"version":info["RuncCommit"]["ID"]}},
            "system":{"architecture":architecture,"cpu_threads":info["NCPU"],"memory_bytes":info["MemTotal"],"kernel_version":kernel,"os":info["OperatingSystem"],"os_type":info["OSType"],"disk_bytes":{"total":disk_total,"free":disk_free}}}),
        ))
    } else {
        Ok(Json(
            json!({"architecture":architecture,"cpu_count":info["NCPU"],"kernel_version":kernel,"os":info["OSType"],
            "version":env!("CARGO_PKG_VERSION"),"implementation":"panasaurus-wings-rust","disk_bytes":{"total":disk_total,"free":disk_free}}),
        ))
    }
}

async fn response(state: &State, id: &str) -> Result<Value> {
    let server = state.server(id).await?;
    let settings = server.settings.read().await.clone();
    let mut usage = state.engine.resources(id).await?;
    usage["hibernation"] = crate::hibernation::summary(state, id).await;
    if server.installing.load(Ordering::SeqCst) {
        usage["state"] = json!("offline");
    }
    Ok(
        json!({"state":usage["state"],"is_suspended":settings["suspended"],"utilization":usage,"configuration":settings}),
    )
}

async fn details(
    AppState(state): AppState<Arc<State>>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    if state.server(&id).await.is_err() {
        return Err(ApiError(StatusCode::NOT_FOUND, "Server not found.".into()));
    }
    Ok(Json(response(&state, &id).await?))
}

async fn servers(AppState(state): AppState<Arc<State>>) -> ApiResult<Json<Value>> {
    let ids: Vec<String> = state.servers.read().await.keys().cloned().collect();
    let mut result = Vec::new();
    for id in ids {
        result.push(response(&state, &id).await?);
    }
    Ok(Json(json!(result)))
}

#[derive(Deserialize)]
struct Creation {
    uuid: String,
    #[serde(default)]
    start_on_completion: bool,
}

async fn create(
    AppState(state): AppState<Arc<State>>,
    Json(body): Json<Creation>,
) -> ApiResult<StatusCode> {
    Engine::name(&body.uuid).map_err(|_| {
        ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Invalid server UUID.".into(),
        )
    })?;
    let settings = state
        .remote_servers()
        .await?
        .remove(&body.uuid)
        .ok_or_else(|| {
            ApiError(
                StatusCode::NOT_FOUND,
                "Server is not assigned to this node.".into(),
            )
        })?;
    let mut servers = state.servers.write().await;
    if servers.contains_key(&body.uuid) {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "Server already exists.".into(),
        ));
    }
    servers.insert(
        body.uuid.clone(),
        Arc::new(Server {
            settings: tokio::sync::RwLock::new(settings),
            operation: Arc::new(tokio::sync::Mutex::new(())),
            installing: AtomicBool::new(true),
            console_slots: Arc::new(tokio::sync::Semaphore::new(16)),
            console_users: tokio::sync::Mutex::new(HashMap::new()),
            console_snapshot: tokio::sync::Mutex::new(crate::ConsoleSnapshot::default()),
        }),
    );
    drop(servers);
    spawn_install(state, body.uuid, body.start_on_completion, false);
    Ok(StatusCode::ACCEPTED)
}

fn spawn_install(state: Arc<State>, id: String, start: bool, reinstall: bool) {
    tokio::spawn(async move {
        let Ok(server) = state.server(&id).await else {
            return;
        };
        let _operation = server.operation.lock().await;
        if let Err(error) = crate::hibernation::manual_power(&state, &id, "stop").await {
            tracing::error!(server = %id, error = %error, "Cannot disarm hibernation for installation");
            server.installing.store(false, Ordering::SeqCst);
            return;
        }
        server.installing.store(true, Ordering::SeqCst);
        let result = install(&state, &id).await;
        if let Err(error) = &result {
            tracing::error!(server = %id, error = %error, "Server installation failed");
        }
        if let Err(error) = state
            .installation_callback(&id, result.is_ok(), reinstall)
            .await
        {
            tracing::error!(error = %error, "Panel installation callback failed");
        }
        server.installing.store(false, Ordering::SeqCst);
        if result.is_ok()
            && start
            && let Err(error) = state
                .engine
                .power(&server.settings.read().await.clone(), "start")
                .await
        {
            tracing::error!(error = %error, "Automatic server start failed");
        }
    });
}

async fn install(state: &State, id: &str) -> Result<()> {
    let server = state.server(id).await?;
    let settings = server.settings.read().await.clone();
    state.engine.power(&settings, "stop").await?;
    crate::storage::provision(&state.engine.config, &settings).await?;
    let directory = state.engine.directory(id)?;
    tokio::fs::create_dir_all(&directory).await?;
    if !settings["skip_egg_scripts"].as_bool().unwrap_or(false) {
        let script = state.remote(&format!("servers/{id}/install")).await?;
        let image = script["container_image"]
            .as_str()
            .context("Missing installer image")?;
        let entrypoint = script["entrypoint"]
            .as_str()
            .context("Missing installer entrypoint")?;
        let contents = script["script"]
            .as_str()
            .context("Missing install script")?;
        ensure!(
            !image.starts_with('-') && !entrypoint.starts_with('-'),
            "Invalid installer image or entrypoint"
        );
        let temporary = state
            .engine
            .config
            .system
            .data
            .join(format!(".install-{id}"));
        tokio::fs::create_dir_all(&temporary).await?;
        let script_path = temporary.join("install.sh");
        tokio::fs::write(&script_path, normalize_install_script(contents)).await?;
        state.engine.run(&strings(&["pull", image])).await?;
        let name = format!("panasaurus-install-{id}");
        let mut args = strings(&[
            "run",
            "--rm",
            "--name",
            &name,
            "--label",
            "dev.panasaurus.managed=true",
            "--label",
            &format!("dev.panasaurus.node={}", state.engine.config.uuid),
            "--memory",
            "512m",
            "--cpus",
            "1",
            "--pids-limit",
            "512",
            "--security-opt",
            "no-new-privileges:true",
            "--workdir",
            "/mnt/server",
            "--mount",
            &format!("type=bind,src={},dst=/mnt/server", directory.display()),
            "--mount",
            &format!(
                "type=bind,src={},dst=/mnt/install.sh,readonly",
                script_path.display()
            ),
            "--entrypoint",
            entrypoint,
        ]);
        if let Some(env) = settings["environment"].as_object() {
            for (key, value) in env {
                ensure!(!key.contains(['=', '\0']) && !key.starts_with('-'), "Invalid environment key");
                args.extend(strings(&[
                    "--env",
                    &format!(
                        "{key}={}",
                        value
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| value.to_string())
                    ),
                ]));
            }
        }
        if let Some(port) = settings["allocations"]["default"]["port"].as_u64() {
            args.extend(strings(&["--env", &format!("SERVER_PORT={port}")]));
        }
        args.extend(strings(&[image, "/mnt/install.sh"]));
        let installed = state.engine.run(&args).await;
        let _ = tokio::fs::remove_dir_all(&temporary).await;
        if installed.is_err() {
            let _ = state.engine.run(&strings(&["rm", "--force", &name])).await;
        }
        installed?;
    }
    state.engine.create(&settings).await?;
    Ok(())
}

async fn reinstall(
    AppState(state): AppState<Arc<State>>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let server = state.server(&id).await?;
    if server.operation.try_lock().is_err() {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "A server operation is already running.".into(),
        ));
    }
    spawn_install(state, id, false, true);
    Ok(StatusCode::ACCEPTED)
}

#[derive(Deserialize)]
struct Power {
    action: String,
}

pub async fn power_action(state: &State, id: &str, action: &str) -> Result<()> {
    let server = state.server(id).await?;
    let _operation = server.operation.lock().await;
    ensure!(
        !server.installing.load(Ordering::SeqCst),
        "Server is installing"
    );
    ensure!(
        ["start", "restart", "stop", "kill"].contains(&action),
        "Invalid power action"
    );
    crate::hibernation::manual_power(state, id, action).await?;
    let mut action = action;
    if matches!(action, "start" | "restart") {
        let settings = state
            .remote_servers()
            .await?
            .remove(id)
            .context("Server is no longer assigned to this node")?;
        if reconcile_settings(state, &server, id, settings, true)
            .await
            .map_err(|error| anyhow::anyhow!(error.1))?
        {
            action = "start";
        }
    }
    state
        .engine
        .power(&server.settings.read().await.clone(), action)
        .await
}

async fn power(
    AppState(state): AppState<Arc<State>>,
    Path(id): Path<String>,
    Json(body): Json<Power>,
) -> ApiResult<StatusCode> {
    if !["start", "stop", "restart", "kill"].contains(&body.action.as_str()) {
        return Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Invalid power action.".into(),
        ));
    }
    power_action(&state, &id, &body.action).await?;
    Ok(StatusCode::ACCEPTED)
}

#[derive(Deserialize)]
struct Commands {
    commands: Vec<String>,
}

async fn commands(
    AppState(state): AppState<Arc<State>>,
    Path(id): Path<String>,
    Json(body): Json<Commands>,
) -> ApiResult<StatusCode> {
    state.server(&id).await?;
    if body.commands.len() > 20 {
        return Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Too many commands.".into(),
        ));
    }
    for command in body.commands {
        state.engine.send_command(&id, &command).await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn sync(
    AppState(state): AppState<Arc<State>>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let server = state.server(&id).await?;
    let _operation = server.operation.lock().await;
    let settings = state
        .remote_servers()
        .await?
        .remove(&id)
        .context("Server is no longer assigned to this node")?;
    reconcile_settings(&state, &server, &id, settings, false).await?;
    Ok(StatusCode::NO_CONTENT)
}

fn container_configuration(settings: &Value) -> Value {
    let mut result = settings.clone();
    if let Some(build) = result["build"].as_object_mut() {
        for key in ["memory_limit", "swap", "cpu_limit", "threads"] {
            build.remove(key);
        }
    }
    result
}

pub(crate) async fn reconcile_settings(
    state: &State,
    server: &Server,
    id: &str,
    settings: Value,
    allow_restart: bool,
) -> ApiResult<bool> {
    if let Some(route) = crate::hibernation::route_for(&state.engine.config, id) {
        route.validate_settings(&settings)?;
    }
    let previous = server.settings.read().await.clone();
    let reset_limits = state.engine.needs_limit_reset(&settings).await?;
    if previous == settings && !reset_limits {
        return Ok(false);
    }
    if settings["suspended"].as_bool().unwrap_or(false) {
        state.engine.power(&settings, "stop").await?;
    }
    let rebuild =
        reset_limits || container_configuration(&previous) != container_configuration(&settings);
    if rebuild {
        if state.engine.state(id).await? != "offline" {
            if !allow_restart {
                return Err(ApiError(
                    StatusCode::CONFLICT,
                    "Restart the server to apply its updated container configuration.".into(),
                ));
            }
            state.engine.power(&previous, "stop").await?;
        }
        state.engine.remove(id).await?;
    } else {
        state.engine.update_limits(&settings).await?;
    }
    *server.settings.write().await = settings;
    Ok(rebuild)
}

async fn delete(
    AppState(state): AppState<Arc<State>>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let server = state.server(&id).await?;
    let _operation = server.operation.lock().await;
    crate::hibernation::manual_power(&state, &id, "stop").await?;
    state.engine.remove(&id).await?;
    crate::hibernation::remove(&state, &id).await;
    state.servers.write().await.remove(&id);
    Ok(StatusCode::NO_CONTENT)
}

async fn logs(
    AppState(state): AppState<Arc<State>>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    state.server(&id).await?;
    let lines: Vec<String> = state
        .engine
        .logs(&id)
        .await?
        .into_iter()
        .map(|line| {
            line.split_once(' ')
                .map(|(_, text)| text)
                .unwrap_or(&line)
                .to_owned()
        })
        .collect();
    Ok(Json(json!({"data":lines})))
}

#[derive(Deserialize)]
struct Revocations {
    jtis: Vec<String>,
}

async fn revoke(
    AppState(state): AppState<Arc<State>>,
    Path(id): Path<String>,
    Json(body): Json<Revocations>,
) -> ApiResult<StatusCode> {
    state.server(&id).await?;
    for jti in body.jtis {
        state.revoked.write().await.insert(
            format!("jti:{id}:{jti}"),
            chrono::Utc::now().timestamp() as u64,
        );
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct UserRevocation {
    user: String,
    #[serde(default)]
    servers: Vec<String>,
}

async fn deauthorize(
    AppState(state): AppState<Arc<State>>,
    Json(body): Json<UserRevocation>,
) -> ApiResult<StatusCode> {
    let ids = if body.servers.is_empty() {
        state.servers.read().await.keys().cloned().collect()
    } else {
        body.servers
    };
    for id in ids {
        state.revoked.write().await.insert(
            format!("user:{id}:{}", body.user),
            chrono::Utc::now().timestamp() as u64,
        );
    }
    Ok(StatusCode::NO_CONTENT)
}

fn normalize_install_script(script: &str) -> String {
    script.replace("\r\n", "\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calculates_system_disk_space() {
        let root = tempfile::tempdir().unwrap();
        let (total, free) = disk_space(root.path());
        if cfg!(unix) {
            assert!(total > 0);
            assert!(free > 0);
        } else {
            assert_eq!((total, free), (0, 0));
        }
    }

    #[test]
    fn normalizes_windows_installers_without_altering_other_carriage_returns() {
        let script = "#!/bin/ash\r\nif true; then\r\n  printf 'ok\\n'\r\nfi\r\n";
        assert_eq!(
            normalize_install_script(script),
            "#!/bin/ash\nif true; then\n  printf 'ok\\n'\nfi\n"
        );
        assert_eq!(normalize_install_script("printf '\r'\n"), "printf '\r'\n");
    }
}
