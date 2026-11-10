//! Bridges: forward topics between this broker and others.
//!
//! A bridge is a pair of MQTT clients, one connected to this broker in
//! process and one to the remote broker. Each forwards what it receives
//! through the other. Subscriptions use No Local, so a message a bridge
//! forwarded is never sent back to it, and loops can't form even when a
//! topic goes both ways. MQTT 3.1.1 has no No Local, so with such a remote
//! broker the copies it echoes back are recognised and dropped instead.

use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use bytes::Bytes;
use mercurio_client::{
    ConnectOptions, Event, MqttClient, ProtocolVersion, QoS, SubscriptionOptions,
};
use mercurio_core::message::unix_millis;
use mercurio_core::topic::topic_matches_filter;
use mercurio_server::server::LocalConnector;
use serde::Serialize;
use tracing::{error, info, warn};

use crate::config::{BridgeConfig, BridgeProtocol, Direction};

const RETRY_DELAY: Duration = Duration::from_secs(5);

// ponytail: fixed, so the remote broker keeps inbound messages through a
// day of downtime; make it configurable if someone needs another value
const REMOTE_SESSION_EXPIRY: u32 = 24 * 60 * 60;

/// Messages sent to an MQTT 3.1.1 remote broker remembered to recognise its
/// echo
// ponytail: a fixed window; an identical message published on the remote
// broker right after one was forwarded is taken for the echo and dropped
const ECHO_WINDOW: usize = 1000;

/// What one side subscribes to, and how the topics it receives are renamed
/// for the other side.
#[derive(Debug, PartialEq)]
struct Route {
    filter: String,
    qos: QoS,
    strip: String,
    add: String,
}

impl Route {
    fn rename(&self, topic: &str) -> Option<String> {
        let rest = topic.strip_prefix(&self.strip)?;
        Some(format!("{}{}", self.add, rest))
    }
}

/// The topic to forward `topic` to through `route`, if any. A remote
/// broker may send any topic with any subscription id, so only what the
/// route covers is forwarded, and nothing into this broker's own `$`
/// topics, such as `$CONTROL` commands.
fn forwarded_topic(route: Option<&Route>, topic: &str, outbound: bool) -> Option<String> {
    route
        .filter(|route| topic_matches_filter(topic, &route.filter))
        .and_then(|route| route.rename(topic))
        .filter(|renamed| outbound || !renamed.starts_with('$'))
}

/// The routes of the local side (outbound) and of the remote side (inbound).
fn routes(config: &BridgeConfig) -> Result<(Vec<Route>, Vec<Route>), String> {
    let mut outbound = Vec::new();
    let mut inbound = Vec::new();
    for topic in &config.topics {
        let qos = QoS::from(topic.qos);
        if qos == QoS::Invalid {
            return Err(format!(
                "Bridge {}: invalid QoS {} for `{}`",
                config.name, topic.qos, topic.pattern
            ));
        }
        if topic.direction != Direction::In {
            outbound.push(Route {
                filter: format!("{}{}", topic.local_prefix, topic.pattern),
                qos,
                strip: topic.local_prefix.clone(),
                add: topic.remote_prefix.clone(),
            });
        }
        if topic.direction != Direction::Out {
            inbound.push(Route {
                filter: format!("{}{}", topic.remote_prefix, topic.pattern),
                qos,
                strip: topic.remote_prefix.clone(),
                add: topic.local_prefix.clone(),
            });
        }
    }
    Ok((outbound, inbound))
}

/// How a bridge's link to its remote broker stands, for `listBridges`. Its
/// task writes it only when the link comes up or goes down.
pub struct BridgeStatus {
    name: String,
    client_id: String,
    link: Mutex<Link>,
}

struct Link {
    connected: bool,
    /// The remote broker's `host:port`, the last one while disconnected
    remote_address: Option<String>,
    /// Unix seconds since the link came up or went down, or since the
    /// bridge started before it first connected
    since: u64,
}

