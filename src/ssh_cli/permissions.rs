use super::{CliContext, SshCliCommand};

pub struct PermissionsCommand;

#[async_trait::async_trait]
impl SshCliCommand for PermissionsCommand {
    fn name(&self) -> &'static str {
        "permissions"
    }

    fn about(&self) -> &'static str {
        "Show the permissions granted to your SFTP session"
    }

    async fn execute(
        &self,
        ctx: &CliContext<'_>,
        _args: &[&str],
        out: &mut Vec<String>,
    ) -> bool {
        if ctx.permissions.is_empty() {
            out.push("No permissions are granted to this session.".into());
        } else {
            out.push("Granted permissions:".into());
            for permission in ctx.permissions {
                out.push(format!("  - {permission}"));
            }
        }
        true
    }
}
