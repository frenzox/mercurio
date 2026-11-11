//! JSON Web Tokens given as CONNECT passwords.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use jsonwebtoken::errors::ErrorKind;
use jsonwebtoken::jwk::{Jwk, PublicKeyUse};
use jsonwebtoken::{decode, decode_header, Algorithm, AlgorithmFamily, DecodingKey, Validation};
use mercurio_core::reason::ReasonCode;
use mercurio_server::admin::{Admin, DisconnectOptions, SessionKey};
use mercurio_server::plugin::{
    ready, BoxFuture, ClientInfo, Credentials, Decision, Event, Plugin, PluginKind, PluginStatus,
};
use serde_json::{Map, Value};
use tracing::warn;

use crate::config::{redacted_http_url, JwtAuthConfig};
use crate::http_auth::is_plaintext_to_remote;
use crate::reload::Reload;

/// Longest wait for the key set server, under the default 5 s
/// `plugin_timeout` so a refresh usually ends before the CONNECT gives up.
const JWKS_TIMEOUT: Duration = Duration::from_secs(4);

/// Least `jwks_refresh_seconds`, as any client can make the broker fetch the
/// set by naming a key it lacks.
const MIN_JWKS_REFRESH: u64 = 10;

pub(crate) struct Verifier {
    keys: Keys,
    validation: Validation,
    username_claim: String,
}

enum Keys {
    File(DecodingKey),
    Set(Jwks),
}

/// A key and the `kid` it goes by.
type KeyWithId = (Option<String>, DecodingKey);

