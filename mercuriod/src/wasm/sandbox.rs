//! What a plugin may reach: nothing on disk, the HTTP hosts and socket
//! addresses its configuration lists, a key-value store, and the log.

use std::{
    collections::BTreeMap,
    future::Future,
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use http_body_util::BodyExt;
use wasmtime::{component::ResourceTable, ResourceLimiter, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{sockets::SocketAddrUse, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpCtxView, WasiHttpHooks, WasiHttpView};

use super::runtime::bindings::mercurio::plugin::{host, types::Level};

/// The data of one plugin copy's store.
pub(crate) struct State {
    wasi: WasiCtx,
    http: WasiHttpCtx,
    hooks: AllowHttp,
    table: ResourceTable,
    pub(crate) limits: Limits,
    kv: Arc<Kv>,
    /// Epoch ticks left for the running call
    pub(crate) ticks_left: u64,
    name: Arc<str>,
}

/// How many bytes a plugin's key-value store may hold, counting each
/// entry's key, value and [`ENTRY_OVERHEAD`]. The store lives outside the
/// copies' memory, so their memory limit doesn't cover it.
const KV_CAPACITY: usize = 16 << 20;

/// What an entry costs the host beyond its key and value, roughly: the
/// map's node and the two allocations. Without it, millions of empty
/// entries would fit in the quota.
const ENTRY_OVERHEAD: usize = 64;

/// The resources one component may create, beyond its memory size. A Rust
/// plugin for wasm32-wasip2 needs 3 instances, 2 tables, 1 memory and
/// fewer than 128 table elements; components from other toolchains may
/// need more, but not this much.
const INSTANCES: usize = 32;
const TABLES: usize = 32;
const MEMORIES: usize = 8;
const TABLE_ELEMENTS: usize = 100_000;

/// How many host resources, such as sockets, streams and HTTP requests, one
/// copy may hold at once. A copy keeps its resources between calls, so
/// without a limit it could hold sockets until the broker runs out of file
/// descriptors.
const RESOURCES: usize = 4096;

/// A plugin's key-value store, shared by its copies.
pub(crate) struct Kv {
    entries: Mutex<Entries>,
    capacity: usize,
}

#[derive(Default)]
struct Entries {
    map: BTreeMap<String, Vec<u8>>,
    /// Bytes of the entries in `map`, see [`Kv::cost`]
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

    fn cost(key: &str, value: &[u8]) -> usize {
        key.len() + value.len() + ENTRY_OVERHEAD
    }

    fn get(&self, key: &str) -> Option<Vec<u8>> {
        self.entries.lock().unwrap().map.get(key).cloned()
    }

    fn set(&self, key: String, value: Vec<u8>) -> Result<(), String> {
        let mut entries = self.entries.lock().unwrap();
        let old = entries.map.get(&key).map_or(0, |old| Kv::cost(&key, old));
        let size = entries.size - old + Kv::cost(&key, &value);
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
            entries.size -= Kv::cost(key, &value);
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
                let addr: SocketAddr = entry
                    .parse()
                    .map_err(|error| anyhow::anyhow!("`{entry}` in allow_sockets: {error}"))?;
                if addr.ip().is_unspecified() || addr.port() == 0 {
                    anyhow::bail!(
                        "`{entry}` in allow_sockets: name the address and port to connect to"
                    );
                }
                Ok(addr)
            })
            .collect::<anyhow::Result<_>>()?;
        Ok(Allowed { http, sockets })
    }
}

