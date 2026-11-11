//! Authentication and access control delegated to HTTP endpoints, in the
//! style of mosquitto-go-auth's HTTP backend: the broker POSTs a JSON
//! description of each check, and a 2xx answer allows it.
//!
//! - `user_url` receives `{"username", "password", "clientid"}`
//! - `acl_url` receives `{"username", "clientid", "topic", "acc"}`, where
//!   `acc` is 1 to receive, 2 to publish and 4 to subscribe, and 8 to
//!   publish to a `$CONTROL/...` topic, which administers the broker
//! - `superuser_url` receives `{"username"}`; a 2xx answer lets that user
//!   do everything, `$CONTROL` included, without asking `acl_url`
//!
//! ACL and superuser answers are cached for `cache_seconds`, as every
//! message is checked. Only 2xx (allow) and 4xx (deny) are answers: other
//! statuses and failed requests deny the check at hand, or make the user no
//! superuser for it, and are asked again next time.

use std::{
    collections::{HashMap, VecDeque},
    hash::Hash,
    net::IpAddr,
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};

use mercurio_server::plugin::{
    ready, Access, BoxFuture, ClientInfo, Credentials, Decision, Plugin, PluginKind, PluginStatus,
};
use serde_json::json;
use tokio::sync::{OnceCell, Semaphore};
use tracing::warn;
use ureq::tls::{Certificate, ClientCert, PemItem, PrivateKey, RootCerts, TlsConfig};

use crate::{
    config::{redacted_http_url, HttpAuthConfig},
    reload::Reload,
};

/// The `acc` of a request to publish to a `$CONTROL` topic.
const CONTROL_ACC: u8 = 8;

/// Most ACL answers kept; the oldest goes first once full.
const CACHE_LIMIT: usize = 10_000;

/// Most requests to the backends at once. Each takes a thread of the
/// blocking pool, which storage and DNS lookups share, for up to
/// `timeout_seconds`.
const MAX_REQUESTS: usize = 32;

/// Shortest time between two warnings about failed requests, so a backend
/// that is down does not flood the log.
const WARN_INTERVAL: Duration = Duration::from_secs(10);

type CacheKey = (Option<String>, String, String, u8);

/// A URL and the body sent to it.
type RequestKey = (String, String);

type Answer = Arc<OnceCell<Option<bool>>>;

pub struct HttpAuth {
    /// Replaced when a reload reads the certificate files again
    agent: RwLock<ureq::Agent>,
    timeout: Duration,
    /// The certificate files the agent was built with, when there are any
    tls_files: Option<TlsFiles>,
    user_url: Option<String>,
    acl_url: Option<String>,
    superuser_url: Option<String>,
    cache: Mutex<Cache<CacheKey>>,
    /// Whether each user name is a superuser's
    superusers: Mutex<Cache<String>>,
    failures: Mutex<Throttle>,
    requests: Arc<Semaphore>,
    in_flight: Mutex<HashMap<RequestKey, Answer>>,
}

impl HttpAuth {
    /// The checks `[auth.http]` describes, with its certificate files
    /// loaded.
    pub fn from_config(config: &HttpAuthConfig) -> Result<Self, String> {
        let mut auth = Self::new(
            config.user_url.clone(),
            config.acl_url.clone(),
            Duration::from_secs(config.timeout_seconds),
            Duration::from_secs(config.cache_seconds),
        );
        let files = TlsFiles {
            ca_path: config.ca_path.clone(),
            cert_path: config.cert_path.clone(),
            key_path: config.key_path.clone(),
        };
        if let Some(tls) = tls_config(&files)? {
            auth.agent = RwLock::new(agent(auth.timeout, tls));
            auth.tls_files = Some(files);
        }
        auth.superuser_url = config.superuser_url.clone();
        Ok(auth)
    }

    pub fn new(
        user_url: Option<String>,
        acl_url: Option<String>,
        timeout: Duration,
        cache_for: Duration,
    ) -> Self {
        if let Some(url) = user_url
            .as_deref()
            .filter(|url| is_plaintext_to_remote(url))
        {
            warn!(
                "[auth.http] user_url {} is plain HTTP to another host, so passwords cross the network unencrypted; use https://",
                redacted_http_url(url)
            );
        }
        Self {
            agent: RwLock::new(agent(timeout, TlsConfig::default())),
            timeout,
            tls_files: None,
            user_url,
            acl_url,
            superuser_url: None,
            cache: Mutex::new(Cache::new(CACHE_LIMIT, cache_for)),
            superusers: Mutex::new(Cache::new(CACHE_LIMIT, cache_for)),
            failures: Mutex::default(),
            requests: Arc::new(Semaphore::new(MAX_REQUESTS)),
            in_flight: Mutex::default(),
        }
    }