/// One entry of `listBridges`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BridgeEntry {
    name: String,
    connected: bool,
    remote_address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    connected_since: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    disconnected_since: Option<u64>,
    /// Messages waiting in the bridge's session on this broker
    queued: usize,
}

impl BridgeStatus {
    pub fn new(name: &str, client_id: &str) -> BridgeStatus {
        BridgeStatus {
            name: name.to_string(),
            client_id: client_id.to_string(),
            link: Mutex::new(Link {
                connected: false,
                remote_address: None,
                since: unix_millis() / 1000,
            }),
        }
    }

    /// The bridge's client id, on this broker as on the remote one.
    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    pub fn entry(&self, queued: usize) -> BridgeEntry {
        let link = self.link.lock().unwrap();
        BridgeEntry {
            name: self.name.clone(),
            connected: link.connected,
            remote_address: link.remote_address.clone(),
            connected_since: link.connected.then_some(link.since),
            disconnected_since: (!link.connected).then_some(link.since),
            queued,
        }
    }

    fn set(&self, connected: bool, remote_address: Option<&str>) {
        let mut link = self.link.lock().unwrap();
        link.connected = connected;
        if let Some(address) = remote_address {
            link.remote_address = Some(address.to_string());
        }
        link.since = unix_millis() / 1000;
    }
}

/// Starts the bridge in the background and returns the status it keeps up
/// to date. Fails only on an invalid configuration; an unreachable remote
/// broker is retried.
pub fn spawn(config: BridgeConfig, local: LocalConnector) -> Result<Arc<BridgeStatus>, String> {
    let bridge = prepare(config, local)?;
    let status = Arc::clone(&bridge.status);
    tokio::spawn(bridge.run());
    Ok(status)
}

/// The bridge, ready to run.
fn prepare(config: BridgeConfig, local: LocalConnector) -> Result<Bridge, String> {
    let (outbound, inbound) = routes(&config)?;
    let client_id = config.client_id();
    let addresses = addresses(&config)?;

    // Stays connected while the remote broker is unreachable, so messages
    // to forward wait in its queue meanwhile. The connector only fails
    // once this broker stopped.
    let broker_stopped = Arc::new(AtomicBool::new(false));
    let stopped = Arc::clone(&broker_stopped);
    let local_options = ConnectOptions::new("localhost", 0)
        .client_id(client_id.clone())
        .connector(move || {
            local
                .connect()
                .inspect_err(|_| stopped.store(true, Ordering::Relaxed))
        });

    let protocol_version = match config.protocol_version {
        BridgeProtocol::Mqttv5 => ProtocolVersion::V5,
        BridgeProtocol::Mqttv311 => ProtocolVersion::V3_1_1,
    };
    let remotes = addresses
        .into_iter()
        .map(|(host, port)| {
            let address = crate::address(&host, port);
            let mut options = ConnectOptions::new(host, port)
                .client_id(client_id.clone())
                .protocol_version(protocol_version)
                .clean_start(false)
                .session_expiry(REMOTE_SESSION_EXPIRY)
                .tls(config.tls.enabled);
            if let Some(ca_path) = &config.tls.ca_path {
                options = options.ca_path(ca_path.clone());
            }
            if let (Some(cert), Some(key)) = (&config.tls.cert_path, &config.tls.key_path) {
                options = options.client_cert(cert.clone(), key.clone());
            }
            if let Some(username) = &config.username {
                options = options.username(username.clone());
            }
            if let Some(password) = &config.password {
                options = options.password(password.clone());
            }
            (address, options)
        })
        .collect();

    Ok(Bridge {
        status: Arc::new(BridgeStatus::new(&config.name, &client_id)),
        name: config.name,
        state_topic: config
            .notifications
            .then(|| format!("$SYS/broker/connection/{client_id}/state")),
        echoes: (protocol_version == ProtocolVersion::V3_1_1).then(|| Mutex::new(VecDeque::new())),
        outbound,
        inbound,
        local_options,
        remotes,
        retry_delay: RETRY_DELAY,
        broker_stopped,
    })
}

