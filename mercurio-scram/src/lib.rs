#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]

use std::fmt;
use std::str::FromStr;

use base64::{engine::general_purpose::STANDARD, Engine};
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// The mechanism's name, the MQTT Authentication Method.
pub const MECHANISM: &str = "SCRAM-SHA-256";

/// The fewest PBKDF2 iterations RFC 7677 allows. A client refuses a server
/// that asks for fewer, as their only use would be to make its proof
/// cheaper to guess passwords from.
pub const MIN_ITERATIONS: u32 = 4096;

/// PBKDF2 iterations for new credentials.
///
/// With SCRAM the client derives the salted password on every exchange,
/// and the server only once, when the password is set, so the count costs
/// the server nothing per login. It is OWASP's current figure for
/// PBKDF2-HMAC-SHA256: about a quarter of a second on a desktop CPU for each
/// guess an attacker makes from stolen keys. [`ScramClient`] keeps the salted
/// password between exchanges, so a client pays it once, not on every
/// reconnection.
pub const DEFAULT_ITERATIONS: u32 = 600_000;

/// Length of a SHA-256 key, proof or signature.
pub const KEY_LEN: usize = 32;

/// A SHA-256 sized key.
pub type Key = [u8; KEY_LEN];

/// Bytes of salt [`StoredCredentials::generate`] picks.
const SALT_LEN: usize = 16;

/// Random bytes in a nonce, before base64.
const NONCE_LEN: usize = 18;

/// The GS2 header of a client that supports no channel binding, the only
/// kind this implementation speaks.
const GS2_HEADER: &str = "n,,";

type HmacSha256 = Hmac<Sha256>;

fn hmac(key: &[u8], message: &[u8]) -> Key {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC takes keys of any length");
    mac.update(message);
    mac.finalize().into_bytes().into()
}

fn sha256(data: &[u8]) -> Key {
    Sha256::digest(data).into()
}

/// `SaltedPassword := Hi(Normalize(password), salt, i)`, with the password
/// prepared by SASLprep (RFC 4013) when it can be, and taken as it is when
/// SASLprep refuses it, as other implementations do.
pub fn salted_password(password: &str, salt: &[u8], iterations: u32) -> Key {
    let prepared = stringprep::saslprep(password).unwrap_or(password.into());
    pbkdf2::pbkdf2_hmac_array::<Sha256, KEY_LEN>(prepared.as_bytes(), salt, iterations)
}

/// `ClientKey := HMAC(SaltedPassword, "Client Key")`
pub fn client_key(salted_password: &Key) -> Key {
    hmac(salted_password, b"Client Key")
}

/// `ServerKey := HMAC(SaltedPassword, "Server Key")`
pub fn server_key(salted_password: &Key) -> Key {
    hmac(salted_password, b"Server Key")
}

/// `StoredKey := H(ClientKey)`
pub fn stored_key(client_key: &Key) -> Key {
    sha256(client_key)
}

/// Fills `buffer` from the operating system's random source.
fn random(buffer: &mut [u8]) -> Result<(), String> {
    getrandom::fill(buffer).map_err(|e| e.to_string())
}

fn random_nonce() -> Result<String, String> {
    let mut bytes = [0u8; NONCE_LEN];
    random(&mut bytes)?;
    Ok(STANDARD.encode(bytes))
}

/// What a server keeps to check a user's SCRAM-SHA-256 exchanges: enough to
/// verify a proof, nothing to make one. Written and read in the form of
/// RFC 5803, `SCRAM-SHA-256$<iterations>:<salt>$<StoredKey>:<ServerKey>`,
/// base64 inside.
#[derive(Clone, PartialEq, Eq)]
pub struct StoredCredentials {
    pub salt: Vec<u8>,
    pub iterations: u32,
    pub stored_key: Key,
    pub server_key: Key,
}

impl StoredCredentials {
    /// The credentials of `password` with this salt and iteration count.
    pub fn derive(password: &str, salt: &[u8], iterations: u32) -> Self {
        let salted = salted_password(password, salt, iterations);
        Self {
            salt: salt.to_vec(),
            iterations,
            stored_key: stored_key(&client_key(&salted)),
            server_key: server_key(&salted),
        }
    }

