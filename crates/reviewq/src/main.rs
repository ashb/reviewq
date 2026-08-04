//! reviewq — a deterministic PR review queue.
//!
//! This binary wires the pieces together: it reads config, resolves a forge via
//! `reviewq-forge`, and runs subcommands over the pure logic in `reviewq-core`.

mod cli;
mod commands;
mod config;
mod paths;

use std::process::ExitCode;

use clap::Parser as _;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = cli::Cli::parse();
    init_tracing(cli.verbose);

    match commands::dispatch(cli).await {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

/// Quiet by default; `-v` raises our own level, `RUST_LOG` overrides entirely.
fn init_tracing(verbose: u8) {
    let default = match verbose {
        0 => "warn",
        1 => "reviewq=info",
        2 => "reviewq=debug",
        _ => "reviewq=trace,octocrab=debug",
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .without_time()
        .init();
}
