use crate::config::Config;
use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use std::{path::PathBuf, process::Stdio, time::Duration};
use tokio::{io::AsyncWriteExt, process::Command};

#[derive(Clone)]
pub struct Engine {
    pub config: Config,
}

impl Engine {
    pub fn command(&self) -> Command {
        let mut command = Command::new(&self.config.docker.executable);
        if let Some(context) = &self.config.docker.context {
            command.args(["--context", context]);
        }
        if let Some(directory) = &self.config.docker.config_directory {
            command.env("DOCKER_CONFIG", directory);
        }
        command.kill_on_drop(true);
        command
    }

    pub async fn run(&self, arguments: &[String]) -> Result<String> {
        let output = tokio::time::timeout(
            self.config.timeout(),
            self.command().args(arguments).output(),
        )
        .await
        .context("Docker operation timed out")??;
        ensure!(
            output.status.success(),
            "Docker operation failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    pub async fn json(&self, arguments: &[String]) -> Result<Value> {
        Ok(serde_json::from_str(&self.run(arguments).await?)?)
    }

    pub fn name(id: &str) -> Result<String> {
        ensure!(
            uuid::Uuid::parse_str(id)?.to_string() == id,
            "Use a canonical server UUID"
        );
        Ok(format!("panasaurus-{id}"))
    }

    pub fn directory(&self, id: &str) -> Result<PathBuf> {
        Self::name(id)?;
        Ok(self.config.system.data.join(id).join(".server-data"))
    }

    pub async fn inspect(&self, id: &str) -> Result<Option<Value>> {
        self.inspect_owned(&Self::name(id)?).await
    }
    async fn inspect_owned(&self, name: &str) -> Result<Option<Value>> {
        let output = tokio::time::timeout(
            self.config.timeout(),
            self.command().args(["inspect", name]).output(),
        )
        .await
        .context("Docker inspect timed out")??;
        if !output.status.success() {
            let error = String::from_utf8_lossy(&output.stderr);
            if error.to_lowercase().contains("no such object")
                || error.to_lowercase().contains("no such container")
            {
                return Ok(None);
            }
            bail!("Docker inspect failed: {error}");
        }
        let values: Vec<Value> = serde_json::from_slice(&output.stdout)?;
        let value = values
            .into_iter()
            .next()
            .context("Missing Docker inspect response")?;
        ensure!(
            value["Config"]["Labels"]["dev.panasaurus.managed"] == "true",
            "Refusing an unmanaged container"
        );
        ensure!(
            value["Config"]["Labels"]["dev.panasaurus.node"] == self.config.uuid,
            "Container belongs to another node"
        );
        Ok(Some(value))
    }

    pub async fn create(&self, settings: &Value) -> Result<()> {
        let id = settings["uuid"]
            .as_str()
            .context("Server configuration has no UUID")?;
        ensure!(
            self.inspect_owned(&format!("panasaurus-install-{id}"))
                .await?
                .is_none(),
            "An installation container is still running"
        );
        if let Some(container) = self.inspect(id).await?
            && (!crate::hibernation::network_matches(&self.config, settings, &container)?
                || container["HostConfig"]["NanoCpus"].as_u64().unwrap_or(0) > 0)
        {
            ensure!(
                container["State"]["Running"] != true,
                "Stop the container before changing its hibernation port mapping"
            );
            self.remove(id).await?;
        }
        if let Some(container) = self.inspect(id).await? {
            crate::storage::verify(&self.config, settings).await?;
            ensure!(
                container["HostConfig"]["ReadonlyRootfs"] == true,
                "Recreate the container offline to apply filesystem protection"
            );
            self.update_limits(settings).await?;
            if container["State"]["Running"] != true {
                crate::process::apply_engine(self, settings).await?;
            }
            return Ok(());
        }
        crate::storage::provision(&self.config, settings).await?;
        crate::process::apply_engine(self, settings).await?;
        let image = settings["container"]["image"]
            .as_str()
            .context("Server image is missing")?;
        ensure!(
            !image.starts_with('-') && !image.is_empty(),
            "Invalid Docker image"
        );
        let local_image = self
            .command()
            .args(["image", "inspect", image])
            .output()
            .await?;
        if !local_image.status.success() {
            self.pull(image).await?;
        }
        let directory = self.directory(id)?;
        tokio::fs::create_dir_all(&directory).await?;
        let mut args = strings(&[
            "create",
            "--name",
            &Self::name(id)?,
            "--label",
            "dev.panasaurus.managed=true",
            "--label",
            &format!("dev.panasaurus.node={}", self.config.uuid),
            "--read-only",
            "--tmpfs",
            &format!(
                "/tmp:rw,nosuid,nodev,size={},mode=1777",
                self.config
                    .docker
                    .tmpfs_size
                    .checked_mul(1024 * 1024)
                    .context("Tmpfs size overflow")?
            ),
            "--log-driver",
            "json-file",
            "--log-opt",
            "max-size=10m",
            "--log-opt",
            "max-file=3",
            "--interactive",
            "--init",
            "--user",
            &self.config.docker.user,
            "--workdir",
            "/home/container",
            "--cap-drop",
            "ALL",
            "--security-opt",
            "no-new-privileges:true",
            "--pids-limit",
            &self.config.docker.container_pid_limit.to_string(),
            "--restart",
            "no",
            "--mount",
            &format!("type=bind,src={},dst=/home/container", directory.display()),
        ]);
        if let Some(route) = crate::hibernation::route_for(&self.config, id) {
            route.validate_settings(settings)?;
            args.extend(strings(&[
                "--label",
                &format!("dev.panasaurus.hibernation={}", route.listen),
            ]));
        }
        args.extend(self.runtime_arguments(settings, false)?);
        if let Some(network) = self.ensure_network().await? {
            args.extend(strings(&["--network", &network]));
        }
        for ip in &self.config.docker.network.dns {
            ip.parse::<std::net::IpAddr>()?;
            args.extend(strings(&["--dns", ip]));
        }
        if !self.config.docker.userns_mode.is_empty() {
            ensure!(
                self.config.docker.userns_mode == "host",
                "Unsupported user namespace mode"
            );
            args.extend(strings(&["--userns", "host"]));
        }
        if let Some(tz) = &self.config.docker.timezone {
            args.extend(strings(&["--env", &format!("TZ={tz}")]));
        }
        args.extend(self.mount_arguments(settings)?);
        if let Some(environment) = settings["environment"].as_object() {
            for (key, value) in environment {
                ensure!(!key.contains(['=', '\0']), "Invalid environment key");
                let value = value
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| value.to_string());
                args.extend(strings(&["--env", &format!("{key}={value}")]));
            }
        }
        if let Some(invocation) = settings["invocation"].as_str() {
            args.extend(strings(&["--env", &format!("STARTUP={invocation}")]));
        }
        if let Some(mappings) = settings["allocations"]["mappings"].as_object() {
            for (ip, ports) in mappings {
                ip.parse::<std::net::IpAddr>()
                    .context("Invalid allocation IP")?;
                for port in ports.as_array().context("Invalid allocation ports")? {
                    let port = port
                        .as_u64()
                        .filter(|p| *p > 0 && *p <= 65535)
                        .context("Invalid allocation port")?;
                    for protocol in ["tcp", "udp"] {
                        let published = crate::hibernation::publish_address(
                            &self.config,
                            settings,
                            ip,
                            port as u16,
                            protocol,
                        )?;
                        args.extend(strings(&["--publish", &published]));
                    }
                }
            }
        }
        args.push(image.to_owned());
        self.run(&args).await?;
        Ok(())
    }