impl State {
    pub(crate) fn new(name: Arc<str>, allowed: &Allowed, memory: usize, kv: Arc<Kv>) -> State {
        let sockets = Arc::clone(&allowed.sockets);
        // Creating a socket opens it before any address is checked, so a
        // plugin with no allowed address may not create one at all
        let networked = !sockets.is_empty();
        // Standard input, output and error stay closed: a plugin logs
        // through the host
        let wasi = WasiCtxBuilder::new()
            .allow_tcp(networked)
            .allow_udp(networked)
            .socket_addr_check(move |addr, use_| {
                Box::pin(std::future::ready(socket_allowed(&sockets, addr, use_)))
            })
            .build();
        let mut table = ResourceTable::new();
        table.set_max_capacity(RESOURCES);
        State {
            wasi,
            http: WasiHttpCtx::new(),
            hooks: AllowHttp {
                hosts: Arc::clone(&allowed.http),
                name: Arc::clone(&name),
            },
            table,
            limits: Limits {
                memory,
                used: 0,
                tables: StoreLimitsBuilder::new()
                    .instances(INSTANCES)
                    .tables(TABLES)
                    .memories(MEMORIES)
                    .table_elements(TABLE_ELEMENTS)
                    .build(),
            },
            kv,
            ticks_left: 0,
            name,
        }
    }
}

/// What one copy may allocate: `memory` bytes across all its linear
/// memories, where wasmtime's `StoreLimits` caps each memory on its own;
/// the counts and table sizes are left to `StoreLimits`.
pub(crate) struct Limits {
    memory: usize,
    /// Bytes granted so far. Memories live as long as the copy's store, so
    /// they only grow; a growth that fails after being granted stays
    /// counted, which errs on the side of less memory
    used: usize,
    tables: StoreLimits,
}

impl ResourceLimiter for Limits {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        let more = desired.saturating_sub(current);
        let used = self.used.saturating_add(more);
        if used > self.memory || maximum.is_some_and(|max| desired > max) {
            return Ok(false);
        }
        self.used = used;
        Ok(true)
    }

    fn table_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        self.tables.table_growing(current, desired, maximum)
    }

    fn instances(&self) -> usize {
        self.tables.instances()
    }

    fn tables(&self) -> usize {
        self.tables.tables()
    }

    fn memories(&self) -> usize {
        self.tables.memories()
    }
}

