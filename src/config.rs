use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Clone, Deserialize, Serialize)]
pub struct Config {
    pub uuid: String,
    pub token_id: String,
    pub token: String,
    pub remote: String,
    pub jwt_audience: String,
    pub api: Api,
    pub system: System,
    #[serde(default)]
    pub docker: Docker,
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    #[serde(default)]
    pub hibernation: Vec<crate::hibernation::Route>,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Api {
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub ssl: Ssl,
}

#[derive(Clone, Default, Deserialize, Serialize)]
pub struct Ssl {
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct System {
    pub data: PathBuf,
    #[serde(default = "default_disk_limit")]
    pub default_disk_limit_mb: u64,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Docker {
    pub executable: String,
    pub context: Option<String>,
    pub config_directory: Option<PathBuf>,
    pub user: String,
    pub command_timeout_seconds: u64,
}

impl Default for Docker {
    fn default() -> Self {
        Self {
            executable: "docker".into(),
            context: None,
            config_directory: None,
            user: "1000:1000".into(),
            command_timeout_seconds: 300,
        }
    }
}

fn default_disk_limit() -> u64 {
    20 * 1024
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let config: Self =
            serde_yaml::from_slice(&std::fs::read(path).context("Read node configuration")?)?;
        uuid::Uuid::parse_str(&config.uuid).context("Invalid node UUID")?;
        ensure!(
            !config.token_id.is_empty() && config.token.len() >= 32,
            "Set real node credentials before starting"
        );
        ensure!(
            !config.api.ssl.enabled,
            "Direct TLS is not implemented. Use a local listener behind a TLS reverse proxy."
        );
        ensure!(
            config
                .api
                .host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback()),
            "The plaintext API must bind to a loopback IP. Use a TLS reverse proxy for remote access."
        );
        ensure!(
            config.system.default_disk_limit_mb > 0,
            "Set a finite default disk limit for servers with an unlimited panel allocation"
        );
        ensure!(
            config.system.data.is_absolute(),
            "system.data must be an absolute path shared with Docker"
        );
        reqwest::Url::parse(&config.remote).context("Invalid panel URL")?;
        ensure!(
            config.docker.command_timeout_seconds > 0,
            "Docker command timeout must be positive"
        );
        crate::hibernation::validate_config(&config)?;
        Ok(config)
    }

    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.docker.command_timeout_seconds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn plaintext_listener_must_be_loopback_and_default_quota_finite() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.yml");
        let mut config = serde_json::json!({
            "uuid":uuid::Uuid::new_v4().to_string(), "token_id":"fixture", "token":"a".repeat(64),
            "remote":"http://127.0.0.1:8080", "jwt_audience":"http://127.0.0.1:8081",
            "api":{"host":"127.0.0.1","port":8081}, "system":{"data":root.path()}
        });
        std::fs::write(&path, config.to_string()).unwrap();
        assert!(Config::load(&path).is_ok());
        for host in ["0.0.0.0", "::", "192.168.1.1", "localhost"] {
            config["api"]["host"] = serde_json::json!(host);
            std::fs::write(&path, config.to_string()).unwrap();
            assert!(Config::load(&path).is_err(), "Accepted {host}");
        }
        config["api"]["host"] = serde_json::json!("::1");
        std::fs::write(&path, config.to_string()).unwrap();
        assert!(Config::load(&path).is_ok());
        config["system"]["default_disk_limit_mb"] = serde_json::json!(0);
        std::fs::write(&path, config.to_string()).unwrap();
        assert!(Config::load(&path).is_err());
    }
}
