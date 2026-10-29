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
    #[serde(default)]
    pub allowed_mounts: Vec<PathBuf>,
    #[serde(default)]
    pub downloads: Downloads,
    #[serde(default)]
    pub limits: Limits,
    #[serde(default)]
    pub recovery: Recovery,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Api {
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub ssl: Ssl,
    #[serde(default = "upload_limit")]
    pub upload_limit: u64,
}

#[derive(Clone, Default, Deserialize, Serialize)]
pub struct Ssl {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub cert: PathBuf,
    #[serde(default)]
    pub key: PathBuf,
    #[serde(default)]
    pub acme_domain: Option<String>,
    #[serde(default)]
    pub acme_email: Option<String>,
    #[serde(default)]
    pub acme_production: bool,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct System {
    pub data: PathBuf,
    #[serde(default = "default_disk_limit")]
    pub default_disk_limit_mb: u64,
    #[serde(default)]
    pub sftp: Sftp,
    /// File revision history settings. When enabled, wings snapshots files
    /// as they're written so users can browse and restore prior revisions
    /// from the panel. New in v0.2.1.
    #[serde(default)]
    pub file_history: FileHistory,
}

/// Configuration for the file revision history subsystem. Disabled by
/// default; enabling it costs disk proportional to write activity.
#[derive(Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct FileHistory {
    /// Whether to track file revisions at all.
    pub enabled: bool,
    /// zstd compression level (1–22). Higher = better compression, more CPU.
    pub zstd_level: u32,
    /// Files larger than this many bytes are not snapshotted.
    pub file_size_cap: u64,
    /// How many revisions to keep per file before pruning the oldest.
    pub keep_chains: usize,
    /// Soft cap on total disk used by file_history for a single server.
    pub per_server_disk_budget: u64,
}

impl Default for FileHistory {
    fn default() -> Self {
        Self {
            enabled: false,
            zstd_level: 19,
            file_size_cap: 1024 * 1024,        // 1 MiB
            keep_chains: 10,
            per_server_disk_budget: 256 * 1024 * 1024, // 256 MiB
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Docker {
    pub executable: String,
    pub context: Option<String>,
    pub config_directory: Option<PathBuf>,
    pub user: String,
    pub command_timeout_seconds: u64,
    #[serde(default)]
    pub network: Network,
    #[serde(default)]
    pub registries: std::collections::HashMap<String, Registry>,
    #[serde(default = "tmpfs_size")]
    pub tmpfs_size: u64,
    #[serde(default = "pid_limit")]
    pub container_pid_limit: u64,
    #[serde(default)]
    pub installer_limits: InstallerLimits,
    #[serde(default)]
    pub memory_overhead_percent: u64,
    #[serde(default)]
    pub cpu_shares: u64,
    #[serde(default = "cpu_period")]
    pub cpu_period: u64,
    #[serde(default)]
    pub cpu_burst_percent: u64,
    #[serde(default)]
    pub userns_mode: String,
    #[serde(default)]
    pub timezone: Option<String>,
}

impl Default for Docker {
    fn default() -> Self {
        Self {
            executable: "docker".into(),
            context: None,
            config_directory: None,
            user: "1000:1000".into(),
            command_timeout_seconds: 300,
            network: Network::default(),
            registries: Default::default(),
            tmpfs_size: tmpfs_size(),
            container_pid_limit: pid_limit(),
            installer_limits: Default::default(),
            memory_overhead_percent: 0,
            cpu_shares: 0,
            cpu_period: cpu_period(),
            cpu_burst_percent: 0,
            userns_mode: String::new(),
            timezone: None,
        }
    }
}

fn default_disk_limit() -> u64 {
    20 * 1024
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let config: Self = serde_yaml::from_str(
            &std::fs::read_to_string(path).context("Read node configuration")?,
        )
        .map_err(|_| anyhow::anyhow!("Node configuration could not be parsed"))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        let config = self;
        uuid::Uuid::parse_str(&config.uuid).context("Invalid node UUID")?;
        ensure!(
            !config.token_id.is_empty() && config.token.len() >= 32,
            "Set real node credentials before starting"
        );
        ensure!(
            !config.api.ssl.enabled
                || config.api.ssl.acme_domain.is_some()
                || (config.api.ssl.cert.is_absolute() && config.api.ssl.key.is_absolute()),
            "TLS requires certificate paths or an ACME domain"
        );
        ensure!(
            config
                .api
                .host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| config.api.ssl.enabled || ip.is_loopback()),
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
        let panel = reqwest::Url::parse(&config.remote).context("Invalid panel URL")?;
        ensure!(
            panel.scheme() == "https"
                || panel.scheme() == "http"
                    && panel.host_str().is_some_and(|h| h == "localhost"
                        || h.trim_matches(['[', ']'])
                            .parse::<std::net::IpAddr>()
                            .is_ok_and(|ip| ip.is_loopback())),
            "Panel credentials require HTTPS or loopback"
        );
        ensure!(
            !config.docker.user.trim().is_empty()
                && !config.docker.user.starts_with('-')
                && config
                    .docker
                    .user
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == ':' || c == '_' || c == '-'),
            "docker.user must be a valid user/group specification (e.g., '1000:1000')"
        );
        ensure!(
            config.docker.command_timeout_seconds > 0,
            "Docker command timeout must be positive"
        );
        ensure!(
            (1..=10240).contains(&config.api.upload_limit),
            "Upload limit must be 1–10240 MiB"
        );
        ensure!(
            (1..=64).contains(&config.limits.jobs)
                && (1..=4096).contains(&config.limits.sftp_connections),
            "Invalid concurrency limits"
        );
        ensure!(
            (1..=7200).contains(&config.docker.installer_limits.timeout_seconds),
            "Invalid installer timeout"
        );
        ensure!(
            config.docker.container_pid_limit > 0
                && config.docker.memory_overhead_percent <= 100
                && config.docker.cpu_burst_percent == 0
                && (1000..=1_000_000).contains(&config.docker.cpu_period),
            "Invalid container resource settings"
        );
        ensure!(
            config
                .downloads
                .allowed_hosts
                .iter()
                .all(|h| !h.is_empty() && !h.contains(['/', '*', '@'])),
            "Download hosts must be exact host names"
        );
        ensure!(
            config.allowed_mounts.iter().all(|p| p.is_absolute()),
            "Allowed mounts must be absolute"
        );
        config
            .system
            .sftp
            .bind_address
            .parse::<std::net::IpAddr>()
            .context("Invalid SFTP bind address")?;
        ensure!(
            (1..=512).contains(&config.system.sftp.max_handles),
            "Invalid SFTP handle limit"
        );
        ensure!(
            (1..=3600).contains(&config.recovery.stop_timeout_seconds),
            "Invalid graceful stop timeout"
        );
        ensure!(
            (1..=1024 * 1024 * 1024 * 1024).contains(&config.limits.transfer_bytes),
            "Invalid transfer limit"
        );
        ensure!(
            config.docker.installer_limits.memory > 0
                && config.docker.installer_limits.cpu > 0
                && config.docker.tmpfs_size > 0,
            "Container limits must be finite"
        );
        for value in config
            .allowed_origins
            .iter()
            .chain([&config.remote, &config.jwt_audience])
        {
            let url = reqwest::Url::parse(value)?;
            ensure!(
                matches!(url.scheme(), "http" | "https")
                    && url.host().is_some()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none(),
                "Invalid origin or panel URL"
            );
        }
        if let Some(domain) = &config.api.ssl.acme_domain {
            ensure!(
                !domain.is_empty()
                    && domain.len() < 254
                    && domain
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-'),
                "Invalid ACME domain"
            );
        }
        crate::hibernation::validate_config(config)?;
        Ok(())
    }

    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.docker.command_timeout_seconds)
    }
}

