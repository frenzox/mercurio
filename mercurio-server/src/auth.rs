//! Built-in authentication plugins: SCRAM-SHA-256 enhanced
//! authentication over credentials an embedder stores, and, for tests and
//! demos, PLAIN enhanced authentication and an in-memory user name and
//! password check.
//!
//! The PLAIN and in-memory checks keep passwords in plaintext, in memory.
//! They compare them in constant time, and take as long for an unknown user
//! as for a wrong password, but a real deployment wants hashed passwords,
//! such as `mercuriod`'s password file or dynamic security, or a plugin of
//! its own.

use std::{
    collections::HashMap,
    sync::{Arc, OnceLock},
};

use bytes::Bytes;

use mercurio_core::reason::ReasonCode;
pub use mercurio_scram::StoredCredentials;
use mercurio_scram::{AwaitingProof, ClientFirst, ServerError, DEFAULT_ITERATIONS, MECHANISM};
use tracing::warn;

use crate::plugin::{
    ready, AuthExchange, AuthResult, BoxFuture, ClientInfo, Credentials, Decision, Plugin,
    PluginKind, PluginStatus,
};

/// PLAIN authentication method (RFC 4616), for tests and demos.
///
/// Expects authentication data in the format: `\0username\0password`
/// where the first byte is NUL (authorization identity is empty).
pub struct PlainAuth {
    /// Map of username -> password for credential validation.
    credentials: Arc<HashMap<String, String>>,
}

impl PlainAuth {
    /// Create a new PLAIN authenticator with the given credentials.
    pub fn new(credentials: HashMap<String, String>) -> Self {
        PlainAuth {
            credentials: Arc::new(credentials),
        }
    }
}

/// Parse PLAIN authentication data.
///
/// Format: [authzid] NUL authcid NUL passwd
/// We expect authzid to be empty, so format is: NUL username NUL password
fn parse_plain_data(data: &Bytes) -> Option<(String, String)> {
    let data = data.as_ref();

    // Must start with NUL (empty authorization identity)
    if data.is_empty() || data[0] != 0 {
        return None;
    }

    // Find second NUL separating username and password
    let second_nul = data[1..].iter().position(|&b| b == 0)?;
    let username_end = 1 + second_nul;

    // Extract username and password
    let username = std::str::from_utf8(&data[1..username_end]).ok()?;
    let password = std::str::from_utf8(&data[username_end + 1..]).ok()?;

    Some((username.to_string(), password.to_string()))
}

impl Plugin for PlainAuth {
    fn name(&self) -> &str {
        "plain"
    }

    fn status(&self) -> PluginStatus {
        PluginStatus::new(self.name(), PluginKind::Builtin).with_hooks(&["begin_auth"])
    }

    fn begin_auth(&self, _client: &ClientInfo, method: &str) -> Option<Box<dyn AuthExchange>> {
        (method == "PLAIN").then(|| {
            Box::new(PlainExchange {
                credentials: Arc::clone(&self.credentials),
                done: false,
            }) as Box<dyn AuthExchange>
        })
    }
}

struct PlainExchange {
    credentials: Arc<HashMap<String, String>>,
    done: bool,
}

impl AuthExchange for PlainExchange {
    fn step<'a>(&'a mut self, data: Option<&'a Bytes>) -> BoxFuture<'a, AuthResult> {
        ready(self.check(data))
    }
}

impl PlainExchange {
    fn check(&mut self, data: Option<&Bytes>) -> AuthResult {
        // PLAIN is a single step
        if std::mem::replace(&mut self.done, true) {
            return AuthResult::Failed(ReasonCode::ProtocolError);
        }

        let Some(data) = data else {
            // PLAIN requires data in the initial request
            return AuthResult::Failed(ReasonCode::BadAuthenticationMethod);
        };

        let Some((username, password)) = parse_plain_data(data) else {
            return AuthResult::Failed(ReasonCode::MalformedPacket);
        };

        match check_password(&self.credentials, &username, password.as_bytes()) {
            Some(true) => AuthResult::Success {
                username: Some(username),
                data: None,
            },
            _ => AuthResult::Failed(ReasonCode::BadUserNameOrPassword),
        }
    }
}