    /// Whether `url` allows `body`: Some(true) on a 2xx status, Some(false)
    /// on a 4xx one, and None when the backend gave no answer. Identical
    /// checks made meanwhile wait for the same request.
    async fn allows(&self, url: &str, body: serde_json::Value) -> Option<bool> {
        let waiter = Waiter::join(&self.in_flight, (url.to_string(), body.to_string()));
        *waiter
            .answer
            .get_or_init(|| self.request(url, waiter.key.1.clone()))
            .await
    }

    async fn request(&self, url: &str, body: String) -> Option<bool> {
        // The semaphore is never closed
        let permit = Arc::clone(&self.requests).acquire_owned().await.ok()?;
        let agent = self.agent.read().unwrap().clone();
        let target = url.to_string();
        // The permit goes with the request, which runs on after a check
        // that gave up waiting for it
        let send = move || {
            let _permit = permit;
            agent
                .post(&target)
                .header("Content-Type", "application/json")
                .send(body)
                .map(|_| ())
        };
        let failure = match tokio::task::spawn_blocking(send).await {
            Ok(Ok(())) => return Some(true),
            Ok(Err(ureq::Error::StatusCode(400..=499))) => return Some(false),
            Ok(Err(ureq::Error::StatusCode(status))) => format!("status {status}"),
            // Its message repeats the URL, which may hold secrets
            Ok(Err(ureq::Error::BadUri(_))) => "bad URL".to_string(),
            Ok(Err(e)) => e.to_string(),
            Err(e) => e.to_string(),
        };
        if let Some(skipped) = self.failures.lock().unwrap().allow(Instant::now()) {
            let skipped = match skipped {
                0 => String::new(),
                n => format!(" ({n} more failed since the last warning)"),
            };
            warn!(
                "Auth request to {} failed, so it is denied: {}{}",
                redacted_http_url(url),
                failure,
                skipped
            );
        }
        None
    }
}

fn agent(timeout: Duration, tls: TlsConfig) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .tls_config(tls)
        .build()
        .into()
}

/// The certificate files `[auth.http]` names.
struct TlsFiles {
    ca_path: Option<String>,
    cert_path: Option<String>,
    key_path: Option<String>,
}

/// The TLS settings of `config`'s certificate files, None when it names
/// none.
fn tls_config(config: &TlsFiles) -> Result<Option<TlsConfig>, String> {
    let mut tls = TlsConfig::builder();
    match &config.ca_path {
        Some(path) => tls = tls.root_certs(RootCerts::new_with_certs(&certificates(path)?)),
        None if config.cert_path.is_none() && config.key_path.is_none() => return Ok(None),
        None => {}
    }
    match (&config.cert_path, &config.key_path) {
        (Some(cert), Some(key)) => {
            let key = private_key(key)?;
            tls = tls.client_cert(Some(ClientCert::new_with_certs(&certificates(cert)?, key)));
        }
        (None, None) => {}
        _ => return Err("[auth.http] cert_path and key_path go together".to_string()),
    }
    Ok(Some(tls.build()))
}

fn read_pem(path: &str) -> Result<Vec<PemItem<'static>>, String> {
    let contents = std::fs::read(path).map_err(|e| format!("Failed to read {path}: {e}"))?;
    ureq::tls::parse_pem(&contents)
        .collect::<Result<_, _>>()
        .map_err(|e| format!("Invalid PEM in {path}: {e}"))
}

/// The certificates of the PEM file at `path`, at least one.
fn certificates(path: &str) -> Result<Vec<Certificate<'static>>, String> {
    let certificates: Vec<_> = read_pem(path)?
        .into_iter()
        .filter_map(|item| match item {
            PemItem::Certificate(certificate) => Some(certificate),
            _ => None,
        })
        .collect();
    if certificates.is_empty() {
        return Err(format!("{path} holds no certificate"));
    }
    Ok(certificates)
}

/// The first private key of the PEM file at `path`.
fn private_key(path: &str) -> Result<PrivateKey<'static>, String> {
    read_pem(path)?
        .into_iter()
        .find_map(|item| match item {
            PemItem::PrivateKey(key) => Some(key),
            _ => None,
        })
        .ok_or_else(|| format!("{path} holds no private key"))
}

/// The certificate files, read again for the requests that follow; on an
/// error the previous ones stay in use. Reported by the CA's path, or the
/// client certificate's.
impl Reload for HttpAuth {
    fn reload(&self) -> Result<(), String> {
        let Some(files) = &self.tls_files else {
            return Ok(());
        };
        if let Some(tls) = tls_config(files)? {
            *self.agent.write().unwrap() = agent(self.timeout, tls);
        }
        Ok(())
    }

    fn path(&self) -> &str {
        self.tls_files
            .as_ref()
            .and_then(|files| files.ca_path.as_deref().or(files.cert_path.as_deref()))
            .unwrap_or_default()
    }

    fn item(&self) -> &str {
        "http-auth"
    }
}

impl HttpAuth {
    /// Whether `[auth.http]` names certificate files, for a reload to read.
    pub fn has_tls_files(&self) -> bool {
        self.tls_files.is_some()
    }
}

