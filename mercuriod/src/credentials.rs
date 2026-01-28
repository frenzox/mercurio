//! Credential validation with password hashing support.

use std::collections::HashMap;

use argon2::{Argon2, PasswordHash, PasswordVerifier};
use mercurio_server::auth::CredentialValidator;

/// Credential validator that supports argon2 password hashes.
///
/// The password file stores entries as `username:hash` where hash is an
/// argon2id PHC string. Plaintext passwords (without `$` prefix) are also
/// supported for backwards compatibility.
pub struct HashedCredentialValidator {
    /// Map of username -> password hash (or plaintext).
    credentials: HashMap<String, String>,
}

impl HashedCredentialValidator {
    pub fn new(credentials: HashMap<String, String>) -> Self {
        Self { credentials }
    }
}

impl CredentialValidator for HashedCredentialValidator {
    fn validate(&self, username: &str, password: &[u8]) -> bool {
        let Some(stored) = self.credentials.get(username) else {
            return false;
        };

        if stored.starts_with('$') {
            // Argon2 hashed password
            let Ok(parsed_hash) = PasswordHash::new(stored) else {
                return false;
            };
            Argon2::default()
                .verify_password(password, &parsed_hash)
                .is_ok()
        } else {
            // Plaintext fallback for backwards compatibility
            let password_str = match std::str::from_utf8(password) {
                Ok(s) => s,
                Err(_) => return false,
            };
            stored == password_str
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use argon2::password_hash::SaltString;
    use argon2::PasswordHasher;

    fn hash_password(password: &str) -> String {
        let salt = SaltString::generate(&mut argon2::password_hash::rand_core::OsRng);
        Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .unwrap()
            .to_string()
    }

    #[test]
    fn test_hashed_credential_valid() {
        let mut creds = HashMap::new();
        creds.insert("admin".to_string(), hash_password("secret"));
        let validator = HashedCredentialValidator::new(creds);

        assert!(validator.validate("admin", b"secret"));
    }

    #[test]
    fn test_hashed_credential_invalid() {
        let mut creds = HashMap::new();
        creds.insert("admin".to_string(), hash_password("secret"));
        let validator = HashedCredentialValidator::new(creds);

        assert!(!validator.validate("admin", b"wrong"));
    }

    #[test]
    fn test_plaintext_fallback() {
        let mut creds = HashMap::new();
        creds.insert("legacy".to_string(), "plainpass".to_string());
        let validator = HashedCredentialValidator::new(creds);

        assert!(validator.validate("legacy", b"plainpass"));
        assert!(!validator.validate("legacy", b"wrong"));
    }

    #[test]
    fn test_unknown_user() {
        let creds = HashMap::new();
        let validator = HashedCredentialValidator::new(creds);

        assert!(!validator.validate("nobody", b"pass"));
    }
}