/// Whether `password` is `username`'s in `credentials`; None for an unknown
/// user, found as slowly as a wrong password so timing doesn't tell which
/// users exist.
fn check_password(
    credentials: &HashMap<String, String>,
    username: &str,
    password: &[u8],
) -> Option<bool> {
    match credentials.get(username) {
        Some(stored) => Some(same_password(stored.as_bytes(), password)),
        None => {
            same_password(UNKNOWN_USER_PASSWORD, password);
            None
        }
    }
}

/// Compared with for unknown users, so they take as long as known ones.
const UNKNOWN_USER_PASSWORD: &[u8] = b"no password matches this one, as the user is unknown";

/// Compares in time that depends on the longer length only, never on where
/// the two differ, so timing tells nothing about the stored password.
fn same_password(stored: &[u8], given: &[u8]) -> bool {
    let mut difference = stored.len() ^ given.len();
    for i in 0..stored.len().max(given.len()) {
        let (a, b) = (stored.get(i), given.get(i));
        difference |= usize::from(a.unwrap_or(&0) ^ b.unwrap_or(&0));
    }
    std::hint::black_box(difference) == 0
}

/// In-memory user name and password check, for tests and demos.
///
/// It answers only for the users it holds: a wrong password is denied, and
/// an unknown user is left to the next plugin, so it can sit in front of
/// another source of users.
pub struct SimpleCredentialValidator {
    credentials: HashMap<String, String>,
}

impl SimpleCredentialValidator {
    /// Create a new validator with the given credentials.
    pub fn new(credentials: HashMap<String, String>) -> Self {
        Self { credentials }
    }
}

impl Plugin for SimpleCredentialValidator {
    fn name(&self) -> &str {
        "credentials"
    }

    fn status(&self) -> PluginStatus {
        PluginStatus::new(self.name(), PluginKind::Builtin).with_hooks(&["authenticate"])
    }

    fn authenticate<'a>(
        &'a self,
        _: &'a ClientInfo,
        credentials: &'a Credentials<'a>,
    ) -> BoxFuture<'a, Decision> {
        ready(
            match check_password(
                &self.credentials,
                credentials.username,
                credentials.password,
            ) {
                Some(true) => Decision::Allow,
                Some(false) => Decision::Deny,
                None => Decision::Defer,
            },
        )
    }
}

/// Where [`ScramAuth`] finds users' SCRAM-SHA-256 credentials.
pub trait ScramCredentialStore: Send + Sync + 'static {
    /// `username`'s credentials, or None for a user who doesn't exist, has
    /// no SCRAM credentials or may not log in as `client`. The exchange
    /// then goes on with stand-in credentials and fails at the proof, so
    /// the client can't tell which.
    fn scram_credentials<'a>(
        &'a self,
        client: &'a ClientInfo,
        username: &'a str,
    ) -> BoxFuture<'a, Option<StoredCredentials>>;
}

/// Users and their credentials held in memory, for tests and demos.
impl ScramCredentialStore for HashMap<String, StoredCredentials> {
    fn scram_credentials<'a>(
        &'a self,
        _: &'a ClientInfo,
        username: &'a str,
    ) -> BoxFuture<'a, Option<StoredCredentials>> {
        ready(self.get(username).cloned())
    }
}

/// SCRAM-SHA-256 enhanced authentication (RFC 5802, RFC 7677), method
/// `SCRAM-SHA-256`, without channel binding.
///
/// The client proves it knows the password without sending it, and the
/// broker proves it knows the credentials: its final message, sent as the
/// Authentication Data of the CONNACK or of the AUTH that ends a
/// re-authentication, carries the server signature. A user the store
/// doesn't give credentials for is answered with a salt derived from the
/// user name and a secret of the process, and with the iteration count of
/// [`decoy_iterations`](Self::decoy_iterations), so the broker's first
/// message doesn't tell whether the user exists; the exchange then fails
/// at the proof, as a wrong password does.
pub struct ScramAuth<S: ?Sized> {
    store: Arc<S>,
    decoy_iterations: u32,
}

