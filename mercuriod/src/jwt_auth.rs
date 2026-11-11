//! JSON Web Tokens given as CONNECT passwords.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use jsonwebtoken::errors::ErrorKind;
use jsonwebtoken::{decode, AlgorithmFamily, DecodingKey, Validation};
use serde_json::{Map, Value};

use crate::config::JwtAuthConfig;

pub(crate) struct Verifier {
    key: DecodingKey,
    validation: Validation,
    username_claim: String,
}

#[derive(Debug, PartialEq)]
pub(crate) enum Checked {
    /// Not a JWT: let another plugin decide
    NotAToken,
    Valid {
        username: String,
        expires: SystemTime,
    },
    Invalid(&'static str),
}

impl Verifier {
    pub(crate) fn from_config(config: &JwtAuthConfig) -> Result<Self, String> {
        let sources = [
            &config.secret_file,
            &config.public_key_file,
            &config.jwks_url,
        ]
        .into_iter()
        .filter(|source| source.is_some())
        .count();
        if sources != 1 {
            return Err(
                "auth.jwt needs exactly one of secret_file, public_key_file and jwks_url".into(),
            );
        }
        let Some(&first) = config.algorithms.first() else {
            return Err("auth.jwt.algorithms must name at least one algorithm".into());
        };
        let family = first.family();
        if config.algorithms.iter().any(|alg| alg.family() != family) {
            return Err("auth.jwt.algorithms must all take the same kind of key".into());
        }
        let key = if let Some(path) = &config.secret_file {
            if family != AlgorithmFamily::Hmac {
                return Err("auth.jwt.secret_file only takes HS256, HS384 and HS512".into());
            }
            let secret = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
            let secret = secret
                .strip_suffix(b"\r\n")
                .or_else(|| secret.strip_suffix(b"\n"))
                .unwrap_or(&secret);
            if secret.len() < 32 {
                return Err(format!("{path}: the secret must be at least 32 bytes long"));
            }
            DecodingKey::from_secret(secret)
        } else if let Some(path) = &config.public_key_file {
            let pem = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
            match family {
                AlgorithmFamily::Hmac => {
                    return Err(
                        "auth.jwt.public_key_file does not take HS256, HS384 or HS512".into(),
                    );
                }
                AlgorithmFamily::Rsa => DecodingKey::from_rsa_pem(&pem),
                AlgorithmFamily::Ec => DecodingKey::from_ec_pem(&pem),
                AlgorithmFamily::Ed => DecodingKey::from_ed_pem(&pem),
            }
            .map_err(|e| format!("{path}: {e}"))?
        } else {
            return Err("auth.jwt.jwks_url is not supported yet".into());
        };

        let mut validation = Validation::new(first);
        validation.algorithms = config.algorithms.clone();
        // jsonwebtoken only checks iss and aud when the token carries them
        let mut required = vec!["exp"];
        required.extend(config.issuer.as_ref().map(|_| "iss"));
        required.extend(config.audience.as_ref().map(|_| "aud"));
        validation.set_required_spec_claims(&required);
        validation.leeway = config.leeway_seconds;
        validation.validate_nbf = true;
        if let Some(issuer) = &config.issuer {
            validation.set_issuer(&[issuer]);
        }
        match &config.audience {
            Some(audience) => validation.set_audience(&[audience]),
            None => validation.validate_aud = false,
        }
        Ok(Self {
            key,
            validation,
            username_claim: config.username_claim.clone(),
        })
    }

