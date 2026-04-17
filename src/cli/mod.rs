//! CLI surface for the `tl` binary. `args` defines the clap structure;
//! `commands` routes subcommands to handlers.

pub mod args;
pub mod commands;
pub mod doctor;

pub use args::Cli;
pub use commands::dispatch;