    pub async fn needs_limit_reset(&self, settings: &Value) -> Result<bool> {
        let id = settings["uuid"].as_str().context("Missing server UUID")?;
        let Some(container) = self.inspect(id).await? else {
            return Ok(false);
        };
        Ok(limits_need_rebuild(&container, settings)
            || container["HostConfig"]["NanoCpus"].as_u64().unwrap_or(0) > 0
            || !crate::hibernation::network_matches(&self.config, settings, &container)?)
    }

    pub async fn update_limits(&self, settings: &Value) -> Result<()> {
        let id = settings["uuid"].as_str().context("Missing server UUID")?;
        if self.inspect(id).await?.is_some() {
            let mut arguments = strings(&["update"]);
            arguments.extend(self.runtime_arguments(settings, true)?);
            arguments.push(Self::name(id)?);
            self.run(&arguments).await?;
        }
        Ok(())
    }

    pub async fn state(&self, id: &str) -> Result<String> {
        let Some(container) = self.inspect(id).await? else {
            return Ok("offline".into());
        };
        Ok(
            if container["State"]["Running"].as_bool().unwrap_or(false) {
                "running"
            } else {
                "offline"
            }
            .into(),
        )
    }

    pub async fn power(&self, settings: &Value, action: &str) -> Result<()> {
        let id = settings["uuid"].as_str().context("Missing UUID")?;
        if matches!(action, "start" | "restart") {
            ensure!(
                !settings["suspended"].as_bool().unwrap_or(false),
                "Cannot start a suspended server"
            );
            self.create(settings).await?;
        }
        let name = Self::name(id)?;
        match action {
            "start" => {
                self.run(&strings(&["start", &name])).await?;
            }
            "stop" => {
                if self.state(id).await? != "offline" {
                    self.run(&strings(&["stop", "--time", "15", &name])).await?;
                }
            }
            "restart" => {
                self.run(&strings(&["restart", "--time", "15", &name]))
                    .await?;
            }
            "kill" => {
                if self.state(id).await? != "offline" {
                    self.run(&strings(&["kill", &name])).await?;
                }
            }
            _ => bail!("Invalid power action"),
        }
        Ok(())
    }