    /// The credentials of `password` with a fresh random salt.
    pub fn generate(password: &str, iterations: u32) -> Result<Self, Error> {
        let mut salt = [0u8; SALT_LEN];
        random(&mut salt).map_err(Error::Random)?;
        Ok(Self::derive(password, &salt, iterations))
    }

    /// Stand-in credentials for a user the server does not know, so its
    /// first message looks as it would for a known one: the same
    /// `username` and `secret` give the same salt, and no password gives
    /// the keys. `secret` must stay the server's.
    pub fn decoy(secret: &[u8], username: &str, iterations: u32) -> Self {
        let derive = |purpose: &str| hmac(secret, format!("{purpose}\0{username}").as_bytes());
        Self {
            salt: derive("salt")[..SALT_LEN].to_vec(),
            iterations,
            stored_key: derive("stored key"),
            server_key: derive("server key"),
        }
    }
}

impl fmt::Debug for StoredCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoredCredentials")
            .field("iterations", &self.iterations)
            .finish_non_exhaustive()
    }
}

impl fmt::Display for StoredCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{MECHANISM}${}:{}${}:{}",
            self.iterations,
            STANDARD.encode(&self.salt),
            STANDARD.encode(self.stored_key),
            STANDARD.encode(self.server_key)
        )
    }
}

/// Why stored credentials do not parse. It never repeats the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidCredentials(&'static str);

impl fmt::Display for InvalidCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for InvalidCredentials {}

impl FromStr for StoredCredentials {
    type Err = InvalidCredentials;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let invalid = InvalidCredentials;
        let rest = text
            .strip_prefix(MECHANISM)
            .and_then(|rest| rest.strip_prefix('$'))
            .ok_or(invalid("SCRAM credentials start with `SCRAM-SHA-256$`"))?;
        let (parameters, keys) = rest.split_once('$').ok_or(invalid(
            "SCRAM credentials have the form \
             `SCRAM-SHA-256$<iterations>:<salt>$<StoredKey>:<ServerKey>`",
        ))?;
        let (iterations, salt) = parameters
            .split_once(':')
            .ok_or(invalid("SCRAM credentials lack the salt"))?;
        let (stored_key, server_key) = keys
            .split_once(':')
            .ok_or(invalid("SCRAM credentials lack the ServerKey"))?;
        let iterations = iterations.parse().ok().filter(|&i| i > 0).ok_or(invalid(
            "the SCRAM iteration count is not a positive number",
        ))?;
        let salt = STANDARD
            .decode(salt)
            .ok()
            .filter(|s| !s.is_empty())
            .ok_or(invalid("the SCRAM salt is not base64"))?;
        let key = |text: &str| -> Option<Key> { STANDARD.decode(text).ok()?.try_into().ok() };
        match (key(stored_key), key(server_key)) {
            (Some(stored_key), Some(server_key)) => Ok(Self {
                salt,
                iterations,
                stored_key,
                server_key,
            }),
            _ => Err(invalid("a SCRAM key is not 32 bytes of base64")),
        }
    }
}

/// `user` as a `saslname`, with `,` and `=` escaped.
fn escape_username(username: &str) -> String {
    username.replace('=', "=3D").replace(',', "=2C")
}

/// `name` prepared with SASLprep, or None if it is refused or left empty.
fn prepare_username(name: &str) -> Option<String> {
    stringprep::saslprep(name)
        .ok()
        .map(|prepared| prepared.into_owned())
        .filter(|prepared| !prepared.is_empty())
}

/// A `saslname` back as the user name, or None if an `=` escapes anything
/// else.
fn unescape_username(name: &str) -> Option<String> {
    let mut username = String::with_capacity(name.len());
    let mut rest = name;
    while let Some(at) = rest.find('=') {
        username.push_str(&rest[..at]);
        let escaped = rest.get(at..at + 3)?;
        username.push(match escaped {
            "=2C" => ',',
            "=3D" => '=',
            _ => return None,
        });
        rest = &rest[at + 3..];
    }
    username.push_str(rest);
    Some(username)
}

/// The `name=value` attributes of a message, in order.
fn attributes(message: &str) -> Option<Vec<(char, &str)>> {
    message
        .split(',')
        .map(|attribute| {
            let mut chars = attribute.chars();
            let name = chars.next().filter(char::is_ascii_alphabetic)?;
            Some((name, chars.as_str().strip_prefix('=')?))
        })
        .collect()
}

