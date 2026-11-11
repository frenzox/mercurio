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
        conflicts_with_all = ["host", "port", "tls", "cafile", "insecure"]
    )]
    pub unix: Option<PathBuf>,

    /// Client ID (auto-generated if not specified)
    #[arg(short = 'i', long)]
    pub client_id: Option<String>,

    /// Username for authentication
    #[arg(short = 'u', long)]
    pub username: Option<String>,

    /// Password for authentication
    #[arg(short = 'P', long)]
    pub password: Option<String>,

    /// Enable TLS for the connection
    #[arg(long)]
    pub tls: bool,

    /// Path to CA certificate file (PEM format) for TLS verification
    #[arg(long)]
    pub cafile: Option<String>,

    /// Skip TLS certificate verification (insecure, for testing only)
    #[arg(long)]
    pub insecure: bool,
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
            .finish()
    }
}

impl ConnectArgs {
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
    fn credentials_and_client_id_are_passed_on() {
        let options = parse(&["-i", "me", "-u", "alice", "-P", "secret", "--insecure"])
            .unwrap()
            .to_connect_options();
        assert_eq!(options.client_id.as_deref(), Some("me"));
        assert_eq!(options.username.as_deref(), Some("alice"));
        assert_eq!(options.password.as_deref(), Some(&b"secret"[..]));
        assert!(options.tls.danger_skip_verify);
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
        ] {
            let args: Vec<&str> = ["--unix", "/x.sock"]
                .into_iter()
                .chain(flag.iter().copied())
                .collect();
            assert!(parse(&args).is_err(), "--unix accepted with {flag:?}");
        }
    }
}
