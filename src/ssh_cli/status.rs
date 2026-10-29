use super::{CliContext, SshCliCommand};
use serde_json::json;

pub struct StatusCommand;

#[async_trait::async_trait]
impl SshCliCommand for StatusCommand {
    fn name(&self) -> &'static str {
        "status"
    }

    fn about(&self) -> &'static str {
        "Show the server state"
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
        let state = match crate::lifecycle::status(ctx.state, ctx.server).await {
            Ok(state) => state,
            Err(error) => {
                out.push(format!("Failed to query server state: {error:#}"));
                return false;
            }
        };

        match args {
            [] => {
                out.push(format!("State: {state}"));
            }
            ["--json"] => {
                out.push(
                    json!({
                        "state": state,
                        "server": ctx.server,
                    })
                    .to_string(),
                );
            }
            _ => {
                out.push(ctx.usage_line(self));
                return false;
            }
        }
        true
    }
}
