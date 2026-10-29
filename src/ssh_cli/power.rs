use super::{CliContext, SshCliCommand};

pub struct PowerCommand;

#[async_trait::async_trait]
impl SshCliCommand for PowerCommand {
    fn name(&self) -> &'static str {
        "power"
    }

    fn about(&self) -> &'static str {
        "Send a power action to the server"
    }

    fn usage(&self) -> Option<&'static str> {
        Some("<start|restart|stop|kill>")
    }

    async fn execute(
        &self,
        ctx: &CliContext<'_>,
        args: &[&str],
        out: &mut Vec<String>,
    ) -> bool {
        let action = match args {
            [action] if matches!(action.as_ref(), "start" | "restart" | "stop" | "kill") => {
                *action
            }
            _ => {
                out.push(ctx.usage_line(self));
                return false;
            }
        };

        // The panel grants power permissions as `power.start`, `power.stop`,
        // etc. Require the matching one before acting.
        let permission = format!("power.{action}");
        if !ctx.has_permission(&permission) {
            out.push(format!(
                "You are missing the `{permission}` permission to do this."
            ));
            return false;
        }

        match crate::api::power_action(ctx.state, ctx.server, action).await {
            Ok(()) => {
                out.push(format!("Power action `{action}` accepted."));
                let _ = ctx.state.audit(
                    "power.ssh",
                    ctx.server,
                    ctx.user,
                    &ctx.user_ip.to_string(),
                    serde_json::json!({"action": action}),
                ).await;
                true
            }
            Err(error) => {
                out.push(format!("Failed to {action} the server: {error:#}"));
                false
            }
        }
    }
}