/// Keys fetched from `jwks_url`, fetched again when a token names a key the
/// set lacks.
struct Jwks {
    agent: ureq::Agent,
    url: String,
    algorithms: Vec<Algorithm>,
    refresh_every: Duration,
    /// Shared with the fetch, which stores its result even after the check
    /// that started it has given up
    keys: Arc<RwLock<Vec<KeyWithId>>>,
    /// When the set was last fetched; held while fetching it, so a burst of
    /// unknown keys makes a single request
    fetched: tokio::sync::Mutex<Instant>,
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
        let keys = if let Some(path) = &config.secret_file {
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
            Keys::File(DecodingKey::from_secret(secret))
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
            .map_err(|e| format!("{path}: {e}"))
            .map(Keys::File)?
        } else if let Some(url) = &config.jwks_url {
            if family == AlgorithmFamily::Hmac {
                return Err(
                    "auth.jwt.jwks_url does not take HS256, HS384 or HS512, whose keys are secret"
                        .into(),
                );
            }
            let lower = url.to_ascii_lowercase();
            if !lower.starts_with("https://")
                && !(lower.starts_with("http://") && !is_plaintext_to_remote(url))
            {
                return Err(format!(
                    "auth.jwt.jwks_url {} must be https://, or http:// to a loopback address",
                    redacted_http_url(url)
                ));
            }
            if config.jwks_refresh_seconds < MIN_JWKS_REFRESH {
                return Err(format!(
                    "auth.jwt.jwks_refresh_seconds must be at least {MIN_JWKS_REFRESH}"
                ));
            }
            // A redirect could lead to plain HTTP
            let agent: ureq::Agent = ureq::Agent::config_builder()
                .timeout_global(Some(JWKS_TIMEOUT))
                .max_redirects(0)
                .build()
                .into();
            let keys = fetch(&agent, url, &config.algorithms)
                .map_err(|e| format!("auth.jwt.jwks_url {}: {e}", redacted_http_url(url)))?;
            Keys::Set(Jwks {
                agent,
                url: url.clone(),
                algorithms: config.algorithms.clone(),
                refresh_every: Duration::from_secs(config.jwks_refresh_seconds),
                keys: Arc::new(RwLock::new(keys)),
                fetched: tokio::sync::Mutex::new(Instant::now()),
            })
        } else {
            unreachable!("exactly one key source is set")
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
            keys,
            validation,
            username_claim: config.username_claim.clone(),
        })
    }

    /// Like `check`, fetching the key set again first when the token names
    /// a key it lacks.
    pub(crate) async fn check_async(&self, password: &[u8]) -> Checked {
        let Keys::Set(jwks) = &self.keys else {
            return self.check(password);
        };
        if !looks_like_token(password) {
            return Checked::NotAToken;
        }
        if let Some(checked) = self.check_against(jwks, password) {
            return checked;
        }
        jwks.refresh().await;
        self.check(password)
    }

    pub(crate) fn check(&self, password: &[u8]) -> Checked {
        if !looks_like_token(password) {
            return Checked::NotAToken;
        }
        match &self.keys {
            Keys::File(key) => self.verify(password, key),
            Keys::Set(jwks) => self
                .check_against(jwks, password)
                .unwrap_or(Checked::Invalid("the token's key is not in the key set")),
        }
    }

    /// None when the token names a key the set lacks.
    fn check_against(&self, jwks: &Jwks, password: &[u8]) -> Option<Checked> {
        let kid = match decode_header(password) {
            Ok(header) => header.kid,
            Err(error) => return Some(Checked::Invalid(reason(error.kind()))),
        };
        let keys = jwks.keys.read().unwrap();
        let key = match (kid, keys.as_slice()) {
            (Some(kid), keys) => {
                &keys
                    .iter()
                    .find(|(id, _)| id.as_deref() == Some(kid.as_str()))?
                    .1
            }
            (None, [(_, key)]) => key,
            (None, _) => return Some(Checked::Invalid("the token names no key")),
        };
        Some(self.verify(password, key))
    }

    fn verify(&self, password: &[u8], key: &DecodingKey) -> Checked {
        let claims = match decode::<Map<String, Value>>(password, key, &self.validation) {
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

impl Jwks {
    async fn refresh(&self) {
        let mut fetched = self.fetched.lock().await;
        if fetched.elapsed() < self.refresh_every {
            return;
        }
        *fetched = Instant::now();
        let (agent, url, algorithms, keys) = (
            self.agent.clone(),
            self.url.clone(),
            self.algorithms.clone(),
            Arc::clone(&self.keys),
        );
        let fetch = move || match fetch(&agent, &url, &algorithms) {
            Ok(fetched) => *keys.write().unwrap() = fetched,
            Err(e) => warn!(
                "Fetching the JWT key set from {} failed, so the old one stays: {e}",
                redacted_http_url(&url)
            ),
        };
        if let Err(e) = tokio::task::spawn_blocking(fetch).await {
            warn!("Fetching the JWT key set failed: {e}");
        }
    }
}

/// The signing keys at `url` that one of `algorithms` can use. Keys of other
/// types, and ones this library cannot read, are skipped.
fn fetch(
    agent: &ureq::Agent,
    url: &str,
    algorithms: &[Algorithm],
) -> Result<Vec<KeyWithId>, String> {
    let body = agent
        .get(url)
        .call()
        .and_then(|mut response| match response.status() {
            status if status.is_success() => response.body_mut().read_to_string(),
            status => Err(ureq::Error::StatusCode(status.as_u16())),
        })
        .map_err(|e| match e {
            // Its message repeats the URL, which may hold secrets
            ureq::Error::BadUri(_) => "bad URL".to_string(),
            e => e.to_string(),
        })?;
    let set: Value = serde_json::from_str(&body).map_err(|e| format!("not a key set: {e}"))?;
    let Some(keys) = set.get("keys").and_then(Value::as_array) else {
        return Err("not a key set: no keys array".to_string());
    };
    let keys: Vec<KeyWithId> = keys
        .iter()
        .filter_map(|jwk| serde_json::from_value::<Jwk>(jwk.clone()).ok())
        .filter(|jwk| jwk.common.public_key_use != Some(PublicKeyUse::Encryption))
        .filter(|jwk| {
            jwk.common.key_algorithm.is_none_or(|alg| {
                alg.to_string()
                    .parse::<Algorithm>()
                    .is_ok_and(|alg| algorithms.contains(&alg))
            })
        })
        .filter_map(|jwk| {
            let key = DecodingKey::from_jwk(&jwk).ok()?;
            algorithms
                .iter()
                .any(|alg| alg.family() == key.family())
                .then_some((jwk.common.key_id, key))
        })
        .collect();
    if keys.is_empty() {
        return Err("the key set holds no key for the configured algorithms".to_string());
    }
    Ok(keys)
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

/// Lets in clients whose CONNECT password is a token for their user name,
/// and disconnects them once it expires.
pub(crate) struct JwtAuth {
    config: JwtAuthConfig,
    verifier: RwLock<Arc<Verifier>>,
    expiries: Mutex<Expiries>,
    admin: OnceLock<Admin>,
}

/// How long a login stays pending before its client is looked up, well past
/// any CONNECT that has not ended by then.
const PENDING_FOR: Duration = Duration::from_secs(30);

/// When the tokens of clients expire, by their session.
#[derive(Default)]
struct Expiries {
    /// Authenticated, not connected yet. A list, as clients whose id is
    /// given after authentication share the empty one.
    pending: Vec<Pending>,
    live: HashMap<SessionKey, SystemTime>,
}

struct Pending {
    key: SessionKey,
    username: String,
    expires: SystemTime,
    since: Instant,
}

impl Expiries {
    fn authenticated(&mut self, key: SessionKey, username: &str, expires: SystemTime) {
        self.forget(&key);
        self.pending.push(Pending {
            key,
            username: username.to_string(),
            expires,
            since: Instant::now(),
        });
    }

    /// Drops the pending login of `key`, which logged in again some other
    /// way, so it does not inherit that token's expiry.
    fn forget(&mut self, key: &SessionKey) {
        if !key.client_id.is_empty() {
            self.pending.retain(|pending| pending.key != *key);
        }
    }

    fn connected(&mut self, client: &ClientInfo) {
        let key = session_key(client);
        let username = client.username.as_deref();
        // The latest login of this user under this client id, or else one
        // whose id the broker assigned or replaced with the user name after it
        let found = self
            .pending
            .iter()
            .rposition(|pending| pending.key == key && Some(pending.username.as_str()) == username)
            .or_else(|| {
                self.pending.iter().position(|pending| {
                    pending.key.mount_point == key.mount_point
                        && Some(pending.username.as_str()) == username
                        && (pending.key.client_id.is_empty()
                            || Some(key.client_id.as_str()) == username)
                })
            });
        match found {
            Some(index) => {
                let pending = self.pending.remove(index);
                self.live.insert(key, pending.expires);
            }
            // Let in by something else this time
            None => {
                self.live.remove(&key);
            }
        }
    }

    /// The logins pending since before `now - PENDING_FOR`, forgotten: their
    /// CONNECT was refused, or its Connected event was dropped.
    fn stale(&mut self, now: Instant) -> Vec<Pending> {
        self.pending
            .extract_if(.., |pending| {
                now.duration_since(pending.since) >= PENDING_FOR
            })
            .collect()
    }

    /// The connected clients whose token expired by `now`, forgotten.
    fn expired(&mut self, now: SystemTime) -> Vec<SessionKey> {
        self.live
            .extract_if(|_, expires| *expires <= now)
            .map(|(key, _)| key)
            .collect()
    }
}

fn session_key(client: &ClientInfo) -> SessionKey {
    let mount_point = client
        .connection
        .as_ref()
        .and_then(|c| c.mount_point.clone());
    SessionKey::new(mount_point, client.client_id.clone())
}

impl JwtAuth {
    pub(crate) fn from_config(config: &JwtAuthConfig) -> Result<Self, String> {
        Ok(Self {
            verifier: RwLock::new(Arc::new(Verifier::from_config(config)?)),
            config: config.clone(),
            expiries: Mutex::default(),
            admin: OnceLock::new(),
        })
    }

    /// Hands over the broker's `Admin`, which exists only once the broker
    /// does.
    pub(crate) fn set_admin(&self, admin: Admin) {
        let _ = self.admin.set(admin);
    }

    /// Whether its keys come from a file, which a reload rereads; a key set
    /// is fetched again on its own.
    pub(crate) fn has_key_file(&self) -> bool {
        self.config.jwks_url.is_none()
    }
}

impl Plugin for JwtAuth {
    fn name(&self) -> &str {
        "jwt"
    }

    fn status(&self) -> PluginStatus {
        PluginStatus::new("jwt", PluginKind::Builtin).with_hooks(&[
            "authenticate",
            "on_event",
            "on_tick",
        ])
    }

    fn authenticate<'a>(
        &'a self,
        client: &'a ClientInfo,
        credentials: &'a Credentials<'a>,
    ) -> BoxFuture<'a, Decision> {
        let verifier = Arc::clone(&self.verifier.read().unwrap());
        Box::pin(async move {
            let peer = client
                .peer
                .map_or_else(|| "-".to_string(), |peer| peer.to_string());
            let checked = verifier.check_async(credentials.password).await;
            if !matches!(&checked, Checked::Valid { username, .. } if username == credentials.username)
            {
                self.expiries.lock().unwrap().forget(&session_key(client));
            }
            match checked {
                Checked::NotAToken => Decision::Defer,
                Checked::Invalid(why) => {
                    warn!(
                        "Refusing the token of client `{}` from {}: {}",
                        client.client_id, peer, why
                    );
                    Decision::Deny
                }
                Checked::Valid { username, .. } if username != credentials.username => {
                    warn!(
                        "Refusing the token of client `{}` from {}: it is for another user",
                        client.client_id, peer
                    );
                    Decision::Deny
                }
                Checked::Valid { username, expires } => {
                    if self.config.disconnect_on_expiry {
                        // Accepted within the leeway, so kept as long
                        let expires = expires
                            .checked_add(Duration::from_secs(self.config.leeway_seconds))
                            .unwrap_or(expires);
                        self.expiries.lock().unwrap().authenticated(
                            session_key(client),
                            &username,
                            expires,
                        );
                    }
                    Decision::Allow
                }
            }
        })
    }

    fn on_event<'a>(&'a self, event: &'a Event) -> BoxFuture<'a, ()> {
        // A takeover reports the old connection's Disconnected after the
        // new one authenticated, so only Connected changes anything
        if let Event::Connected { client, .. } = event {
            self.expiries.lock().unwrap().connected(client);
        }
        ready(())
    }

    // ponytail: expiry is checked on the plugin tick, so a client outlives its token by up to one tick interval; a timer per client if that matters.
    // Clients whose id is assigned after authentication are tracked only through their Connected event, best effort.
    // A login whose Connected event was dropped is looked up after PENDING_FOR, so it can outlive its token by that much.
    // A fresh-token reconnect landing between the check below and the disconnect is still dropped; it can reconnect.
    fn on_tick(&self) -> BoxFuture<'_, ()> {
        let stale = self.expiries.lock().unwrap().stale(Instant::now());
        Box::pin(async move {
            let Some(admin) = self.admin.get() else {
                return;
            };
            // Connected events can be dropped, so a client that logged in with
            // the token and is still there is tracked from now on
            for pending in stale {
                if pending.key.client_id.is_empty() {
                    continue;
                }
                let Ok(client) = admin.client(&pending.key).await else {
                    continue;
                };
                if client.connected && client.username.as_deref() == Some(&pending.username) {
                    self.expiries
                        .lock()
                        .unwrap()
                        .live
                        .insert(pending.key, pending.expires);
                }
            }
            let expired = self.expiries.lock().unwrap().expired(SystemTime::now());
            let mut options = DisconnectOptions::default();
            options.reason = ReasonCode::NotAuthorized;
            for key in expired {
                // Back with a fresh token since
                if self.expiries.lock().unwrap().live.contains_key(&key) {
                    continue;
                }
                // It may have left meanwhile
                let _ = admin.disconnect(&key, options).await;
            }
        })
    }
}

