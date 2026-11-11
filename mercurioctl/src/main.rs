//! mercurioctl: administers a running Mercurio broker through its
//! `$CONTROL/broker/v1` API.

use std::{io::Write, process::ExitCode, time::Duration};

use clap::Parser;

mod cli;
mod client;
mod commands;

use cli::Cli;
use client::Failure;

/// The exit status for wrong usage; clap's own, 2, is mercurioctl's for
/// connection failures.
const USAGE: u8 = 3;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            let _ = e.print();
            return ExitCode::from(if e.use_stderr() { USAGE } else { 0 });
        }
    };
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            eprintln!("mercurioctl: {failure}");
            ExitCode::from(failure.exit_code())
        }
    }
}

async fn run(cli: Cli) -> Result<(), Failure> {
    let request = commands::request(cli.command);
    let wait = Duration::from_secs(cli.timeout);
    if let Some(data) = client::send(&cli.connect, &request, wait).await? {
        // A reader that went away, such as `head`, isn't an error
        let _ = std::io::stdout().write_all(format!("{data:#}\n").as_bytes());
    }
    Ok(())
}