/// The remote brokers to try, in order.
fn addresses(config: &BridgeConfig) -> Result<Vec<(String, u16)>, String> {
    if config.addresses.is_empty() {
        let host = config.host.clone().ok_or(format!(
            "Bridge {}: needs `host` or `addresses`",
            config.name
        ))?;
        return Ok(vec![(host, config.port)]);
    }
    config
        .addresses
        .iter()
        .map(|address| {
            let (host, port) = address
                .rsplit_once(':')
                .and_then(|(host, port)| Some((host, port.parse().ok()?)))
                .ok_or(format!(
                    "Bridge {}: `{address}` is not host:port",
                    config.name
                ))?;
            let host = host.trim_start_matches('[').trim_end_matches(']');
            Ok((host.to_string(), port))
        })
        .collect()
}

struct Bridge {
    status: Arc<BridgeStatus>,
    name: String,
    /// Where the bridge's state is published, `1` when connected
    state_topic: Option<String>,
    /// Recently forwarded messages, with an MQTT 3.1.1 remote broker
    echoes: Option<Mutex<VecDeque<(String, Bytes)>>>,
    outbound: Vec<Route>,
    inbound: Vec<Route>,
    local_options: ConnectOptions,
    /// Each remote broker's address and options
    remotes: Vec<(String, ConnectOptions)>,
    retry_delay: Duration,
    broker_stopped: Arc<AtomicBool>,
}

impl Bridge {
    /// Forwards messages through the remote brokers, trying them in turn,
    /// until this broker stops. This broker may also close the local
    /// connection, as when another client takes the bridge's client id;
    /// the bridge then connects again.
    async fn run(self) {
        let mut remotes = self.remotes.iter().cycle();
        while let Some(local) = self.connect_locally().await {
            // What was published before may be stale, after a crash or a
            // lost connection
            self.notify(&local, "0").await;
            self.forward_through(&local, &mut remotes).await;
            warn!("Bridge {}: connection to this broker lost", self.name);
        }
    }

    /// The local client, subscribed to the outbound routes, or `None` once
    /// this broker stopped.
    async fn connect_locally(&self) -> Option<MqttClient> {
        loop {
            match MqttClient::connect(self.local_options.clone()).await {
                Ok(local) if self.subscribe(&local, &self.outbound).await => return Some(local),
                Ok(_) => {}
                Err(e) => warn!("Bridge {}: connecting locally failed: {}", self.name, e),
            }
            if self.broker_stopped.load(Ordering::Relaxed) {
                return None;
            }
            tokio::time::sleep(self.retry_delay).await;
        }
    }

