use crate::{
    Server, State, backup,
    engine::{Engine, strings},
    file_api, files, transfers, websocket,
};
use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, Request, State as AppState},
    http::{HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete as delete_route, get, post, put},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, atomic::Ordering},
};
use subtle::ConstantTimeEq;

pub struct ApiError(pub StatusCode, pub String);

impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> Self {
        let error = match error.downcast::<reqwest::Error>() {
            Ok(e) => anyhow::Error::from(e.without_url()),
            Err(e) => e,
        };
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
        .route(
            "/api/servers/{id}/files/create-directory",
            post(file_api::mkdir),
        )
        .route("/api/servers/{id}/files/rename", put(file_api::rename))
        .route("/api/servers/{id}/files/copy", post(file_api::copy))
        .route("/api/servers/{id}/files/chmod", post(file_api::chmod))
        .route("/api/servers/{id}/files/compress", post(file_api::compress))
        .route(
            "/api/servers/{id}/files/decompress",
            post(file_api::decompress),
        )
        .route(
            "/api/servers/{id}/files/pull",
            post(file_api::pull).get(file_api::pulling),
        )
        .route(
            "/api/servers/{id}/files/pull/{key}",
            delete_route(file_api::cancel_pull),
        )
        .route("/api/servers/{id}/files/pulling", get(file_api::pulling))
        .route("/api/servers/{id}/backup", post(backup::create))
        .route(
            "/api/servers/{id}/backup/{backup}",
            delete_route(backup::delete),
        )
        .route(
            "/api/servers/{id}/backup/{backup}/restore",
            post(backup::restore),
        )
        .route(
            "/api/servers/{id}/transfer",
            post(transfers::send).delete(transfers::cancel),
        )
        .route("/api/servers/{id}/power", post(power))
        .route("/api/servers/{id}/commands", post(commands))
        .route("/api/servers/{id}/sync", post(sync))
        .route("/api/servers/{id}/install", post(reinstall))
        .route("/api/servers/{id}/reinstall", post(reinstall))
        .route("/api/servers/{id}/ws/deny", post(revoke))
        .route("/api/deauthorize-user", post(deauthorize))
        .route("/api/transfers/{id}", delete_route(transfers::cancel))
        .fallback(unsupported)
        .route_layer(middleware::from_fn_with_state(state.clone(), authorize));
    Ok(Router::new()
        .route("/api/servers/{id}/ws", get(websocket::upgrade))
        .route("/download/file", get(file_api::download))
        .route("/download/backup", get(backup::download))
        .route(
            "/upload/file",
            post(file_api::upload).layer(DefaultBodyLimit::max(
                (config.api.upload_limit * 1024 * 1024 + 1024 * 1024) as usize,
            )),
        )
        .route(
            "/api/transfers",
            post(transfers::receive).layer(DefaultBodyLimit::max(
                (config.limits.transfer_bytes + 1024 * 1024) as usize,
            )),
        )
        .merge(protected)
        .layer(DefaultBodyLimit::max(1024 * 1024))
        .layer(middleware::from_fn_with_state(state.clone(), dynamic_cors))
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

async fn update_config(
    AppState(state): AppState<Arc<State>>,
    Json(body): Json<Value>,
) -> ApiResult<Response> {
    let mut current = state.config.write().await;
    let mut value = serde_json::to_value(&*current)?;
    let object = body
        .as_object()
        .context("Configuration must be an object")?;

    for key in [
        "token_id",
        "token",
        "remote",
        "jwt_audience",
        "allowed_origins",
        "api",
        "downloads",
        "recovery",
        "hibernation",
        "limits",
    ] {
        if let Some(v) = object.get(key) {
            merge_config(&mut value[key], v);
        }
    }
    if let Some(sftp) = object.get("system").and_then(|v| v.get("sftp")) {
        merge_config(&mut value["system"]["sftp"], sftp);
    }
    if object
        .get("uuid")
        .is_some_and(|uuid| uuid == &json!(current.uuid))
    {
        value["api"]["host"] = json!(current.api.host);
        if current.api.ssl.enabled {
            value["api"]["ssl"]["cert"] = json!(current.api.ssl.cert);
            value["api"]["ssl"]["key"] = json!(current.api.ssl.key);
        }
    }
    let candidate: crate::config::Config = serde_json::from_value(value)?;
    candidate.validate().map_err(|_| {
        ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Node configuration is invalid.".into(),
        )
    })?;
    let restart = serde_json::to_value(&candidate.api)? != serde_json::to_value(&current.api)?
        || serde_json::to_value(&candidate.system.sftp)?
            != serde_json::to_value(&current.system.sftp)?
        || serde_json::to_value(&candidate.hibernation)?
            != serde_json::to_value(&current.hibernation)?
        || serde_json::to_value(&candidate.limits)? != serde_json::to_value(&current.limits)?;
    candidate.save(&state.config_path)?;
    *current = candidate;
    Ok(Response::builder()
        .status(StatusCode::NO_CONTENT)
        .header("x-panasaurus-restart-required", restart.to_string())
        .body(axum::body::Body::empty())
        .map_err(anyhow::Error::from)?)
}
fn merge_config(target: &mut Value, patch: &Value) {
    if let (Some(to), Some(from)) = (target.as_object_mut(), patch.as_object()) {
        for (k, v) in from {
            if let Some(child) = to.get_mut(k) {
                merge_config(child, v);
            }
        }
    } else {
        *target = patch.clone();
    }
}
async fn dynamic_cors(
    AppState(state): AppState<Arc<State>>,
    request: Request,
    next: Next,
) -> Response {
    let origin = request.headers().get(header::ORIGIN).cloned();
    let allowed = if let Some(origin) = &origin {
        let config = state.config.read().await;
        origin.to_str().is_ok_and(|o| {
            o == config.remote.trim_end_matches('/')
                || config.allowed_origins.iter().any(|v| v == o)
        })
    } else {
        false
    };
    if origin.is_some() && !allowed {
        return ApiError(StatusCode::FORBIDDEN, "Origin is not allowed.".into()).into_response();
    }
    let mut response = if request.method() == Method::OPTIONS {
        StatusCode::NO_CONTENT.into_response()
    } else {
        next.run(request).await
    };
    if let Some(origin) = origin.filter(|_| allowed) {
        let h = response.headers_mut();
        h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
        h.insert(header::VARY, HeaderValue::from_static("Origin"));
        h.insert(
            header::ACCESS_CONTROL_ALLOW_METHODS,
            HeaderValue::from_static("GET, POST, PUT, DELETE, OPTIONS"),
        );
        h.insert(
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            HeaderValue::from_static("Content-Type, Authorization"),
        );
    }
    response
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
    usage["state"] = json!(crate::lifecycle::status(state, id).await?);
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
    servers.insert(body.uuid.clone(), Arc::new(Server::new(settings)));
    drop(servers);
    spawn_install(state, body.uuid, body.start_on_completion, false).await?;
    Ok(StatusCode::ACCEPTED)
}

async fn spawn_install(
    state: Arc<State>,
    id: String,
    start: bool,
    reinstall: bool,
) -> ApiResult<()> {
    let server = state.server(&id).await?;
    let operation = server.operation.clone().try_lock_owned().map_err(|_| {
        ApiError(
            StatusCode::CONFLICT,
            "A server operation is already running.".into(),
        )
    })?;
    let permit = state.jobs.clone().try_acquire_owned().map_err(|_| {
        ApiError(
            StatusCode::TOO_MANY_REQUESTS,
            "Node job limit reached.".into(),
        )
    })?;
    if server.lifecycle.lock().await.transferring {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "Server is being transferred.".into(),
        ));
    }
    server.installing.store(true, Ordering::SeqCst);
    server.emit("install started", vec![]);
    tokio::spawn(async move {
        let _operation = operation;
        let _permit = permit;
        let result = async {
            crate::hibernation::manual_power(&state, &id, "stop").await?;
            install(&state, &id).await
        }
        .await;
        if let Err(error) = &result {
            tracing::error!(server=%id,error=%error,"Server installation failed");
        }
        if state
            .installation_callback(&id, result.is_ok(), reinstall)
            .await
            .is_err()
        {
            tracing::warn!(server=%id,"Panel installation callback failed");
        }
        server.installing.store(false, Ordering::SeqCst);
        server.emit("install completed", vec![]);
        if result.is_ok() && start {
            let settings = server.settings.read().await.clone();
            if let Err(error) = async {
                crate::lifecycle::begin(&state, &server, &id).await?;
                state.engine.power(&settings, "start").await
            }
            .await
            {
                server.lifecycle.lock().await.desired = false;
                tracing::error!(error=%error,"Automatic server start failed");
            }
        }
    });
    Ok(())
}

