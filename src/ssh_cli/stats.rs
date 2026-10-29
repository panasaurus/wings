use super::{CliContext, SshCliCommand};

pub struct StatsCommand;

#[async_trait::async_trait]
impl SshCliCommand for StatsCommand {
    fn name(&self) -> &'static str {
        "stats"
    }

    fn about(&self) -> &'static str {
        "Show current resource utilization for the server"
    }

    fn usage(&self) -> Option<&'static str> {
        Some("[--json]")
    }

    async fn execute(
        &self,
        ctx: &CliContext<'_>,
        args: &[&str],
        out: &mut Vec<String>,
    ) -> bool {
        if !ctx.has_permission("control.stats") && !ctx.has_permission("control") {
            out.push("You are missing the `control.stats` permission to do this.".into());
            return false;
        }
        let snapshot = match ctx.state.console_snapshot(ctx.server).await {
            Ok(Some(value)) => value,
            Ok(None) => serde_json::json!({}),
            Err(error) => {
                out.push(format!("Failed to query server stats: {error:#}"));
                return false;
            }
        };

        match args {
            [] => {
                let state = snapshot["state"].as_str().unwrap_or("unknown");
                let cpu = snapshot["cpu_absolute"].as_f64().map(|v| format!("{v:.1}%")).unwrap_or_else(|| "n/a".into());
                let mem = snapshot["memory_bytes"].as_u64().map(human_bytes::human_bytes).unwrap_or_else(|| "n/a".into());
                let mem_limit = snapshot["memory_limit_bytes"].as_u64().map(human_bytes::human_bytes);
                let disk = snapshot["disk_bytes"].as_u64().map(human_bytes::human_bytes).unwrap_or_else(|| "n/a".into());
                let uptime = snapshot["uptime"].as_u64().map(format_uptime).unwrap_or_else(|| "n/a".into());

                out.push(format!("State:   {state}"));
                out.push(format!("Uptime:  {uptime}"));
                out.push(format!("CPU:     {cpu}"));
                match mem_limit {
                    Some(limit) => out.push(format!("Memory:  {mem} / {limit}")),
                    None => out.push(format!("Memory:  {mem}")),
                }
                out.push(format!("Disk:    {disk}"));
            }
            ["--json"] => {
                out.push(snapshot.to_string());
            }
            _ => {
                out.push(ctx.usage_line(self));
                return false;
            }
        }
        true
    }
}

fn format_uptime(seconds: u64) -> String {
    let days = seconds / 86400;
    let hours = (seconds % 86400) / 3600;
    let minutes = (seconds % 3600) / 60;
    let secs = seconds % 60;
    if days > 0 {
        format!("{days}d {hours}h {minutes}m")
    } else if hours > 0 {
        format!("{hours}h {minutes}m {secs}s")
    } else if minutes > 0 {
        format!("{minutes}m {secs}s")
    } else {
        format!("{secs}s")
    }
}
