//! Credential validation with password hashing support.

use std::collections::HashMap;

use argon2::{Argon2, PasswordHash, PasswordVerifier};
use mercurio_server::auth::{ScramCredentialStore, StoredCredentials};
use mercurio_server::plugin::{
    ready, BoxFuture, ClientInfo, Credentials, Decision, Plugin, PluginKind, PluginStatus,
};

/// Credential validator for the password file's `username:hash` entries,
/// where the hash is an Argon2 PHC string, as `mercuriod passwd` writes, or
/// one of `mosquitto_passwd`'s, see [`check_hash`], and the SCRAM-SHA-256
/// credentials set along with some of them.
pub struct HashedCredentialValidator {
    /// Map of username -> password hash.
    credentials: HashMap<String, String>,
    scram: HashMap<String, StoredCredentials>,
}

impl HashedCredentialValidator {
    pub fn new(credentials: HashMap<String, String>) -> Self {
        Self {
            credentials,
            scram: HashMap::new(),
        }
    }

    /// With these users' SCRAM-SHA-256 credentials.
    pub fn with_scram(mut self, scram: HashMap<String, StoredCredentials>) -> Self {
        self.scram = scram;
        self
    }
}

impl ScramCredentialStore for HashedCredentialValidator {
    fn scram_credentials<'a>(
        &'a self,
        _: &'a ClientInfo,
        username: &'a str,
    ) -> BoxFuture<'a, Option<StoredCredentials>> {
        ready(self.scram.get(username).cloned())
    }
}

/// A new password's Argon2 hash, for user name and password logins, and
/// its SCRAM-SHA-256 credentials, as [`scram_entry`] stores them.
pub fn hash_new_password(password: &str) -> Result<(String, String), String> {
    use argon2::PasswordHasher;

    let hash = Argon2::default()
        .hash_password(password.as_bytes())
        .map_err(|e| format!("Failed to hash password: {e}"))?
        .to_string();
    let scram = StoredCredentials::generate(password, mercurio_scram::DEFAULT_ITERATIONS)
        .map_err(|e| format!("Failed to derive SCRAM credentials: {e}"))?;
    let entry = scram_entry(&hash, &scram);
    Ok((hash, entry))
}

/// What ties SCRAM credentials to the password hash set with them: a
/// digest of the hash. A tool that changes the hash and not the SCRAM
/// credentials, such as `mosquitto_passwd` or an older `mercuriod`, leaves
/// a digest that no longer matches, so the old password stops working.
fn scram_binding(hash: &str) -> String {
    use base64::{engine::general_purpose::STANDARD, Engine};
    use sha2::{Digest, Sha256};

    STANDARD.encode(Sha256::digest(hash.as_bytes()))
}

/// `credentials` stored next to the password `hash`:
/// `<binding>:SCRAM-SHA-256$<iterations>:<salt>$<StoredKey>:<ServerKey>`,
/// the binding being [`scram_binding`] and the rest RFC 5803's form.
pub fn scram_entry(hash: &str, credentials: &StoredCredentials) -> String {
    format!("{}:{credentials}", scram_binding(hash))
}

/// The SCRAM credentials of `entry`, or None when they were set with
/// another password than `hash`'s. The error does not repeat `entry`.
pub fn scram_credentials(hash: &str, entry: &str) -> Result<Option<StoredCredentials>, String> {
    let (binding, credentials) = entry
        .split_once(':')
        .ok_or("SCRAM credentials have the form `<binding>:SCRAM-SHA-256$...`")?;
    let credentials: StoredCredentials = credentials.parse().map_err(|e| format!("{e}"))?;
    Ok((binding == scram_binding(hash)).then_some(credentials))
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
        Ok(Hash::Argon2(hash)) => Argon2::default().verify_password(password, &*hash).is_ok(),
        Ok(Hash::Mosquitto {
            iterations,
            salt,
            hash,
        }) => mosquitto_hash_matches(iterations, &salt, &hash, password),
        Err(_) => false,
    }
}

/// A password hash of the formats the password file may hold.
enum Hash {
    Argon2(Box<PasswordHash>),
    /// Written by `mosquitto_passwd`: PBKDF2-HMAC-SHA512 with `iterations`,
    /// its default since 2.0, or one SHA-512 of the password and salt
    /// without, the default before
    Mosquitto {
        iterations: Option<u32>,
        salt: Vec<u8>,
        hash: Vec<u8>,
    },
}

impl Hash {
    /// Parses `$argon2id$...` (or `argon2i`, `argon2d`) PHC strings,
    /// `$7$<iterations>$<salt>$<hash>` and `$6$<salt>$<hash>`, whose salts
    /// and hashes are base64. The error does not repeat `stored`.
    fn parse(stored: &str) -> Result<Hash, &'static str> {
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
                        Ok(Hash::Argon2(Box::new(hash)))
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
pub(crate) fn unknown_user_hash() -> PasswordHash {
    static HASH: std::sync::OnceLock<PasswordHash> = std::sync::OnceLock::new();
    HASH.get_or_init(|| {
        use argon2::PasswordHasher;
        Argon2::default()
            .hash_password(b"unknown user")
            .expect("argon2 hashes with its default parameters")
    })
    .clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use argon2::PasswordHasher;

    fn hash_password(password: &str) -> String {
        Argon2::default()
            .hash_password(password.as_bytes())
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
    fn test_hashes_written_by_argon2_0_5_still_verify() {
        // Written by `mercuriod passwd` with argon2 0.5
        let stored = "$argon2id$v=19$m=19456,t=2,p=1$YKsXpDe9ltkVh711Fl4iog$ydtCozrDCyoytDHGftjr2OpfsgsPHDjY8g9KtBTM4Yc";
        assert!(check_hash(stored).is_ok());
        assert!(verify(stored, b"stored before argon2 0.6"));
        assert!(!verify(stored, b"stored before argon2 0.7"));
    }

    /// The parameters of a PHC string: everything before the salt.
    fn parameters(hash: &str) -> &str {
        let salt = hash
            .rfind('$')
            .and_then(|end| hash[..end].rfind('$'))
            .unwrap();
        &hash[..salt]
    }

    #[test]
    fn test_new_hashes_keep_their_parameters() {
        let (hash, _) = hash_new_password("secret").unwrap();
        assert_eq!(parameters(&hash), "$argon2id$v=19$m=19456,t=2,p=1");
    }

    #[test]
    fn test_unknown_users_cost_as_much_as_known_ones() {
        let (hash, _) = hash_new_password("secret").unwrap();
        assert_eq!(
            parameters(&unknown_user_hash().to_string()),
            parameters(&hash)
        );
    }

    #[test]
    fn test_unknown_user() {
        let creds = HashMap::new();
        let validator = HashedCredentialValidator::new(creds);

        assert!(!validator.check("nobody", b"pass"));
    }

    #[test]
    fn test_scram_credentials_hold_only_for_the_hash_set_with_them() {
        let (hash, entry) = hash_new_password("secret").unwrap();
        let credentials = scram_credentials(&hash, &entry).unwrap().unwrap();
        assert_eq!(credentials.iterations, mercurio_scram::DEFAULT_ITERATIONS);
        assert!(verify(&hash, b"secret"));

        // The password changed by a tool that kept the old SCRAM keys
        let other = hash_password("other");
        assert_eq!(scram_credentials(&other, &entry).unwrap(), None);

        for bad in ["", "binding", "binding:SCRAM-SHA-256$4096:c2FsdA=="] {
            let error = scram_credentials(&hash, bad).unwrap_err();
            assert!(!error.contains("c2FsdA"), "{error}");
        }
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