fn upload_limit() -> u64 {
    100
}
fn tmpfs_size() -> u64 {
    64
}
fn pid_limit() -> u64 {
    512
}
fn cpu_period() -> u64 {
    100_000
}
#[derive(Clone, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct Downloads {
    pub allowed_hosts: Vec<String>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Limits {
    pub jobs: usize,
    pub sftp_connections: usize,
    pub transfer_bytes: u64,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            jobs: 8,
            sftp_connections: 64,
            transfer_bytes: 64 * 1024 * 1024 * 1024,
        }
    }
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Recovery {
    pub enabled: bool,
    pub restart_crashed: bool,
    pub detect_clean_exit: bool,
    pub crash_cooldown_seconds: u64,
    pub stop_timeout_seconds: u64,
}
impl Default for Recovery {
    fn default() -> Self {
        Self {
            enabled: true,
            restart_crashed: true,
            detect_clean_exit: false,
            crash_cooldown_seconds: 60,
            stop_timeout_seconds: 120,
        }
    }
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Sftp {
    pub enabled: bool,
    pub bind_address: String,
    pub bind_port: u16,
    pub max_handles: usize,
    /// Whether to enable the `.wings` shell CLI over SSH. When enabled, users
    /// can `ssh` in and run `.wings help`, `.wings power start`, etc. New in
    /// v0.2.1.
    pub shell: Shell,
}

/// Configuration for the SSH shell (the `.wings` CLI). New in v0.2.1.
#[derive(Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Shell {
    /// Whether to enable the wings remote shell at all.
    pub enabled: bool,
    /// The command prefix users type to invoke the CLI. Defaults to `.wings`.
    pub cli_name: String,
}

impl Default for Shell {
    fn default() -> Self {
        Self {
            enabled: true,
            cli_name: ".wings".into(),
        }
    }
}

impl Default for Sftp {
    fn default() -> Self {
        Self {
            enabled: true,
            bind_address: "127.0.0.1".into(),
            bind_port: 2022,
            max_handles: 64,
            shell: Shell::default(),
        }
    }
}
#[derive(Clone, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct Network {
    pub name: Option<String>,
    pub dns: Vec<String>,
    pub mtu: Option<u64>,
    pub subnet: Option<String>,
    pub gateway: Option<String>,
    pub internal: bool,
    pub enable_icc: bool,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct InstallerLimits {
    pub memory: u64,
    pub cpu: u64,
    pub timeout_seconds: u64,
}
impl Default for InstallerLimits {
    fn default() -> Self {
        Self {
            memory: 1024,
            cpu: 100,
            timeout_seconds: 3600,
        }
    }
}
#[derive(Clone, Deserialize, Serialize)]
pub struct Registry {
    pub username: String,
    pub password: String,
}
impl Config {
    pub fn private_root(&self) -> PathBuf {
        self.system.data.join(".panasaurus-node")
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let parent = path.parent().context("Configuration directory missing")?;
        let temp = parent.join(format!(".config-{}", uuid::Uuid::new_v4()));
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        let result = (|| -> Result<()> {
            f.write_all(serde_yaml::to_string(self)?.as_bytes())?;
            f.sync_all()?;
            std::fs::rename(&temp, path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(temp);
        }
        result
    }
}
