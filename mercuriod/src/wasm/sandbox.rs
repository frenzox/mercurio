//! What a plugin may reach: nothing on disk, the HTTP hosts and socket
//! addresses its configuration lists, a key-value store, and the log.

use std::{
    collections::BTreeMap,
    future::Future,
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use http_body_util::BodyExt;
use wasmtime::{component::ResourceTable, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{sockets::SocketAddrUse, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpCtxView, WasiHttpHooks, WasiHttpView};

use super::runtime::bindings::mercurio::plugin::{host, types::Level};

/// The data of one plugin copy's store.
pub(crate) struct State {
    wasi: WasiCtx,
    http: WasiHttpCtx,
    hooks: AllowHttp,
    table: ResourceTable,
    pub(crate) limits: StoreLimits,
    kv: Arc<Kv>,
    /// Epoch ticks left for the running call
    pub(crate) ticks_left: u64,
    name: Arc<str>,
}

/// How many bytes, keys and values together, a plugin's key-value store
/// may hold. The store lives outside the copies' memory, so their memory
/// limit doesn't cover it.
// ponytail: fixed for now; Task 4 may make it configurable
const KV_CAPACITY: usize = 16 << 20;

/// The resources one component may create, beyond its memory size. A Rust
/// plugin for wasm32-wasip2 needs 3 instances, 2 tables, 1 memory and
/// fewer than 128 table elements; components from other toolchains may
/// need more, but not this much.
const INSTANCES: usize = 32;
const TABLES: usize = 32;
const MEMORIES: usize = 8;
const TABLE_ELEMENTS: usize = 100_000;

/// A plugin's key-value store, shared by its copies.
pub(crate) struct Kv {
    entries: Mutex<Entries>,
    capacity: usize,
}

#[derive(Default)]
struct Entries {
    map: BTreeMap<String, Vec<u8>>,
    /// Bytes of the keys and values in `map`
    size: usize,
}

impl Default for Kv {
    fn default() -> Self {
        Kv::with_capacity(KV_CAPACITY)
    }
}

impl Kv {
    fn with_capacity(capacity: usize) -> Kv {
        Kv {
            entries: Mutex::default(),
            capacity,
        }
    }

    fn get(&self, key: &str) -> Option<Vec<u8>> {
        self.entries.lock().unwrap().map.get(key).cloned()
    }

    fn set(&self, key: String, value: Vec<u8>) -> Result<(), String> {
        let mut entries = self.entries.lock().unwrap();
        let old = entries.map.get(&key).map_or(0, |old| key.len() + old.len());
        let size = entries.size - old + key.len() + value.len();
        if size > self.capacity {
            return Err("kv store full".into());
        }
        entries.size = size;
        entries.map.insert(key, value);
        Ok(())
    }

    fn delete(&self, key: &str) {
        let mut entries = self.entries.lock().unwrap();
        if let Some(value) = entries.map.remove(key) {
            entries.size -= key.len() + value.len();
        }
    }

    fn list(&self, prefix: &str) -> Vec<String> {
        self.entries
            .lock()
            .unwrap()
            .map
            .range(prefix.to_string()..)
            .map(|(key, _)| key)
            .take_while(|key| key.starts_with(prefix))
            .cloned()
            .collect()
    }
}

/// The network a plugin may reach.
pub(crate) struct Allowed {
    /// `host:port`, lower case, with the default port filled in
    pub(crate) http: Arc<[String]>,
    pub(crate) sockets: Arc<[SocketAddr]>,
}

impl Allowed {
    /// Parses the `allow_http` and `allow_sockets` entries of a plugin's
    /// configuration. An HTTP host without a port means HTTPS, port 443.
    pub(crate) fn parse(http: &[String], sockets: &[String]) -> anyhow::Result<Allowed> {
        // ponytail: exact hosts and addresses only; CIDR ranges wait until
        // someone needs them
        let http = http
            .iter()
            .map(|entry| {
                let authority: http::uri::Authority = entry
                    .parse()
                    .map_err(|error| anyhow::anyhow!("`{entry}` in allow_http: {error}"))?;
                let port = authority.port_u16().unwrap_or(443);
                Ok(format!("{}:{port}", authority.host()).to_ascii_lowercase())
            })
            .collect::<anyhow::Result<_>>()?;
        let sockets = sockets
            .iter()
            .map(|entry| {
                entry
                    .parse()
                    .map_err(|error| anyhow::anyhow!("`{entry}` in allow_sockets: {error}"))
            })
            .collect::<anyhow::Result<_>>()?;
        Ok(Allowed { http, sockets })
    }
}

impl State {
    pub(crate) fn new(name: Arc<str>, allowed: &Allowed, memory: usize, kv: Arc<Kv>) -> State {
        let sockets = Arc::clone(&allowed.sockets);
        // Standard input, output and error stay closed: a plugin logs
        // through the host
        let wasi = WasiCtxBuilder::new()
            .allow_tcp(true)
            .allow_udp(true)
            .socket_addr_check(move |addr, use_| {
                let allowed = match use_ {
                    // A connect binds to the wildcard address first, so
                    // that bind has to pass for any connect to work
                    SocketAddrUse::TcpBind | SocketAddrUse::UdpBind => {
                        addr.ip().is_unspecified() && addr.port() == 0
                    }
                    _ => sockets.contains(&addr),
                };
                Box::pin(std::future::ready(allowed))
            })
            .build();
        State {
            wasi,
            http: WasiHttpCtx::new(),
            hooks: AllowHttp {
                hosts: Arc::clone(&allowed.http),
                name: Arc::clone(&name),
            },
            table: ResourceTable::new(),
            limits: StoreLimitsBuilder::new()
                .memory_size(memory)
                .instances(INSTANCES)
                .tables(TABLES)
                .memories(MEMORIES)
                .table_elements(TABLE_ELEMENTS)
                .build(),
            kv,
            ticks_left: 0,
            name,
        }
    }
}

impl WasiView for State {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl WasiHttpView for State {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: &mut self.hooks,
        }
    }
}

/// Sends a plugin's HTTP requests to the hosts it may reach and refuses
/// the others.
struct AllowHttp {
    hosts: Arc<[String]>,
    name: Arc<str>,
}

type IoFuture = Box<dyn Future<Output = wasmtime_wasi_http::Result<()>> + Send>;

impl WasiHttpHooks for AllowHttp {
    fn send_request(
        &mut self,
        request: http::Request<wasmtime_wasi_http::WasiBody>,
        options: Option<wasmtime_wasi_http::RequestOptions>,
        _io: IoFuture,
    ) -> Box<
        dyn Future<
                Output = wasmtime_wasi_http::Result<(
                    http::Response<wasmtime_wasi_http::WasiBody>,
                    IoFuture,
                )>,
            > + Send,
    > {
        let target = authority(request.uri());
        if !target.as_ref().is_some_and(|t| self.hosts.contains(t)) {
            tracing::warn!(
                plugin = %self.name,
                "Refused an HTTP request to {}",
                target.as_deref().unwrap_or("no host")
            );
            return Box::new(async { Err(wasmtime_wasi_http::Error::HttpRequestDenied) });
        }
        Box::new(async move {
            let (response, io) = wasmtime_wasi_http::default_send_request(request, options).await?;
            Ok((
                response.map(BodyExt::boxed_unsync),
                Box::new(io) as IoFuture,
            ))
        })
    }
}

/// `host:port` of a request, lower case, with the scheme's default port
/// when the URI has none.
fn authority(uri: &http::Uri) -> Option<String> {
    let authority = uri.authority()?;
    let port = match authority.port_u16() {
        Some(port) => port,
        None if uri.scheme() == Some(&http::uri::Scheme::HTTP) => 80,
        None => 443,
    };
    Some(format!("{}:{port}", authority.host()).to_ascii_lowercase())
}

impl super::runtime::bindings::mercurio::plugin::types::Host for State {}

impl host::Host for State {
    async fn log(&mut self, level: Level, message: String) {
        let name = &self.name;
        match level {
            Level::Trace => tracing::trace!(plugin = %name, "{message}"),
            Level::Debug => tracing::debug!(plugin = %name, "{message}"),
            Level::Info => tracing::info!(plugin = %name, "{message}"),
            Level::Warn => tracing::warn!(plugin = %name, "{message}"),
            Level::Error => tracing::error!(plugin = %name, "{message}"),
        }
    }

    async fn kv_get(&mut self, key: String) -> Option<Vec<u8>> {
        self.kv.get(&key)
    }

    async fn kv_set(&mut self, key: String, value: Vec<u8>) -> Result<(), String> {
        self.kv.set(key, value)
    }

    async fn kv_delete(&mut self, key: String) {
        self.kv.delete(&key);
    }

    async fn kv_list(&mut self, prefix: String) -> Vec<String> {
        self.kv.list(&prefix)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_allowed_http_hosts_get_a_port() {
        let allowed = Allowed::parse(
            &[
                "Audit.example.com".into(),
                "10.0.0.5:8080".into(),
                "plain.example.com:80".into(),
            ],
            &[],
        )
        .unwrap();
        assert_eq!(
            &*allowed.http,
            [
                "audit.example.com:443",
                "10.0.0.5:8080",
                "plain.example.com:80"
            ]
        );
    }

    #[test]
    fn test_a_bad_socket_entry_is_named() {
        let error = Allowed::parse(&[], &["localhost".into()]).err().unwrap();
        assert!(error.to_string().contains("`localhost`"), "{error}");
    }

    #[test]
    fn test_requests_match_with_the_default_port() {
        let uri = |s: &str| s.parse::<http::Uri>().unwrap();
        assert_eq!(
            authority(&uri("https://Audit.example.com/x")).as_deref(),
            Some("audit.example.com:443")
        );
        assert_eq!(
            authority(&uri("http://example.com/")).as_deref(),
            Some("example.com:80")
        );
        assert_eq!(
            authority(&uri("http://example.com:8080/")).as_deref(),
            Some("example.com:8080")
        );
    }

    #[tokio::test]
    async fn test_kv_list_returns_keys_with_the_prefix_in_order() {
        use host::Host;
        let allowed = Allowed::parse(&[], &[]).unwrap();
        let mut state = State::new("t".into(), &allowed, 1 << 20, Arc::default());
        for key in ["b/2", "a", "b/1", "c"] {
            state.kv_set(key.into(), Vec::new()).await.unwrap();
        }
        assert_eq!(state.kv_list("b/".into()).await, ["b/1", "b/2"]);
        assert_eq!(state.kv_list(String::new()).await.len(), 4);
    }

    #[test]
    fn test_the_kv_store_refuses_values_past_its_capacity() {
        let kv = Kv::with_capacity(10);
        kv.set("a".into(), vec![0; 5]).unwrap();
        assert_eq!(kv.set("b".into(), vec![0; 4]), Err("kv store full".into()));
        // Overwriting counts the new value, not both
        kv.set("a".into(), vec![0; 9]).unwrap();
        kv.delete("a");
        kv.set("b".into(), vec![0; 9]).unwrap();
        assert_eq!(kv.get("b").map(|v| v.len()), Some(9));
        assert!(kv.get("a").is_none());
    }
}
