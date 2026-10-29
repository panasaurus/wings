// Panasaurus Wings — SSH `.wings` CLI
//
// Provides a small command-line interface reachable over SSH (alongside SFTP).
// Users connect with their panel credentials, then run e.g. `.wings help` or
// `.wings power start` to manage their server from a terminal.
//
// Adapted from the Calagopus Wings shell CLI design, retargeted to the
// Panasaurus Wings `State` / `Server` model.

use crate::State;
use async_trait::async_trait;

pub mod help;
pub mod logs;
pub mod permissions;
pub mod power;
pub mod stats;
pub mod status;
pub mod version;

/// A single `.wings` subcommand.
///
/// Implementations are gathered in [`COMMANDS`] and dispatched by name from
/// [`CliContext::run`]. Each command receives the shared [`CliContext`], the
/// raw subcommand arguments, and an output sink it should push lines into.
/// Returning `false` signals a failure to the caller (used to set the SSH
/// channel exit status).
#[async_trait]
pub trait SshCliCommand: Sync {
    fn name(&self) -> &'static str;
    fn about(&self) -> &'static str;
    fn usage(&self) -> Option<&'static str> {
        None
    }

    async fn execute(
        &self,
        ctx: &CliContext<'_>,
        args: &[&str],
        out: &mut Vec<String>,
    ) -> bool;
}

/// All registered `.wings` subcommands, in display order.
pub static COMMANDS: &[&dyn SshCliCommand] = &[
    &help::HelpCommand,
    &version::VersionCommand,
    &status::StatusCommand,
    &power::PowerCommand,
    &stats::StatsCommand,
    &logs::LogsCommand,
    &permissions::PermissionsCommand,
];

/// Context handed to every command. Holds everything a command needs to
/// inspect or act on the user's server: a shared [`State`] reference, the
/// server UUID the user authenticated against, their panel user UUID, their
/// IP (for audit logging), the permission strings the panel granted them
/// for this SFTP session, and the configured CLI prefix (`.wings` by
/// default, configurable via `system.sftp.shell.cli_name`).
pub struct CliContext<'a> {
    pub state: &'a State,
    pub server: &'a str,
    pub user: &'a str,
    pub user_ip: std::net::IpAddr,
    pub permissions: &'a [String],
    pub console_attached: bool,
    pub prefix: &'a str,
}

/// Result of dispatching a single `.wings` line.
pub struct CliOutput {
    pub lines: Vec<String>,
    pub success: bool,
}

impl CliContext<'_> {
    /// Whether the user holds a given permission (or the `*` wildcard).
    pub fn has_permission(&self, permission: &str) -> bool {
        self.permissions
            .iter()
            .any(|p| p == permission || p == "*")
    }

    /// The configured `.wings` prefix used in help / usage strings.
    pub fn prefix(&self) -> &str {
        self.prefix
    }

    pub fn usage_line(&self, command: &dyn SshCliCommand) -> String {
        match command.usage() {
            Some(usage) => format!("Usage: {} {} {usage}", self.prefix(), command.name()),
            None => format!("Usage: {} {}", self.prefix(), command.name()),
        }
    }

    /// Dispatch a full `.wings <subcommand> [args...]` line.
    pub async fn run(&self, line: &str) -> CliOutput {
        let mut segments = line.split_whitespace();
        segments.next(); // consume the ".wings" prefix

        let mut lines = Vec::new();
        let name = segments.next();
        let args = segments.collect::<Vec<_>>();

        let success = match COMMANDS.iter().find(|command| Some(command.name()) == name) {
            Some(command) => command.execute(self, &args, &mut lines).await,
            None => {
                lines.push(format!(
                    "Unknown command. Type '{} help' for a list of commands.",
                    self.prefix()
                ));
                false
            }
        };

        CliOutput { lines, success }
    }
}

/// Detect whether a raw shell/exec line is a CLI invocation. Used by the
/// SSH handler to decide whether to route a session through the CLI. The
/// `prefix` is typically `config.system.sftp.shell.cli_name` (`.wings` by
/// default).
pub fn is_cli_command(prefix: &str, line: &str) -> bool {
    line.split_whitespace().next() == Some(prefix)
}
