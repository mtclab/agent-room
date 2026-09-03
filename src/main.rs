//! `agent-room` - the connector daemon.

#![forbid(unsafe_code)]

use agent_room::cli::{Cli, run};
use clap::Parser;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match run(Cli::parse()).await {
        Ok(0) => std::process::ExitCode::SUCCESS,
        Ok(code) => std::process::ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(exc) => {
            tracing::error!("{exc:#}");
            std::process::ExitCode::FAILURE
        }
    }
}
