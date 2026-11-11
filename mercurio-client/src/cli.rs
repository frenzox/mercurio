//! Command-line arguments for reaching a broker, shared by the Mercurio
//! tools. Needs the `clap` feature.

#[cfg(unix)]
use std::path::PathBuf;

use clap::Args;

use crate::ConnectOptions;

/// Where the broker is and how to log in to it. Flatten it into a clap
/// command with `#[command(flatten)]`.
#[derive(Args, Clone)]
#[non_exhaustive]
pub struct ConnectArgs {
    /// MQTT broker hostname [default: localhost]
    #[arg(short = 'H', long)]
    pub host: Option<String>,

    /// MQTT broker port [default: 1883]
    #[arg(short = 'p', long)]
    pub port: Option<u16>,

    /// Connect through this Unix socket instead of TCP
    #[cfg(unix)]
    #[arg(
        long,
        value_name = "PATH",
        conflicts_with_all = ["host", "port", "tls", "cafile", "insecure", "cert", "key", "ws"]
    )]
    pub unix: Option<PathBuf>,

    /// Client ID [default: one the broker assigns, or a random one where
    /// the MQTT version needs it]
    #[arg(short = 'i', long)]
    pub client_id: Option<String>,

    /// Username for authentication
    #[arg(short = 'u', long)]
    pub username: Option<String>,

    /// Password for authentication; other users can see it in `ps`, so
    /// prefer the tool's variable, or leave it out with -u to be asked
    #[arg(short = 'P', long)]
    pub password: Option<String>,

    /// Enable TLS for the connection, verifying the broker with the
    /// operating system's certificate store, or with the Mozilla roots
    /// bundled with the client when that store holds none
    #[arg(long)]
    pub tls: bool,

    /// CA certificates (PEM) to verify the broker with, the only ones then
    /// trusted; implies --tls
    #[arg(long)]
    pub cafile: Option<String>,

    /// Skip TLS certificate verification (insecure, for testing only);
    /// implies --tls
    #[arg(long)]
    pub insecure: bool,

    /// Client certificate (PEM) to log in with; needs --key and implies
    /// --tls
    #[arg(long, value_name = "PATH", requires = "key")]
    pub cert: Option<String>,

    /// Private key (PEM) of the --cert certificate
    #[arg(long, value_name = "PATH", requires = "cert")]
    pub key: Option<String>,

    /// Speak MQTT over a WebSocket on the host and port: ws://, or wss://
    /// with --tls
    #[arg(long)]
    pub ws: bool,

    /// Path of the WebSocket endpoint [default: /mqtt]
    #[arg(long, value_name = "PATH", requires = "ws")]
    pub ws_path: Option<String>,
}

impl std::fmt::Debug for ConnectArgs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_struct("ConnectArgs");
        debug.field("host", &self.host).field("port", &self.port);
        #[cfg(unix)]
        debug.field("unix", &self.unix);
        debug
            .field("client_id", &self.client_id)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("tls", &self.tls)
            .field("cafile", &self.cafile)
            .field("insecure", &self.insecure)
            .field("cert", &self.cert)
            .field("key", &self.key)
            .field("ws", &self.ws)
            .field("ws_path", &self.ws_path)
            .finish()
    }
}

impl ConnectArgs {
    /// `command`, which flattens these arguments, with the password also
    /// read from the environment `variable` when `-P` isn't given. An empty
    /// variable counts as unset, and `--help` never shows its value.
    pub fn password_variable(command: clap::Command, variable: &'static str) -> clap::Command {
        // Systemd units and CI often leave variables empty, which means
        // unset, as for the shell's `${VAR:-}`
        if std::env::var_os(variable).is_some_and(|value| value.is_empty()) {
            return command;
        }
        command.mut_arg("password", |arg| arg.env(variable).hide_env_values(true))
    }

    /// With a user name and no password, asks for the password when
    /// standard error is a terminal. Elsewhere, as in scripts, it connects
    /// with the user name alone.
    pub fn ask_password(&mut self) -> std::io::Result<()> {
        use std::io::IsTerminal;

        if self.username.is_some() && self.password.is_none() && std::io::stderr().is_terminal() {
            self.password = Some(rpassword::prompt_password("Password: ")?);
        }
        Ok(())
    }

    /// Connection options for these arguments; everything else keeps the
    /// defaults of [`ConnectOptions::new`].
    pub fn to_connect_options(&self) -> ConnectOptions {
        let mut options = self.transport();
        if let Some(id) = &self.client_id {
            options = options.client_id(id);
        }
        if let Some(user) = &self.username {
            options = options.username(user);
        }
        if let Some(password) = &self.password {
            options = options.password(password.as_bytes().to_vec());
        }
        if self.tls {
            options = options.tls(true);
        }
        if let Some(ca) = &self.cafile {
            options = options.ca_path(ca);
        }
        if self.insecure {
            options = options.danger_skip_tls_verify(true);
        }
        if let (Some(cert), Some(key)) = (&self.cert, &self.key) {
            options = options.client_cert(cert, key);
        }
        if self.ws {
            options = options.websocket(self.ws_path.as_deref().unwrap_or("/mqtt"));
        }
        options
    }

