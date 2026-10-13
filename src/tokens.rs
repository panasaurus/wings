use crate::State;
use anyhow::{Context, Result, ensure};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode};
use serde::Deserialize;
#[derive(Clone, Deserialize)]
pub struct Claims {
    pub exp: u64,
    pub iat: u64,
    pub scope: String,
    #[serde(default)]
    pub permissions: Vec<String>,
    #[serde(default)]
    pub server_uuid: String,
    #[serde(default)]
    pub user_uuid: String,
    #[serde(default)]
    pub unique_id: String,
    #[serde(default)]
    pub file_path: String,
    #[serde(default)]
    pub backup_uuid: String,
    #[serde(default)]
    pub sub: String,
}
pub async fn verify(state: &State, token: &str, scope: &str, consume: bool) -> Result<Claims> {
    ensure!(token.len() <= 16 * 1024, "Token too large");
    let config = state.config.read().await;
    let mut validation = Validation::new(Algorithm::HS256);
    validation.leeway = 0;
    validation.validate_nbf = true;
    validation.set_issuer(&[config.remote.trim_end_matches('/')]);
    validation.set_audience(&[config.jwt_audience.trim_end_matches('/')]);
    let claims = decode::<Claims>(
        token,
        &DecodingKey::from_secret(config.token.as_bytes()),
        &validation,
    )?
    .claims;
    drop(config);
    let now = chrono::Utc::now().timestamp() as u64;
    ensure!(
        claims.iat >= state.booted_at
            && claims.iat <= now + 15
            && claims.exp > now
            && claims.exp <= now + 3600,
        "Token lifetime invalid"
    );
    ensure!(
        claims.scope.split_whitespace().any(|s| s == scope),
        "Token scope rejected"
    );
    let id = if scope == "transfer" {
        &claims.sub
    } else {
        &claims.server_uuid
    };
    uuid::Uuid::parse_str(id)?;
    if scope != "transfer" {
        let server = state.server(id).await?;
        ensure!(
            !server.settings.read().await["suspended"]
                .as_bool()
                .unwrap_or(false),
            "Server suspended"
        );
        uuid::Uuid::parse_str(&claims.user_uuid)?;
        ensure!(!claims.user_uuid.is_empty(), "Missing token identity");
        let revoked = state.revoked.read().await;
        ensure!(
            !revoked
                .get(&format!("user:{id}:{}", claims.user_uuid))
                .is_some_and(|t| claims.iat <= *t),
            "Access revoked"
        );
    }
    if consume {
        ensure!(
            !claims.unique_id.is_empty() && claims.unique_id.len() <= 128,
            "Missing single-use token ID"
        );
        let mut used = state.used_tokens.lock().await;
        used.retain(|_, expiry| *expiry > now);
        ensure!(used.len() < 100_000, "Token capacity exceeded");
        ensure!(
            !used.contains_key(&claims.unique_id),
            "Token was already used"
        );
        used.insert(claims.unique_id.clone(), claims.exp);
    }
    Ok(claims)
}
pub fn query_token(value: Option<&String>) -> Result<&str> {
    value
        .map(String::as_str)
        .context("A scoped token is required")
}