/// Nonces are printable ASCII without `,`.
fn valid_nonce(nonce: &str) -> bool {
    !nonce.is_empty()
        && nonce
            .bytes()
            .all(|b| (0x21..=0x7e).contains(&b) && b != b',')
}

/// A failure on the client's side of an exchange.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// The server's message does not follow RFC 5802.
    Malformed(&'static str),
    /// The server refused the exchange with this `server-error-value`.
    Server(String),
    /// The server's nonce does not extend the client's.
    NonceMismatch,
    /// The server asks for fewer than [`MIN_ITERATIONS`].
    TooFewIterations(u32),
    /// The server's signature is missing or does not prove it knows the
    /// credentials.
    InvalidServerSignature,
    /// A message arrived when the exchange did not expect one.
    OutOfOrder,
    /// The operating system's random source failed.
    Random(String),
    /// SASLprep refuses the user name, or leaves it empty.
    InvalidUsername,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Malformed(what) => write!(f, "malformed SCRAM message from the server: {what}"),
            Error::Server(error) => write!(f, "the server refused SCRAM authentication: {error}"),
            Error::InvalidUsername => f.write_str("the user name is not valid for SCRAM"),
            Error::NonceMismatch => f.write_str("the server's SCRAM nonce does not extend ours"),
            Error::TooFewIterations(i) => write!(
                f,
                "the server asks for {i} SCRAM iterations, fewer than the {MIN_ITERATIONS} allowed"
            ),
            Error::InvalidServerSignature => {
                f.write_str("the server did not prove it knows the SCRAM credentials")
            }
            Error::OutOfOrder => f.write_str("SCRAM message out of order"),
            Error::Random(e) => write!(f, "no random nonce: {e}"),
        }
    }
}

impl std::error::Error for Error {}

/// The client's side of SCRAM-SHA-256 exchanges, without channel binding.
///
/// Each exchange is [`start`](Self::start), [`respond`](Self::respond) to
/// the server's first message, then [`finish`](Self::finish) with its
/// final one, which proves the server knows the credentials. The client
/// keeps the salted password of the last exchange, so the next one with
/// the same salt and iteration count skips PBKDF2.
#[derive(Clone)]
pub struct ScramClient {
    username: String,
    password: String,
    salted: Option<(Vec<u8>, u32, Key)>,
    state: ClientState,
}

#[derive(Clone)]
enum ClientState {
    Idle,
    SentFirst { nonce: String, first_bare: String },
    SentFinal { server_signature: Key },
}

impl fmt::Debug for ScramClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScramClient")
            .field("username", &self.username)
            .finish_non_exhaustive()
    }
}