    pub(crate) fn check(&self, password: &[u8]) -> Checked {
        if !looks_like_token(password) {
            return Checked::NotAToken;
        }
        let claims = match decode::<Map<String, Value>>(password, &self.key, &self.validation) {
            Ok(data) => data.claims,
            Err(error) => return Checked::Invalid(reason(error.kind())),
        };
        let Some(username) = claims.get(&self.username_claim).and_then(Value::as_str) else {
            return Checked::Invalid("the token has no user name claim");
        };
        let Some(exp) = claims.get("exp").and_then(Value::as_u64) else {
            return Checked::Invalid("the token's exp is not a whole number of seconds");
        };
        let Some(expires) = UNIX_EPOCH.checked_add(Duration::from_secs(exp)) else {
            return Checked::Invalid("the token's exp is out of range");
        };
        Checked::Valid {
            username: username.to_string(),
            expires,
        }
    }
}

/// Three dot-separated segments, the first a base64url JSON object with an
/// `alg`: anything else is somebody's ordinary password.
fn looks_like_token(password: &[u8]) -> bool {
    let mut segments = password.split(|&byte| byte == b'.');
    let (Some(header), Some(_), Some(_), None) = (
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    ) else {
        return false;
    };
    URL_SAFE_NO_PAD
        .decode(header)
        .ok()
        .and_then(|json| serde_json::from_slice::<Map<String, Value>>(&json).ok())
        .is_some_and(|header| header.contains_key("alg"))
}

fn reason(kind: &ErrorKind) -> &'static str {
    match kind {
        ErrorKind::InvalidSignature => "the token's signature does not match",
        ErrorKind::InvalidAlgorithm => "the token's algorithm is not allowed",
        ErrorKind::ExpiredSignature => "the token has expired",
        ErrorKind::ImmatureSignature => "the token is not valid yet",
        ErrorKind::InvalidIssuer => "the token has the wrong issuer",
        ErrorKind::InvalidAudience => "the token has the wrong audience",
        ErrorKind::MissingRequiredClaim(_) => "the token lacks a required claim",
        _ => "the token is malformed",
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
    use serde_json::{json, Value};

    use super::*;

    fn fixture(name: &str) -> String {
        format!("{}/tests/fixtures/jwt/{name}", env!("CARGO_MANIFEST_DIR"))
    }

    fn config(algorithms: &[Algorithm]) -> JwtAuthConfig {
        JwtAuthConfig {
            secret_file: None,
            public_key_file: None,
            jwks_url: None,
            algorithms: algorithms.to_vec(),
            issuer: None,
            audience: None,
            username_claim: "sub".to_string(),
            leeway_seconds: 30,
            disconnect_on_expiry: true,
            jwks_refresh_seconds: 300,
        }
    }

    fn hs_config() -> JwtAuthConfig {
        JwtAuthConfig {
            secret_file: Some(fixture("hs256.secret")),
            ..config(&[Algorithm::HS256])
        }
    }

    fn public_key_config(file: &str, algorithm: Algorithm) -> JwtAuthConfig {
        JwtAuthConfig {
            public_key_file: Some(fixture(file)),
            ..config(&[algorithm])
        }
    }

    fn secret() -> Vec<u8> {
        let mut secret = std::fs::read(fixture("hs256.secret")).unwrap();
        assert_eq!(secret.pop(), Some(b'\n'));
        secret
    }

    fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    fn hs_token(claims: &Value) -> String {
        encode(
            &Header::new(Algorithm::HS256),
            claims,
            &EncodingKey::from_secret(&secret()),
        )
        .unwrap()
    }

    fn check(config: &JwtAuthConfig, token: &str) -> Checked {
        Verifier::from_config(config)
            .unwrap()
            .check(token.as_bytes())
    }

    fn is_invalid(checked: Checked) -> bool {
        matches!(checked, Checked::Invalid(_))
    }

    #[test]
    fn test_a_valid_hs256_token_gives_its_subject_and_expiry() {
        let exp = now() + 300;
        let token = hs_token(&json!({ "sub": "alice", "exp": exp }));
        assert_eq!(
            check(&hs_config(), &token),
            Checked::Valid {
                username: "alice".to_string(),
                expires: UNIX_EPOCH + Duration::from_secs(exp),
            }
        );
    }

    #[test]
    fn test_a_valid_rs256_token_verifies_against_the_public_key() {
        let key = EncodingKey::from_rsa_pem(&std::fs::read(fixture("rsa.key")).unwrap()).unwrap();
        let claims = json!({ "sub": "alice", "exp": now() + 300 });
        let token = encode(&Header::new(Algorithm::RS256), &claims, &key).unwrap();
        let config = public_key_config("rsa.pub", Algorithm::RS256);
        assert!(matches!(check(&config, &token), Checked::Valid { .. }));
    }

    #[test]
    fn test_a_valid_es256_token_verifies_against_the_public_key() {
        let key = EncodingKey::from_ec_pem(&std::fs::read(fixture("ec.key")).unwrap()).unwrap();
        let claims = json!({ "sub": "alice", "exp": now() + 300 });
        let token = encode(&Header::new(Algorithm::ES256), &claims, &key).unwrap();
        let config = public_key_config("ec.pub", Algorithm::ES256);
        assert!(matches!(check(&config, &token), Checked::Valid { .. }));
    }

    #[test]
    fn test_the_username_claim_is_configurable() {
        let config = JwtAuthConfig {
            username_claim: "preferred_username".to_string(),
            ..hs_config()
        };
        let token = hs_token(&json!({
            "sub": "1234",
            "preferred_username": "alice",
            "exp": now() + 300,
        }));
        assert!(matches!(
            check(&config, &token),
            Checked::Valid { username, .. } if username == "alice"
        ));
    }

    #[test]
    fn test_a_missing_username_claim_is_invalid() {
        let config = JwtAuthConfig {
            username_claim: "preferred_username".to_string(),
            ..hs_config()
        };
        let token = hs_token(&json!({ "sub": "alice", "exp": now() + 300 }));
        assert!(is_invalid(check(&config, &token)));
    }

    #[test]
    fn test_a_username_claim_that_is_not_a_string_is_invalid() {
        let token = hs_token(&json!({ "sub": 1234, "exp": now() + 300 }));
        assert!(is_invalid(check(&hs_config(), &token)));
    }

    #[test]
    fn test_a_token_expired_beyond_the_leeway_is_invalid() {
        let token = hs_token(&json!({ "sub": "alice", "exp": now() - 60 }));
        assert!(is_invalid(check(&hs_config(), &token)));
    }

    #[test]
    fn test_a_token_expired_within_the_leeway_is_valid() {
        let token = hs_token(&json!({ "sub": "alice", "exp": now() - 10 }));
        assert!(matches!(check(&hs_config(), &token), Checked::Valid { .. }));
    }

    #[test]
    fn test_a_token_without_exp_is_invalid() {
        let token = hs_token(&json!({ "sub": "alice" }));
        assert!(is_invalid(check(&hs_config(), &token)));
    }

    #[test]
    fn test_a_token_not_valid_yet_is_invalid() {
        let token = hs_token(&json!({ "sub": "alice", "exp": now() + 600, "nbf": now() + 300 }));
        assert!(is_invalid(check(&hs_config(), &token)));
    }

    #[test]
    fn test_the_issuer_is_checked_when_configured() {
        let config = JwtAuthConfig {
            issuer: Some("https://issuer.example".to_string()),
            ..hs_config()
        };
        let wrong =
            hs_token(&json!({ "sub": "alice", "exp": now() + 300, "iss": "https://evil.example" }));
        assert!(is_invalid(check(&config, &wrong)));
        let right = hs_token(
            &json!({ "sub": "alice", "exp": now() + 300, "iss": "https://issuer.example" }),
        );
        assert!(matches!(check(&config, &right), Checked::Valid { .. }));
    }

    #[test]
    fn test_the_issuer_is_ignored_when_not_configured() {
        let token =
            hs_token(&json!({ "sub": "alice", "exp": now() + 300, "iss": "https://any.example" }));
        assert!(matches!(check(&hs_config(), &token), Checked::Valid { .. }));
    }

    #[test]
    fn test_the_audience_is_checked_when_configured() {
        let config = JwtAuthConfig {
            audience: Some("mercurio".to_string()),
            ..hs_config()
        };
        let wrong = hs_token(&json!({ "sub": "alice", "exp": now() + 300, "aud": "other" }));
        assert!(is_invalid(check(&config, &wrong)));
        let right = hs_token(&json!({ "sub": "alice", "exp": now() + 300, "aud": "mercurio" }));
        assert!(matches!(check(&config, &right), Checked::Valid { .. }));
    }

    #[test]
    fn test_a_missing_issuer_is_invalid_when_configured() {
        let config = JwtAuthConfig {
            issuer: Some("https://issuer.example".to_string()),
            ..hs_config()
        };
        let token = hs_token(&json!({ "sub": "alice", "exp": now() + 300 }));
        assert_eq!(
            check(&config, &token),
            Checked::Invalid("the token lacks a required claim")
        );
    }

    #[test]
    fn test_a_missing_audience_is_invalid_when_configured() {
        let config = JwtAuthConfig {
            audience: Some("mercurio".to_string()),
            ..hs_config()
        };
        let token = hs_token(&json!({ "sub": "alice", "exp": now() + 300 }));
        assert_eq!(
            check(&config, &token),
            Checked::Invalid("the token lacks a required claim")
        );
    }

    #[test]
    fn test_an_expiry_beyond_what_the_clock_holds_is_invalid() {
        let token = hs_token(&json!({ "sub": "alice", "exp": u64::MAX }));
        assert!(is_invalid(check(&hs_config(), &token)));
    }

    #[test]
    fn test_the_audience_is_ignored_when_not_configured() {
        let token = hs_token(&json!({ "sub": "alice", "exp": now() + 300, "aud": "other" }));
        assert!(matches!(check(&hs_config(), &token), Checked::Valid { .. }));
    }

    #[test]
    fn test_an_unsigned_token_is_invalid() {
        let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"none","typ":"JWT"}"#);
        let payload =
            URL_SAFE_NO_PAD.encode(json!({ "sub": "alice", "exp": now() + 300 }).to_string());
        let token = format!("{header}.{payload}.");
        assert!(is_invalid(check(&hs_config(), &token)));
    }