    pub async fn cleanup_installer(&self, id: &str) -> Result<bool> {
        Self::name(id)?;
        let name = format!("panasaurus-install-{id}");
        if self.inspect_owned(&name).await?.is_some() {
            self.run(&strings(&["rm", "--force", &name])).await?;
            return Ok(true);
        }
        Ok(false)
    }
    pub async fn remove(&self, id: &str) -> Result<()> {
        if self.inspect(id).await?.is_some() {
            self.run(&strings(&["rm", "--force", &Self::name(id)?]))
                .await?;
        }
        Ok(())
    }

    pub async fn logs(&self, id: &str) -> Result<Vec<String>> {
        if self.inspect(id).await?.is_none() {
            return Ok(Vec::new());
        }
        let output = tokio::time::timeout(
            self.config.timeout(),
            self.command()
                .args(["logs", "--tail", "200", "--timestamps", &Self::name(id)?])
                .output(),
        )
        .await
        .context("Docker logs timed out")??;
        ensure!(output.status.success(), "Failed to read server logs");
        let mut lines: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .chain(String::from_utf8_lossy(&output.stderr).lines())
            .map(str::to_owned)
            .collect();
        lines.sort();
        Ok(lines)
    }

    pub async fn send_command(&self, id: &str, input: &str) -> Result<()> {
        ensure!(
            self.state(id).await? == "running",
            "Cannot send commands to an offline server"
        );
        ensure!(
            input.len() <= 8192 && !input.contains(['\0', '\u{10}', '\u{11}']),
            "Invalid console command"
        );
        let mut child = self
            .command()
            .args([
                "attach",
                "--sig-proxy=false",
                "--detach-keys=ctrl-p,ctrl-q",
                &Self::name(id)?,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut stdin = child
            .stdin
            .take()
            .context("Cannot attach container stdin")?;
        stdin.write_all(format!("{input}\n").as_bytes()).await?;
        stdin.flush().await?;
        tokio::time::sleep(Duration::from_millis(200)).await;
        stdin.write_all(&[0x10, 0x11]).await?;
        drop(stdin);
        if tokio::time::timeout(Duration::from_secs(2), child.wait())
            .await
            .is_err()
        {
            child.kill().await?;
        }
        Ok(())
    }

    pub async fn resources(&self, id: &str) -> Result<Value> {
        let state = self.state(id).await?;
        let mut usage = json!({"memory_bytes":0,"memory_limit_bytes":0,"cpu_absolute":0.0,"network_rx_bytes":0,"network_tx_bytes":0,"uptime":0,"disk_bytes":0,"state":state});
        if state == "running" {
            let stats = self
                .json(&strings(&[
                    "stats",
                    "--no-stream",
                    "--format",
                    "{{json .}}",
                    &Self::name(id)?,
                ]))
                .await?;
            usage["cpu_absolute"] = json!(
                stats["CPUPerc"]
                    .as_str()
                    .unwrap_or("0%")
                    .trim_end_matches('%')
                    .parse::<f64>()?
            );
            let memory: Vec<&str> = stats["MemUsage"]
                .as_str()
                .unwrap_or("0B / 0B")
                .split('/')
                .collect();
            usage["memory_bytes"] = json!(parse_size(memory[0]));
            usage["memory_limit_bytes"] = json!(parse_size(memory.get(1).unwrap_or(&"0B")));
            let network: Vec<&str> = stats["NetIO"]
                .as_str()
                .unwrap_or("0B / 0B")
                .split('/')
                .collect();
            usage["network_rx_bytes"] = json!(parse_size(network[0]));
            usage["network_tx_bytes"] = json!(parse_size(network.get(1).unwrap_or(&"0B")));
            if let Some(container) = self.inspect(id).await?
                && let Some(started) = container["State"]["StartedAt"].as_str()
            {
                let started = DateTime::parse_from_rfc3339(started)?;
                usage["uptime"] = json!(
                    (Utc::now() - started.with_timezone(&Utc))
                        .num_milliseconds()
                        .max(0)
                );
            }
        }
        let directory = self.directory(id)?;
        usage["disk_bytes"] =
            json!(tokio::task::spawn_blocking(move || disk_usage(&directory)).await??);
        usage["network"] =
            json!({"rx_bytes":usage["network_rx_bytes"],"tx_bytes":usage["network_tx_bytes"]});
        Ok(usage)
    }
}

pub fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| (*arg).to_owned()).collect()
}