impl ScramClient {
    pub fn new(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            username: username.into(),
            password: password.into(),
            salted: None,
            state: ClientState::Idle,
        }
    }

    pub fn username(&self) -> &str {
        &self.username
    }

    /// Begins an exchange with a random nonce: the `client-first-message`.
    pub fn start(&mut self) -> Result<String, Error> {
        let nonce = random_nonce().map_err(Error::Random)?;
        self.start_with_nonce(&nonce)
    }

    /// Begins an exchange with `nonce`, which must be printable ASCII
    /// without `,` and never used before. For tests; use
    /// [`start`](Self::start).
    pub fn start_with_nonce(&mut self, nonce: &str) -> Result<String, Error> {
        let username = prepare_username(&self.username).ok_or(Error::InvalidUsername)?;
        let first_bare = format!("n={},r={nonce}", escape_username(&username));
        let message = format!("{GS2_HEADER}{first_bare}");
        self.state = ClientState::SentFirst {
            nonce: nonce.to_string(),
            first_bare,
        };
        Ok(message)
    }

    /// Answers the `server-first-message` with the
    /// `client-final-message`, holding the proof.
    pub fn respond(&mut self, server_first: &[u8]) -> Result<String, Error> {
        let ClientState::SentFirst { nonce, first_bare } =
            std::mem::replace(&mut self.state, ClientState::Idle)
        else {
            return Err(Error::OutOfOrder);
        };
        let server_first = std::str::from_utf8(server_first)
            .map_err(|_| Error::Malformed("the server's first message is not UTF-8"))?;
        if let Some(error) = server_first.strip_prefix("e=") {
            return Err(Error::Server(error.to_string()));
        }
        let attributes = attributes(server_first).ok_or(Error::Malformed(
            "the server's first message is not attributes",
        ))?;
        let (combined, salt, iterations) = match attributes[..] {
            [('m', _), ..] => {
                return Err(Error::Malformed("the server requires an extension"));
            }
            [('r', combined), ('s', salt), ('i', iterations), ..] => (combined, salt, iterations),
            _ => {
                return Err(Error::Malformed(
                    "the server's first message lacks the nonce, salt or iteration count",
                ))
            }
        };
        if !combined.starts_with(&nonce) || combined.len() == nonce.len() || !valid_nonce(combined)
        {
            return Err(Error::NonceMismatch);
        }
        let salt = STANDARD
            .decode(salt)
            .map_err(|_| Error::Malformed("the salt is not base64"))?;
        let iterations: u32 = iterations
            .parse()
            .map_err(|_| Error::Malformed("the iteration count is not a number"))?;
        if iterations < MIN_ITERATIONS {
            return Err(Error::TooFewIterations(iterations));
        }

        let salted = match &self.salted {
            Some((s, i, key)) if *s == salt && *i == iterations => *key,
            _ => {
                let key = salted_password(&self.password, &salt, iterations);
                self.salted = Some((salt, iterations, key));
                key
            }
        };
        let client_key = client_key(&salted);
        let without_proof = format!("c={},r={combined}", STANDARD.encode(GS2_HEADER));
        let auth_message = format!("{first_bare},{server_first},{without_proof}");
        let signature = hmac(&stored_key(&client_key), auth_message.as_bytes());
        let proof: Vec<u8> = client_key
            .iter()
            .zip(signature)
            .map(|(k, s)| k ^ s)
            .collect();
        self.state = ClientState::SentFinal {
            server_signature: hmac(&server_key(&salted), auth_message.as_bytes()),
        };
        Ok(format!("{without_proof},p={}", STANDARD.encode(proof)))
    }

    /// Checks the `server-final-message`: Ok only if it carries the
    /// signature that proves the server knows the credentials.
    pub fn finish(&mut self, server_final: &[u8]) -> Result<(), Error> {
        let ClientState::SentFinal { server_signature } =
            std::mem::replace(&mut self.state, ClientState::Idle)
        else {
            return Err(Error::OutOfOrder);
        };
        let server_final = std::str::from_utf8(server_final)
            .map_err(|_| Error::Malformed("the server's final message is not UTF-8"))?;
        match attributes(server_final).unwrap_or_default().first() {
            Some(('e', error)) => Err(Error::Server(error.to_string())),
            Some(('v', verifier)) => {
                let given = STANDARD.decode(verifier).unwrap_or_default();
                if bool::from(given.ct_eq(&server_signature)) {
                    Ok(())
                } else {
                    Err(Error::InvalidServerSignature)
                }
            }
            _ => Err(Error::InvalidServerSignature),
        }
    }
}

/// Why a server refuses an exchange, with the `server-error-value` of
/// RFC 5802 as its text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ServerError {
    InvalidEncoding,
    ExtensionsNotSupported,
    InvalidProof,
    ChannelBindingsDontMatch,
    /// The client requires channel binding, which this implementation
    /// does not offer.
    ChannelBindingNotSupported,
    InvalidUsernameEncoding,
    /// The random source failed.
    NoResources,
    OtherError,
}

impl ServerError {
    /// The `server-error-value`.
    pub fn as_str(&self) -> &'static str {
        match self {
            ServerError::InvalidEncoding => "invalid-encoding",
            ServerError::ExtensionsNotSupported => "extensions-not-supported",
            ServerError::InvalidProof => "invalid-proof",
            ServerError::ChannelBindingsDontMatch => "channel-bindings-dont-match",
            ServerError::ChannelBindingNotSupported => "channel-binding-not-supported",
            ServerError::InvalidUsernameEncoding => "invalid-username-encoding",
            ServerError::NoResources => "no-resources",
            ServerError::OtherError => "other-error",
        }
    }
}

impl fmt::Display for ServerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::error::Error for ServerError {}

/// A client's first message, parsed by the server, which looks the user
/// up before answering it with [`respond`](Self::respond).
#[derive(Debug, Clone)]
pub struct ClientFirst {
    gs2_header: String,
    username: String,
    nonce: String,
    bare: String,
}