async fn install(state: &Arc<State>, id: &str) -> Result<()> {
    let server = state.server(id).await?;
    let settings = server.settings.read().await.clone();
    state.engine.cleanup_installer(id).await?;
    crate::lifecycle::stop(state, &server, id, false).await?;
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
        let temporary = tempfile::Builder::new()
            .prefix("install-")
            .tempdir_in(state.engine.config.private_root())?;
        let script_path = temporary.path().join("install.sh");
        tokio::fs::write(&script_path, normalize_install_script(contents)).await?;
        state.engine.pull(image).await?;
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
            &format!(
                "{}m",
                state
                    .engine
                    .config
                    .docker
                    .installer_limits
                    .memory
                    .max(settings["build"]["memory_limit"].as_u64().unwrap_or(0))
            ),
            "--cpus",
            &format!(
                "{:.2}",
                (state.engine.config.docker.installer_limits.cpu as f64)
                    .max(settings["build"]["cpu_limit"].as_f64().unwrap_or(0.0))
                    / 100.0
            ),
            "--pids-limit",
            &state.engine.config.docker.container_pid_limit.to_string(),
            "--cap-drop",
            "ALL",
            "--cap-add",
            "CHOWN",
            "--cap-add",
            "FOWNER",
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
                ensure!(
                    !key.contains(['=', '\0']) && !key.starts_with('-'),
                    "Invalid environment key"
                );
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
        server.emit(
            "install output",
            vec!["Starting installation script.".into()],
        );
        let mut child = state
            .engine
            .command()
            .args(&args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        let stdout = child.stdout.take().context("Installer output missing")?;
        let stderr = child
            .stderr
            .take()
            .context("Installer error output missing")?;
        let deadline = std::time::Duration::from_secs(
            state.engine.config.docker.installer_limits.timeout_seconds,
        );
        let installed = tokio::time::timeout(deadline, async {
            let out = crate::lifecycle::forward(
                state.clone(),
                id.to_owned(),
                server.clone(),
                stdout,
                "install output",
            );
            let err = crate::lifecycle::forward(
                state.clone(),
                id.to_owned(),
                server.clone(),
                stderr,
                "install output",
            );
            let (_, _, status) = tokio::join!(out, err, child.wait());
            ensure!(status?.success(), "Installation script failed");
            Ok::<(), anyhow::Error>(())
        })
        .await
        .context("Installer timeout")
        .and_then(|r| r);
        if installed.is_err() {
            let _ = child.kill().await;
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
    spawn_install(state, id, false, true).await?;
    Ok(StatusCode::ACCEPTED)
}

#[derive(Deserialize)]
struct Power {
    action: String,
}

pub async fn power_action(state: &State, id: &str, action: &str) -> Result<()> {
    let server = state.server(id).await?;
    let _operation = server
        .operation
        .clone()
        .try_lock_owned()
        .context("Another server operation is running")?;
    ensure!(
        !server.installing.load(Ordering::SeqCst),
        "Server is installing"
    );
    ensure!(
        !server.lifecycle.lock().await.transferring,
        "Server is being transferred"
    );
    crate::hibernation::manual_power(state, id, action).await?;
    match action {
        "stop" => crate::lifecycle::stop(state, &server, id, false).await,
        "kill" => crate::lifecycle::stop(state, &server, id, true).await,
        "start" | "restart" => {
            if action == "restart" {
                crate::lifecycle::stop(state, &server, id, false).await?;
            }
            let response = state.remote(&format!("servers/{id}")).await?;
            let mut settings = response["settings"].clone();
            ensure!(settings["uuid"] == id, "Panel UUID mismatch");
            settings["_process_configuration"] = response["process_configuration"].clone();
            reconcile_settings(state, &server, id, settings.clone(), true)
                .await
                .map_err(|e| anyhow::anyhow!(e.1))?;
            crate::lifecycle::begin(state, &server, id).await?;
            state.engine.power(&settings, "start").await
        }
        _ => anyhow::bail!("Invalid power action"),
    }
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
    if let Some(object) = result.as_object_mut() {
        object.remove("_process_configuration");
    }
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
            crate::lifecycle::stop(state, server, id, false).await?;
        }
        state.engine.remove(id).await?;
        crate::storage::resize(&state.engine.config, &settings).await?;
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
    crate::storage::destroy(&state.engine.config, &id).await?;
    for job in state
        .downloads
        .lock()
        .await
        .values()
        .filter(|j| j.server == id)
    {
        job.cancel.cancel();
    }
    server.emit("daemon message", vec!["Server deleted.".into()]);
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

macro_rules! api_error_conversion {($($t:ty),*)=>{$(impl From<$t> for ApiError{fn from(e:$t)->Self{anyhow::Error::from(e).into()}})*}}
api_error_conversion!(
    std::io::Error,
    reqwest::Error,
    uuid::Error,
    serde_json::Error,
    std::num::ParseIntError,
    tokio::task::JoinError
);
