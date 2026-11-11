//! Credential validation with password hashing support.

use std::collections::HashMap;

use argon2::{Argon2, PasswordHash, PasswordVerifier};
use mercurio_server::plugin::{
    BoxFuture, ClientInfo, Credentials, Decision, Plugin, PluginKind, PluginStatus,
};

/// Credential validator for the password file's `username:hash` entries,
/// where the hash is an Argon2 PHC string, as `mercuriod passwd` writes, or
/// one of `mosquitto_passwd`'s, see [`check_hash`].
pub struct HashedCredentialValidator {
    /// Map of username -> password hash.
    credentials: HashMap<String, String>,
}

impl HashedCredentialValidator {
    pub fn new(credentials: HashMap<String, String>) -> Self {
        Self { credentials }
    }
}

impl HashedCredentialValidator {
    fn check(&self, username: &str, password: &[u8]) -> bool {
        let Some(stored) = self.credentials.get(username) else {
            // As slow as a wrong password, so timing doesn't tell which
            // users exist
            let _ = Argon2::default().verify_password(password, &unknown_user_hash());
            return false;
        };

        verify(stored, password)
    }
}

/// Whether `password` matches `stored`, a hash of one of the forms
/// [`check_hash`] accepts; never for any other.
pub(crate) fn verify(stored: &str, password: &[u8]) -> bool {
    match Hash::parse(stored) {
        Ok(Hash::Argon2(hash)) => Argon2::default().verify_password(password, &hash).is_ok(),
        Ok(Hash::Mosquitto {
            iterations,
            salt,
            hash,
        }) => mosquitto_hash_matches(iterations, &salt, &hash, password),
        Err(_) => false,
    }
}

/// A password hash of the formats the password file may hold.
enum Hash<'a> {
    Argon2(PasswordHash<'a>),
    /// Written by `mosquitto_passwd`: PBKDF2-HMAC-SHA512 with `iterations`,
    /// its default since 2.0, or one SHA-512 of the password and salt
    /// without, the default before
    Mosquitto {
        iterations: Option<u32>,
        salt: Vec<u8>,
        hash: Vec<u8>,
    },
}

impl<'a> Hash<'a> {
    /// Parses `$argon2id$...` (or `argon2i`, `argon2d`) PHC strings,
    /// `$7$<iterations>$<salt>$<hash>` and `$6$<salt>$<hash>`, whose salts
    /// and hashes are base64. The error does not repeat `stored`.
    fn parse(stored: &'a str) -> Result<Hash<'a>, &'static str> {
        use base64::{engine::general_purpose::STANDARD, Engine};

        let fields: Vec<&str> = stored.split('$').collect();
        let (iterations, salt, hash) = match fields[..] {
            ["", "7", iterations, salt, hash] => match iterations.parse() {
                Ok(iterations) if iterations > 0 => (Some(iterations), salt, hash),
                _ => return Err("the iteration count of a `$7$` hash is not a positive number"),
            },
            ["", "7", ..] => {
                return Err("a `$7$` hash has the form `$7$<iterations>$<salt>$<hash>`")
            }
            ["", "6", salt, hash] => (None, salt, hash),
            ["", "6", ..] => return Err("a `$6$` hash has the form `$6$<salt>$<hash>`"),
            ["", algorithm, ..] if algorithm.starts_with("argon2") => {
                return match PasswordHash::new(stored) {
                    Ok(hash) if hash.salt.is_some() && hash.hash.is_some() => {
                        Ok(Hash::Argon2(hash))
                    }
                    _ => Err("the Argon2 hash is malformed"),
                }
            }
            _ => {
                return Err(
                    "not a hash `mercuriod passwd` or `mosquitto_passwd` writes; \
                     plain-text passwords are not accepted",
                )
            }
        };
        match (STANDARD.decode(salt), STANDARD.decode(hash)) {
            (Ok(salt), Ok(hash)) if !hash.is_empty() => Ok(Hash::Mosquitto {
                iterations,
                salt,
                hash,
            }),
            _ => Err("the salt or hash of a mosquitto hash is not base64"),
        }
    }
}

