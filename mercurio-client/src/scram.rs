//! SCRAM-SHA-256 enhanced authentication.

use bytes::Bytes;
use mercurio_scram::{ClientBinding, ScramClient, MECHANISM, MECHANISM_PLUS};

use crate::Authenticator;

/// Logs in with SCRAM-SHA-256 (RFC 5802, RFC 7677), method
/// `SCRAM-SHA-256`: the password never reaches the broker, and the broker
/// proves it knows the user's credentials. With
/// [`channel_binding`](Self::channel_binding) the method is
/// `SCRAM-SHA-256-PLUS`, bound to the TLS 1.3 connection (RFC 9266), so a
/// man in the middle holding a trusted certificate can't relay it.
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
    bind: bool,
    /// The connection's tls-exporter data, when binding
    tls_exporter: Option<[u8; 32]>,
}

impl ScramSha256 {
    pub fn new(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            client: ScramClient::new(username, password),
            bind: false,
            tls_exporter: None,
        }
    }

    /// Binds the exchange to the TLS channel, with method
    /// `SCRAM-SHA-256-PLUS`. Connecting without TLS 1.3 then fails with
    /// [`ClientError::AuthenticationFailed`](crate::ClientError::AuthenticationFailed)
    /// before anything goes to the broker.
    pub fn channel_binding(mut self, bind: bool) -> Self {
        self.bind = bind;
        self
    }
}

impl Authenticator for ScramSha256 {
    fn method(&self) -> String {
        if self.bind { MECHANISM_PLUS } else { MECHANISM }.to_string()
    }

    fn channel(&mut self, tls_exporter: Option<[u8; 32]>) {
        self.tls_exporter = tls_exporter;
    }

    fn start(&mut self) -> Result<Option<Bytes>, String> {
        let binding = match (self.bind, self.tls_exporter) {
            (false, _) => ClientBinding::Unsupported,
            (true, Some(exporter)) => ClientBinding::TlsExporter(exporter),
            (true, None) => return Err("channel binding needs a TLS 1.3 connection".into()),
        };
        self.client.set_binding(binding);
        self.client
            .start()
            .map(|first| Some(Bytes::from(first)))
            .map_err(|e| e.to_string())
    }

    fn respond(&mut self, challenge: Option<Bytes>) -> Result<Option<Bytes>, String> {
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