fn limits_need_rebuild(container: &Value, settings: &Value) -> bool {
    let build = &settings["build"];
    let host = &container["HostConfig"];
    (build["memory_limit"].as_u64().unwrap_or(0) == 0 && host["Memory"].as_u64().unwrap_or(0) > 0)
        || (build["cpu_limit"].as_f64().unwrap_or(0.0) == 0.0
            && (host["NanoCpus"].as_u64().unwrap_or(0) > 0
                || host["CpuQuota"].as_i64().unwrap_or(0) > 0))
        || (build["threads"].as_str().unwrap_or("").is_empty()
            && !host["CpusetCpus"].as_str().unwrap_or("").is_empty())
}

fn limit_arguments(settings: &Value, update: bool) -> Result<Vec<String>> {
    let build = &settings["build"];
    let memory = build["memory_limit"].as_u64().unwrap_or(0);
    let cpu = build["cpu_limit"].as_f64().unwrap_or(0.0);
    let swap = build["swap"].as_i64().unwrap_or(0);
    ensure!(
        cpu.is_finite() && cpu >= 0.0 && swap >= -1,
        "Invalid container limits"
    );
    let mut args = Vec::new();
    if memory > 0 || update {
        let combined = if memory == 0 || swap == -1 {
            "-1".to_owned()
        } else {
            format!(
                "{}m",
                memory
                    .checked_add(swap as u64)
                    .context("Memory limit overflow")?
            )
        };
        args.extend(strings(&[
            "--memory",
            &format!("{memory}m"),
            "--memory-swap",
            &combined,
        ]));
    }
    if cpu > 0.0 || update {
        args.extend(strings(&["--cpus", &(cpu / 100.0).to_string()]));
    }
    let threads = build["threads"].as_str().unwrap_or("");
    if !threads.is_empty() || update {
        args.extend(strings(&["--cpuset-cpus", threads]));
    }
    Ok(args)
}

fn disk_usage(path: &std::path::Path) -> Result<u64> {
    match cap_std::fs::Dir::open_ambient_dir(path, cap_std::ambient_authority()) {
        Ok(dir) => Ok(crate::files::disk_usage(&dir)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error.into()),
    }
}

fn parse_size(input: &str) -> u64 {
    let input = input.trim();
    let split = input
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(input.len());
    let amount = input[..split].parse::<f64>().unwrap_or(0.0);
    let multiplier = match input[split..].trim() {
        "KiB" => 1024.0,
        "MiB" => 1024.0_f64.powi(2),
        "GiB" => 1024.0_f64.powi(3),
        "kB" => 1000.0,
        "MB" => 1000.0_f64.powi(2),
        "GB" => 1000.0_f64.powi(3),
        _ => 1.0,
    };
    (amount * multiplier) as u64
}

