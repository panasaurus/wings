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
        let name = Self::name(id)?;
        let output = tokio::time::timeout(
            self.config.timeout(),
            self.command().args(["inspect", &name]).output(),
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
        if let Some(container) = self.inspect(id).await?
            && !crate::hibernation::network_matches(&self.config, settings, &container)?
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
            return Ok(());
        }
        crate::storage::provision(&self.config, settings).await?;
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
            self.run(&strings(&["pull", image])).await?;
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
            "/tmp:rw,nosuid,nodev,size=67108864,mode=1777",
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
            "512",
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
        args.extend(limit_arguments(settings, false)?);
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
        ensure!(
            settings["mounts"].as_array().is_none_or(Vec::is_empty),
            "Additional host mounts are not supported in this prototype"
        );
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
            || !crate::hibernation::network_matches(&self.config, settings, &container)?)
    }

    pub async fn update_limits(&self, settings: &Value) -> Result<()> {
        let id = settings["uuid"].as_str().context("Missing server UUID")?;
        if self.inspect(id).await?.is_some() {
            let mut arguments = strings(&["update"]);
            arguments.extend(limit_arguments(settings, true)?);
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
            && host["NanoCpus"].as_u64().unwrap_or(0) > 0)
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn clearing_existing_limits_requires_container_recreation() {
        let container = json!({"HostConfig":{"Memory":128,"NanoCpus":250000000,"CpusetCpus":"0"}});
        assert!(limits_need_rebuild(&container, &json!({"build":{}})));
        assert!(!limits_need_rebuild(
            &container,
            &json!({"build":{"memory_limit":128,"cpu_limit":25,"threads":"0"}})
        ));
        assert!(!limits_need_rebuild(
            &json!({"HostConfig":{}}),
            &json!({"build":{}})
        ));
    }

    #[test]
    fn resource_flags_honor_zero_swap_and_unlimited_resets() {
        let limited = json!({"build":{"memory_limit":128,"swap":0,"cpu_limit":25}});
        let args = limit_arguments(&limited, true).unwrap();
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--memory-swap", "128m"])
        );
        assert!(args.windows(2).any(|pair| pair == ["--cpus", "0.25"]));
        let unlimited = limit_arguments(&json!({"build":{}}), true).unwrap();
        assert!(unlimited.windows(2).any(|pair| pair == ["--memory", "0m"]));
        assert!(
            unlimited
                .windows(2)
                .any(|pair| pair == ["--memory-swap", "-1"])
        );
        assert!(limit_arguments(&json!({"build":{"swap":-2}}), false).is_err());
    }

    #[test]
    fn racing_directory_symlinks_cannot_escape_disk_scans() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let root = tempfile::tempdir().unwrap();
        let inside = root.path().join("inside");
        let outside = root.path().join("outside");
        std::fs::create_dir(&inside).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("marker"), vec![0; 1024 * 1024]).unwrap();
        let slot = inside.join("moving");
        let parked = root.path().join("parked");
        std::fs::create_dir(&slot).unwrap();
        let done = Arc::new(AtomicBool::new(false));
        let stop = done.clone();
        let worker = std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                std::fs::rename(&slot, &parked).unwrap();
                std::os::unix::fs::symlink(&outside, &slot).unwrap();
                std::thread::yield_now();
                std::fs::remove_file(&slot).unwrap();
                std::fs::rename(&parked, &slot).unwrap();
                std::thread::yield_now();
            }
        });
        let mut escaped = false;
        for _ in 0..5000 {
            if let Ok(bytes) = disk_usage(&inside) {
                escaped |= bytes > 0;
            }
        }
        done.store(true, Ordering::Relaxed);
        worker.join().unwrap();
        assert!(
            !escaped,
            "The scanner followed a racing symlink outside server storage"
        );
    }

    #[test]
    fn rejects_container_argument_and_path_injection() {
        for id in [
            "../other",
            "--privileged",
            "a/b",
            "",
            "11111111-1111-4111-A111-111111111111",
        ] {
            assert!(Engine::name(id).is_err());
        }
        assert!(Engine::name("11111111-1111-4111-a111-111111111111").is_ok());
    }
    #[test]
    fn parses_docker_decimal_and_binary_units() {
        assert_eq!(parse_size("1.25GiB"), 1342177280);
        assert_eq!(parse_size(" 42.5kB "), 42500);
        assert_eq!(parse_size("4096B"), 4096);
    }
    #[test]
    fn disk_stats_do_not_follow_symlinks() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("file"), "abc").unwrap();
        std::os::unix::fs::symlink("/", root.path().join("escape")).unwrap();
        assert_eq!(disk_usage(root.path()).unwrap(), 3);
    }
}
