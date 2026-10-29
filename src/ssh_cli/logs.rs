use super::{CliContext, SshCliCommand};

pub struct LogsCommand;

#[async_trait::async_trait]
impl SshCliCommand for LogsCommand {
    fn name(&self) -> &'static str {
        "logs"
    }

    fn about(&self) -> &'static str {
        "Show recent console output for the server"
    }

    fn usage(&self) -> Option<&'static str> {
        Some("[--limit N]")
    }

    async fn execute(
        &self,
        ctx: &CliContext<'_>,
        args: &[&str],
        out: &mut Vec<String>,
    ) -> bool {
        if !ctx.has_permission("control.console") && !ctx.has_permission("control") {
            out.push("You are missing the `control.console` permission to do this.".into());
            return false;
        }

        let mut limit: usize = 50;
        let mut i = 0;
        while i < args.len() {
            match args[i] {
                "--limit" => {
                    i += 1;
                    let Some(value) = args.get(i) else {
                        out.push(ctx.usage_line(self));
                        return false;
                    };
                    match value.parse::<usize>() {
                        Ok(n) if (1..=1000).contains(&n) => limit = n,
                        _ => {
                            out.push("--limit must be an integer between 1 and 1000.".into());
                            return false;
                        }
                    }
                }
                "--json" => {
                    out.push(ctx.usage_line(self));
                    return false;
                }
                other => {
                    out.push(format!("Unknown argument: {other}"));
                    out.push(ctx.usage_line(self));
                    return false;
                }
            }
            i += 1;
        }

        match ctx.state.engine.logs(ctx.server).await {
            Ok(lines) => {
                let start = lines.len().saturating_sub(limit);
                for line in &lines[start..] {
                    // Drop the leading ISO timestamp; keep just the message.
                    let text = line
                        .split_once(' ')
                        .map(|(_, msg)| msg)
                        .unwrap_or(line.as_str());
                    out.push(text.to_owned());
                }
                if lines.is_empty() {
                    out.push("(no logs yet)".into());
                }
                true
            }
            Err(error) => {
                out.push(format!("Failed to read server logs: {error:#}"));
                false
            }
        }
    }
}