    /// Forwards between `local` and the remote brokers until `local`
    /// closes.
    async fn forward_through<'a>(
        &self,
        local: &MqttClient,
        remotes: &mut impl Iterator<Item = &'a (String, ConnectOptions)>,
    ) {
        for (address, options) in remotes {
            let remote = match MqttClient::connect(options.clone()).await {
                Ok(remote) => remote,
                Err(e) => {
                    warn!(
                        "Bridge {}: connecting to {} failed, trying the next address in {:?}: {}",
                        self.name, address, self.retry_delay, e
                    );
                    tokio::time::sleep(self.retry_delay).await;
                    continue;
                }
            };
            if self.subscribe(&remote, &self.inbound).await {
                info!("Bridge {} connected to {}", self.name, address);
                self.status.set(true, Some(address.as_str()));
                self.notify(local, "1").await;
                let local_closed = tokio::select! {
                    _ = self.forward(local, &remote, &self.outbound, true) => true,
                    _ = self.forward(&remote, local, &self.inbound, false) => false,
                };
                self.status.set(false, None);
                if local_closed {
                    let _ = remote.disconnect().await;
                    return;
                }
                error!("Bridge {} lost its connection", self.name);
                self.notify(local, "0").await;
            }
            let _ = remote.disconnect().await;
            tokio::time::sleep(self.retry_delay).await;
        }
    }

    async fn notify(&self, local: &MqttClient, state: &'static str) {
        if let Some(topic) = &self.state_topic {
            let _ = local
                .publish_with_retain(topic, state, QoS::AtLeastOnce, true)
                .await;
        }
    }

    /// Subscribes to every route, each tagged with its index plus one as
    /// subscription identifier where the broker supports them, so `forward`
    /// knows which route a message came through.
    async fn subscribe(&self, client: &MqttClient, routes: &[Route]) -> bool {
        for (index, route) in routes.iter().enumerate() {
            let options = SubscriptionOptions {
                qos: route.qos,
                no_local: true,
                retain_as_pub: true,
                ..Default::default()
            };
            let result = client
                .subscribe_with(&[(&route.filter, options)], Some(index as u32 + 1))
                .await;
            match result {
                Ok(results) if results.iter().all(|r| r.success) => {}
                Ok(results) => warn!(
                    "Bridge {}: subscription to `{}` refused: {:?}",
                    self.name, route.filter, results[0].reason
                ),
                Err(e) => {
                    error!(
                        "Bridge {}: subscribing to `{}` failed: {}",
                        self.name, route.filter, e
                    );
                    return false;
                }
            }
        }
        true
    }

    /// Forwards what `from` receives through `to` until `from` closes;
    /// `outbound` when `to` is the remote broker.
    async fn forward(&self, from: &MqttClient, to: &MqttClient, routes: &[Route], outbound: bool) {
        while let Some(event) = from.recv().await {
            match event {
                Event::Message {
                    topic,
                    payload,
                    qos,
                    retain,
                    properties,
                    subscription_ids,
                    ..
                } => {
                    // MQTT 3.1.1 brokers send no subscription identifiers
                    let route = match subscription_ids.first() {
                        Some(id) => (*id as usize).checked_sub(1).and_then(|i| routes.get(i)),
                        None => routes
                            .iter()
                            .find(|r| topic_matches_filter(&topic, &r.filter)),
                    };
                    let Some(renamed) = forwarded_topic(route, &topic, outbound) else {
                        warn!(
                            "Bridge {}: dropping `{}`, which no route covers",
                            self.name, topic
                        );
                        continue;
                    };
                    if let Some(echoes) = &self.echoes {
                        let mut echoes = echoes.lock().unwrap();
                        if outbound {
                            if echoes.len() == ECHO_WINDOW {
                                echoes.pop_front();
                            }
                            echoes.push_back((renamed.clone(), payload.clone()));
                        } else if let Some(position) = echoes
                            .iter()
                            .position(|(t, p)| *t == topic && *p == payload)
                        {
                            // The remote broker sent back what was forwarded
                            echoes.remove(position);
                            continue;
                        }
                    }
                    let published = to
                        .publish_with_properties(&renamed, payload, qos, retain, properties)
                        .await;
                    if let Err(e) = published {
                        warn!("Bridge {}: forwarding `{}` failed: {}", self.name, topic, e);
                    }
                }
                Event::Reconnected { .. } => {}
                Event::Disconnected { reason } => {
                    warn!("Bridge {} disconnected: {:?}", self.name, reason);
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{forwarded_topic, routes, BridgeStatus, Route};
    use crate::config::Config;
    use mercurio_client::QoS;

    #[test]
    fn test_addresses_replace_host_and_port() {
        let config: Config = toml::from_str(
            r#"
            [[bridges]]
            name = "cloud"
            addresses = ["a.example.com:8883", "[2001:db8::1]:1883"]
            topics = []
            "#,
        )
        .unwrap();
        assert_eq!(
            super::addresses(&config.bridges[0]).unwrap(),
            [
                ("a.example.com".to_string(), 8883),
                ("2001:db8::1".to_string(), 1883)
            ]
        );
    }

    #[test]
    fn test_a_ca_or_client_certificate_turns_tls_on() {
        for tls in [
            "ca_path = \"ca.pem\"",
            "cert_path = \"c.pem\"\nkey_path = \"k.pem\"",
        ] {
            let config: Config = toml::from_str(&format!(
                "[[bridges]]\nname = \"cloud\"\nhost = \"h\"\ntopics = []\n\
                 [bridges.tls]\nenabled = false\n{tls}\n"
            ))
            .unwrap();
            let config = config.bridges.into_iter().next().unwrap();
            let (connector, _) = mercurio_server::server::LocalConnector::new();
            let bridge = super::prepare(config, connector).unwrap();
            let options = format!("{:?}", bridge.remotes[0].1);
            assert!(options.contains("enabled: true"), "{tls}: {options}");
        }
    }

    #[test]
    fn test_inbound_topics_stay_within_their_route() {
        let route = Route {
            filter: "remote/sensors/#".into(),
            qos: QoS::AtMostOnce,
            strip: "remote/".into(),
            add: String::new(),
        };
        assert_eq!(
            forwarded_topic(Some(&route), "remote/sensors/t", false).as_deref(),
            Some("sensors/t")
        );
        // Sent under the route's subscription id, but outside its filter
        assert_eq!(forwarded_topic(Some(&route), "remote/admin", false), None);

        let unprefixed = Route {
            filter: "#".into(),
            qos: QoS::AtMostOnce,
            strip: String::new(),
            add: String::new(),
        };
        assert_eq!(
            forwarded_topic(Some(&unprefixed), "$CONTROL/dynamic-security/v1", false),
            None
        );
        // Outbound, `$` topics a route names are the remote's business
        let sys = Route {
            filter: "$SYS/#".into(),
            ..unprefixed
        };
        assert!(forwarded_topic(Some(&sys), "$SYS/x", true).is_some());
        assert!(forwarded_topic(Some(&sys), "$SYS/x", false).is_none());
    }

    #[test]
    fn test_routes_follow_direction_and_prefixes() {
        let config: Config = toml::from_str(
            r#"
            [[bridges]]
            name = "cloud"
            host = "example.com"
            [[bridges.topics]]
            pattern = "sensors/#"
            qos = 1
            local_prefix = "site/"
            remote_prefix = "plant1/"
            [[bridges.topics]]
            pattern = "commands/#"
            direction = "in"
            "#,
        )
        .unwrap();

        let (outbound, inbound) = routes(&config.bridges[0]).unwrap();
        assert_eq!(
            outbound,
            [Route {
                filter: "site/sensors/#".into(),
                qos: QoS::AtLeastOnce,
                strip: "site/".into(),
                add: "plant1/".into(),
            }]
        );
        assert_eq!(inbound[0].filter, "commands/#");
        assert_eq!(inbound.len(), 1);
        assert_eq!(
            outbound[0].rename("site/sensors/t1").as_deref(),
            Some("plant1/sensors/t1")
        );
    }

    #[test]
    fn test_status_follows_the_link() {
        let status = BridgeStatus::new("cloud", "mercurio-bridge-cloud");
        assert_eq!(status.client_id(), "mercurio-bridge-cloud");
        let entry = serde_json::to_value(status.entry(3)).unwrap();
        assert_eq!(entry["name"], "cloud");
        assert_eq!(entry["connected"], false);
        assert!(entry["remoteAddress"].is_null(), "{entry}");
        assert!(entry["disconnectedSince"].is_u64(), "{entry}");
        assert!(entry.get("connectedSince").is_none(), "{entry}");
        assert_eq!(entry["queued"], 3);

        status.set(true, Some("10.0.0.1:1883"));
        let entry = serde_json::to_value(status.entry(0)).unwrap();
        assert_eq!(entry["connected"], true);
        assert_eq!(entry["remoteAddress"], "10.0.0.1:1883");
        assert!(entry["connectedSince"].is_u64(), "{entry}");
        assert!(entry.get("disconnectedSince").is_none(), "{entry}");

        status.set(false, None);
        let entry = serde_json::to_value(status.entry(0)).unwrap();
        assert_eq!(entry["connected"], false);
        assert_eq!(entry["remoteAddress"], "10.0.0.1:1883");

        status.set(true, Some(&crate::address("2001:db8::1", 1883)));
        let entry = serde_json::to_value(status.entry(0)).unwrap();
        assert_eq!(entry["remoteAddress"], "[2001:db8::1]:1883");
    }

    mod running {
        use std::{
            future::Future,
            net::SocketAddr,
            sync::Arc,
            time::{Duration, Instant},
        };

        use mercurio_client::{ConnectOptions, Event, MqttClient, QoS};
        use mercurio_server::server::{LocalConnector, ServerConfig};
        use tokio::{
            net::{TcpListener, TcpStream},
            sync::{oneshot, watch},
            task::JoinHandle,
            time::timeout,
        };

        use super::super::{prepare, BridgeStatus};
        use crate::config::BridgeConfig;

        const STATE_TOPIC: &str = "$SYS/broker/connection/mercurio-bridge-cloud/state";

        async fn start_remote() -> SocketAddr {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(mercurio_server::server::run(
                listener,
                std::future::pending::<()>(),
            ));
            addr
        }

        /// This broker, reached in process, until `shutdown` completes.
        async fn start_local(shutdown: impl Future + Send + 'static) -> LocalConnector {
            let (connector, local_listener) = LocalConnector::new();
            let mut config = ServerConfig::default();
            config.local_listener = Some(local_listener);
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            tokio::spawn(mercurio_server::server::run_with_config(
                listener, config, shutdown,
            ));
            connector
        }

        /// Passes connections through to `target` while `up` holds, and
        /// cuts them when it stops holding.
        async fn proxy(target: SocketAddr, up: watch::Receiver<bool>) -> u16 {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            tokio::spawn(async move {
                loop {
                    let (mut inbound, _) = listener.accept().await.unwrap();
                    if !*up.borrow() {
                        continue;
                    }
                    let mut up = up.clone();
                    tokio::spawn(async move {
                        let mut outbound = TcpStream::connect(target).await.unwrap();
                        tokio::select! {
                            _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound) => {}
                            _ = up.wait_for(|up| !up) => {}
                        }
                    });
                }
            });
            port
        }

        fn local_client(connector: &LocalConnector, id: &str) -> ConnectOptions {
            let connector = connector.clone();
            ConnectOptions::new("localhost", 0)
                .client_id(id)
                .connector(move || connector.connect())
        }

        /// Runs a bridge forwarding `a/#` to `port`, its local client
        /// pinging every half second and given up on after 1.5 s of silence.
        fn run_bridge(
            connector: &LocalConnector,
            port: u16,
        ) -> (Arc<BridgeStatus>, JoinHandle<()>) {
            let config: BridgeConfig = toml::from_str(&format!(
                "name = \"cloud\"\nhost = \"127.0.0.1\"\nport = {port}\n\
                 [[topics]]\npattern = \"a/#\"\nqos = 1\n"
            ))
            .unwrap();
            let mut bridge = prepare(config, connector.clone()).unwrap();
            bridge.retry_delay = Duration::from_millis(200);
            bridge.local_options = bridge.local_options.keep_alive(1).connect_timeout(1);
            let status = Arc::clone(&bridge.status);
            (status, tokio::spawn(bridge.run()))
        }

        fn connected(status: &BridgeStatus) -> bool {
            status.link.lock().unwrap().connected
        }

        async fn wait_until(what: &str, condition: impl Fn() -> bool) {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !condition() {
                assert!(Instant::now() < deadline, "timed out waiting until {what}");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }

        async fn remote_watcher(remote: SocketAddr) -> MqttClient {
            let watcher =
                MqttClient::connect(ConnectOptions::new("127.0.0.1", remote.port()).client_id("w"))
                    .await
                    .unwrap();
            watcher
                .subscribe(&[("a/#", QoS::AtLeastOnce)])
                .await
                .unwrap();
            watcher
        }

        async fn next_payload(client: &MqttClient) -> String {
            loop {
                match timeout(Duration::from_secs(10), client.recv()).await {
                    Ok(Some(Event::Message { payload, .. })) => {
                        return String::from_utf8(payload.to_vec()).unwrap();
                    }
                    Ok(Some(_)) => {}
                    other => panic!("no message: {other:?}"),
                }
            }
        }

        /// The retained state of the bridge on this broker.
        async fn state(connector: &LocalConnector) -> String {
            let client = MqttClient::connect(local_client(connector, "state"))
                .await
                .unwrap();
            client
                .subscribe(&[(STATE_TOPIC, QoS::AtMostOnce)])
                .await
                .unwrap();
            let state = next_payload(&client).await;
            client.disconnect().await.unwrap();
            state
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn test_bridge_comes_back_after_a_long_outage_with_a_backlog() {
            let remote = start_remote().await;
            let (up, up_rx) = watch::channel(true);
            let port = proxy(remote, up_rx).await;
            let connector = start_local(std::future::pending::<()>()).await;
            let watcher = remote_watcher(remote).await;
            let (status, _bridge) = run_bridge(&connector, port);
            wait_until("connected", || connected(&status)).await;

            up.send_replace(false);
            wait_until("disconnected", || !connected(&status)).await;
            assert_eq!(state(&connector).await, "0");

            // More than the bridge's client holds, over twice its keep-alive
            let publisher = MqttClient::connect(local_client(&connector, "pub"))
                .await
                .unwrap();
            for i in 0..300 {
                publisher
                    .publish(&format!("a/{i}"), i.to_string(), QoS::AtLeastOnce)
                    .await
                    .unwrap();
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
            assert!(!connected(&status));

            up.send_replace(true);
            wait_until("reconnected", || connected(&status)).await;
            for i in 0..300 {
                assert_eq!(next_payload(&watcher).await, i.to_string());
            }
            assert_eq!(state(&connector).await, "1");
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn test_bridge_reconnects_when_this_broker_drops_it() {
            let remote = start_remote().await;
            let connector = start_local(std::future::pending::<()>()).await;
            let watcher = remote_watcher(remote).await;
            let (status, _bridge) = run_bridge(&connector, remote.port());
            wait_until("connected", || connected(&status)).await;

            // Takes the bridge's session over, closing its connection
            let intruder = MqttClient::connect(local_client(&connector, "mercurio-bridge-cloud"))
                .await
                .unwrap();
            assert!(matches!(
                timeout(Duration::from_secs(10), intruder.recv()).await,
                Ok(Some(Event::Disconnected { .. }))
            ));
            wait_until("reconnected", || connected(&status)).await;

            let publisher = MqttClient::connect(local_client(&connector, "pub"))
                .await
                .unwrap();
            publisher
                .publish("a/after", "after", QoS::AtLeastOnce)
                .await
                .unwrap();
            assert_eq!(next_payload(&watcher).await, "after");
            assert_eq!(state(&connector).await, "1");
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn test_bridge_stops_with_this_broker() {
            let remote = start_remote().await;
            let (stop, stopped) = oneshot::channel::<()>();
            let connector = start_local(stopped).await;
            let (status, bridge) = run_bridge(&connector, remote.port());
            wait_until("connected", || connected(&status)).await;

            stop.send(()).unwrap();
            timeout(Duration::from_secs(10), bridge)
                .await
                .expect("the bridge outlived the broker")
                .unwrap();
            assert!(!connected(&status));
        }
    }
}
