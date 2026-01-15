//! Authentication module for MQTT 5.0 enhanced authentication.
//!
//! This module provides an extensible authentication framework supporting
//! challenge-response authentication flows as defined in MQTT 5.0.

use std::collections::HashMap;

use bytes::Bytes;

use mercurio_core::reason::ReasonCode;

/// Result of an authentication step.
#[derive(Debug, Clone)]
pub enum AuthResult {
    /// Authentication completed successfully.
    Success,
    /// Authentication requires another round - send this challenge to client.
    Continue(Bytes),
    /// Authentication failed with the given reason.
    Failed(ReasonCode),
}

/// Trait for implementing authentication methods.
///
/// Each authentication method (e.g., PLAIN, SCRAM-SHA-256) implements this trait
/// to provide its specific authentication logic.
pub trait AuthMethod: Send + Sync {
    /// Returns the authentication method name (e.g., "PLAIN", "SCRAM-SHA-256").
    fn name(&self) -> &str;

    /// Start authentication, optionally processing initial client data.
    ///
    /// Returns the result of the initial authentication step, which may be:
    /// - `Success` if authentication completed (e.g., PLAIN with valid credentials)
    /// - `Continue` with a challenge for multi-step auth methods
    /// - `Failed` if initial data is invalid
    fn auth_start(&self, initial_data: Option<&Bytes>) -> AuthResult;

    /// Continue authentication with client response data.
    ///
    /// Called for each subsequent AUTH packet in a multi-step authentication.
    fn auth_continue(&self, response_data: &Bytes) -> AuthResult;
}

/// PLAIN authentication method (RFC 4616).
///
/// Expects authentication data in the format: `\0username\0password`
/// where the first byte is NUL (authorization identity is empty).
pub struct PlainAuth {
    /// Map of username -> password for credential validation.
    credentials: HashMap<String, String>,
}

impl PlainAuth {
    /// Create a new PLAIN authenticator with the given credentials.
    pub fn new(credentials: HashMap<String, String>) -> Self {
        PlainAuth { credentials }
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
}

impl AuthMethod for PlainAuth {
    fn name(&self) -> &str {
        "PLAIN"
    }

    fn auth_start(&self, initial_data: Option<&Bytes>) -> AuthResult {
        let Some(data) = initial_data else {
            // PLAIN requires data in the initial request
            return AuthResult::Failed(ReasonCode::BadAuthenticationMethod);
        };

        let Some((username, password)) = Self::parse_plain_data(data) else {
            return AuthResult::Failed(ReasonCode::MalformedPacket);
        };

        // Validate credentials
        match self.credentials.get(&username) {
            Some(stored_password) if stored_password == &password => AuthResult::Success,
            _ => AuthResult::Failed(ReasonCode::BadUserNameOrPassword),
        }
    }

    fn auth_continue(&self, _response_data: &Bytes) -> AuthResult {
        // PLAIN is single-step, should never reach here
        AuthResult::Failed(ReasonCode::ProtocolError)
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

    /// Start authentication using the specified method.
    pub fn start_auth(&self, method_name: &str, initial_data: Option<&Bytes>) -> AuthResult {
        match self.methods.get(method_name) {
            Some(method) => method.auth_start(initial_data),
            None => AuthResult::Failed(ReasonCode::BadAuthenticationMethod),
        }
    }

    /// Continue an ongoing authentication.
    pub fn continue_auth(&self, method_name: &str, response_data: &Bytes) -> AuthResult {
        match self.methods.get(method_name) {
            Some(method) => method.auth_continue(response_data),
            None => AuthResult::Failed(ReasonCode::BadAuthenticationMethod),
        }
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
        match auth.auth_start(Some(&data)) {
            AuthResult::Success => {}
            other => panic!("Expected Success, got {:?}", other),
        }
    }

    #[test]
    fn test_plain_auth_wrong_password() {
        let mut credentials = HashMap::new();
        credentials.insert("user".to_string(), "pass".to_string());
        let auth = PlainAuth::new(credentials);

        let data = create_plain_data("user", "wrongpass");
        match auth.auth_start(Some(&data)) {
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
        match auth.auth_start(Some(&data)) {
            AuthResult::Failed(ReasonCode::BadUserNameOrPassword) => {}
            other => panic!("Expected BadUserNameOrPassword, got {:?}", other),
        }
    }

    #[test]
    fn test_plain_auth_no_data() {
        let credentials = HashMap::new();
        let auth = PlainAuth::new(credentials);

        match auth.auth_start(None) {
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
        match auth.auth_start(Some(&data)) {
            AuthResult::Failed(ReasonCode::MalformedPacket) => {}
            other => panic!("Expected MalformedPacket, got {:?}", other),
        }
    }

    #[test]
    fn test_auth_manager_unsupported_method() {
        let manager = AuthManager::new();

        match manager.start_auth("UNKNOWN", None) {
            AuthResult::Failed(ReasonCode::BadAuthenticationMethod) => {}
            other => panic!("Expected BadAuthenticationMethod, got {:?}", other),
        }
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
        match manager.start_auth("PLAIN", Some(&data)) {
            AuthResult::Success => {}
            other => panic!("Expected Success, got {:?}", other),
        }
    }
}
