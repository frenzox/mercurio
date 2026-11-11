//! SCRAM-SHA-256 enhanced authentication.

use bytes::Bytes;
use mercurio_scram::{ScramClient, MECHANISM};

use crate::Authenticator;

/// Logs in with SCRAM-SHA-256 (RFC 5802, RFC 7677), method
/// `SCRAM-SHA-256`, without channel binding: the password never reaches
/// the broker, and the broker proves it knows the user's credentials.
/// Without that proof, in the CONNACK or in the AUTH that ends a
/// re-authentication, the connection fails with
/// [`ClientError::AuthenticationFailed`](crate::ClientError::AuthenticationFailed).
///
/// It keeps the salted password between exchanges, so reconnections and
/// re-authentications skip the costly key derivation while the broker's
/// salt and iteration count stay the same. Needs MQTT 5.
///
/// ```no_run
/// use mercurio_client::{ConnectOptions, ScramSha256};
///
/// let options = ConnectOptions::new("localhost", 1883)
///     .authenticator(ScramSha256::new("alice", "secret"));
/// ```
#[derive(Debug, Clone)]
pub struct ScramSha256 {
    client: ScramClient,
    /// Why the exchange could not start, reported at the first challenge
    failed: Option<String>,
}

impl ScramSha256 {
    pub fn new(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            client: ScramClient::new(username, password),
            failed: None,
        }
    }
}

impl Authenticator for ScramSha256 {
    fn method(&self) -> String {
        MECHANISM.to_string()
    }

    fn start(&mut self) -> Option<Bytes> {
        match self.client.start() {
            Ok(first) => {
                self.failed = None;
                Some(Bytes::from(first))
            }
            Err(e) => {
                self.failed = Some(e.to_string());
                None
            }
        }
    }

    fn respond(&mut self, challenge: Option<Bytes>) -> Result<Option<Bytes>, String> {
        if let Some(failed) = self.failed.take() {
            return Err(failed);
        }
        let challenge = challenge.ok_or("the broker sent no SCRAM challenge")?;
        self.client
            .respond(&challenge)
            .map(|last| Some(Bytes::from(last)))
            .map_err(|e| e.to_string())
    }

    fn finish(&mut self, data: Option<Bytes>) -> Result<(), String> {
        self.client
            .finish(data.as_deref().unwrap_or_default())
            .map_err(|e| e.to_string())
    }
}
