//! `HTTPing` executable entry point.

use std::process::ExitCode;

use httping::{Cli, run};

#[tokio::main]
async fn main() -> ExitCode {
    run(Cli::parse()).await
}
