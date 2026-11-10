//! Why recado failed, which sets its exit status.

use std::fmt;

use mercurio_client::{ClientError, DisconnectReason};

/// The exit status for wrong usage; clap's own, 2, is recado's for
/// connection failures.
pub const USAGE: u8 = 3;

#[derive(Debug, PartialEq)]
pub enum Failure {
    /// The broker refused a subscription or a message
    Rejected(String),
    /// The broker couldn't be reached, refused the login or closed the
    /// connection
    Connection(String),
    /// The command line asks for something that can't be done
    Usage(String),
}

impl Failure {
    pub fn exit_code(&self) -> u8 {
        match self {
            Failure::Rejected(_) => 1,
            Failure::Connection(_) => 2,
            Failure::Usage(_) => USAGE,
        }
    }

    /// The connection ended for `reason` while recado still needed it.
    pub fn disconnected(reason: &DisconnectReason) -> Failure {
        Failure::Connection(match reason {
            DisconnectReason::ServerInitiated {
                reason,
                reason_string,
                ..
            } => match reason_string {
                Some(why) => format!("the broker closed the connection: {reason} ({why})"),
                None => format!("the broker closed the connection: {reason}"),
            },
            DisconnectReason::ConnectionClosed => "the broker closed the connection".to_string(),
            DisconnectReason::KeepAliveTimeout => {
                "the broker stopped answering keep-alive pings".to_string()
            }
            DisconnectReason::ProtocolError(why) => format!("protocol error: {why}"),
            _ => "the connection was lost".to_string(),
        })
    }
}

impl From<ClientError> for Failure {
    fn from(error: ClientError) -> Self {
        match error {
            ClientError::PublishRefused(reason) => {
                Failure::Rejected(format!("the broker refused the message: {reason}"))
            }
            ClientError::NotSupported(_) => Failure::Rejected(error.to_string()),
            ClientError::TooLong(_) | ClientError::PacketTooLarge => {
                Failure::Usage(error.to_string())
            }
            error => Failure::Connection(error.to_string()),
        }
    }
}

impl From<std::io::Error> for Failure {
    fn from(error: std::io::Error) -> Self {
        Failure::Usage(error.to_string())
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Failure::Rejected(message) | Failure::Connection(message) | Failure::Usage(message) => {
                f.write_str(message)
            }
        }
    }
}
