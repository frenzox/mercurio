//! The command line.

use clap::{Args, Parser, Subcommand, ValueEnum};
use mercurio_client::cli::ConnectArgs;

use crate::dynsec::Dynsec;

const AFTER_HELP: &str = "\
Exit status: 0 on success, 1 when the broker refused the command, 2 when \
the broker couldn't be reached, refused the login or didn't answer in time, \
3 on wrong usage.";

/// Administer a running Mercurio broker
#[derive(Parser, Debug)]
#[command(name = "mercurioctl", version, after_help = AFTER_HELP)]
pub struct Cli {
    #[command(flatten)]
    pub connect: ConnectArgs,

    /// Print the response data as JSON instead of a table
    #[arg(long, global = true)]
    pub json: bool,

    /// Seconds to wait for the broker's answer
    #[arg(long, global = true, value_name = "SECONDS", default_value_t = 10)]
    pub timeout: u64,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Clients and their sessions, connected or not
    #[command(subcommand)]
    Client(ClientCommand),
    /// Banned client ids, user names and addresses
    #[command(subcommand)]
    Ban(BanCommand),
    /// Retained messages
    #[command(subcommand)]
    Retained(RetainedCommand),
    /// Subscriptions and their subscribers
    #[command(subcommand)]
    Subscription(SubscriptionCommand),
    /// The broker: version and statistics, configuration, reloading, log level
    #[command(subcommand)]
    Broker(BrokerCommand),
    /// The broker's listeners
    #[command(subcommand)]
    Listener(ListCommand),
    /// mercuriod's bridges to other brokers
    #[command(subcommand)]
    Bridge(ListCommand),
    /// The broker's plugins
    #[command(subcommand)]
    Plugin(ListCommand),
    /// Dynamic security: clients, groups, roles and their ACLs
    #[command(subcommand)]
    Dynsec(Dynsec),
}

#[derive(Subcommand, Debug)]
pub enum ListCommand {
    /// List them
    List,
}

/// Which part of a long list to show.
#[derive(Args, Debug)]
pub struct PageArgs {
    /// Skip this many items
    #[arg(long)]
    pub offset: Option<usize>,
    /// Show at most this many items, up to 1000 [default: 100]
    #[arg(long)]
    pub count: Option<usize>,
}

#[derive(Subcommand, Debug)]
pub enum ClientCommand {
    /// List clients and stored sessions
    List {
        /// Only connected clients
        #[arg(long, conflicts_with = "offline")]
        connected: bool,
        /// Only stored sessions whose client isn't connected
        #[arg(long)]
        offline: bool,
        /// Only client ids starting with this
        #[arg(long)]
        prefix: Option<String>,
        /// Only clients logged in as this user
        #[arg(long, value_name = "USERNAME")]
        user: Option<String>,
        /// Only clients from this IP address or CIDR range
        #[arg(long)]
        address: Option<String>,
        #[command(flatten)]
        page: PageArgs,
    },
    /// Show a client's session, subscriptions and traffic
    Show { clientid: String },
    /// Close a client's connection; its session stays
    Disconnect {
        clientid: String,
        /// Don't publish the client's will
        #[arg(long)]
        no_will: bool,
        /// Reason code MQTT 5 clients get, decimal or 0x.. [default: 0x98,
        /// Administrative action]
        #[arg(long, value_parser = parse_reason_code)]
        reason_code: Option<u8>,
    },
    /// Delete a client's stored session, subscriptions and queued messages
    DeleteSession {
        clientid: String,
        /// Disconnect the client first if it is connected
        #[arg(long)]
        disconnect: bool,
    },
}

/// What a ban matches clients by.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum BanType {
    Clientid,
    Username,
    Address,
}

impl BanType {
    pub fn as_str(self) -> &'static str {
        match self {
            BanType::Clientid => "clientid",
            BanType::Username => "username",
            BanType::Address => "address",
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum BanCommand {
    /// Ban clients by client id, user name or address; matching connected
    /// clients are disconnected at once
    Add {
        #[arg(value_name = "TYPE")]
        kind: BanType,
        /// The client id or user name, or an IP address or CIDR range
        value: String,
        /// Match client ids or user names starting with VALUE
        #[arg(long)]
        prefix: bool,
        /// Seconds until the ban expires [default: never]
        #[arg(long, value_name = "SECONDS")]
        duration: Option<u32>,
        /// Why, shown by `ban list`
        #[arg(long)]
        reason: Option<String>,
    },
    /// Lift a ban
    Remove {
        #[arg(value_name = "TYPE")]
        kind: BanType,
        value: String,
        /// The ban matches by prefix
        #[arg(long)]
        prefix: bool,
    },
    /// List bans
    List {
        #[command(flatten)]
        page: PageArgs,
    },
}

#[derive(Subcommand, Debug)]
pub enum RetainedCommand {
    /// List retained messages with a preview of their payload
    List {
        /// Only topics matching this filter
        #[arg(default_value = "#")]
        filter: String,
        #[command(flatten)]
        page: PageArgs,
    },
    /// Show a retained message with its whole payload and properties
    Show { topic: String },
    /// Delete the retained message of a topic, or of every topic a filter
    /// with wildcards matches
    Delete { filter: String },
}

#[derive(Subcommand, Debug)]
pub enum SubscriptionCommand {
    /// List topic filters with how many clients subscribe to each
    List {
        /// Only filters starting with this
        #[arg(long)]
        prefix: Option<String>,
        #[command(flatten)]
        page: PageArgs,
    },
    /// List the clients subscribed to exactly this filter
    Subscribers { filter: String },
}

#[derive(Subcommand, Debug)]
pub enum BrokerCommand {
    /// Version, uptime, storage and statistics
    Info,
    /// The configuration in use, as JSON, with its secrets redacted
    Config,
    /// Reread the password, ACL, dynamic security and plugin files, as
    /// SIGHUP does
    Reload,
    /// Show the log level, or set it until the broker restarts
    LogLevel {
        /// Directives as `[logging] level` takes, such as
        /// `info,mercurio_server::session=debug`
        level: Option<String>,
    },
}

fn parse_reason_code(value: &str) -> Result<u8, String> {
    let parsed = match value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        Some(hex) => u8::from_str_radix(hex, 16),
        None => value.parse(),
    };
    parsed.map_err(|_| format!("`{value}` isn't a reason code, 0 to 255 or 0x00 to 0xff"))
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};

    use super::{parse_reason_code, Cli};

    #[test]
    fn arguments_are_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn reason_codes_are_decimal_or_hex() {
        assert_eq!(parse_reason_code("0x98"), Ok(0x98));
        assert_eq!(parse_reason_code("0X8E"), Ok(0x8e));
        assert_eq!(parse_reason_code("152"), Ok(152));
        assert!(parse_reason_code("0x198").is_err());
        assert!(parse_reason_code("256").is_err());
        assert!(parse_reason_code("x").is_err());
    }

    #[test]
    fn connection_options_go_before_the_group_and_timeout_anywhere() {
        let cli = Cli::try_parse_from([
            "mercurioctl",
            "-H",
            "broker",
            "-p",
            "8883",
            "--tls",
            "client",
            "list",
            "--timeout",
            "3",
        ])
        .unwrap();
        assert_eq!(cli.connect.host.as_deref(), Some("broker"));
        assert_eq!(cli.connect.port, Some(8883));
        assert!(cli.connect.tls);
        assert_eq!(cli.timeout, 3);
        assert!(Cli::try_parse_from(["mercurioctl", "client", "list", "-H", "broker"]).is_err());
    }
}