/// Whether a plugin allowed to reach `sockets` may use `addr` as `use_`
/// says: connect to them and exchange datagrams with them, never listen.
fn socket_allowed(sockets: &[SocketAddr], addr: SocketAddr, use_: SocketAddrUse) -> bool {
    match use_ {
        // A connect binds to the wildcard address first, so that bind has
        // to pass for any connect to work
        SocketAddrUse::TcpBind | SocketAddrUse::UdpBind => {
            addr.ip().is_unspecified() && addr.port() == 0
        }
        SocketAddrUse::TcpListen | SocketAddrUse::TcpAccept => false,
        SocketAddrUse::TcpConnect | SocketAddrUse::UdpSend | SocketAddrUse::UdpReceive => {
            sockets.contains(&addr)
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

/// The most bytes of one message a plugin logs; the rest is cut.
const LOG_MESSAGE: usize = 4096;

/// `message` cut to [`LOG_MESSAGE`] bytes, with control characters
/// escaped, so a plugin can't flood the log or forge lines in it.
fn loggable(message: String) -> String {
    let mut end = message.len().min(LOG_MESSAGE);
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    let mut line = String::with_capacity(end);
    for c in message[..end].chars() {
        match c.is_control() {
            true => line.extend(c.escape_default()),
            false => line.push(c),
        }
    }
    if end < message.len() {
        line.push('…');
    }
    line
}

impl super::runtime::bindings::mercurio::plugin::types::Host for State {}

impl host::Host for State {
    async fn log(&mut self, level: Level, message: String) {
        let name = &self.name;
        let message = loggable(message);
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
    fn test_sockets_may_connect_out_but_never_listen() {
        let allowed: SocketAddr = "10.0.0.5:6379".parse().unwrap();
        let wildcard: SocketAddr = "0.0.0.0:0".parse().unwrap();
        let check = |addr, use_| socket_allowed(&[allowed], addr, use_);
        assert!(check(wildcard, SocketAddrUse::TcpBind));
        assert!(check(allowed, SocketAddrUse::TcpConnect));
        assert!(check(allowed, SocketAddrUse::UdpSend));
        assert!(check(allowed, SocketAddrUse::UdpReceive));
        assert!(!check(wildcard, SocketAddrUse::UdpReceive));
        assert!(!check(
            "10.0.0.6:6379".parse().unwrap(),
            SocketAddrUse::TcpConnect
        ));
        assert!(!check(
            "0.0.0.0:1883".parse().unwrap(),
            SocketAddrUse::TcpBind
        ));
        for use_ in [SocketAddrUse::TcpListen, SocketAddrUse::TcpAccept] {
            assert!(!check(wildcard, use_));
            assert!(!check(allowed, use_));
            assert!(!socket_allowed(&[wildcard], wildcard, use_));
        }
    }

    #[test]
    fn test_wildcard_socket_entries_are_refused() {
        for entry in ["0.0.0.0:0", "0.0.0.0:6379", "[::]:6379", "10.0.0.5:0"] {
            let error = Allowed::parse(&[], &[entry.into()]).err().unwrap();
            assert!(error.to_string().contains(&format!("`{entry}`")), "{error}");
        }
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
        let kv = Kv::with_capacity(ENTRY_OVERHEAD + 10);
        kv.set("a".into(), vec![0; 5]).unwrap();
        assert_eq!(kv.set("b".into(), vec![0; 1]), Err("kv store full".into()));
        // Overwriting counts the new value, not both
        kv.set("a".into(), vec![0; 9]).unwrap();
        kv.delete("a");
        kv.set("b".into(), vec![0; 9]).unwrap();
        assert_eq!(kv.get("b").map(|v| v.len()), Some(9));
        assert!(kv.get("a").is_none());
    }

    #[test]
    fn test_logged_messages_stay_on_one_bounded_line() {
        assert_eq!(
            loggable("ok\nINFO forged\r\x1b[31m\tx".into()),
            "ok\\nINFO forged\\r\\u{1b}[31m\\tx"
        );
        let long = loggable("é".repeat(LOG_MESSAGE));
        assert!(long.len() <= LOG_MESSAGE + "…".len(), "{}", long.len());
        assert!(long.ends_with('…'));
        assert_eq!(loggable("plain".into()), "plain");
    }

    #[test]
    fn test_the_memory_limit_covers_all_memories_of_a_copy() {
        use wasmtime::{Engine, Memory, MemoryType, Store};

        const PAGE: usize = 64 << 10;
        let allowed = Allowed::parse(&[], &[]).unwrap();
        let state = State::new("t".into(), &allowed, 100 * PAGE, Arc::default());
        let mut store = Store::new(&Engine::default(), state);
        store.limiter(|state| &mut state.limits);

        let first = Memory::new(&mut store, MemoryType::new(40, None)).unwrap();
        let second = Memory::new(&mut store, MemoryType::new(40, None)).unwrap();
        // 80 pages of 100 are taken, whichever memory asks for more
        assert!(Memory::new(&mut store, MemoryType::new(40, None)).is_err());
        assert!(second.grow(&mut store, 30).is_err());
        assert!(first.grow(&mut store, 20).is_ok());
        assert!(second.grow(&mut store, 1).is_err());
        assert!(Memory::new(&mut store, MemoryType::new(0, None)).is_ok());
    }

    #[test]
    fn test_every_kv_entry_costs_its_overhead() {
        // Empty values still cost their key and the entry
        let kv = Kv::with_capacity(4 * (ENTRY_OVERHEAD + 1));
        for key in ["a", "b", "c", "d"] {
            kv.set(key.into(), Vec::new()).unwrap();
        }
        assert_eq!(kv.set("e".into(), Vec::new()), Err("kv store full".into()));
        kv.delete("a");
        kv.set("e".into(), Vec::new()).unwrap();
    }
}
