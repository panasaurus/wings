use crate::{
    State,
    api::{ApiError, ApiResult, power_action},
};
use anyhow::{Result, ensure};
use axum::{
    extract::{
        Path, State as AppState,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, StatusCode},
    response::Response,
};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode};
use serde::Deserialize;
use serde_json::json;
use std::{
    collections::HashSet,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

#[derive(Clone, Deserialize)]
pub struct Claims {
    pub exp: u64,
    pub iat: u64,
    pub server_uuid: String,
    pub user_uuid: String,
    pub jti: String,
    pub permissions: Vec<String>,
    pub scope: String,
}

#[derive(Deserialize)]
struct Packet {
    event: String,
    #[serde(default)]
    args: Vec<String>,
}

impl Claims {
    fn permission(&self, permission: &str) -> bool {
        self.permissions.iter().any(|p| p == permission || p == "*")
    }
}

pub async fn upgrade(
    AppState(state): AppState<Arc<State>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> ApiResult<Response> {
    let server = state
        .server(&id)
        .await
        .map_err(|_| ApiError(StatusCode::NOT_FOUND, "Server not found.".into()))?;
    if let Some(origin) = headers.get("origin") {
        let config = state.config.read().await;
        let allowed = origin.to_str().ok().is_some_and(|o| {
            o == config.remote.trim_end_matches('/')
                || config.allowed_origins.iter().any(|allowed| allowed == o)
        });
        if !allowed {
            return Err(ApiError(
                StatusCode::FORBIDDEN,
                "WebSocket origin is not allowed.".into(),
            ));
        }
    }
    let node_permit = state
        .console_slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| {
            ApiError(
                StatusCode::TOO_MANY_REQUESTS,
                "Too many console connections.".into(),
            )
        })?;
    let server_permit = server
        .console_slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| {
            ApiError(
                StatusCode::TOO_MANY_REQUESTS,
                "Too many server console connections.".into(),
            )
        })?;
    Ok(ws
        .max_message_size(64 * 1024)
        .max_frame_size(64 * 1024)
        .on_upgrade(move |socket| handle(socket, state, id, node_permit, server_permit)))
}

fn verify(token: &str, id: &str, config: &crate::config::Config) -> Result<Claims> {
    let mut validation = Validation::new(Algorithm::HS256);
    validation.leeway = 0;
    validation.validate_nbf = true;
    validation.set_issuer(&[config.remote.trim_end_matches('/')]);
    validation.set_audience(&[&config.jwt_audience]);
    let claims = decode::<Claims>(
        token,
        &DecodingKey::from_secret(config.token.as_bytes()),
        &validation,
    )?
    .claims;
    ensure!(
        claims.server_uuid == id,
        "JWT belongs to a different server"
    );
    ensure!(
        !claims.user_uuid.is_empty() && !claims.jti.is_empty(),
        "JWT user claims are missing"
    );
    ensure!(
        claims
            .scope
            .split_whitespace()
            .any(|scope| scope == "websocket"),
        "JWT does not have websocket scope"
    );
    ensure!(
        claims.permission("websocket.connect"),
        "JWT cannot connect to the console"
    );
    Ok(claims)
}

async fn valid(state: &State, claims: &Claims) -> bool {
    let now = chrono::Utc::now().timestamp() as u64;
    if claims.exp <= now || claims.iat < state.booted_at || claims.iat > now + 15 {
        return false;
    }
    let revoked = state.revoked.read().await;
    ![
        format!("jti:{}:{}", claims.server_uuid, claims.jti),
        format!("user:{}:{}", claims.server_uuid, claims.user_uuid),
    ]
    .iter()
    .any(|key| revoked.get(key).is_some_and(|time| claims.iat <= *time))
}

async fn send(socket: &mut WebSocket, event: &str, args: Vec<String>) -> Result<()> {
    tokio::time::timeout(
        Duration::from_secs(5),
        socket.send(Message::Text(
            json!({"event":event,"args":args}).to_string().into(),
        )),
    )
    .await??;
    Ok(())
}

async fn send_status(
    socket: &mut WebSocket,
    previous: &mut Option<String>,
    status: String,
) -> Result<()> {
    if previous.as_ref() != Some(&status) {
        send(socket, "status", vec![status.clone()]).await?;
        *previous = Some(status);
    }
    Ok(())
}

struct MessageLimit {
    window: Instant,
    count: usize,
    capacity: usize,
}

impl MessageLimit {
    fn new(capacity: usize) -> Self {
        Self {
            window: Instant::now(),
            count: 0,
            capacity,
        }
    }

    fn accept(&mut self) -> bool {
        if self.window.elapsed() >= Duration::from_secs(1) {
            self.window = Instant::now();
            self.count = 0;
        }
        self.count += 1;
        self.count <= self.capacity
    }
}