    fn transport(&self) -> ConnectOptions {
        #[cfg(unix)]
        if let Some(path) = &self.unix {
            return ConnectOptions::unix(path);
        }
        ConnectOptions::new(
            self.host.as_deref().unwrap_or("localhost"),
            self.port.unwrap_or(1883),
        )
    }
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};

    use super::ConnectArgs;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        connect: ConnectArgs,
    }

    fn parse(args: &[&str]) -> Result<ConnectArgs, clap::Error> {
        Cli::try_parse_from(std::iter::once("test").chain(args.iter().copied()))
            .map(|cli| cli.connect)
    }

    #[test]
    fn debug_output_hides_the_password() {
        let shown = format!("{:?}", parse(&["-P", "hunter2"]).unwrap());
        assert!(!shown.contains("hunter2"), "{shown}");
        assert!(shown.contains(r#"password: Some("<redacted>")"#), "{shown}");
    }

    #[test]
    fn the_password_can_come_from_a_variable() {
        let parse_with = |variable: &'static str, args: &[&str]| {
            let command = ConnectArgs::password_variable(Cli::command(), variable);
            let matches = command
                .try_get_matches_from(std::iter::once("test").chain(args.iter().copied()))
                .unwrap();
            <Cli as clap::FromArgMatches>::from_arg_matches(&matches)
                .unwrap()
                .connect
                .password
        };
        std::env::set_var("MERCURIO_CLIENT_TEST_PASSWORD", "from-env");
        std::env::set_var("MERCURIO_CLIENT_TEST_EMPTY", "");
        assert_eq!(
            parse_with("MERCURIO_CLIENT_TEST_PASSWORD", &[]).as_deref(),
            Some("from-env")
        );
        assert_eq!(
            parse_with("MERCURIO_CLIENT_TEST_PASSWORD", &["-P", "flag"]).as_deref(),
            Some("flag")
        );
        assert_eq!(parse_with("MERCURIO_CLIENT_TEST_EMPTY", &[]), None);
        assert_eq!(parse_with("MERCURIO_CLIENT_TEST_UNSET", &[]), None);

        let help = ConnectArgs::password_variable(Cli::command(), "MERCURIO_CLIENT_TEST_PASSWORD")
            .render_help()
            .to_string();
        assert!(help.contains("MERCURIO_CLIENT_TEST_PASSWORD"), "{help}");
        assert!(!help.contains("from-env"), "{help}");
    }

    #[test]
    fn arguments_are_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn host_and_port_default_to_localhost_1883() {
        let options = parse(&[]).unwrap().to_connect_options();
        assert_eq!((options.host.as_str(), options.port), ("localhost", 1883));
        assert!(!options.tls.enabled);

        let args = parse(&["-H", "broker", "-p", "8883", "--tls", "--cafile", "ca.pem"]).unwrap();
        assert_eq!(args.host.as_deref(), Some("broker"));
        let options = args.to_connect_options();
        assert_eq!((options.host.as_str(), options.port), ("broker", 8883));
        assert!(options.tls.enabled);
        assert_eq!(options.tls.ca_path.as_deref(), Some("ca.pem"));
    }

    #[test]
    fn a_ca_file_or_insecure_turns_tls_on() {
        for args in [&["--cafile", "ca.pem"][..], &["--insecure"]] {
            let options = parse(args).unwrap().to_connect_options();
            assert!(options.tls.enabled, "{args:?}");
        }
    }

    #[test]
    fn a_client_certificate_needs_its_key_and_turns_tls_on() {
        let options = parse(&["--cert", "client.pem", "--key", "client.key"])
            .unwrap()
            .to_connect_options();
        assert!(options.tls.enabled);
        assert_eq!(options.tls.cert_path.as_deref(), Some("client.pem"));
        assert_eq!(options.tls.key_path.as_deref(), Some("client.key"));
        assert!(parse(&["--cert", "client.pem"]).is_err());
        assert!(parse(&["--key", "client.key"]).is_err());
    }

    #[test]
    fn credentials_and_client_id_are_passed_on() {
        let options = parse(&["-i", "me", "-u", "alice", "-P", "secret", "--insecure"])
            .unwrap()
            .to_connect_options();
        assert_eq!(options.client_id.as_deref(), Some("me"));
        assert_eq!(options.username.as_deref(), Some("alice"));
        assert_eq!(options.password.as_deref(), Some(&b"secret"[..]));
        assert!(options.tls.danger_skip_verify);
    }

    #[test]
    fn websocket_takes_an_optional_path() {
        let options = parse(&[]).unwrap().to_connect_options();
        assert_eq!(options.websocket_path, None);
        let options = parse(&["--ws"]).unwrap().to_connect_options();
        assert_eq!(options.websocket_path.as_deref(), Some("/mqtt"));
        let options = parse(&["--ws", "--ws-path", "/ws", "--tls"])
            .unwrap()
            .to_connect_options();
        assert_eq!(options.websocket_path.as_deref(), Some("/ws"));
        assert!(options.tls.enabled);
        assert!(
            parse(&["--ws-path", "/ws"]).is_err(),
            "--ws-path without --ws"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_unix_socket_replaces_host_port_and_tls() {
        let options = parse(&["--unix", "/run/mercurio/admin.sock"])
            .unwrap()
            .to_connect_options();
        assert_eq!(
            options.unix_path.as_deref(),
            Some(std::path::Path::new("/run/mercurio/admin.sock"))
        );
        for flag in [
            &["-H", "broker"][..],
            &["-p", "1884"],
            &["--tls"],
            &["--cafile", "ca.pem"],
            &["--insecure"],
            &["--cert", "c.pem", "--key", "c.key"],
            &["--ws"],
        ] {
            let args: Vec<&str> = ["--unix", "/x.sock"]
                .into_iter()
                .chain(flag.iter().copied())
                .collect();
            assert!(parse(&args).is_err(), "--unix accepted with {flag:?}");
        }
    }
}
