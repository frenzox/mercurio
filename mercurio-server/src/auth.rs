//! Authentication module for MQTT 5.0 enhanced authentication.
//!
//! This module provides an extensible authentication framework supporting
//! challenge-response authentication flows as defined in MQTT 5.0.

use std::{collections::HashMap, sync::Arc};

use bytes::Bytes;

use mercurio_core::reason::ReasonCode;

/// Result of an authentication step.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AuthResult {
    /// The client is authenticated, as `username` if the method identifies
    /// users.
    Success { username: Option<String> },
    /// Authentication requires another round - send this challenge to client.
    Continue(Bytes),
    /// Authentication failed with the given reason.
    Failed(ReasonCode),
}

/// An enhanced authentication method (e.g., PLAIN, SCRAM-SHA-256).
pub trait AuthMethod: Send + Sync {
    /// Returns the authentication method name (e.g., "PLAIN", "SCRAM-SHA-256").
    fn name(&self) -> &str;

    /// Begins authenticating one client.
    fn begin(&self) -> Box<dyn AuthExchange>;
}

/// One client's authentication, carried out step by step. It keeps whatever
/// the method needs between steps, such as the nonce of a challenge.
pub trait AuthExchange: Send {
    /// Processes the data the client sent in CONNECT or AUTH, if any.
    fn step(&mut self, data: Option<&Bytes>) -> AuthResult;
}

/// PLAIN authentication method (RFC 4616).
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

impl AuthMethod for PlainAuth {
    fn name(&self) -> &str {
        "PLAIN"
    }

    fn begin(&self) -> Box<dyn AuthExchange> {
        Box::new(PlainExchange {
            credentials: Arc::clone(&self.credentials),
            done: false,
        })
    }
}

struct PlainExchange {
    credentials: Arc<HashMap<String, String>>,
    done: bool,
}

impl AuthExchange for PlainExchange {
    fn step(&mut self, data: Option<&Bytes>) -> AuthResult {
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

        match self.credentials.get(&username) {
            Some(stored_password) if stored_password == &password => AuthResult::Success {
                username: Some(username),
            },
            _ => AuthResult::Failed(ReasonCode::BadUserNameOrPassword),
        }
    }
}

/// Trait for validating username/password credentials from CONNECT packets.
///
/// This is separate from the MQTT 5.0 Enhanced Authentication (AUTH packets).
/// It validates the simple username/password fields in the CONNECT packet.
pub trait CredentialValidator: Send + Sync {
    /// Validate a username/password pair.
    /// Returns true if the credentials are valid.
    fn validate(&self, username: &str, password: &[u8]) -> bool;
}

/// Simple in-memory credential validator using a username -> password map.
pub struct SimpleCredentialValidator {
    credentials: HashMap<String, String>,
}

impl SimpleCredentialValidator {
    /// Create a new validator with the given credentials.
    pub fn new(credentials: HashMap<String, String>) -> Self {
        Self { credentials }
    }
}

impl CredentialValidator for SimpleCredentialValidator {
    fn validate(&self, username: &str, password: &[u8]) -> bool {
        let password_str = match std::str::from_utf8(password) {
            Ok(s) => s,
            Err(_) => return false,
        };
        self.credentials
            .get(username)
            .map(|stored| stored == password_str)
            .unwrap_or(false)
    }
}

/// Manages authentication methods and ongoing authentication sessions.
pub struct AuthManager {
    /// Available authentication methods by name.
    methods: HashMap<String, Box<dyn AuthMethod>>,
}

impl Default for AuthManager {
    fn default() -> Self {
        Self::new()
    }
}

impl AuthManager {
    /// Create a new AuthManager with no authentication methods.
    pub fn new() -> Self {
        AuthManager {
            methods: HashMap::new(),
        }
    }

    /// Register an authentication method.
    pub fn register(&mut self, method: Box<dyn AuthMethod>) {
        self.methods.insert(method.name().to_string(), method);
    }

    /// Check if an authentication method is supported.
    pub fn supports_method(&self, method_name: &str) -> bool {
        self.methods.contains_key(method_name)
    }

    /// Get the list of supported authentication method names.
    pub fn supported_methods(&self) -> Vec<&str> {
        self.methods.keys().map(|s| s.as_str()).collect()
    }