async fn user_permit(state: &State, id: &str, user: &str) -> Result<OwnedSemaphorePermit> {
    let server = state.server(id).await?;
    let mut users = server.console_users.lock().await;
    users.retain(|_, slots| slots.strong_count() > 0);
    let slots = users
        .get(user)
        .and_then(std::sync::Weak::upgrade)
        .unwrap_or_else(|| {
            let slots = Arc::new(Semaphore::new(2));
            users.insert(user.to_owned(), Arc::downgrade(&slots));
            slots
        });
    Ok(slots.try_acquire_owned()?)
}

async fn handle(
    mut socket: WebSocket,
    state: Arc<State>,
    id: String,
    _node_permit: OwnedSemaphorePermit,
    _server_permit: OwnedSemaphorePermit,
) {
    let mut claims: Option<Claims> = None;
    let mut user_slot = None;
    let mut messages = MessageLimit::new(20);
    let mut authentications = MessageLimit::new(1);
    let mut last_status = None;
    let mut seen = HashSet::new();
    let mut timer = tokio::time::interval(Duration::from_secs(3));
    let deadline = tokio::time::sleep(Duration::from_secs(15));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline, if claims.is_none() => { let _ = socket.send(Message::Close(None)).await; return; }
            message = socket.recv() => {
                let Some(Ok(message)) = message else { return; };
                if !messages.accept() { let _ = send(&mut socket, "daemon error", vec!["Console message rate limit exceeded.".into()]).await; return; }
                match message {
                    Message::Close(_) => return,
                    Message::Ping(bytes) => { if socket.send(Message::Pong(bytes)).await.is_err() { return; } continue; }
                    Message::Text(text) => {
                        let Ok(packet) = serde_json::from_str::<Packet>(&text) else { continue; };
                        if packet.event == "auth" {
                            if !authentications.accept() { let _ = send(&mut socket, "daemon error", vec!["Console authentication rate limit exceeded.".into()]).await; return; }
                            let token = packet.args.first().map(String::as_str).unwrap_or("");
                            let config = state.config.read().await;
                            let verified = verify(token, &id, &config);
                            drop(config);
                            match verified {
                                Ok(value) if valid(&state, &value).await => {
                                    if claims.as_ref().is_some_and(|old| old.user_uuid != value.user_uuid) {
                                        let _ = send(&mut socket, "jwt error", vec!["Console identity cannot change.".into()]).await; return;
                                    }
                                    if user_slot.is_none() {
                                        match user_permit(&state, &id, &value.user_uuid).await {
                                            Ok(permit) => user_slot = Some(permit),
                                            Err(_) => { let _ = send(&mut socket, "daemon error", vec!["Too many console connections for this account.".into()]).await; return; }
                                        }
                                    }
                                    claims = Some(value);
                                    if send(&mut socket, "auth success", Vec::new()).await.is_err() { return; }
                                    if let Ok(status) = state.engine.state(&id).await && send_status(&mut socket, &mut last_status, status).await.is_err() { return; }
                                }
                                Ok(_) => { let _ = send(&mut socket, "jwt error", vec!["jwt: created too far in past (denylist)".into()]).await; return; }
                                Err(error) => {
                                    let message = if error.downcast_ref::<jsonwebtoken::errors::Error>().is_some_and(|error| matches!(error.kind(), jsonwebtoken::errors::ErrorKind::ExpiredSignature)) { "jwt: exp claim is invalid" } else { "JWT authentication failed." };
                                    let _ = send(&mut socket, "jwt error", vec![message.into()]).await;
                                    return;
                                }
                            }
                            continue;
                        }
                        let Some(authorization) = &claims else { let _ = send(&mut socket, "jwt error", vec!["Authenticate before sending commands.".into()]).await; return; };
                        if !valid(&state, authorization).await { let _ = send(&mut socket, "jwt error", vec!["jwt: exp claim is invalid".into()]).await; return; }
                        let argument = packet.args.first().map(String::as_str).unwrap_or("");
                        let result = match packet.event.as_str() {
                            "send logs" => { seen.clear(); Ok(()) },
                            "send stats" => Ok(()),
                            "send command" if authorization.permission("control.console") => state.engine.send_command(&id, argument).await,
                            "set state" => {
                                let permission = match argument { "start" => "control.start", "restart" => "control.restart", "stop" | "kill" => "control.stop", _ => "invalid" };
                                if authorization.permission(permission) { power_action(&state, &id, argument).await } else { Err(anyhow::anyhow!("Power permission denied")) }
                            }
                            _ => Err(anyhow::anyhow!("Unknown or forbidden console event")),
                        };
                        if let Err(error) = result {
                            tracing::warn!(server = %id, event = %packet.event, error = %error, "Console operation failed");
                            if send(&mut socket, "daemon error", vec![console_error_message(&error).into()]).await.is_err() { return; }
                        }
                    }
                    _ => continue,
                }
            }
            _ = timer.tick(), if claims.is_some() => {
                let authorization = claims.as_ref().unwrap();
                if !valid(&state, authorization).await { let _ = send(&mut socket, "jwt error", vec!["jwt: exp claim is invalid".into()]).await; return; }
                if authorization.exp < chrono::Utc::now().timestamp() as u64 + 60
                    && send(&mut socket, "token expiring", Vec::new()).await.is_err() { return; }
                let Ok((usage, lines)) = state.console_snapshot(&id).await else { return; };
                if let Some(usage) = usage {
                    let status = usage["state"].as_str().unwrap_or("offline").to_owned();
                    if send_status(&mut socket, &mut last_status, status).await.is_err() { return; }
                    if send(&mut socket, "stats", vec![usage.to_string()]).await.is_err() { return; }
                }
                {
                    for line in &lines {
                        if !seen.contains(line) {
                            let text = line.split_once(' ').map(|(_,text)| text).unwrap_or(line).to_owned();
                            if send(&mut socket, "console output", vec![text]).await.is_err() { return; }
                        }
                    }
                    seen = lines.into_iter().collect();
                }
            }
        }
    }
}

