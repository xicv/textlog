//! `clap`-derived command-line surface for the `tl` binary.
//!
//! Subcommand layout mirrors spec §CLI Commands. Subcommands that
//! depend on later phases (LaunchAgent install/uninstall/start/stop/
//! status, doctor) are scaffolded here and route to "not yet
//! implemented" handlers in `commands.rs`.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "tl",
    about = "macOS clipboard + OCR daemon, MCP server for Claude Code",
    version
)]
pub struct Cli {
    /// Override config directory (defaults to `$TEXTLOG_CONFIG_DIR` or `~/textlog/`).
    #[arg(long, global = true, env = "TEXTLOG_CONFIG_DIR")]
    pub config_dir: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the MCP stdio server (spawned by Claude Code via `claude mcp add textlog -- tl mcp`).
    Mcp,

    /// Print the textlog version.
    Version,

    /// Configuration management.
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },

    /// Inspect captured clipboard logs.
    Logs {
        #[command(subcommand)]
        cmd: LogsCmd,
    },

    /// Verify environment, permissions, and dependencies.
    Doctor,

    /// Install the LaunchAgent (Phase 13).
    Install,
    /// Remove the LaunchAgent (Phase 13).
    Uninstall,
    /// Start the daemon. With `--foreground`, runs the pipeline inline.
    Start {
        #[arg(long)]
        foreground: bool,
    },
    /// Stop the daemon.
    Stop,
    /// Show the daemon status.
    Status,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCmd {
    /// Print the current effective configuration as TOML.
    Show,
    /// Print the resolved config file path.
    Path,
    /// Overwrite the config file with the v2.0 defaults.
    Reset,
}

#[derive(Debug, Subcommand)]
pub enum LogsCmd {
    /// Show today's captures.
    Today,
    /// Search captures via FTS5.
    Search {
        /// Query string (FTS5 syntax).
        query: String,
        /// Max results (default 20).
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// Print the configured log directory.
    Path,
}

impl Cli {
    /// Try-parse from a string slice — used by tests so we can avoid
    /// touching the real process argv.
    #[cfg(test)]
    pub fn try_parse_argv(argv: &[&str]) -> std::result::Result<Self, clap::Error> {
        <Self as clap::Parser>::try_parse_from(argv)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mcp_subcommand() {
        let cli = Cli::try_parse_argv(&["tl", "mcp"]).unwrap();
        assert!(matches!(cli.command, Command::Mcp));
    }

    #[test]
    fn parses_version_subcommand() {
        let cli = Cli::try_parse_argv(&["tl", "version"]).unwrap();
        assert!(matches!(cli.command, Command::Version));
    }

    #[test]
    fn parses_config_show() {
        let cli = Cli::try_parse_argv(&["tl", "config", "show"]).unwrap();
        match cli.command {
            Command::Config { cmd: ConfigCmd::Show } => {}
            other => panic!("expected Config Show, got {other:?}"),
        }
    }

    #[test]
    fn parses_config_reset() {
        let cli = Cli::try_parse_argv(&["tl", "config", "reset"]).unwrap();
        match cli.command {
            Command::Config { cmd: ConfigCmd::Reset } => {}
            other => panic!("expected Config Reset, got {other:?}"),
        }
    }

    #[test]
    fn parses_logs_search_with_default_limit() {
        let cli = Cli::try_parse_argv(&["tl", "logs", "search", "panic"]).unwrap();
        match cli.command {
            Command::Logs {
                cmd: LogsCmd::Search { query, limit },
            } => {
                assert_eq!(query, "panic");
                assert_eq!(limit, 20);
            }
            other => panic!("expected Logs Search, got {other:?}"),
        }
    }

    #[test]
    fn parses_logs_search_with_custom_limit() {
        let cli = Cli::try_parse_argv(&["tl", "logs", "search", "x", "--limit", "5"]).unwrap();
        match cli.command {
            Command::Logs {
                cmd: LogsCmd::Search { query, limit },
            } => {
                assert_eq!(query, "x");
                assert_eq!(limit, 5);
            }
            other => panic!("expected Logs Search, got {other:?}"),
        }
    }

    #[test]
    fn config_dir_flag_overrides() {
        let cli =
            Cli::try_parse_argv(&["tl", "--config-dir", "/tmp/foo", "config", "path"]).unwrap();
        assert_eq!(cli.config_dir, Some(PathBuf::from("/tmp/foo")));
    }

    #[test]
    fn parses_start_foreground() {
        let cli = Cli::try_parse_argv(&["tl", "start", "--foreground"]).unwrap();
        match cli.command {
            Command::Start { foreground } => assert!(foreground),
            other => panic!("expected Start, got {other:?}"),
        }
    }

    #[test]
    fn missing_subcommand_errors() {
        // clap may report this as either `MissingSubcommand` or
        // `DisplayHelpOnMissingArgumentOrSubcommand` depending on the
        // version — accept anything that prevents a successful parse.
        assert!(Cli::try_parse_argv(&["tl"]).is_err());
    }
}
