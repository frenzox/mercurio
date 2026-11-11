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

use mercurio_server::plugin::{
    ready, Access, BoxFuture, ClientInfo, Credentials, Decision, Plugin, PluginKind, PluginStatus,
};
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
    async fn allows(&self, url: &str, body: serde_json::Value) -> bool {
        let (agent, url) = (self.agent.clone(), url.to_string());
        let send = move || {
            agent
                .post(&url)
                .header("Content-Type", "application/json")
                .send(body.to_string())
                .map(|_| ())
                .map_err(|e| (url, e))
        };
        match tokio::task::spawn_blocking(send).await {
            Ok(Ok(())) => true,
            Ok(Err((_, ureq::Error::StatusCode(_)))) => false,
            Ok(Err((url, e))) => {
                warn!("Auth request to {} failed: {}", url, e);
                false
            }
            Err(_) => false,
        }
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
        if self.acl_url.is_some() {
            hooks.push("authorize");
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
            if self.allows(url, body).await {
                Decision::Allow
            } else {
                Decision::Deny
            }
        })
    }

    fn authorize<'a>(
        &'a self,
        client: &'a ClientInfo,
        topic: &'a str,
        access: Access,
    ) -> BoxFuture<'a, Decision> {
        let Some(url) = &self.acl_url else {
            return ready(Decision::Defer);
        };
        let decision = |allowed| match allowed {
            true => Decision::Allow,
            false => Decision::Deny,
        };
        let acc = match access {
            Access::Read => 1,
            Access::Write => 2,
            Access::Subscribe => 4,
            // A kind of access added later is denied until handled
            _ => return ready(Decision::Deny),
        };
        let key = (
            client.username.clone(),
            client.client_id.clone(),
            topic.to_string(),
            acc,
        );
        Box::pin(async move {
            if let Some((allowed, at)) = self.cache.lock().unwrap().get(&key) {
                if at.elapsed() < self.cache_for {
                    return decision(*allowed);
                }
            }
            let body = json!({
                "username": client.username,
                "clientid": client.client_id,
                "topic": topic,
                "acc": acc,
            });
            let allowed = self.allows(url, body).await;
            let mut cache = self.cache.lock().unwrap();
            // Making room one entry at a time: clearing it all would let one
            // client, publishing to many topics, send every other client's
            // checks back to the HTTP service
            if cache.len() >= CACHE_LIMIT {
                cache.retain(|_, (_, at)| at.elapsed() < self.cache_for);
            }
            if cache.len() >= CACHE_LIMIT {
                // ponytail: an arbitrary entry, an LRU if hit rates ever matter
                if let Some(evicted) = cache.keys().next().cloned() {
                    cache.remove(&evicted);
                }
            }
            cache.insert(key, (allowed, Instant::now()));
            decision(allowed)
        })
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
            PluginStatus::new("http-auth", PluginKind::Builtin)
                .with_hooks(&["authenticate", "authorize"])
        );
        assert_eq!(
            status(Some("http://u"), None),
            PluginStatus::new("http-auth", PluginKind::Builtin).with_hooks(&["authenticate"])
        );
        assert_eq!(
            status(None, Some("http://a")),
            PluginStatus::new("http-auth", PluginKind::Builtin).with_hooks(&["authorize"])
        );
    }
}