fn console_error_message(error: &anyhow::Error) -> &'static str {
    let detail = error.to_string().to_lowercase();
    if detail.contains("registry") && (detail.contains("denied") || detail.contains("unauthorized"))
    {
        "Docker could not download the server image. Check its image name and registry access."
    } else if detail.contains("power permission denied") || detail.contains("unknown or forbidden")
    {
        "Your account does not have permission to perform this console action."
    } else if detail.contains("server is installing") {
        "Wait for the server installation to finish before changing its power state."
    } else if detail.contains("offline server") {
        "Start the server before sending console commands."
    } else if detail.contains("suspended server") {
        "This server is suspended and cannot be started."
    } else {
        "The server operation failed. Check the node logs for the cause."
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{EncodingKey, Header, encode};

    #[test]
    fn console_message_limits_reset_and_reject_bursts() {
        let mut limit = MessageLimit::new(2);
        assert!(limit.accept());
        assert!(limit.accept());
        assert!(!limit.accept());
        limit.window -= Duration::from_secs(1);
        assert!(limit.accept());
        let mut auth = MessageLimit::new(1);
        assert!(auth.accept());
        assert!(!auth.accept());
    }

    #[test]
    fn registry_failure_is_not_reported_as_account_permissions() {
        let registry = anyhow::anyhow!("Docker error: denied access to registry");
        let permission = anyhow::anyhow!("Power permission denied");
        assert!(console_error_message(&registry).contains("server image"));
        assert!(console_error_message(&permission).contains("Your account"));
        assert!(
            console_error_message(&anyhow::anyhow!("Server is installing"))
                .contains("installation")
        );
    }

    #[test]
    fn enforces_jwt_signature_scope_issuer_and_server() {
        let config: crate::config::Config =
            serde_yaml::from_str(include_str!("../config.example.yml")).unwrap();
        let id = "11111111-1111-4111-a111-111111111111";
        let payload = json!({"exp":chrono::Utc::now().timestamp()+60,"iat":chrono::Utc::now().timestamp(),
            "iss":config.remote,"aud":config.jwt_audience,"server_uuid":id,"user_uuid":"user","jti":"test",
            "permissions":["websocket.connect"],"scope":"websocket"});
        let token = encode(
            &Header::default(),
            &payload,
            &EncodingKey::from_secret(config.token.as_bytes()),
        )
        .unwrap();
        assert!(verify(&token, id, &config).is_ok());
        assert!(verify(&token, "another-server", &config).is_err());
        let bad_token = encode(
            &Header::default(),
            &payload,
            &EncodingKey::from_secret(b"incorrect-key"),
        )
        .unwrap();
        assert!(verify(&bad_token, id, &config).is_err());
        for (field, value) in [
            ("scope", json!("file-upload")),
            ("iss", json!("http://another-panel")),
            ("aud", json!("http://another-node")),
            ("permissions", json!(["control.console"])),
            ("exp", json!(0)),
        ] {
            let mut rejected = payload.clone();
            rejected[field] = value;
            let token = encode(
                &Header::default(),
                &rejected,
                &EncodingKey::from_secret(config.token.as_bytes()),
            )
            .unwrap();
            assert!(
                verify(&token, id, &config).is_err(),
                "Accepted invalid {field}"
            );
        }
    }
}