impl ClientFirst {
    pub fn parse(message: &[u8]) -> Result<Self, ServerError> {
        let message = std::str::from_utf8(message).map_err(|_| ServerError::InvalidEncoding)?;
        let mut parts = message.splitn(3, ',');
        let (Some(flag), Some(authzid), Some(bare)) = (parts.next(), parts.next(), parts.next())
        else {
            return Err(ServerError::InvalidEncoding);
        };
        match flag {
            // A client that could bind channels but believes the server
            // can't is right
            "n" | "y" => {}
            _ if flag.starts_with("p=") => return Err(ServerError::ChannelBindingNotSupported),
            _ => return Err(ServerError::InvalidEncoding),
        }
        let attributes = attributes(bare).ok_or(ServerError::InvalidEncoding)?;
        let (name, nonce) = match attributes[..] {
            [('m', _), ..] => return Err(ServerError::ExtensionsNotSupported),
            [('n', name), ('r', nonce), ..] => (name, nonce),
            _ => return Err(ServerError::InvalidEncoding),
        };
        let username = unescape_username(name)
            .and_then(|u| prepare_username(&u))
            .ok_or(ServerError::InvalidUsernameEncoding)?;
        // Logging in as someone else is not offered
        if !authzid.is_empty() {
            let authzid = authzid
                .strip_prefix("a=")
                .and_then(unescape_username)
                .and_then(|a| prepare_username(&a))
                .ok_or(ServerError::InvalidEncoding)?;
            if authzid != username {
                return Err(ServerError::OtherError);
            }
        }
        if !valid_nonce(nonce) {
            return Err(ServerError::InvalidEncoding);
        }
        Ok(Self {
            gs2_header: format!("{flag},{authzid},"),
            username,
            nonce: nonce.to_string(),
            bare: bare.to_string(),
        })
    }

    /// The user the client logs in as.
    pub fn username(&self) -> &str {
        &self.username
    }

    /// The `server-first-message` for `credentials`, with a random nonce,
    /// and the state that checks the client's proof.
    pub fn respond(
        self,
        credentials: &StoredCredentials,
    ) -> Result<(String, AwaitingProof), ServerError> {
        let nonce = random_nonce().map_err(|_| ServerError::NoResources)?;
        Ok(self.respond_with_nonce(credentials, &nonce))
    }

    /// As [`respond`](Self::respond), with `nonce` as the server's part of
    /// the nonce. For tests.
    pub fn respond_with_nonce(
        self,
        credentials: &StoredCredentials,
        nonce: &str,
    ) -> (String, AwaitingProof) {
        let combined = format!("{}{nonce}", self.nonce);
        let server_first = format!(
            "r={combined},s={},i={}",
            STANDARD.encode(&credentials.salt),
            credentials.iterations
        );
        let awaiting = AwaitingProof {
            channel_binding: STANDARD.encode(&self.gs2_header),
            nonce: combined,
            auth_prefix: format!("{},{server_first}", self.bare),
            stored_key: credentials.stored_key,
            server_key: credentials.server_key,
        };
        (server_first, awaiting)
    }
}

/// The server, having sent its first message, waits for the client's proof.
pub struct AwaitingProof {
    channel_binding: String,
    nonce: String,
    /// `client-first-message-bare + "," + server-first-message`
    auth_prefix: String,
    stored_key: Key,
    server_key: Key,
}

impl fmt::Debug for AwaitingProof {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AwaitingProof").finish_non_exhaustive()
    }
}

