//! Authentication and access control delegated to HTTP endpoints, in the
//! style of mosquitto-go-auth's HTTP backend: the broker POSTs a JSON
//! description of each check, and a 2xx answer allows it.
//!
//! - `user_url` receives `{"username", "password", "clientid"}`
//! - `acl_url` receives `{"username", "clientid", "topic", "acc"}`, where
//!   `acc` is 1 to receive, 2 to publish and 4 to subscribe
//!
//! ACL answers are cached for `cache_seconds`, as every message is checked.
//! Only 2xx (allow) and 4xx (deny) are answers: other statuses and failed
//! requests deny the check at hand and are asked again next time.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use mercurio_server::plugin::{
    ready, Access, BoxFuture, ClientInfo, Credentials, Decision, Plugin, PluginKind, PluginStatus,
};
use serde_json::json;
use tokio::sync::{OnceCell, Semaphore};
use tracing::warn;

use crate::config::redacted_http_url;

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
    agent: ureq::Agent,
    user_url: Option<String>,
    acl_url: Option<String>,
    cache: Mutex<Cache>,
    failures: Mutex<Throttle>,
    requests: Arc<Semaphore>,
    in_flight: Mutex<HashMap<RequestKey, Answer>>,
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
            cache: Mutex::new(Cache::new(CACHE_LIMIT, cache_for)),
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
        let (agent, target) = (self.agent.clone(), url.to_string());
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
struct Cache {
    limit: usize,
    ttl: Duration,
    answers: HashMap<CacheKey, (bool, Instant)>,
    order: VecDeque<(CacheKey, Instant)>,
}

impl Cache {
    fn new(limit: usize, ttl: Duration) -> Self {
        Self {
            limit,
            ttl,
            answers: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn get(&self, key: &CacheKey) -> Option<bool> {
        let (allowed, at) = self.answers.get(key)?;
        (at.elapsed() < self.ttl).then_some(*allowed)
    }

    fn insert(&mut self, key: CacheKey, allowed: bool) {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        future::Future,
        io::{BufRead, BufReader, Read, Write},
        net::TcpListener,
        sync::atomic::{AtomicUsize, Ordering},
        task::Poll,
    };

    struct Backend {
        url: String,
        requests: Arc<AtomicUsize>,
        most_at_once: Arc<AtomicUsize>,
    }

    /// A tiny HTTP server answering each request, after `delay`, with the
    /// status `status` picks from its body and its index, counting the
    /// requests it gets and the most it handles at once.
    fn backend(
        delay: Duration,
        status: impl Fn(&str, usize) -> &'static str + Send + Sync + 'static,
    ) -> Backend {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let most_at_once = Arc::new(AtomicUsize::new(0));
        let (counter, most) = (Arc::clone(&requests), Arc::clone(&most_at_once));
        let status = Arc::new(status);
        std::thread::spawn(move || {
            let active = Arc::new(AtomicUsize::new(0));
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let (counter, most, active, status) = (
                    Arc::clone(&counter),
                    Arc::clone(&most),
                    Arc::clone(&active),
                    Arc::clone(&status),
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
                    let status = status(&String::from_utf8_lossy(&body), index);
                    active.fetch_sub(1, Ordering::SeqCst);
                    let response = format!(
                        "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
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
    async fn join_all<F: Future>(futures: impl IntoIterator<Item = F>) -> Vec<F::Output> {
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