impl Engine {
    pub async fn pull(&self, image: &str) -> Result<()> {
        ensure!(
            !image.is_empty() && !image.starts_with('-') && !image.contains(['\0', '\n', '\r']),
            "Invalid image name"
        );
        let first = image.split('/').next().unwrap_or("docker.io");
        let registry = if first.contains('.') || first.contains(':') || first == "localhost" {
            first
        } else {
            "docker.io"
        };
        if let Some(auth) = self.config.docker.registries.get(registry) {
            let mut child = self
                .command()
                .args([
                    "login",
                    registry,
                    "--username",
                    &auth.username,
                    "--password-stdin",
                ])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?;
            let mut input = child.stdin.take().context("Registry input missing")?;
            input.write_all(auth.password.as_bytes()).await?;
            input.shutdown().await?;
            drop(input);
            let result = tokio::time::timeout(self.config.timeout(), child.wait()).await?;
            ensure!(result?.success(), "Registry authentication failed");
        }
        self.run(&strings(&["pull", image])).await?;
        Ok(())
    }
    fn runtime_arguments(&self, settings: &Value, update: bool) -> Result<Vec<String>> {
        let mut adjusted = settings.clone();
        let memory = settings["build"]["memory_limit"].as_u64().unwrap_or(0);
        if memory > 0 {
            adjusted["build"]["memory_limit"] = json!(
                memory
                    .checked_mul(100 + self.config.docker.memory_overhead_percent)
                    .context("Memory overflow")?
                    .div_ceil(100)
            );
        }
        let mut args = limit_arguments(&adjusted, update)?;
        if self.config.docker.cpu_shares > 0 {
            args.extend(strings(&[
                "--cpu-shares",
                &self.config.docker.cpu_shares.to_string(),
            ]));
        }
        let cpu = settings["build"]["cpu_limit"].as_f64().unwrap_or(0.0);
        if cpu > 0.0 {
            let period = self.config.docker.cpu_period;
            if let Some(index) = args.iter().position(|a| a == "--cpus") {
                args.drain(index..index + 2);
            }
            args.extend(strings(&[
                "--cpu-period",
                &period.to_string(),
                "--cpu-quota",
                &((cpu / 100.0 * period as f64) as u64).max(1000).to_string(),
            ]));
        }
        Ok(args)
    }
    pub fn mount_arguments(&self, settings: &Value) -> Result<Vec<String>> {
        let mut args = Vec::new();
        if let Some(mounts) = settings["mounts"].as_array() {
            ensure!(mounts.len() <= 64, "Too many host mounts");
            for mount in mounts {
                let source =
                    std::path::Path::new(mount["source"].as_str().context("Mount source missing")?)
                        .canonicalize()?;
                ensure!(
                    self.config.allowed_mounts.iter().any(|p| p
                        .canonicalize()
                        .is_ok_and(|p| source == p || source.starts_with(p))),
                    "Host mount is not approved on this node"
                );
                let target = mount["target"].as_str().context("Mount target missing")?;
                let target_path = std::path::Path::new(target);
                ensure!(
                    target_path.is_absolute()
                        && !target_path
                            .components()
                            .any(|p| matches!(p, std::path::Component::ParentDir))
                        && !["/", "/home", "/home/container"].contains(&target)
                        && !["/proc", "/sys", "/dev", "/etc"]
                            .iter()
                            .any(|p| target_path.starts_with(p)),
                    "Unsafe mount target"
                );
                ensure!(
                    !source.to_string_lossy().contains([',', '\n', '\r'])
                        && !target.contains([',', '\0', '\n', '\r']),
                    "Invalid mount syntax"
                );
                ensure!(
                    !source.starts_with(&self.config.system.data),
                    "Mount may not expose node or other server storage"
                );
                let readonly = mount["read_only"].as_bool().unwrap_or(true);
                args.extend(strings(&[
                    "--mount",
                    &format!(
                        "type=bind,src={},dst={target}{}",
                        source.display(),
                        if readonly { ",readonly" } else { "" }
                    ),
                ]));
            }
        }
        Ok(args)
    }
    async fn ensure_network(&self) -> Result<Option<String>> {
        let Some(name) = &self.config.docker.network.name else {
            return Ok(None);
        };
        ensure!(
            !["host", "none", "bridge"].contains(&name.as_str())
                && name.len() <= 63
                && !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
            "Use a dedicated Docker network name"
        );
        let inspected = self
            .command()
            .args(["network", "inspect", name])
            .output()
            .await?;
        if inspected.status.success() {
            let networks: Vec<Value> = serde_json::from_slice(&inspected.stdout)?;
            ensure!(
                networks
                    .first()
                    .is_some_and(|n| n["Labels"]["dev.panasaurus.node"] == self.config.uuid),
                "Docker network belongs to another operator"
            );
            return Ok(Some(name.clone()));
        }
        let net = &self.config.docker.network;
        let mut args = strings(&[
            "network",
            "create",
            "--driver",
            "bridge",
            "--label",
            &format!("dev.panasaurus.node={}", self.config.uuid),
            "--opt",
            &format!("com.docker.network.bridge.enable_icc={}", net.enable_icc),
        ]);
        if net.internal {
            args.push("--internal".into())
        }
        if let Some(mtu) = net.mtu {
            ensure!((576..=9000).contains(&mtu), "Invalid MTU");
            args.extend(strings(&[
                "--opt",
                &format!("com.docker.network.driver.mtu={mtu}"),
            ]));
        }
        if let Some(subnet) = &net.subnet {
            ensure!(!subnet.starts_with('-'), "Invalid subnet");
            args.extend(strings(&["--subnet", subnet]));
        }
        if let Some(gateway) = &net.gateway {
            gateway.parse::<std::net::IpAddr>()?;
            args.extend(strings(&["--gateway", gateway]));
        }
        args.push(name.clone());
        self.run(&args).await?;
        Ok(Some(name.clone()))
    }
}