    #[test]
    fn test_an_hmac_token_keyed_with_the_rsa_public_key_is_invalid() {
        let public_key = std::fs::read(fixture("rsa.pub")).unwrap();
        let claims = json!({ "sub": "alice", "exp": now() + 300 });
        let token = encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(&public_key),
        )
        .unwrap();
        let config = public_key_config("rsa.pub", Algorithm::RS256);
        assert!(is_invalid(check(&config, &token)));
    }

    #[test]
    fn test_a_tampered_payload_is_invalid() {
        let token = hs_token(&json!({ "sub": "alice", "exp": now() + 300 }));
        let parts: Vec<&str> = token.split('.').collect();
        let payload =
            URL_SAFE_NO_PAD.encode(json!({ "sub": "admin", "exp": now() + 300 }).to_string());
        let tampered = format!("{}.{payload}.{}", parts[0], parts[2]);
        assert!(is_invalid(check(&hs_config(), &tampered)));
    }

    #[test]
    fn test_passwords_that_are_not_tokens_are_left_to_other_plugins() {
        let verifier = Verifier::from_config(&hs_config()).unwrap();
        assert_eq!(verifier.check(b"hunter2"), Checked::NotAToken);
        assert_eq!(verifier.check(b"a.b.c"), Checked::NotAToken);
    }

    #[test]
    fn test_exactly_one_key_source_is_required() {
        assert!(Verifier::from_config(&config(&[Algorithm::HS256])).is_err());
        let two = JwtAuthConfig {
            public_key_file: Some(fixture("rsa.pub")),
            ..hs_config()
        };
        assert!(Verifier::from_config(&two).is_err());
    }

    #[test]
    fn test_algorithms_must_not_be_empty() {
        let config = JwtAuthConfig {
            algorithms: Vec::new(),
            ..hs_config()
        };
        assert!(Verifier::from_config(&config).is_err());
    }

    #[test]
    fn test_algorithms_must_match_the_key() {
        let hmac_with_public_key = public_key_config("rsa.pub", Algorithm::HS256);
        assert!(Verifier::from_config(&hmac_with_public_key).is_err());
        let rsa_with_secret = JwtAuthConfig {
            algorithms: vec![Algorithm::RS256],
            ..hs_config()
        };
        assert!(Verifier::from_config(&rsa_with_secret).is_err());
        let mixed = JwtAuthConfig {
            algorithms: vec![Algorithm::RS256, Algorithm::ES256],
            ..public_key_config("rsa.pub", Algorithm::RS256)
        };
        assert!(Verifier::from_config(&mixed).is_err());
    }

    #[test]
    fn test_secrets_shorter_than_32_bytes_are_refused() {
        let config = JwtAuthConfig {
            secret_file: Some(fixture("short.secret")),
            ..hs_config()
        };
        assert!(Verifier::from_config(&config).is_err());
    }
}