impl Reload for JwtAuth {
    fn reload(&self) -> Result<(), String> {
        let verifier = Verifier::from_config(&self.config)?;
        *self.verifier.write().unwrap() = Arc::new(verifier);
        Ok(())
    }

    fn path(&self) -> &str {
        let config = &self.config;
        config
            .secret_file
            .as_deref()
            .or(config.public_key_file.as_deref())
            .unwrap_or_default()
    }

    fn item(&self) -> &str {
        "jwt"
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, UNIX_EPOCH};

    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    use jsonwebtoken::jwk::Jwk;
    use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
    use serde_json::{json, Value};

    use super::*;
    use crate::http_auth::tests::{backend_with_body, join_all};

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

    fn rsa_key() -> EncodingKey {
        EncodingKey::from_rsa_pem(&std::fs::read(fixture("rsa.key")).unwrap()).unwrap()
    }

    /// A key set of the RSA fixture under each of `kids`.
    fn rsa_set(kids: &[&str]) -> String {
        let keys: Vec<Value> = kids
            .iter()
            .map(|kid| {
                let mut jwk = Jwk::from_encoding_key(&rsa_key(), Algorithm::RS256).unwrap();
                jwk.common.key_id = Some(kid.to_string());
                serde_json::to_value(jwk).unwrap()
            })
            .collect();
        json!({ "keys": keys }).to_string()
    }

