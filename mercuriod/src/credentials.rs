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
            // As slow as a wrong password, so timing doesn't tell which
            // users exist
            let _ = Argon2::default().verify_password(password, &unknown_user_hash());
            return false;
        };

        if stored.starts_with("$6$") || stored.starts_with("$7$") {
            mosquitto_hash_matches(stored, password)
        } else if stored.starts_with('$') {
            // Argon2 hashed password
            let Ok(parsed_hash) = PasswordHash::new(stored) else {
                return false;
            };
            Argon2::default()
                .verify_password(password, &parsed_hash)
                .is_ok()
        } else {
            // Plaintext fallback for backwards compatibility
            constant_time_eq(stored.as_bytes(), password)
        }
    }
}

/// Checks a password against a hash written by `mosquitto_passwd`:
/// `$7$<iterations>$<salt>$<hash>` for PBKDF2-HMAC-SHA512, its default since
/// 2.0, or `$6$<salt>$<hash>` for one SHA-512 of the password and salt, the
/// default before. Salts and hashes are base64.
fn mosquitto_hash_matches(stored: &str, password: &[u8]) -> bool {
    use base64::{engine::general_purpose::STANDARD, Engine};
    use sha2::{Digest, Sha512};

    let fields: Vec<&str> = stored.split('$').collect();
    let (iterations, salt, expected) = match fields[..] {
        ["", "7", iterations, salt, hash] => match iterations.parse() {
            Ok(iterations) => (Some(iterations), salt, hash),
            Err(_) => return false,
        },
        ["", "6", salt, hash] => (None, salt, hash),
        _ => return false,
    };
    let (Ok(salt), Ok(expected)) = (STANDARD.decode(salt), STANDARD.decode(expected)) else {
        return false;
    };

    let hash: Vec<u8> = match iterations {
        Some(iterations) => {
            let mut hash = [0u8; 64];
            pbkdf2::pbkdf2_hmac::<Sha512>(password, &salt, iterations, &mut hash);
            hash.to_vec()
        }
        None => Sha512::new()
            .chain_update(password)
            .chain_update(&salt)
            .finalize()
            .to_vec(),
    };
    constant_time_eq(&hash, &expected)
}

/// Compares in time independent of where the two differ, so timing tells
/// nothing about the secret.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |d, (a, b)| d | (a ^ b)) == 0
}

/// An argon2 hash of a password no one has, checked for unknown users.
fn unknown_user_hash() -> PasswordHash<'static> {
    static HASH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let hash = HASH.get_or_init(|| {
        use argon2::{password_hash::SaltString, PasswordHasher};
        let salt = SaltString::generate(&mut argon2::password_hash::rand_core::OsRng);
        Argon2::default()
            .hash_password(b"unknown user", &salt)
            .expect("argon2 hashes with its default parameters")
            .to_string()
    });
    PasswordHash::new(hash).expect("a hash argon2 wrote parses")
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
    fn test_mosquitto_passwd_hashes() {
        let mut creds = HashMap::new();
        // Written by mosquitto_passwd 2.0.18, password "secret"
        creds.insert(
            "pbkdf2".to_string(),
            "$7$101$lT8RRynk8Dr5uL1b$1FhG/H49+FnZ9qx8YFDOUZOiE+Yd+yOKjQ9AzfsTHwepsiXOTbzCSojC7kA3U/VicSJdksyipSowQpvFhpo0iQ==".to_string(),
        );
        // Written by mosquitto_passwd -H sha512, password "secret"
        creds.insert(
            "sha512".to_string(),
            "$6$pZufTypp2a6RmJv0$0E3Uw01KVS5L6/s+8dWtsoMjdF9mnjUcloZJLTWqsIQbvp1b3crPcunceFO+VmiVpj+Wde8NRnyJs4ZAFgaQUA==".to_string(),
        );
        let validator = HashedCredentialValidator::new(creds);
        assert!(validator.validate("pbkdf2", b"secret"));
        assert!(!validator.validate("pbkdf2", b"wrong"));
        assert!(validator.validate("sha512", b"secret"));
        assert!(!validator.validate("sha512", b"wrong"));
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
