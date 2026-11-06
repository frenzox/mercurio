//! Authentication and access control delegated to HTTP endpoints, in the
//! style of mosquitto-go-auth's HTTP backend: the broker POSTs a JSON
//! description of each check, and a 2xx answer allows it.
//!
//! - `user_url` receives `{"username", "password", "clientid"}`
//! - `acl_url` receives `{"username", "clientid", "topic", "acc"}`, where
//!   `acc` is 1 to receive, 2 to publish and 4 to subscribe
//!
//! ACL answers are cached for `cache_seconds`, as every message is checked.

use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

use mercurio_server::acl::{Access, Authorizer, ClientIdentity};
use mercurio_server::auth::CredentialValidator;
use serde_json::json;
use tracing::warn;

// ponytail: the cache is cleared once it holds this many answers; an LRU if
// clients use more distinct topics than that within `cache_seconds`
const CACHE_LIMIT: usize = 10_000;

type CacheKey = (Option<String>, String, String, u8);

pub struct HttpAuth {
    agent: ureq::Agent,
    user_url: Option<String>,
    acl_url: Option<String>,
    cache_for: Duration,
    cache: Mutex<HashMap<CacheKey, (bool, Instant)>>,
}

impl HttpAuth {
    pub fn new(
        user_url: Option<String>,
        acl_url: Option<String>,
        timeout: Duration,
        cache_for: Duration,
    ) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .build()
            .into();
        Self {
            agent,
            user_url,
            acl_url,
            cache_for,
            cache: Mutex::default(),
        }
    }

    /// Whether `url` answers `body` with a 2xx status. Any error denies.
    fn allows(&self, url: &str, body: serde_json::Value) -> bool {
        // The auth traits are synchronous; this lets the runtime move other
        // tasks off this thread while the request is under way
        let send = || {
            self.agent
                .post(url)
                .header("Content-Type", "application/json")
                .send(body.to_string())
        };
        match tokio::task::block_in_place(send) {
            Ok(_) => true,
            Err(ureq::Error::StatusCode(_)) => false,
            Err(e) => {
                warn!("Auth request to {} failed: {}", url, e);
                false
            }
        }
    }
}

impl CredentialValidator for HttpAuth {
    fn validate(&self, username: &str, password: &[u8]) -> bool {
        let Some(url) = &self.user_url else {
            return false;
        };
        let body = json!({
            "username": username,
            "password": String::from_utf8_lossy(password),
            "clientid": "",
        });
        self.allows(url, body)
    }
}

impl Authorizer for HttpAuth {
    fn authorize(&self, client: &ClientIdentity, topic: &str, access: Access) -> bool {
        let Some(url) = &self.acl_url else {
            return true;
        };
        let acc = match access {
            Access::Read => 1,
            Access::Write => 2,
            Access::Subscribe => 4,
        };
        let key = (
            client.username.clone(),
            client.client_id.clone(),
            topic.to_string(),
            acc,
        );
        if let Some((allowed, at)) = self.cache.lock().unwrap().get(&key) {
            if at.elapsed() < self.cache_for {
                return *allowed;
            }
        }

        let body = json!({
            "username": client.username,
            "clientid": client.client_id,
            "topic": topic,
            "acc": acc,
        });
        let allowed = self.allows(url, body);
        let mut cache = self.cache.lock().unwrap();
        if cache.len() >= CACHE_LIMIT {
            cache.clear();
        }
        cache.insert(key, (allowed, Instant::now()));
        allowed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader, Read, Write},
        net::TcpListener,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
    };

    /// A tiny HTTP server allowing the requests whose body contains `allow`,
    /// counting the requests it gets.
    fn serve(allow: &'static str) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
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
                counter.fetch_add(1, Ordering::SeqCst);
                let status = if String::from_utf8_lossy(&body).contains(allow) {
                    "200 OK"
                } else {
                    "403 Forbidden"
                };
                let response =
                    format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        (url, requests)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_endpoints_decide_and_acl_answers_are_cached() {
        let (user_url, _) = serve("\"password\":\"secret\"");
        let (acl_url, acl_requests) = serve("\"topic\":\"allowed\"");
        let auth = HttpAuth::new(
            Some(user_url),
            Some(acl_url),
            Duration::from_secs(2),
            Duration::from_secs(60),
        );

        assert!(auth.validate("alice", b"secret"));
        assert!(!auth.validate("alice", b"wrong"));

        let alice = ClientIdentity {
            client_id: "c".to_string(),
            username: Some("alice".to_string()),
        };
        assert!(auth.authorize(&alice, "allowed", Access::Write));
        assert!(!auth.authorize(&alice, "other", Access::Write));
        assert!(auth.authorize(&alice, "allowed", Access::Write));
        assert_eq!(acl_requests.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_unreachable_endpoints_deny() {
        let auth = HttpAuth::new(
            Some("http://127.0.0.1:1/".to_string()),
            None,
            Duration::from_secs(2),
            Duration::from_secs(60),
        );
        assert!(!auth.validate("alice", b"secret"));
    }
}
