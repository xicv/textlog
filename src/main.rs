mod cli;
mod clipboard;
mod config;
mod error;
mod filters;
mod macos_perm;
mod mcp;
mod notifier;
mod ocr;
mod pipeline;
mod service;
mod storage;

use clap::Parser;

#[tokio::main]
async fn main() {
    let cli = cli::Cli::parse();
    if let Err(e) = cli::dispatch(cli).await {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
