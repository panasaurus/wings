use super::{COMMANDS, CliContext, SshCliCommand};

pub struct HelpCommand;

#[async_trait::async_trait]
impl SshCliCommand for HelpCommand {
    fn name(&self) -> &'static str {
        "help"
    }

    fn about(&self) -> &'static str {
        "Show this help message"
    }

    async fn execute(
        &self,
        ctx: &CliContext<'_>,
        _args: &[&str],
        out: &mut Vec<String>,
    ) -> bool {
        let width = COMMANDS
            .iter()
            .map(|command| command.name().len())
            .max()
            .unwrap_or(0);

        out.push("Available commands:".into());
        for command in COMMANDS {
            out.push(format!(
                "  {:width$} - {}",
                command.name(),
                command.about(),
            ));
        }
        out.push(String::new());
        out.push(format!(
            "Type '{} <command>' to run a command. Some commands take additional arguments.",
            ctx.prefix()
        ));
        true
    }
}
