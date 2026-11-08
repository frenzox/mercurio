//! Built-in authentication plugins: PLAIN enhanced authentication and an
//! in-memory user name and password check.

use std::{collections::HashMap, sync::Arc};

use bytes::Bytes;

use mercurio_core::reason::ReasonCode;

use crate::plugin::{
    ready, AuthExchange, AuthResult, BoxFuture, ClientInfo, Credentials, Decision, Plugin,
};

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

impl Plugin for PlainAuth {
    fn name(&self) -> &str {
        "plain"
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

        match self.credentials.get(&username) {
            Some(stored_password) if stored_password == &password => AuthResult::Success {
                username: Some(username),
            },
            _ => AuthResult::Failed(ReasonCode::BadUserNameOrPassword),
        }
    }
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

impl Plugin for SimpleCredentialValidator {
    fn name(&self) -> &str {
        "credentials"
    }

    fn authenticate<'a>(
        &'a self,
        _: &'a ClientInfo,
        credentials: &'a Credentials<'a>,
    ) -> BoxFuture<'a, Decision> {
        let valid = std::str::from_utf8(credentials.password).is_ok_and(|password| {
            self.credentials
                .get(credentials.username)
                .is_some_and(|stored| stored == password)
        });
        ready(if valid {
            Decision::Allow
        } else {
            Decision::Deny
        })
    }
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
                username: Some("user".into())
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
    async fn test_credential_validator_unknown_user() {
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
            Decision::Deny
        );
    }
}
