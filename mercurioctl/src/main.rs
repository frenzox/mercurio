//! mercurioctl: administers a running Mercurio broker through its
//! `$CONTROL/broker/v1` API.

use std::{io::Write, path::Path, process::ExitCode, time::Duration};

use clap::FromArgMatches;

mod cli;
mod client;
mod commands;
mod dynsec;
mod output;

use cli::Cli;
use client::Failure;

/// The exit status for wrong usage; clap's own, 2, is mercurioctl's for
/// connection failures.
const USAGE: u8 = 3;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = match parse() {
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

fn parse() -> Result<Cli, clap::Error> {
    Cli::from_arg_matches(&cli::command().try_get_matches()?)
}

async fn run(cli: Cli) -> Result<(), Failure> {
    if let cli::Command::Completions { shell } = cli.command {
        cli::completions(shell, &mut std::io::stdout());
        return Ok(());
    }
    if let Some(question) = cli.command.confirmation() {
        confirm(&question)?;
    }
    let request = commands::request(cli.command, new_password)?;
    let mut connect = client::resolve(cli.connect, Path::new(client::ADMIN_SOCKET));
    connect
        .ask_password()
        .map_err(|e| Failure::Usage(format!("can't read a password: {e}")))?;
    let wait = Duration::from_secs(cli.timeout);
    let Some(data) = client::send(&connect, &request, wait).await? else {
        return Ok(());
    };
    // A reader that went away, such as `head`, isn't an error
    let _ = std::io::stdout().write_all(output::render(&request, &data, cli.json).as_bytes());
    if request.name() == "reload" && output::reload_failed(&data) {
        return Err(Failure::Rejected {
            code: None,
            message: "some files weren't reloaded; the broker keeps their previous contents"
                .to_string(),
        });
    }
    Ok(())
}

/// Asks twice for a new password for `username`; empty means none.
fn new_password(username: &str) -> Result<String, Failure> {
    let password = prompt(&format!("New password for {username}: "))?;
    if !password.is_empty() && prompt("Again: ")? != password {
        return Err(Failure::Usage("the passwords don't match".to_string()));
    }
    Ok(password)
}

/// Asks `question` on the terminal and fails unless the answer is yes; off
/// a terminal, where nobody can answer, it fails and points at `--yes`.
fn confirm(question: &str) -> Result<(), Failure> {
    use std::io::IsTerminal;

    if !std::io::stdin().is_terminal() {
        return Err(Failure::Usage(format!(
            "{question} Add --yes to confirm when not on a terminal"
        )));
    }
    eprint!("{question} [y/N] ");
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .map_err(|e| Failure::Usage(format!("can't read the answer: {e}")))?;
    match answer.trim() {
        "y" | "Y" | "yes" | "Yes" | "YES" => Ok(()),
        _ => Err(Failure::Usage("not confirmed".to_string())),
    }
}

fn prompt(text: &str) -> Result<String, Failure> {
    rpassword::prompt_password(text)
        .map_err(|e| Failure::Usage(format!("can't read a password: {e}")))
}