/// Whether `url` is plain HTTP to a host other than this machine's loopback.
pub(crate) fn is_plaintext_to_remote(url: &str) -> bool {
    let Some(rest) = url
        .get(..7)
        .filter(|scheme| scheme.eq_ignore_ascii_case("http://"))
        .map(|_| &url[7..])
    else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = match host_port.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => host_port.split(':').next().unwrap_or_default(),
    };
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback());
    !loopback
}

/// One check waiting on a request that identical checks share.
struct Waiter<'a> {
    in_flight: &'a Mutex<HashMap<RequestKey, Answer>>,
    key: RequestKey,
    answer: Answer,
}

impl<'a> Waiter<'a> {
    fn join(in_flight: &'a Mutex<HashMap<RequestKey, Answer>>, key: RequestKey) -> Self {
        let answer = Arc::clone(in_flight.lock().unwrap().entry(key.clone()).or_default());
        Self {
            in_flight,
            key,
            answer,
        }
    }
}

impl Drop for Waiter<'_> {
    /// Takes the request out of the map once it is answered, so later checks
    /// ask again, or once its last waiter gives up. Waiters are only added
    /// under the lock, so the count read under it is exact.
    fn drop(&mut self) {
        let Ok(mut in_flight) = self.in_flight.lock() else {
            return;
        };
        let ours = in_flight
            .get(&self.key)
            .is_some_and(|answer| Arc::ptr_eq(answer, &self.answer));
        // One count for the map, one for this waiter
        let last = Arc::strong_count(&self.answer) == 2;
        if ours && (self.answer.initialized() || last) {
            in_flight.remove(&self.key);
        }
    }
}

/// ACL answers, each kept for the same time, so the order they came in is
/// the order they expire in.
struct Cache<K> {
    limit: usize,
    ttl: Duration,
    answers: HashMap<K, (bool, Instant)>,
    order: VecDeque<(K, Instant)>,
}

impl<K: Hash + Eq + Clone> Cache<K> {
    fn new(limit: usize, ttl: Duration) -> Self {
        Self {
            limit,
            ttl,
            answers: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn get(&self, key: &K) -> Option<bool> {
        let (allowed, at) = self.answers.get(key)?;
        (at.elapsed() < self.ttl).then_some(*allowed)
    }

    fn insert(&mut self, key: K, allowed: bool) {
        let now = Instant::now();
        while let Some((_, at)) = self.order.front() {
            if now.duration_since(*at) < self.ttl && self.answers.len() < self.limit {
                break;
            }
            let (oldest, at) = self.order.pop_front().unwrap();
            // Unless the key was answered again since
            if self.answers.get(&oldest).is_some_and(|(_, t)| *t == at) {
                self.answers.remove(&oldest);
            }
        }
        self.answers.insert(key.clone(), (allowed, now));
        self.order.push_back((key, now));
    }
}

/// Lets one warning through per `WARN_INTERVAL`, counting the others.
#[derive(Default)]
struct Throttle {
    last: Option<Instant>,
    skipped: u64,
}

impl Throttle {
    /// Whether to warn at `now`, with how many warnings were skipped since
    /// the last one.
    fn allow(&mut self, now: Instant) -> Option<u64> {
        if self
            .last
            .is_some_and(|last| now.duration_since(last) < WARN_INTERVAL)
        {
            self.skipped += 1;
            return None;
        }
        self.last = Some(now);
        Some(std::mem::take(&mut self.skipped))
    }
}

impl Plugin for HttpAuth {
    fn name(&self) -> &str {
        "http-auth"
    }

    fn status(&self) -> PluginStatus {
        let mut hooks = Vec::new();
        if self.user_url.is_some() {
            hooks.push("authenticate");
        }
        if self.acl_url.is_some() || self.superuser_url.is_some() {
            hooks.extend(["authorize", "authorize_control"]);
        }
        PluginStatus::new(self.name(), PluginKind::Builtin).with_hooks(&hooks)
    }

    fn authenticate<'a>(
        &'a self,
        client: &'a ClientInfo,
        credentials: &'a Credentials<'a>,
    ) -> BoxFuture<'a, Decision> {
        let Some(url) = &self.user_url else {
            return ready(Decision::Defer);
        };
        let body = json!({
            "username": credentials.username,
            "password": String::from_utf8_lossy(credentials.password),
            "clientid": client.client_id,
        });
        Box::pin(async move {
            match self.allows(url, body).await {
                Some(true) => Decision::Allow,
                _ => Decision::Deny,
            }
        })
    }

    fn authorize<'a>(
        &'a self,
        client: &'a ClientInfo,
        topic: &'a str,
        access: Access,
    ) -> BoxFuture<'a, Decision> {
        let acc = match access {
            Access::Read => 1,
            Access::Write => 2,
            Access::Subscribe => 4,
            // A kind of access added later is denied until handled
            _ => return ready(Decision::Deny),
        };
        self.check(client, topic, acc)
    }

    /// Allows superusers; asks the ACL endpoint about others with an `acc`
    /// of its own, so a backend that grants publishing to every topic
    /// doesn't grant this as well.
    fn authorize_control<'a>(
        &'a self,
        client: &'a ClientInfo,
        topic: &'a str,
    ) -> BoxFuture<'a, Decision> {
        self.check(client, topic, CONTROL_ACC)
    }
}