    fn rsa_token(kid: Option<&str>) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = kid.map(str::to_string);
        let claims = json!({ "sub": "alice", "exp": now() + 300 });
        encode(&header, &claims, &rsa_key()).unwrap()
    }

    /// A key set server answering its `index`th request with `respond`.
    fn jwks_server(
        respond: impl Fn(usize) -> (&'static str, String) + Send + Sync + 'static,
    ) -> (String, Arc<AtomicUsize>) {
        let backend = backend_with_body(Duration::ZERO, move |_, index| respond(index));
        (backend.url, backend.requests)
    }

    fn jwks_config(url: &str, refresh_seconds: u64) -> JwtAuthConfig {
        JwtAuthConfig {
            jwks_url: Some(url.to_string()),
            jwks_refresh_seconds: refresh_seconds,
            ..config(&[Algorithm::RS256])
        }
    }

    /// A verifier refreshing its set more often than `from_config` allows.
    fn jwks_verifier(url: &str, refresh_seconds: u64) -> Verifier {
        let mut verifier = Verifier::from_config(&jwks_config(url, 300)).unwrap();
        let Keys::Set(jwks) = &mut verifier.keys else {
            unreachable!("a jwks_url gives a key set")
        };
        jwks.refresh_every = Duration::from_secs(refresh_seconds);
        verifier
    }

    #[test]
    fn test_jwks_refreshes_closer_than_ten_seconds_are_refused() {
        let (url, _) = jwks_server(|_| ("200 OK", rsa_set(&["k1"])));
        let error = Verifier::from_config(&jwks_config(&url, 9)).err().unwrap();
        assert!(error.contains("jwks_refresh_seconds"), "{error}");
        assert!(Verifier::from_config(&jwks_config(&url, 10)).is_ok());
    }

    fn is_valid(checked: Checked) -> bool {
        matches!(checked, Checked::Valid { .. })
    }

    #[tokio::test]
    async fn test_a_token_signed_by_a_key_in_the_set_verifies() {
        let (url, requests) = jwks_server(|_| ("200 OK", rsa_set(&["a", "b"])));
        let verifier = Verifier::from_config(&jwks_config(&url, 300)).unwrap();
        assert!(is_valid(
            verifier.check_async(rsa_token(Some("b")).as_bytes()).await
        ));
        assert!(is_valid(verifier.check(rsa_token(Some("a")).as_bytes())));
        assert_eq!(requests.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_a_token_without_kid_needs_a_set_of_one_usable_key() {
        let ec = EncodingKey::from_ec_pem(&std::fs::read(fixture("ec.key")).unwrap()).unwrap();
        let mut ec =
            serde_json::to_value(Jwk::from_encoding_key(&ec, Algorithm::ES256).unwrap()).unwrap();
        ec["kid"] = json!("ec");
        let mut set: Value = serde_json::from_str(&rsa_set(&["a"])).unwrap();
        set["keys"].as_array_mut().unwrap().push(ec);
        let (url, _) = jwks_server(move |_| ("200 OK", set.to_string()));
        let verifier = Verifier::from_config(&jwks_config(&url, 300)).unwrap();
        assert!(is_valid(
            verifier.check_async(rsa_token(None).as_bytes()).await
        ));

        let (url, _) = jwks_server(|_| ("200 OK", rsa_set(&["a", "b"])));
        let verifier = Verifier::from_config(&jwks_config(&url, 300)).unwrap();
        assert!(is_invalid(
            verifier.check_async(rsa_token(None).as_bytes()).await
        ));
    }

    #[tokio::test]
    async fn test_a_rotated_set_is_fetched_for_an_unknown_kid() {
        let (url, requests) = jwks_server(|index| match index {
            0 => ("200 OK", rsa_set(&["old"])),
            _ => ("200 OK", rsa_set(&["new"])),
        });
        let verifier = jwks_verifier(&url, 0);
        assert!(is_valid(
            verifier
                .check_async(rsa_token(Some("new")).as_bytes())
                .await
        ));
        assert_eq!(requests.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_unknown_kids_fetch_the_set_once_per_refresh_window() {
        let (url, requests) = jwks_server(|_| ("200 OK", rsa_set(&["a"])));
        let verifier = jwks_verifier(&url, 1);
        let tokens: Vec<String> = (0..20)
            .map(|i| rsa_token(Some(&format!("unknown-{i}"))))
            .collect();
        for token in &tokens[..10] {
            assert!(is_invalid(verifier.check_async(token.as_bytes()).await));
        }
        assert_eq!(requests.load(Ordering::SeqCst), 1);

        tokio::time::sleep(Duration::from_millis(1100)).await;
        let checks = tokens[10..]
            .iter()
            .map(|token| verifier.check_async(token.as_bytes()));
        for checked in join_all(checks).await {
            assert!(is_invalid(checked));
        }
        assert_eq!(requests.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_a_failed_refresh_keeps_the_old_keys() {
        let (url, requests) = jwks_server(|index| match index {
            0 => ("200 OK", rsa_set(&["a"])),
            _ => ("500 Internal Server Error", String::new()),
        });
        let verifier = jwks_verifier(&url, 0);
        assert!(is_invalid(
            verifier.check_async(rsa_token(Some("x")).as_bytes()).await
        ));
        assert_eq!(requests.load(Ordering::SeqCst), 2);
        assert!(is_valid(
            verifier.check_async(rsa_token(Some("a")).as_bytes()).await
        ));
    }

    #[test]
    fn test_the_set_must_load_at_start() {
        let (url, _) = jwks_server(|_| ("500 Internal Server Error", String::new()));
        assert!(Verifier::from_config(&jwks_config(&url, 300)).is_err());
        let (url, _) = jwks_server(|_| ("200 OK", "not json".to_string()));
        assert!(Verifier::from_config(&jwks_config(&url, 300)).is_err());
        let (url, _) = jwks_server(|_| ("200 OK", json!({ "keys": [] }).to_string()));
        assert!(Verifier::from_config(&jwks_config(&url, 300)).is_err());
    }

    #[test]
    fn test_redirects_are_not_followed() {
        let (good, _) = jwks_server(|_| ("200 OK", rsa_set(&["a"])));
        assert!(Verifier::from_config(&jwks_config(&good, 300)).is_ok());
        // The test server writes the status line as given, so it can carry
        // a header too
        let found: &'static str = format!("302 Found\r\nLocation: {good}").leak();
        let (url, _) = jwks_server(move |_| (found, String::new()));
        assert!(Verifier::from_config(&jwks_config(&url, 300)).is_err());
    }

    #[test]
    fn test_hmac_algorithms_are_refused_with_a_jwks_url() {
        let (url, requests) = jwks_server(|_| ("200 OK", rsa_set(&["a"])));
        let config = JwtAuthConfig {
            algorithms: vec![Algorithm::HS256],
            ..jwks_config(&url, 300)
        };
        assert!(Verifier::from_config(&config).is_err());
        assert_eq!(requests.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn test_a_refresh_outlives_the_check_that_gave_up_on_it() {
        let backend = backend_with_body(Duration::from_millis(300), |_, index| match index {
            0 => ("200 OK", rsa_set(&["old"])),
            _ => ("200 OK", rsa_set(&["new"])),
        });
        let verifier = jwks_verifier(&backend.url, 1);
        tokio::time::sleep(Duration::from_millis(1100)).await;
        let token = rsa_token(Some("new"));
        let check = verifier.check_async(token.as_bytes());
        assert!(tokio::time::timeout(Duration::from_millis(50), check)
            .await
            .is_err());
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(is_valid(verifier.check_async(token.as_bytes()).await));
        assert_eq!(backend.requests.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn test_jwks_urls_must_be_https_unless_on_loopback() {
        for url in [
            "http://issuer.example/jwks.json",
            "ftp://issuer.example/jwks.json",
        ] {
            let error = Verifier::from_config(&jwks_config(url, 300)).err().unwrap();
            assert!(error.contains("https"), "{error}");
        }
    }

    fn session(client_id: &str) -> SessionKey {
        SessionKey::new(None, client_id)
    }

    fn connected(expiries: &mut Expiries, client_id: &str, username: &str) {
        expiries.connected(&ClientInfo::new(client_id, Some(username.to_string())));
    }

    #[test]
    fn test_a_connected_client_expires_with_its_token() {
        let start = SystemTime::now();
        let mut expiries = Expiries::default();
        expiries.authenticated(session("c1"), "alice", start + Duration::from_secs(10));
        connected(&mut expiries, "c1", "alice");
        assert!(expiries.expired(start + Duration::from_secs(5)).is_empty());
        assert_eq!(
            expiries.expired(start + Duration::from_secs(10)),
            [session("c1")]
        );
        assert!(expiries.expired(start + Duration::from_secs(20)).is_empty());
    }

    #[test]
    fn test_a_takeover_keeps_the_new_tokens_expiry() {
        let start = SystemTime::now();
        let mut expiries = Expiries::default();
        expiries.authenticated(session("c1"), "alice", start + Duration::from_secs(2));
        connected(&mut expiries, "c1", "alice");
        expiries.authenticated(session("c1"), "alice", start + Duration::from_secs(60));
        connected(&mut expiries, "c1", "alice");
        assert!(expiries.expired(start + Duration::from_secs(5)).is_empty());
    }

    #[test]
    fn test_a_client_back_by_other_means_is_forgotten() {
        let start = SystemTime::now();
        let mut expiries = Expiries::default();
        expiries.authenticated(session("c1"), "alice", start + Duration::from_secs(2));
        connected(&mut expiries, "c1", "alice");
        connected(&mut expiries, "c1", "alice");
        assert!(expiries.expired(start + Duration::from_secs(5)).is_empty());
    }

    #[test]
    fn test_a_client_id_given_after_authentication_finds_its_token() {
        let start = SystemTime::now();
        let mut expiries = Expiries::default();
        // Assigned by the broker, or the user name on a listener that uses it
        expiries.authenticated(session(""), "alice", start + Duration::from_secs(2));
        expiries.authenticated(session("c2"), "bob", start + Duration::from_secs(2));
        connected(&mut expiries, "auto-1", "alice");
        connected(&mut expiries, "bob", "bob");
        let mut expired = expiries.expired(start + Duration::from_secs(5));
        expired.sort();
        assert_eq!(expired, [session("auto-1"), session("bob")]);
    }

    #[test]
    fn test_logins_left_pending_are_dropped_after_a_while() {
        let start = SystemTime::now();
        let mut expiries = Expiries::default();
        expiries.authenticated(session("c1"), "alice", start + Duration::from_secs(2));
        expiries.authenticated(session(""), "bob", start + Duration::from_secs(2));
        assert!(expiries.expired(start + Duration::from_secs(5)).is_empty());
        assert!(expiries.stale(Instant::now()).is_empty());
        let stale = expiries.stale(Instant::now() + PENDING_FOR);
        let stale: Vec<_> = stale
            .iter()
            .map(|p| (p.key.clone(), p.username.as_str()))
            .collect();
        assert_eq!(stale, [(session("c1"), "alice"), (session(""), "bob")]);
        assert!(expiries.pending.is_empty());
    }

    #[test]
    fn test_a_pending_login_of_another_user_is_not_taken_up() {
        let start = SystemTime::now();
        let mut expiries = Expiries::default();
        expiries.authenticated(session("c1"), "mallory", start + Duration::from_secs(2));
        connected(&mut expiries, "c1", "bob");
        assert!(expiries.live.is_empty());
        assert!(expiries.expired(start + Duration::from_secs(5)).is_empty());
    }

    #[test]
    fn test_a_client_id_has_one_pending_login() {
        let start = SystemTime::now();
        let mut expiries = Expiries::default();
        expiries.authenticated(session("c1"), "alice", start + Duration::from_secs(2));
        expiries.authenticated(session("c1"), "alice", start + Duration::from_secs(60));
        assert!(expiries.expired(start + Duration::from_secs(5)).is_empty());
        connected(&mut expiries, "c1", "alice");
        assert!(expiries.expired(start + Duration::from_secs(5)).is_empty());
        assert_eq!(
            expiries.expired(start + Duration::from_secs(60)),
            [session("c1")]
        );
    }

    #[test]
    fn test_a_login_without_a_token_drops_the_pending_one() {
        let start = SystemTime::now();
        let mut expiries = Expiries::default();
        expiries.authenticated(session("c1"), "alice", start + Duration::from_secs(2));
        expiries.forget(&session("c1"));
        connected(&mut expiries, "c1", "alice");
        assert!(expiries.expired(start + Duration::from_secs(5)).is_empty());
    }

    #[tokio::test]
    async fn test_refused_logins_drop_the_pending_token_of_their_client_id() {
        let auth = JwtAuth::from_config(&JwtAuthConfig {
            leeway_seconds: 0,
            ..hs_config()
        })
        .unwrap();
        let client = ClientInfo::new("c1", Some("alice".to_string()));
        let valid = hs_token(&json!({ "sub": "alice", "exp": now() + 2 }));
        let other = hs_token(&json!({ "sub": "bob", "exp": now() + 60 }));
        let expired = hs_token(&json!({ "sub": "alice", "exp": now() - 60 }));
        let later = SystemTime::now() + Duration::from_secs(5);
        for password in ["not a token", other.as_str(), expired.as_str()] {
            let login = Credentials::new("alice", valid.as_bytes());
            assert_eq!(auth.authenticate(&client, &login).await, Decision::Allow);
            let again = Credentials::new("alice", password.as_bytes());
            assert_ne!(auth.authenticate(&client, &again).await, Decision::Allow);
            assert!(
                auth.expiries.lock().unwrap().expired(later).is_empty(),
                "{password}"
            );
        }
    }

    async fn still_connected(admin: &Admin, client_id: &str) -> bool {
        for _ in 0..20 {
            match admin.client(&session(client_id)).await {
                Ok(client) if client.connected => {}
                _ => return false,
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        true
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_a_client_whose_connected_event_was_lost_is_found_and_expires() {
        use mercurio_client::{ConnectOptions, MqttClient};
        use mercurio_server::server::{ListenerConfig, Server, ServerConfig, Transport};

        let auth = Arc::new(JwtAuth::from_config(&hs_config()).unwrap());
        let mut config = ServerConfig::default();
        config.plugins = vec![auth.clone()];
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = Server::builder()
            .config(config)
            .listener(ListenerConfig::new(listener, Transport::Mqtt))
            .storage(Arc::new(mercurio_storage::memory::MemoryStore::new()))
            .build()
            .await
            .unwrap();
        let admin = server.admin();
        auth.set_admin(admin.clone());
        tokio::spawn(server.run(std::future::pending::<()>()));

        let token = hs_token(&json!({ "sub": "alice", "exp": now() + 60 }));
        let mut clients = Vec::new();
        for client_id in ["c1", "c2"] {
            let options = ConnectOptions::new("127.0.0.1", port)
                .client_id(client_id)
                .username("alice")
                .password(token.clone());
            clients.push(MqttClient::connect(options).await.unwrap());
        }
        // The real Connected events arrive through the event queue
        for _ in 0..100 {
            if auth.expiries.lock().unwrap().pending.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // As if both Connected events were lost; c2's login is someone else's
        let past = Instant::now() - PENDING_FOR;
        {
            let mut expiries = auth.expiries.lock().unwrap();
            expiries.live.clear();
            for (client_id, username) in [("c1", "alice"), ("c2", "mallory")] {
                expiries.pending.push(Pending {
                    key: session(client_id),
                    username: username.to_string(),
                    expires: SystemTime::now() - Duration::from_secs(1),
                    since: past,
                });
            }
        }
        auth.on_tick().await;
        assert!(auth.expiries.lock().unwrap().pending.is_empty());
        assert!(!still_connected(&admin, "c1").await);
        assert!(still_connected(&admin, "c2").await);
    }
}