impl AwaitingProof {
    /// Checks the `client-final-message`; Ok holds the
    /// `server-final-message`, with the signature that proves the server
    /// knows the credentials.
    pub fn verify(self, client_final: &[u8]) -> Result<String, ServerError> {
        let client_final =
            std::str::from_utf8(client_final).map_err(|_| ServerError::InvalidEncoding)?;
        let (without_proof, proof) = client_final
            .rsplit_once(",p=")
            .ok_or(ServerError::InvalidEncoding)?;
        let attributes = attributes(without_proof).ok_or(ServerError::InvalidEncoding)?;
        let [('c', channel_binding), ('r', nonce), ..] = attributes[..] else {
            return Err(ServerError::InvalidEncoding);
        };
        if channel_binding != self.channel_binding {
            return Err(ServerError::ChannelBindingsDontMatch);
        }
        if nonce != self.nonce {
            return Err(ServerError::OtherError);
        }
        let proof: Key = STANDARD
            .decode(proof)
            .ok()
            .and_then(|p| p.try_into().ok())
            .ok_or(ServerError::InvalidEncoding)?;

        let auth_message = format!("{},{without_proof}", self.auth_prefix);
        let signature = hmac(&self.stored_key, auth_message.as_bytes());
        let mut client_key = [0u8; KEY_LEN];
        for (key, (p, s)) in client_key.iter_mut().zip(proof.iter().zip(signature)) {
            *key = p ^ s;
        }
        if !bool::from(stored_key(&client_key).ct_eq(&self.stored_key)) {
            return Err(ServerError::InvalidProof);
        }
        let server_signature = hmac(&self.server_key, auth_message.as_bytes());
        Ok(format!("v={}", STANDARD.encode(server_signature)))
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn user_names_are_prepared_with_saslprep() {
        // U+2168 ROMAN NUMERAL NINE maps to "IX"; U+00AD SOFT HYPHEN maps to nothing
        let mut client = ScramClient::new("us\u{00AD}er\u{2168}", "pencil");
        let first = client.start_with_nonce("rOprNGfwEbeRWgbNEkqO").unwrap();
        assert_eq!(first, "n,,n=userIX,r=rOprNGfwEbeRWgbNEkqO");

        let parsed = ClientFirst::parse("n,,n=us\u{00AD}er\u{2168},r=abc".as_bytes()).unwrap();
        assert_eq!(parsed.username(), "userIX");
    }

    #[test]
    fn user_names_saslprep_refuses_fail_the_exchange() {
        // U+0007 is a prohibited control character
        let mut client = ScramClient::new("us\u{0007}er", "pencil");
        assert!(matches!(
            client.start_with_nonce("rOprNGfwEbeRWgbNEkqO"),
            Err(Error::InvalidUsername)
        ));
        assert_eq!(
            ClientFirst::parse("n,,n=us\u{0007}er,r=abc".as_bytes()).err(),
            Some(ServerError::InvalidUsernameEncoding)
        );
    }
    use super::*;

    // RFC 7677, section 3
    const CLIENT_NONCE: &str = "rOprNGfwEbeRWgbNEkqO";
    const SERVER_NONCE: &str = "%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0";
    const CLIENT_FIRST: &str = "n,,n=user,r=rOprNGfwEbeRWgbNEkqO";
    const SERVER_FIRST: &str =
        "r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096";
    const CLIENT_FINAL: &str = "c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,\
                                p=dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ=";
    const SERVER_FINAL: &str = "v=6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4=";

    fn rfc_credentials() -> StoredCredentials {
        let salt = STANDARD.decode("W22ZaJ0SNY7soEsUEjb6gQ==").unwrap();
        StoredCredentials::derive("pencil", &salt, 4096)
    }

    #[test]
    fn client_follows_the_rfc_7677_example() {
        let mut client = ScramClient::new("user", "pencil");
        assert_eq!(client.start_with_nonce(CLIENT_NONCE).unwrap(), CLIENT_FIRST);
        assert_eq!(
            client.respond(SERVER_FIRST.as_bytes()).unwrap(),
            CLIENT_FINAL
        );
        client.finish(SERVER_FINAL.as_bytes()).unwrap();
    }

    #[test]
    fn server_follows_the_rfc_7677_example() {
        let first = ClientFirst::parse(CLIENT_FIRST.as_bytes()).unwrap();
        assert_eq!(first.username(), "user");
        let (server_first, awaiting) = first.respond_with_nonce(&rfc_credentials(), SERVER_NONCE);
        assert_eq!(server_first, SERVER_FIRST);
        assert_eq!(
            awaiting.verify(CLIENT_FINAL.as_bytes()).unwrap(),
            SERVER_FINAL
        );
    }

    #[test]
    fn a_wrong_password_fails_at_the_proof() {
        let mut client = ScramClient::new("user", "pen");
        let first = ClientFirst::parse(client.start().unwrap().as_bytes()).unwrap();
        let (server_first, awaiting) = first.respond(&rfc_credentials()).unwrap();
        let client_final = client.respond(server_first.as_bytes()).unwrap();
        assert_eq!(
            awaiting.verify(client_final.as_bytes()).unwrap_err(),
            ServerError::InvalidProof
        );
    }

    #[test]
    fn a_decoy_is_stable_per_user_and_secret_and_matches_no_password() {
        let decoy = StoredCredentials::decoy(b"secret", "nobody", 4096);
        assert_eq!(decoy, StoredCredentials::decoy(b"secret", "nobody", 4096));
        assert_ne!(
            decoy.salt,
            StoredCredentials::decoy(b"secret", "other", 4096).salt
        );
        assert_ne!(
            decoy.salt,
            StoredCredentials::decoy(b"other", "nobody", 4096).salt
        );
        assert_eq!(decoy.salt.len(), SALT_LEN);

        let mut client = ScramClient::new("nobody", "");
        let first = ClientFirst::parse(client.start().unwrap().as_bytes()).unwrap();
        let (server_first, awaiting) = first.respond(&decoy).unwrap();
        let client_final = client.respond(server_first.as_bytes()).unwrap();
        assert_eq!(
            awaiting.verify(client_final.as_bytes()).unwrap_err(),
            ServerError::InvalidProof
        );
    }

    #[test]
    fn the_client_rejects_a_wrong_or_missing_server_signature() {
        for server_final in [
            "v=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
            "v=6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4",
            "v=",
            "x=1",
            "",
        ] {
            let mut client = ScramClient::new("user", "pencil");
            client.start_with_nonce(CLIENT_NONCE).unwrap();
            client.respond(SERVER_FIRST.as_bytes()).unwrap();
            assert_eq!(
                client.finish(server_final.as_bytes()).unwrap_err(),
                Error::InvalidServerSignature,
                "{server_final:?}"
            );
        }
        let mut client = ScramClient::new("user", "pencil");
        client.start_with_nonce(CLIENT_NONCE).unwrap();
        client.respond(SERVER_FIRST.as_bytes()).unwrap();
        assert_eq!(
            client.finish(b"e=invalid-proof").unwrap_err(),
            Error::Server("invalid-proof".into())
        );
        // Accepted without the exchange having run
        let mut client = ScramClient::new("user", "pencil");
        client.start_with_nonce(CLIENT_NONCE).unwrap();
        assert_eq!(
            client.finish(SERVER_FINAL.as_bytes()).unwrap_err(),
            Error::OutOfOrder
        );
    }

    #[test]
    fn the_client_checks_the_server_first_message() {
        let respond = |server_first: &str| {
            let mut client = ScramClient::new("user", "pencil");
            client.start_with_nonce(CLIENT_NONCE).unwrap();
            client.respond(server_first.as_bytes())
        };
        assert_eq!(
            respond("r=someoneelse,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096").unwrap_err(),
            Error::NonceMismatch
        );
        assert_eq!(
            respond("r=rOprNGfwEbeRWgbNEkqO,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096").unwrap_err(),
            Error::NonceMismatch
        );
        assert_eq!(
            respond("r=rOprNGfwEbeRWgbNEkqOx,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4095").unwrap_err(),
            Error::TooFewIterations(4095)
        );
        assert!(matches!(
            respond("m=x,r=rOprNGfwEbeRWgbNEkqOx,s=c2FsdA==,i=4096"),
            Err(Error::Malformed(_))
        ));
        assert!(matches!(
            respond("r=rOprNGfwEbeRWgbNEkqOx,i=4096"),
            Err(Error::Malformed(_))
        ));
        assert_eq!(
            respond("e=unknown-user").unwrap_err(),
            Error::Server("unknown-user".into())
        );
    }

    #[test]
    fn the_salted_password_is_reused_for_the_same_salt_and_count() {
        let mut client = ScramClient::new("user", "pencil");
        client.start_with_nonce(CLIENT_NONCE).unwrap();
        client.respond(SERVER_FIRST.as_bytes()).unwrap();
        let cached = client.salted.clone().unwrap();
        // Another password would give another proof, unless the cache is used
        client.password = "other".into();
        client.start_with_nonce(CLIENT_NONCE).unwrap();
        assert_eq!(
            client.respond(SERVER_FIRST.as_bytes()).unwrap(),
            CLIENT_FINAL
        );
        assert_eq!(client.salted.clone().unwrap(), cached);
    }

    #[test]
    fn the_server_refuses_channel_binding_extensions_and_bad_names() {
        let parse = |message: &str| ClientFirst::parse(message.as_bytes()).map(|_| ());
        assert_eq!(
            parse("p=tls-server-end-point,,n=user,r=abc"),
            Err(ServerError::ChannelBindingNotSupported)
        );
        assert_eq!(parse("y,,n=user,r=abc"), Ok(()));
        assert_eq!(
            parse("n,,m=ext,n=user,r=abc"),
            Err(ServerError::ExtensionsNotSupported)
        );
        assert_eq!(
            parse("n,,n=us=er,r=abc"),
            Err(ServerError::InvalidUsernameEncoding)
        );
        assert_eq!(
            parse("n,,n=,r=abc"),
            Err(ServerError::InvalidUsernameEncoding)
        );
        assert_eq!(parse("n,a=user,n=user,r=abc"), Ok(()));
        assert_eq!(
            parse("n,a=admin,n=user,r=abc"),
            Err(ServerError::OtherError)
        );
        assert_eq!(parse("n,,r=abc"), Err(ServerError::InvalidEncoding));
        assert_eq!(parse("n,,n=user,r=a,c"), Err(ServerError::InvalidEncoding));
        assert_eq!(parse("x,,n=user,r=abc"), Err(ServerError::InvalidEncoding));
        assert_eq!(parse("n,,n=user"), Err(ServerError::InvalidEncoding));
        assert_eq!(parse("n,n=user,r=abc"), Err(ServerError::InvalidEncoding));
    }

    #[test]
    fn user_names_are_escaped_both_ways() {
        let mut client = ScramClient::new("a,b=c", "pencil");
        let first = client.start_with_nonce(CLIENT_NONCE).unwrap();
        assert_eq!(first, "n,,n=a=2Cb=3Dc,r=rOprNGfwEbeRWgbNEkqO");
        assert_eq!(
            ClientFirst::parse(first.as_bytes()).unwrap().username(),
            "a,b=c"
        );
    }

    #[test]
    fn the_server_checks_the_client_final_message() {
        let verify = |client_final: &str| {
            let first = ClientFirst::parse(CLIENT_FIRST.as_bytes()).unwrap();
            let (_, awaiting) = first.respond_with_nonce(&rfc_credentials(), SERVER_NONCE);
            awaiting.verify(client_final.as_bytes())
        };
        let nonce = "rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0";
        let proof = "dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ=";
        assert_eq!(
            verify(&format!("c=eSws,r={nonce},p={proof}")),
            Err(ServerError::ChannelBindingsDontMatch)
        );
        assert_eq!(
            verify(&format!("c=biws,r={nonce}x,p={proof}")),
            Err(ServerError::OtherError)
        );
        assert_eq!(
            verify(&format!("c=biws,r={nonce},p=AAAA")),
            Err(ServerError::InvalidEncoding)
        );
        assert_eq!(
            verify(&format!(
                "c=biws,r={nonce},p=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
            )),
            Err(ServerError::InvalidProof)
        );
        assert_eq!(
            verify(&format!("r={nonce},p={proof}")),
            Err(ServerError::InvalidEncoding)
        );
    }

    #[test]
    fn stored_credentials_round_trip_through_rfc_5803() {
        let credentials = rfc_credentials();
        let text = credentials.to_string();
        assert!(text.starts_with("SCRAM-SHA-256$4096:W22ZaJ0SNY7soEsUEjb6gQ==$"));
        assert_eq!(text.parse::<StoredCredentials>().unwrap(), credentials);
        for bad in [
            "",
            "SCRAM-SHA-1$4096:c2FsdA==$a:b",
            "SCRAM-SHA-256$4096:c2FsdA==",
            "SCRAM-SHA-256$0:c2FsdA==$AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
            "SCRAM-SHA-256$4096:!!$AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
            "SCRAM-SHA-256$4096:c2FsdA==$AAAA:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
        ] {
            let error = bad.parse::<StoredCredentials>().unwrap_err();
            assert!(!error.to_string().contains("c2FsdA"), "{bad}: {error}");
        }
        assert!(!format!("{credentials:?}").contains(&STANDARD.encode(credentials.stored_key)));
    }

    #[test]
    fn generated_credentials_have_fresh_salts() {
        let a = StoredCredentials::generate("pencil", MIN_ITERATIONS).unwrap();
        let b = StoredCredentials::generate("pencil", MIN_ITERATIONS).unwrap();
        assert_ne!(a.salt, b.salt);
        assert_eq!(a.salt.len(), SALT_LEN);
    }
}