impl<S: ScramCredentialStore + ?Sized> ScramAuth<S> {
    pub fn new(store: Arc<S>) -> Self {
        ScramAuth {
            store,
            decoy_iterations: DEFAULT_ITERATIONS,
        }
    }

    /// The iteration count announced for unknown users; by default
    /// [`mercurio_scram::DEFAULT_ITERATIONS`]. Set it to the count most of
    /// the store's credentials have, so it gives nothing away.
    pub fn decoy_iterations(mut self, iterations: u32) -> Self {
        self.decoy_iterations = iterations;
        self
    }
}

/// The secret stand-in salts are derived from. It lives as long as the
/// process, so a restart changes the salts of unknown users.
fn decoy_secret() -> &'static [u8; 32] {
    static SECRET: OnceLock<[u8; 32]> = OnceLock::new();
    SECRET.get_or_init(|| {
        let mut secret = [0u8; 32];
        getrandom::fill(&mut secret).expect("the operating system provides random bytes");
        secret
    })
}

impl<S: ScramCredentialStore + ?Sized> Plugin for ScramAuth<S> {
    fn name(&self) -> &str {
        "scram-sha-256"
    }

    fn status(&self) -> PluginStatus {
        PluginStatus::new(self.name(), PluginKind::Builtin).with_hooks(&["begin_auth"])
    }

    fn begin_auth(&self, client: &ClientInfo, method: &str) -> Option<Box<dyn AuthExchange>> {
        (method == MECHANISM).then(|| {
            Box::new(ScramExchange {
                store: Arc::clone(&self.store),
                client: client.clone(),
                decoy_iterations: self.decoy_iterations,
                state: ScramState::Start,
            }) as Box<dyn AuthExchange>
        })
    }
}

struct ScramExchange<S: ?Sized> {
    store: Arc<S>,
    client: ClientInfo,
    decoy_iterations: u32,
    state: ScramState,
}

enum ScramState {
    Start,
    AwaitingProof {
        awaiting: AwaitingProof,
        username: String,
        known: bool,
    },
    Done,
}

impl<S: ScramCredentialStore + ?Sized> AuthExchange for ScramExchange<S> {
    fn step<'a>(&'a mut self, data: Option<&'a Bytes>) -> BoxFuture<'a, AuthResult> {
        Box::pin(async move {
            match std::mem::replace(&mut self.state, ScramState::Done) {
                ScramState::Start => self.first(data).await,
                ScramState::AwaitingProof {
                    awaiting,
                    username,
                    known,
                } => {
                    let Some(data) = data else {
                        return AuthResult::Failed(ReasonCode::MalformedPacket);
                    };
                    match awaiting.verify(data) {
                        Ok(server_final) if known => AuthResult::Success {
                            username: Some(username),
                            data: Some(Bytes::from(server_final)),
                        },
                        Ok(_) | Err(ServerError::InvalidProof) => {
                            AuthResult::Failed(ReasonCode::BadUserNameOrPassword)
                        }
                        Err(error) => scram_refusal(error),
                    }
                }
                ScramState::Done => AuthResult::Failed(ReasonCode::ProtocolError),
            }
        })
    }
}