impl HttpAuth {
    fn check<'a>(
        &'a self,
        client: &'a ClientInfo,
        topic: &'a str,
        acc: u8,
    ) -> BoxFuture<'a, Decision> {
        if self.acl_url.is_none() && self.superuser_url.is_none() {
            return ready(Decision::Defer);
        }
        let decision = |allowed| match allowed {
            true => Decision::Allow,
            false => Decision::Deny,
        };
        let key = (
            client.username.clone(),
            client.client_id.clone(),
            topic.to_string(),
            acc,
        );
        Box::pin(async move {
            if self.is_superuser(client).await {
                return Decision::Allow;
            }
            let Some(url) = &self.acl_url else {
                return Decision::Defer;
            };
            if let Some(allowed) = self.cache.lock().unwrap().get(&key) {
                return decision(allowed);
            }
            let body = json!({
                "username": client.username,
                "clientid": client.client_id,
                "topic": topic,
                "acc": acc,
            });
            let Some(allowed) = self.allows(url, body).await else {
                return Decision::Deny;
            };
            self.cache.lock().unwrap().insert(key, allowed);
            decision(allowed)
        })
    }

    /// Whether the superuser endpoint answers 2xx for `client`'s user name;
    /// never for a client without one, nor when the endpoint fails.
    async fn is_superuser(&self, client: &ClientInfo) -> bool {
        let (Some(url), Some(username)) = (&self.superuser_url, &client.username) else {
            return false;
        };
        if let Some(superuser) = self.superusers.lock().unwrap().get(username) {
            return superuser;
        }
        let Some(superuser) = self.allows(url, json!({ "username": username })).await else {
            return false;
        };
        self.superusers
            .lock()
            .unwrap()
            .insert(username.clone(), superuser);
        superuser
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::{
        future::Future,
        io::{BufRead, BufReader, Read, Write},
        net::TcpListener,
        sync::atomic::{AtomicUsize, Ordering},
        task::Poll,
    };

    pub(crate) struct Backend {
        pub(crate) url: String,
        pub(crate) requests: Arc<AtomicUsize>,
        most_at_once: Arc<AtomicUsize>,
    }

    /// A tiny HTTP server answering each request, after `delay`, with the
    /// status `status` picks from its body and its index, counting the
    /// requests it gets and the most it handles at once.
    fn backend(
        delay: Duration,
        status: impl Fn(&str, usize) -> &'static str + Send + Sync + 'static,
    ) -> Backend {
        backend_with_body(delay, move |body, index| {
            (status(body, index), String::new())
        })
    }

    /// Like `backend`, with `respond` giving the body of each answer too.
    pub(crate) fn backend_with_body(
        delay: Duration,
        respond: impl Fn(&str, usize) -> (&'static str, String) + Send + Sync + 'static,
    ) -> Backend {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let most_at_once = Arc::new(AtomicUsize::new(0));
        let (counter, most) = (Arc::clone(&requests), Arc::clone(&most_at_once));
        let respond = Arc::new(respond);
        std::thread::spawn(move || {
            let active = Arc::new(AtomicUsize::new(0));
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let (counter, most, active, respond) = (
                    Arc::clone(&counter),
                    Arc::clone(&most),
                    Arc::clone(&active),
                    Arc::clone(&respond),
                );
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut length = 0;
                    loop {
                        let mut line = String::new();
                        reader.read_line(&mut line).unwrap();
                        if let Some(value) = line.to_lowercase().strip_prefix("content-length:") {
                            length = value.trim().parse().unwrap();
                        }
                        if line == "\r\n" {
                            break;
                        }
                    }
                    let mut body = vec![0; length];
                    reader.read_exact(&mut body).unwrap();
                    let index = counter.fetch_add(1, Ordering::SeqCst);
                    let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                    most.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(delay);
                    let (status, body) = respond(&String::from_utf8_lossy(&body), index);
                    active.fetch_sub(1, Ordering::SeqCst);
                    let response = format!(
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes());
                });
            }
        });
        Backend {
            url,
            requests,
            most_at_once,
        }
    }

    /// A backend allowing the requests whose body contains `allow`.
    fn serve(allow: &'static str) -> (String, Arc<AtomicUsize>) {
        let backend = backend(Duration::ZERO, move |body, _| {
            if body.contains(allow) {
                "200 OK"
            } else {
                "403 Forbidden"
            }
        });
        (backend.url, backend.requests)
    }

    const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/http-auth");

    /// An HTTPS backend allowing every request, with a certificate of the
    /// test CA, that only takes clients presenting a certificate of the
    /// same CA.
    fn tls_backend() -> String {
        use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};

        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let certs = |name: &str| {
            CertificateDer::pem_file_iter(format!("{FIXTURES}/{name}"))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        };
        let mut roots = rustls::RootCertStore::empty();
        roots.add_parsable_certificates(certs("ca.pem"));
        let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
            .build()
            .unwrap();
        let key = PrivateKeyDer::from_pem_file(format!("{FIXTURES}/server-key.pem")).unwrap();
        let config = Arc::new(
            rustls::ServerConfig::builder()
                .with_client_cert_verifier(verifier)
                .with_single_cert(certs("server.pem"), key)
                .unwrap(),
        );
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("https://{}/", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let connection = rustls::ServerConnection::new(Arc::clone(&config)).unwrap();
                let tls = rustls::StreamOwned::new(connection, stream.unwrap());
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(tls);
                    let mut length = 0;
                    loop {
                        let mut line = String::new();
                        // A client that gave up on the handshake
                        if reader.read_line(&mut line).is_err() {
                            return;
                        }
                        if let Some(value) = line.to_lowercase().strip_prefix("content-length:") {
                            length = value.trim().parse().unwrap();
                        }
                        if line == "\r\n" {
                            break;
                        }
                    }
                    let mut body = vec![0; length];
                    reader.read_exact(&mut body).unwrap();
                    let mut tls = reader.into_inner();
                    let response =
                        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                    let _ = tls.write_all(response.as_bytes());
                    tls.conn.send_close_notify();
                    let _ = tls.flush();
                });
            }
        });
        url
    }

    fn tls_config(url: &str, files: [Option<&str>; 3]) -> HttpAuthConfig {
        let path = |name: Option<&str>| name.map(|name| format!("{FIXTURES}/{name}"));
        HttpAuthConfig {
            user_url: None,
            acl_url: Some(url.to_string()),
            superuser_url: None,
            timeout_seconds: 5,
            cache_seconds: 60,
            ca_path: path(files[0]),
            cert_path: path(files[1]),
            key_path: path(files[2]),
        }
    }

    #[tokio::test]
    async fn test_backends_are_reached_with_the_ca_and_client_certificate_given() {
        let url = tls_backend();
        let alice = ClientInfo::new("c", Some("alice".to_string()));
        let all = [Some("ca.pem"), Some("client.pem"), Some("client-key.pem")];
        let auth = HttpAuth::from_config(&tls_config(&url, all)).unwrap();
        assert_eq!(
            auth.authorize(&alice, "t", Access::Write).await,
            Decision::Allow
        );

        // Without the client certificate the backend refuses the
        // connection, and with the bundled roots its certificate is unknown
        for files in [
            [Some("ca.pem"), None, None],
            [None, Some("client.pem"), Some("client-key.pem")],
        ] {
            let auth = HttpAuth::from_config(&tls_config(&url, files)).unwrap();
            assert_eq!(
                auth.authorize(&alice, "t", Access::Write).await,
                Decision::Deny,
                "{files:?}"
            );
        }
    }

    #[tokio::test]
    async fn test_reload_rereads_the_tls_files_and_keeps_them_on_error() {
        use crate::reload::Reload;

        let url = tls_backend();
        let alice = ClientInfo::new("c", Some("alice".to_string()));
        let dir = std::env::temp_dir().join(format!("mercuriod-http-tls-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = |name: &str| dir.join(name).to_str().unwrap().to_string();
        // A CA that did not sign the backend's certificate
        let other_ca = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../mercurio-server/tests/fixtures/cert.pem"
        );
        std::fs::copy(other_ca, file("ca.pem")).unwrap();
        for name in ["client.pem", "client-key.pem"] {
            std::fs::copy(format!("{FIXTURES}/{name}"), file(name)).unwrap();
        }
        let mut config = tls_config(&url, [None; 3]);
        config.ca_path = Some(file("ca.pem"));
        config.cert_path = Some(file("client.pem"));
        config.key_path = Some(file("client-key.pem"));
        let auth = HttpAuth::from_config(&config).unwrap();
        assert_eq!(
            (auth.item(), auth.path()),
            ("http-auth", file("ca.pem").as_str())
        );
        assert_eq!(
            auth.authorize(&alice, "t", Access::Write).await,
            Decision::Deny
        );

        std::fs::copy(format!("{FIXTURES}/ca.pem"), file("ca.pem")).unwrap();
        auth.reload().unwrap();
        assert_eq!(
            auth.authorize(&alice, "t", Access::Write).await,
            Decision::Allow
        );

        std::fs::write(file("client-key.pem"), "not a key").unwrap();
        let error = auth.reload().unwrap_err();
        assert!(error.contains("client-key.pem"), "{error}");
        assert_eq!(
            auth.authorize(&alice, "u", Access::Write).await,
            Decision::Allow
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn test_tls_files_must_load() {
        let url = "https://127.0.0.1:1/";
        for (files, expected) in [
            ([Some("missing.pem"), None, None], "missing.pem"),
            ([Some("client-key.pem"), None, None], "no certificate"),
            ([None, Some("client.pem"), None], "together"),
            ([None, None, Some("client-key.pem")], "together"),
            (
                [None, Some("client.pem"), Some("client.pem")],
                "no private key",
            ),
            (
                [None, Some("client-key.pem"), Some("client-key.pem")],
                "no certificate",
            ),
        ] {
            let error = HttpAuth::from_config(&tls_config(url, files))
                .err()
                .unwrap_or_else(|| panic!("{files:?} loaded"));
            assert!(error.contains(expected), "{files:?}: {error}");
        }
    }

    /// Checks with `superuser` as the superuser endpoint and `acl` as the
    /// ACL one.
    fn superuser_auth(superuser: String, acl: Option<String>) -> HttpAuth {
        HttpAuth {
            superuser_url: Some(superuser),
            ..HttpAuth::new(None, acl, Duration::from_secs(5), Duration::from_secs(60))
        }
    }

    #[tokio::test]
    async fn test_superusers_skip_acl_checks_and_may_administer() {
        let superusers = backend(Duration::ZERO, |body, _| {
            if body == r#"{"username":"alice"}"# {
                "200 OK"
            } else {
                "403 Forbidden"
            }
        });
        let (acl_url, acl_requests) = serve("\"topic\":\"public\"");
        let auth = superuser_auth(superusers.url, Some(acl_url));
        let alice = ClientInfo::new("c", Some("alice".to_string()));
        let bob = ClientInfo::new("c", Some("bob".to_string()));
        for _ in 0..2 {
            for access in [Access::Write, Access::Read, Access::Subscribe] {
                assert_eq!(
                    auth.authorize(&alice, "private", access).await,
                    Decision::Allow
                );
            }
            assert_eq!(
                auth.authorize_control(&alice, "$CONTROL/broker/v1").await,
                Decision::Allow
            );
            assert_eq!(
                auth.authorize(&bob, "private", Access::Write).await,
                Decision::Deny
            );
            assert_eq!(
                auth.authorize(&bob, "public", Access::Write).await,
                Decision::Allow
            );
            assert_eq!(
                auth.authorize_control(&bob, "$CONTROL/broker/v1").await,
                Decision::Deny
            );
        }
        // Each user's answer is cached, as topic answers are
        assert_eq!(superusers.requests.load(Ordering::SeqCst), 2);
        assert_eq!(acl_requests.load(Ordering::SeqCst), 3);

        // Clients without a user name are never superusers
        let anonymous = ClientInfo::new("c", None);
        assert_eq!(
            auth.authorize(&anonymous, "private", Access::Write).await,
            Decision::Deny
        );
        assert_eq!(superusers.requests.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_a_failing_superuser_endpoint_means_not_a_superuser() {
        let superusers = backend(Duration::ZERO, |_, index| match index {
            0 => "500 Internal Server Error",
            _ => "200 OK",
        });
        let (acl_url, _) = serve("\"topic\":\"public\"");
        let auth = superuser_auth(superusers.url, Some(acl_url.clone()));
        let alice = ClientInfo::new("c", Some("alice".to_string()));
        assert_eq!(
            auth.authorize(&alice, "private", Access::Write).await,
            Decision::Deny
        );
        assert_eq!(
            auth.authorize(&alice, "public", Access::Write).await,
            Decision::Allow
        );
        // Not cached, so asked again once the endpoint is back
        assert_eq!(
            auth.authorize(&alice, "private", Access::Write).await,
            Decision::Allow
        );

        let auth = superuser_auth("http://127.0.0.1:1/".to_string(), Some(acl_url));
        assert_eq!(
            auth.authorize_control(&alice, "$CONTROL/broker/v1").await,
            Decision::Deny
        );
    }

    #[tokio::test]
    async fn test_without_an_acl_endpoint_others_are_left_to_the_next_check() {
        let (superuser_url, _) = serve("alice");
        let auth = superuser_auth(superuser_url, None);
        let alice = ClientInfo::new("c", Some("alice".to_string()));
        let bob = ClientInfo::new("c", Some("bob".to_string()));
        assert_eq!(
            auth.authorize(&alice, "t", Access::Write).await,
            Decision::Allow
        );
        assert_eq!(
            auth.authorize(&bob, "t", Access::Write).await,
            Decision::Defer
        );
        assert_eq!(
            auth.authorize_control(&bob, "$CONTROL/broker/v1").await,
            Decision::Defer
        );
        assert_eq!(
            auth.status(),
            PluginStatus::new("http-auth", PluginKind::Builtin)
                .with_hooks(&["authorize", "authorize_control"])
        );
    }

    #[tokio::test]
    async fn test_control_is_asked_with_its_own_access() {
        let (acl_url, _) = serve("\"acc\":8");
        let auth = HttpAuth::new(
            None,
            Some(acl_url),
            Duration::from_secs(2),
            Duration::from_secs(60),
        );
        let alice = ClientInfo::new("c", Some("alice".to_string()));
        let topic = "$CONTROL/broker/v1";
        assert_eq!(
            auth.authorize(&alice, topic, Access::Write).await,
            Decision::Deny
        );
        assert_eq!(auth.authorize_control(&alice, topic).await, Decision::Allow);
    }

    async fn check(auth: &HttpAuth, password: &str) -> Decision {
        let client = ClientInfo::new("c", None);
        let credentials = Credentials::new("alice", password.as_bytes());
        auth.authenticate(&client, &credentials).await
    }

    #[tokio::test]
    async fn test_endpoints_decide_and_acl_answers_are_cached() {
        let (user_url, _) = serve("\"password\":\"secret\"");
        let (acl_url, acl_requests) = serve("\"topic\":\"allowed\"");
        let auth = HttpAuth::new(
            Some(user_url),
            Some(acl_url),
            Duration::from_secs(2),
            Duration::from_secs(60),
        );

        assert_eq!(check(&auth, "secret").await, Decision::Allow);
        assert_eq!(check(&auth, "wrong").await, Decision::Deny);

        let alice = ClientInfo::new("c", Some("alice".to_string()));
        assert_eq!(
            auth.authorize(&alice, "allowed", Access::Write).await,
            Decision::Allow
        );
        assert_eq!(
            auth.authorize(&alice, "other", Access::Write).await,
            Decision::Deny
        );
        assert_eq!(
            auth.authorize(&alice, "allowed", Access::Write).await,
            Decision::Allow
        );
        assert_eq!(acl_requests.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_unreachable_endpoints_deny() {
        let auth = HttpAuth::new(
            Some("http://127.0.0.1:1/".to_string()),
            None,
            Duration::from_secs(2),
            Duration::from_secs(60),
        );
        assert_eq!(check(&auth, "secret").await, Decision::Deny);
    }

    fn acl_auth(url: String) -> HttpAuth {
        HttpAuth::new(
            None,
            Some(url),
            Duration::from_secs(10),
            Duration::from_secs(60),
        )
    }

    #[tokio::test]
    async fn test_backend_failures_deny_without_being_cached() {
        let backend = backend(Duration::ZERO, |_, index| match index {
            0 => "500 Internal Server Error",
            1 => "503 Service Unavailable",
            _ => "200 OK",
        });
        let auth = acl_auth(backend.url);
        let alice = ClientInfo::new("c", Some("alice".to_string()));
        for expected in [
            Decision::Deny,
            Decision::Deny,
            Decision::Allow,
            Decision::Allow,
        ] {
            assert_eq!(auth.authorize(&alice, "t", Access::Write).await, expected);
        }
        assert_eq!(backend.requests.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn test_unreachable_acl_endpoint_is_asked_again() {
        let backend = backend(Duration::ZERO, |_, _| "200 OK");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let unreachable = format!("http://{}/", listener.local_addr().unwrap());
        drop(listener);
        let auth = acl_auth(unreachable);
        let alice = ClientInfo::new("c", Some("alice".to_string()));
        assert_eq!(
            auth.authorize(&alice, "t", Access::Write).await,
            Decision::Deny
        );
        // The same check, once the backend is back
        let auth = HttpAuth {
            acl_url: Some(backend.url),
            ..auth
        };
        assert_eq!(
            auth.authorize(&alice, "t", Access::Write).await,
            Decision::Allow
        );
    }

    /// Runs every future concurrently on this task.
    pub(crate) async fn join_all<F: Future>(
        futures: impl IntoIterator<Item = F>,
    ) -> Vec<F::Output> {
        let mut futures: Vec<_> = futures.into_iter().map(Box::pin).collect();
        let mut outputs: Vec<Option<F::Output>> = futures.iter().map(|_| None).collect();
        std::future::poll_fn(|cx| {
            let mut pending = false;
            for (future, output) in futures.iter_mut().zip(outputs.iter_mut()) {
                if output.is_none() {
                    match future.as_mut().poll(cx) {
                        Poll::Ready(value) => *output = Some(value),
                        Poll::Pending => pending = true,
                    }
                }
            }
            if pending {
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        })
        .await;
        outputs.into_iter().map(Option::unwrap).collect()
    }

    #[tokio::test]
    async fn test_identical_checks_share_one_request() {
        let backend = backend(Duration::from_millis(200), |_, _| "200 OK");
        let auth = acl_auth(backend.url);
        let alice = ClientInfo::new("c", Some("alice".to_string()));
        let checks = (0..10).map(|_| auth.authorize(&alice, "t", Access::Write));
        for decision in join_all(checks).await {
            assert_eq!(decision, Decision::Allow);
        }
        assert_eq!(backend.requests.load(Ordering::SeqCst), 1);
        assert!(auth.in_flight.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_abandoned_checks_leave_nothing_in_flight() {
        let backend = backend(Duration::from_millis(200), |_, _| "200 OK");
        let auth = acl_auth(backend.url);
        let alice = ClientInfo::new("c", Some("alice".to_string()));
        let check = auth.authorize(&alice, "t", Access::Write);
        assert!(tokio::time::timeout(Duration::from_millis(50), check)
            .await
            .is_err());
        assert!(auth.in_flight.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_requests_in_flight_are_bounded() {
        let backend = backend(Duration::from_millis(50), |_, _| "200 OK");
        let auth = acl_auth(backend.url);
        let alice = ClientInfo::new("c", Some("alice".to_string()));
        let topics: Vec<String> = (0..MAX_REQUESTS * 3).map(|i| format!("t/{i}")).collect();
        let checks = topics
            .iter()
            .map(|topic| auth.authorize(&alice, topic, Access::Write));
        for decision in join_all(checks).await {
            assert_eq!(decision, Decision::Allow);
        }
        assert_eq!(backend.requests.load(Ordering::SeqCst), MAX_REQUESTS * 3);
        let most = backend.most_at_once.load(Ordering::SeqCst);
        assert!(most <= MAX_REQUESTS, "{most} requests at once");
    }

    fn key(topic: &str) -> CacheKey {
        (None, "c".to_string(), topic.to_string(), 2)
    }

    #[test]
    fn test_full_cache_evicts_the_oldest_answer() {
        let mut cache = Cache::new(2, Duration::from_secs(60));
        cache.insert(key("a"), true);
        cache.insert(key("b"), false);
        cache.insert(key("c"), true);
        assert_eq!(cache.get(&key("a")), None);
        assert_eq!(cache.get(&key("b")), Some(false));
        assert_eq!(cache.get(&key("c")), Some(true));
        cache.insert(key("d"), true);
        assert_eq!(cache.get(&key("b")), None);
        assert_eq!(cache.answers.len(), 2);
    }

    #[test]
    fn test_expired_answers_are_dropped_on_insert() {
        let mut cache = Cache::new(10, Duration::ZERO);
        for topic in ["a", "b", "c"] {
            cache.insert(key(topic), true);
            assert_eq!(cache.get(&key(topic)), None);
        }
        assert_eq!(cache.answers.len(), 1);
        assert_eq!(cache.order.len(), 1);
    }

    #[test]
    fn test_an_answer_given_again_outlives_its_first_entry() {
        let mut cache = Cache::new(2, Duration::from_millis(50));
        cache.insert(key("a"), true);
        std::thread::sleep(Duration::from_millis(60));
        cache.insert(key("a"), false);
        cache.insert(key("b"), true);
        assert_eq!(cache.get(&key("a")), Some(false));
        assert_eq!(cache.order.len(), 2);
    }

    #[test]
    fn test_failure_warnings_are_rate_limited() {
        let mut throttle = Throttle::default();
        let start = Instant::now();
        assert_eq!(throttle.allow(start), Some(0));
        assert_eq!(throttle.allow(start + Duration::from_secs(1)), None);
        assert_eq!(throttle.allow(start + Duration::from_secs(2)), None);
        assert_eq!(throttle.allow(start + WARN_INTERVAL), Some(2));
        assert_eq!(throttle.allow(start + WARN_INTERVAL), None);
    }

    #[test]
    fn test_plain_http_to_another_host_is_flagged() {
        for url in [
            "http://auth.example.com/user",
            "HTTP://10.0.0.1:8080/user",
            "http://u:p@auth.example.com/user",
            "http://[2001:db8::1]/user",
            "http://localhost.example.com/user",
        ] {
            assert!(is_plaintext_to_remote(url), "{url}");
        }
        for url in [
            "https://auth.example.com/user",
            "http://localhost:8080/user",
            "http://LOCALHOST/user",
            "http://127.0.0.1/user",
            "http://127.1.2.3:80/user?x=@y",
            "http://[::1]:8080/user",
        ] {
            assert!(!is_plaintext_to_remote(url), "{url}");
        }
    }

    #[test]
    fn test_status_is_builtin() {
        use mercurio_server::plugin::{Plugin, PluginKind, PluginStatus};

        let status = |user: Option<&str>, acl: Option<&str>| {
            HttpAuth::new(
                user.map(String::from),
                acl.map(String::from),
                Duration::from_secs(1),
                Duration::from_secs(1),
            )
            .status()
        };
        let both = status(Some("http://u"), Some("http://a"));
        assert_eq!(
            both,
            PluginStatus::new("http-auth", PluginKind::Builtin).with_hooks(&[
                "authenticate",
                "authorize",
                "authorize_control"
            ])
        );
        assert_eq!(
            status(Some("http://u"), None),
            PluginStatus::new("http-auth", PluginKind::Builtin).with_hooks(&["authenticate"])
        );
        assert_eq!(
            status(None, Some("http://a")),
            PluginStatus::new("http-auth", PluginKind::Builtin)
                .with_hooks(&["authorize", "authorize_control"])
        );
    }
}