    /// Begins authenticating a client with `method_name`, if supported.
    pub fn begin(&self, method_name: &str) -> Option<Box<dyn AuthExchange>> {
        self.methods.get(method_name).map(|method| method.begin())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_plain_data(username: &str, password: &str) -> Bytes {
        let mut data = Vec::new();
        data.push(0); // Empty authorization identity
        data.extend_from_slice(username.as_bytes());
        data.push(0); // Separator
        data.extend_from_slice(password.as_bytes());
        Bytes::from(data)
    }

    #[test]
    fn test_plain_auth_success() {
        let mut credentials = HashMap::new();
        credentials.insert("user".to_string(), "pass".to_string());
        let auth = PlainAuth::new(credentials);

        let data = create_plain_data("user", "pass");
        assert_eq!(
            auth.begin().step(Some(&data)),
            AuthResult::Success {
                username: Some("user".into())
            }
        );
    }

    #[test]
    fn test_plain_auth_wrong_password() {
        let mut credentials = HashMap::new();
        credentials.insert("user".to_string(), "pass".to_string());
        let auth = PlainAuth::new(credentials);

        let data = create_plain_data("user", "wrongpass");
        match auth.begin().step(Some(&data)) {
            AuthResult::Failed(ReasonCode::BadUserNameOrPassword) => {}
            other => panic!("Expected BadUserNameOrPassword, got {:?}", other),
        }
    }

    #[test]
    fn test_plain_auth_unknown_user() {
        let mut credentials = HashMap::new();
        credentials.insert("user".to_string(), "pass".to_string());
        let auth = PlainAuth::new(credentials);

        let data = create_plain_data("unknown", "pass");
        match auth.begin().step(Some(&data)) {
            AuthResult::Failed(ReasonCode::BadUserNameOrPassword) => {}
            other => panic!("Expected BadUserNameOrPassword, got {:?}", other),
        }
    }

    #[test]
    fn test_plain_auth_no_data() {
        let credentials = HashMap::new();
        let auth = PlainAuth::new(credentials);

        match auth.begin().step(None) {
            AuthResult::Failed(ReasonCode::BadAuthenticationMethod) => {}
            other => panic!("Expected BadAuthenticationMethod, got {:?}", other),
        }
    }

    #[test]
    fn test_plain_auth_malformed_data() {
        let credentials = HashMap::new();
        let auth = PlainAuth::new(credentials);

        // Missing initial NUL
        let data = Bytes::from("user\0pass");
        match auth.begin().step(Some(&data)) {
            AuthResult::Failed(ReasonCode::MalformedPacket) => {}
            other => panic!("Expected MalformedPacket, got {:?}", other),
        }
    }

    #[test]
    fn test_auth_manager_unsupported_method() {
        let manager = AuthManager::new();

        assert!(manager.begin("UNKNOWN").is_none());
    }

    #[test]
    fn test_auth_manager_with_plain() {
        let mut manager = AuthManager::new();
        let mut credentials = HashMap::new();
        credentials.insert("admin".to_string(), "secret".to_string());
        manager.register(Box::new(PlainAuth::new(credentials)));

        assert!(manager.supports_method("PLAIN"));
        assert!(!manager.supports_method("SCRAM-SHA-256"));

        let data = create_plain_data("admin", "secret");
        let mut exchange = manager.begin("PLAIN").unwrap();
        assert!(matches!(
            exchange.step(Some(&data)),
            AuthResult::Success { .. }
        ));
        // A finished exchange accepts no more steps
        assert_eq!(
            exchange.step(Some(&data)),
            AuthResult::Failed(ReasonCode::ProtocolError)
        );
    }

    #[test]
    fn test_credential_validator_success() {
        let mut creds = HashMap::new();
        creds.insert("user".to_string(), "pass".to_string());
        let validator = SimpleCredentialValidator::new(creds);

        assert!(validator.validate("user", b"pass"));
    }

    #[test]
    fn test_credential_validator_wrong_password() {
        let mut creds = HashMap::new();
        creds.insert("user".to_string(), "pass".to_string());
        let validator = SimpleCredentialValidator::new(creds);

        assert!(!validator.validate("user", b"wrong"));
    }

    #[test]
    fn test_credential_validator_unknown_user() {
        let mut creds = HashMap::new();
        creds.insert("user".to_string(), "pass".to_string());
        let validator = SimpleCredentialValidator::new(creds);

        assert!(!validator.validate("unknown", b"pass"));
    }
}