/// Refuses a password file hash no password can match, such as a
/// plain-text password or an empty field.
pub fn check_hash(stored: &str) -> Result<(), &'static str> {
    Hash::parse(stored).map(drop)
}

impl Plugin for HashedCredentialValidator {
    fn name(&self) -> &str {
        "password-file"
    }

    fn status(&self) -> PluginStatus {
        PluginStatus::new(self.name(), PluginKind::Builtin).with_hooks(&["authenticate"])
    }

    fn authenticate<'a>(
        &'a self,
        _: &'a ClientInfo,
        credentials: &'a Credentials<'a>,
    ) -> BoxFuture<'a, Decision> {
        // Hashes take tens of milliseconds of CPU; keep them off the runtime
        let (username, password) = (
            credentials.username.to_string(),
            credentials.password.to_vec(),
        );
        Box::pin(async move {
            let valid = tokio::task::block_in_place(|| self.check(&username, &password));
            if valid {
                Decision::Allow
            } else {
                Decision::Deny
            }
        })
    }
}

/// Checks a password against a hash written by `mosquitto_passwd`.
fn mosquitto_hash_matches(
    iterations: Option<u32>,
    salt: &[u8],
    expected: &[u8],
    password: &[u8],
) -> bool {
    use sha2::{Digest, Sha512};

    let hash: Vec<u8> = match iterations {
        Some(iterations) => {
            let mut hash = [0u8; 64];
            pbkdf2::pbkdf2_hmac::<Sha512>(password, salt, iterations, &mut hash);
            hash.to_vec()
        }
        None => Sha512::new()
            .chain_update(password)
            .chain_update(salt)
            .finalize()
            .to_vec(),
    };
    constant_time_eq(&hash, expected)
}

/// Compares in time independent of where the two differ, so timing tells
/// nothing about the secret.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |d, (a, b)| d | (a ^ b)) == 0
}

/// An argon2 hash of a password no one has, checked for unknown users.
pub(crate) fn unknown_user_hash() -> PasswordHash<'static> {
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
        assert!(validator.check("pbkdf2", b"secret"));
        assert!(!validator.check("pbkdf2", b"wrong"));
        assert!(validator.check("sha512", b"secret"));
        assert!(!validator.check("sha512", b"wrong"));
    }

    #[test]
    fn test_hashed_credential_valid() {
        let mut creds = HashMap::new();
        creds.insert("admin".to_string(), hash_password("secret"));
        let validator = HashedCredentialValidator::new(creds);

        assert!(validator.check("admin", b"secret"));
    }

    #[test]
    fn test_hashed_credential_invalid() {
        let mut creds = HashMap::new();
        creds.insert("admin".to_string(), hash_password("secret"));
        let validator = HashedCredentialValidator::new(creds);

        assert!(!validator.check("admin", b"wrong"));
    }

    #[test]
    fn test_plaintext_is_not_a_password() {
        let mut creds = HashMap::new();
        creds.insert("legacy".to_string(), "plainpass".to_string());
        creds.insert("truncated".to_string(), String::new());
        let validator = HashedCredentialValidator::new(creds);

        assert!(!validator.check("legacy", b"plainpass"));
        assert!(!validator.check("truncated", b""));
        assert!(check_hash("plainpass").is_err());
        assert!(check_hash("").is_err());
    }

    #[test]
    fn test_unknown_user() {
        let creds = HashMap::new();
        let validator = HashedCredentialValidator::new(creds);

        assert!(!validator.check("nobody", b"pass"));
    }

    #[test]
    fn test_status_is_builtin() {
        use mercurio_server::plugin::{Plugin, PluginKind, PluginStatus};

        let validator = HashedCredentialValidator::new(HashMap::new());
        assert_eq!(
            validator.status(),
            PluginStatus::new("password-file", PluginKind::Builtin).with_hooks(&["authenticate"])
        );
    }
}
