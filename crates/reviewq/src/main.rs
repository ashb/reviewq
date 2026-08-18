//! reviewq — a deterministic PR review queue.
//!
//! This binary wires the pieces together: it reads config, resolves a forge via
//! `reviewq-forge`, and runs subcommands over the pure logic in `reviewq-core`.

mod cli;
mod colour;
mod commands;

/// What this build is, as `git describe` saw it — the released version at a
/// tagged build, and the tag plus its distance, commit and dirtiness anywhere
/// else. See `build.rs`; the manifest's version is the fallback.
pub const VERSION: &str = env!("REVIEWQ_VERSION");

use std::process::ExitCode;

use clap::Parser as _;
use tracing_subscriber::EnvFilter;

use crate::colour::{Output, TerminalOutput};

#[tokio::main]
async fn main() -> ExitCode {
    let cli = cli::Cli::parse();
    init_tracing(cli.verbose);

    let output = TerminalOutput::detect();
    match commands::dispatch(cli, &output).await {
        Ok(code) => code,
        Err(err) => {
            output.eprintln(format!("error: {err:#}"));
            ExitCode::FAILURE
        }
    }
}

/// Quiet by default; `-v` raises our own level, `RUST_LOG` overrides entirely.
///
/// octocrab is pinned quiet at every `-v` level — its per-request HTTP tracing
/// (`HTTP{…}: requesting` / `stream closed`, one pair per page) drowns out our
/// own logs and says nothing useful. Anyone who genuinely wants the raw HTTP
/// trace can ask for it explicitly with `RUST_LOG=octocrab=debug`.
fn init_tracing(verbose: u8) {
    let default = match verbose {
        0 => "warn",
        1 => "reviewq=info,octocrab=warn",
        2 => "reviewq=debug,octocrab=warn",
        _ => "reviewq=trace,octocrab=warn",
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .without_time()
        .init();
}
