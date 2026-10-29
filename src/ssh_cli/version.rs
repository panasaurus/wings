use super::{CliContext, SshCliCommand};

pub struct VersionCommand;

#[async_trait::async_trait]
impl SshCliCommand for VersionCommand {
    fn name(&self) -> &'static str {
        "version"
    }

    fn about(&self) -> &'static str {
        "Show the running wings version"
    }

    async fn execute(
        &self,
        _ctx: &CliContext<'_>,
        _args: &[&str],
        out: &mut Vec<String>,
    ) -> bool {
        out.push(format!(
            "Panasaurus Wings v{} ({}/{})",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH,
        ));
        true
    }
}
