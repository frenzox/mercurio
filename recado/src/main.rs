//! Mercurio - MQTT command-line client

use std::process::ExitCode;

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use mercurio_client::cli::ConnectArgs;

mod common;
mod failure;
mod publish;
mod subscribe;

use common::init_logging;
use failure::Failure;

const AFTER_HELP: &str = "\
Exit status: 0 on success, 1 when the broker refused a subscription or a \
message, 2 when the broker couldn't be reached, refused the login or \
closed the connection, 3 on wrong usage.";

#[derive(Parser, Debug)]
#[command(name = "recado")]
#[command(about = "MQTT command-line client")]
#[command(version, after_help = AFTER_HELP)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Publish a message to a topic
    Pub(publish::PubArgs),
    /// Subscribe to topics and print messages
    Sub(subscribe::SubArgs),
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = match parse() {
        Ok(cli) => cli,
        Err(e) => {
            let _ = e.print();
            return ExitCode::from(if e.use_stderr() { failure::USAGE } else { 0 });
        }
    };
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            eprintln!("recado: {failure}");
            ExitCode::from(failure.exit_code())
        }
    }
}

/// The command line, with the password also read from `RECADO_PASSWORD`.
fn command() -> clap::Command {
    let password = |command| ConnectArgs::password_variable(command, "RECADO_PASSWORD");
    Cli::command()
        .mut_subcommand("pub", password)
        .mut_subcommand("sub", password)
}

fn parse() -> Result<Cli, clap::Error> {
    Cli::from_arg_matches(&command().try_get_matches()?)
}

async fn run(cli: Cli) -> Result<(), Failure> {
    match cli.command {
        Commands::Pub(args) => {
            init_logging(args.connection.verbose);
            publish::run(args).await
        }
        Commands::Sub(args) => {
            init_logging(args.connection.verbose);
            subscribe::run(args).await
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};

    use super::{common, Cli, Commands};

    #[test]
    fn arguments_are_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn connection_flags_keep_their_names() {
        let cli = Cli::try_parse_from([
            "recado",
            "sub",
            "-H",
            "broker",
            "-p",
            "8883",
            "-i",
            "me",
            "-u",
            "alice",
            "-P",
            "secret",
            "--tls",
            "--cafile",
            "ca.pem",
            "--insecure",
            "-k",
            "30",
            "-c",
            "-x",
            "60",
            "-V",
            "311",
            "-v",
            "-t",
            "a",
        ])
        .unwrap();
        let Commands::Sub(args) = cli.command else {
            panic!("expected sub");
        };
        let connection = &args.connection;
        let connect = &connection.connect;
        assert_eq!(connect.host.as_deref(), Some("broker"));
        assert_eq!(connect.port, Some(8883));
        assert_eq!(connect.client_id.as_deref(), Some("me"));
        assert_eq!(connect.username.as_deref(), Some("alice"));
        assert_eq!(connect.password.as_deref(), Some("secret"));
        assert!(connect.tls && connect.insecure);
        assert_eq!(connect.cafile.as_deref(), Some("ca.pem"));
        assert_eq!(connection.keep_alive, 30);
        assert!(connection.no_clean_start && connection.verbose);
        assert_eq!(connection.session_expiry, 60);
        assert_eq!(connection.protocol_version, common::Protocol::V311);
    }

    #[cfg(unix)]
    #[test]
    fn pub_and_sub_connect_through_a_unix_socket() {
        let path = "/run/mercurio/admin.sock";
        for command in ["pub", "sub"] {
            let cli = Cli::try_parse_from(["recado", command, "--unix", path, "-t", "a"]).unwrap();
            let connection = match cli.command {
                Commands::Pub(args) => args.connection,
                Commands::Sub(args) => args.connection,
            };
            assert_eq!(
                connection.connect.unix.as_deref(),
                Some(std::path::Path::new(path))
            );
            let both = ["recado", command, "--unix", path, "-H", "broker", "-t", "a"];
            assert!(Cli::try_parse_from(both).is_err(), "{command}");
        }
    }
}