impl<S: ScramCredentialStore + ?Sized> ScramExchange<S> {
    /// Answers the client's first message with the salt and iteration
    /// count of the user's credentials, or of stand-in ones.
    async fn first(&mut self, data: Option<&Bytes>) -> AuthResult {
        let Some(data) = data else {
            // The client's first message goes in the CONNECT
            return AuthResult::Failed(ReasonCode::BadAuthenticationMethod);
        };
        let first = match ClientFirst::parse(data) {
            Ok(first) => first,
            Err(error) => return scram_refusal(error),
        };
        let username = first.username().to_string();
        let stored = self.store.scram_credentials(&self.client, &username).await;
        let known = stored.is_some();
        let credentials = stored.unwrap_or_else(|| {
            StoredCredentials::decoy(decoy_secret(), &username, self.decoy_iterations)
        });
        match first.respond(&credentials) {
            Ok((server_first, awaiting)) => {
                self.state = ScramState::AwaitingProof {
                    awaiting,
                    username,
                    known,
                };
                AuthResult::Continue(Bytes::from(server_first))
            }
            Err(error) => scram_refusal(error),
        }
    }
}

/// The reason code refusing a client for `error`, logged with the RFC's
/// name for it, as a refusing CONNACK carries no SCRAM message.
fn scram_refusal(error: ServerError) -> AuthResult {
    warn!("SCRAM-SHA-256 authentication refused: {}", error);
    AuthResult::Failed(match error {
        ServerError::ChannelBindingNotSupported | ServerError::ExtensionsNotSupported => {
            ReasonCode::BadAuthenticationMethod
        }
        ServerError::InvalidEncoding
        | ServerError::InvalidUsernameEncoding
        | ServerError::ChannelBindingsDontMatch => ReasonCode::MalformedPacket,
        ServerError::InvalidProof => ReasonCode::BadUserNameOrPassword,
        ServerError::NoResources => ReasonCode::ServerUnavailable,
        _ => ReasonCode::NotAuthorized,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> ClientInfo {
        ClientInfo::new("c", None)
    }

    fn create_plain_data(username: &str, password: &str) -> Bytes {
        let mut data = Vec::new();
        data.push(0); // Empty authorization identity
        data.extend_from_slice(username.as_bytes());
        data.push(0); // Separator
        data.extend_from_slice(password.as_bytes());
        Bytes::from(data)
    }

    #[tokio::test]
    async fn test_plain_auth_success() {
        let mut credentials = HashMap::new();
        credentials.insert("user".to_string(), "pass".to_string());
        let auth = PlainAuth::new(credentials);

        let data = create_plain_data("user", "pass");
        assert_eq!(
            auth.begin_auth(&client(), "PLAIN")
                .unwrap()
                .step(Some(&data))
                .await,
            AuthResult::Success {
                username: Some("user".into()),
                data: None,
            }
        );
    }

    #[tokio::test]
    async fn test_plain_auth_wrong_password() {
        let mut credentials = HashMap::new();
        credentials.insert("user".to_string(), "pass".to_string());
        let auth = PlainAuth::new(credentials);

        let data = create_plain_data("user", "wrongpass");
        match auth
            .begin_auth(&client(), "PLAIN")
            .unwrap()
            .step(Some(&data))
            .await
        {
            AuthResult::Failed(ReasonCode::BadUserNameOrPassword) => {}
            other => panic!("Expected BadUserNameOrPassword, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_plain_auth_unknown_user() {
        let mut credentials = HashMap::new();
        credentials.insert("user".to_string(), "pass".to_string());
        let auth = PlainAuth::new(credentials);

        let data = create_plain_data("unknown", "pass");
        match auth
            .begin_auth(&client(), "PLAIN")
            .unwrap()
            .step(Some(&data))
            .await
        {
            AuthResult::Failed(ReasonCode::BadUserNameOrPassword) => {}
            other => panic!("Expected BadUserNameOrPassword, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_plain_auth_no_data() {
        let credentials = HashMap::new();
        let auth = PlainAuth::new(credentials);

        match auth
            .begin_auth(&client(), "PLAIN")
            .unwrap()
            .step(None)
            .await
        {
            AuthResult::Failed(ReasonCode::BadAuthenticationMethod) => {}
            other => panic!("Expected BadAuthenticationMethod, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_plain_auth_malformed_data() {
        let credentials = HashMap::new();
        let auth = PlainAuth::new(credentials);

        // Missing initial NUL
        let data = Bytes::from("user\0pass");
        match auth
            .begin_auth(&client(), "PLAIN")
            .unwrap()
            .step(Some(&data))
            .await
        {
            AuthResult::Failed(ReasonCode::MalformedPacket) => {}
            other => panic!("Expected MalformedPacket, got {:?}", other),
        }
    }

    #[test]
    fn test_plain_auth_unsupported_method() {
        assert!(PlainAuth::new(HashMap::new())
            .begin_auth(&client(), "UNKNOWN")
            .is_none());
    }

    #[tokio::test]
    async fn test_plain_auth_exchange_is_single_step() {
        let mut credentials = HashMap::new();
        credentials.insert("admin".to_string(), "secret".to_string());
        let auth = PlainAuth::new(credentials);

        let data = create_plain_data("admin", "secret");
        let mut exchange = auth.begin_auth(&client(), "PLAIN").unwrap();
        assert!(matches!(
            exchange.step(Some(&data)).await,
            AuthResult::Success { .. }
        ));
        // A finished exchange accepts no more steps
        assert_eq!(
            exchange.step(Some(&data)).await,
            AuthResult::Failed(ReasonCode::ProtocolError)
        );
    }

    #[tokio::test]
    async fn test_credential_validator_success() {
        let mut creds = HashMap::new();
        creds.insert("user".to_string(), "pass".to_string());
        let validator = SimpleCredentialValidator::new(creds);

        assert_eq!(
            validator
                .authenticate(
                    &ClientInfo::new("c", None),
                    &Credentials::new("user", b"pass")
                )
                .await,
            Decision::Allow
        );
    }

    #[tokio::test]
    async fn test_credential_validator_wrong_password() {
        let mut creds = HashMap::new();
        creds.insert("user".to_string(), "pass".to_string());
        let validator = SimpleCredentialValidator::new(creds);

        assert_eq!(
            validator
                .authenticate(
                    &ClientInfo::new("c", None),
                    &Credentials::new("user", b"wrong")
                )
                .await,
            Decision::Deny
        );
    }

    #[tokio::test]
    async fn test_credential_validator_leaves_unknown_users_to_the_next_plugin() {
        let mut creds = HashMap::new();
        creds.insert("user".to_string(), "pass".to_string());
        let validator = SimpleCredentialValidator::new(creds);

        assert_eq!(
            validator
                .authenticate(
                    &ClientInfo::new("c", None),
                    &Credentials::new("unknown", b"pass")
                )
                .await,
            Decision::Defer
        );
    }

    #[test]
    fn test_passwords_match_only_when_equal() {
        assert!(same_password(b"secret", b"secret"));
        assert!(same_password(b"", b""));
        assert!(!same_password(b"secret", b"secreT"));
        assert!(!same_password(b"secret", b"secret!"));
        assert!(!same_password(b"secret", b"secre"));
        assert!(!same_password(b"", b"x"));
    }

    fn scram_auth() -> ScramAuth<HashMap<String, StoredCredentials>> {
        let mut users = HashMap::new();
        users.insert(
            "user".to_string(),
            StoredCredentials::derive("pencil", b"sixteen byte slt", 4096),
        );
        ScramAuth::new(Arc::new(users)).decoy_iterations(4096)
    }

    fn data(text: String) -> Bytes {
        Bytes::from(text)
    }

    /// Runs a SCRAM exchange as `username` with `password`; the server's
    /// first message, then the result of the last step.
    async fn scram_login(
        auth: &ScramAuth<HashMap<String, StoredCredentials>>,
        username: &str,
        password: &str,
    ) -> (String, AuthResult, mercurio_scram::ScramClient) {
        let mut client = mercurio_scram::ScramClient::new(username, password);
        let mut exchange = auth.begin_auth(&self::client(), "SCRAM-SHA-256").unwrap();
        let first = data(client.start().unwrap());
        let AuthResult::Continue(server_first) = exchange.step(Some(&first)).await else {
            panic!("expected the server's first message");
        };
        let server_first = String::from_utf8(server_first.to_vec()).unwrap();
        let last = data(client.respond(server_first.as_bytes()).unwrap());
        (server_first, exchange.step(Some(&last)).await, client)
    }

    #[tokio::test]
    async fn test_scram_success_carries_the_server_signature() {
        let auth = scram_auth();
        let (_, result, mut client) = scram_login(&auth, "user", "pencil").await;
        let AuthResult::Success {
            username,
            data: Some(server_final),
        } = result
        else {
            panic!("expected success with data, got {result:?}");
        };
        assert_eq!(username.as_deref(), Some("user"));
        client.finish(&server_final).unwrap();
    }

    #[tokio::test]
    async fn test_scram_wrong_password_and_unknown_user_fail_alike() {
        let auth = scram_auth();
        let (known, wrong, _) = scram_login(&auth, "user", "wrong").await;
        assert_eq!(wrong, AuthResult::Failed(ReasonCode::BadUserNameOrPassword));

        let (unknown, result, _) = scram_login(&auth, "nobody", "pencil").await;
        assert_eq!(
            result,
            AuthResult::Failed(ReasonCode::BadUserNameOrPassword)
        );
        // Both answers have the same shape, and an unknown user always
        // gets the same salt
        let shape = |m: &str| {
            m.split(',')
                .map(|a| (a.len(), a.chars().next()))
                .skip(1)
                .collect::<Vec<_>>()
        };
        assert_eq!(shape(&known), shape(&unknown));
        let (again, _, _) = scram_login(&auth, "nobody", "other").await;
        let salt = |m: &str| m.split(',').nth(1).unwrap().to_string();
        assert_eq!(salt(&unknown), salt(&again));
        assert_ne!(salt(&unknown), salt(&known));
    }

    #[tokio::test]
    async fn test_scram_refusals_map_to_reason_codes() {
        let auth = scram_auth();
        let step = |message: Option<&'static str>| {
            let mut exchange = auth.begin_auth(&client(), "SCRAM-SHA-256").unwrap();
            async move { exchange.step(message.map(Bytes::from).as_ref()).await }
        };
        assert_eq!(
            step(None).await,
            AuthResult::Failed(ReasonCode::BadAuthenticationMethod)
        );
        assert_eq!(
            step(Some("p=tls-unique,,n=user,r=abc")).await,
            AuthResult::Failed(ReasonCode::BadAuthenticationMethod)
        );
        assert_eq!(
            step(Some("garbage")).await,
            AuthResult::Failed(ReasonCode::MalformedPacket)
        );
        assert_eq!(
            step(Some("n,a=admin,n=user,r=abc")).await,
            AuthResult::Failed(ReasonCode::NotAuthorized)
        );
        assert!(auth.begin_auth(&client(), "PLAIN").is_none());
    }

    #[tokio::test]
    async fn test_scram_exchange_ends_after_the_proof() {
        let auth = scram_auth();
        let mut client = mercurio_scram::ScramClient::new("user", "pencil");
        let mut exchange = auth.begin_auth(&self::client(), "SCRAM-SHA-256").unwrap();
        let first = data(client.start().unwrap());
        let AuthResult::Continue(server_first) = exchange.step(Some(&first)).await else {
            panic!("expected the server's first message");
        };
        assert_eq!(
            exchange.step(None).await,
            AuthResult::Failed(ReasonCode::MalformedPacket)
        );
        let last = data(client.respond(&server_first).unwrap());
        assert_eq!(
            exchange.step(Some(&last)).await,
            AuthResult::Failed(ReasonCode::ProtocolError)
        );
    }

    #[test]
    fn test_scram_status() {
        assert_eq!(
            scram_auth().status(),
            PluginStatus::new("scram-sha-256", PluginKind::Builtin).with_hooks(&["begin_auth"])
        );
    }
}
