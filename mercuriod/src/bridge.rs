//! Bridges: forward topics between this broker and others.
//!
//! A bridge is a pair of MQTT clients, one connected to this broker in
//! process and one to the remote broker. Each forwards what it receives
//! through the other. Subscriptions use No Local, so a message a bridge
//! forwarded is never sent back to it, and loops can't form even when a
//! topic goes both ways. MQTT 3.1.1 has no No Local, so with such a remote
//! broker the copies it echoes back are recognised and dropped instead.

use std::{collections::VecDeque, sync::Mutex, time::Duration};

use bytes::Bytes;
use mercurio_client::{
    ConnectOptions, Event, MqttClient, ProtocolVersion, QoS, SubscriptionOptions,
};
use mercurio_core::topic::topic_matches_filter;
use mercurio_server::server::LocalConnector;
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

/// Starts the bridge in the background. Fails only on an invalid
/// configuration; an unreachable remote broker is retried.
pub fn spawn(config: BridgeConfig, local: LocalConnector) -> Result<(), String> {
    let (outbound, inbound) = routes(&config)?;
    let client_id = config
        .client_id
        .clone()
        .unwrap_or_else(|| format!("mercurio-bridge-{}", config.name));
    let addresses = addresses(&config)?;

    // Stays connected while the remote broker is unreachable, so messages
    // to forward wait in its queue meanwhile
    let local_options = ConnectOptions::new("localhost", 0)
        .client_id(client_id.clone())
        .connector(move || local.connect());

    let protocol_version = match config.protocol_version {
        BridgeProtocol::Mqttv5 => ProtocolVersion::V5,
        BridgeProtocol::Mqttv311 => ProtocolVersion::V3_1_1,
    };
    let remote_options = addresses
        .into_iter()
        .map(|(host, port)| {
            let address = format!("{host}:{port}");
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

    let bridge = Bridge {
        name: config.name,
        state_topic: config
            .notifications
            .then(|| format!("$SYS/broker/connection/{client_id}/state")),
        echoes: (protocol_version == ProtocolVersion::V3_1_1).then(|| Mutex::new(VecDeque::new())),
        outbound,
        inbound,
    };
    tokio::spawn(bridge.run(local_options, remote_options));
    Ok(())
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
    name: String,
    /// Where the bridge's state is published, `1` when connected
    state_topic: Option<String>,
    /// Recently forwarded messages, with an MQTT 3.1.1 remote broker
    echoes: Option<Mutex<VecDeque<(String, Bytes)>>>,
    outbound: Vec<Route>,
    inbound: Vec<Route>,
}

impl Bridge {
    /// Forwards messages through the remote brokers, trying them in turn,
    /// until this broker stops.
    async fn run(self, local_options: ConnectOptions, remotes: Vec<(String, ConnectOptions)>) {
        let local = loop {
            match MqttClient::connect(local_options.clone()).await {
                Ok(local) => break local,
                Err(e) => warn!("Bridge {}: connecting locally failed: {}", self.name, e),
            }
            tokio::time::sleep(RETRY_DELAY).await;
        };
        if !self.subscribe(&local, &self.outbound).await {
            return;
        }

        for (address, options) in remotes.iter().cycle() {
            let remote = match MqttClient::connect(options.clone()).await {
                Ok(remote) => remote,
                Err(e) => {
                    warn!(
                        "Bridge {}: connecting to {} failed, trying the next address in {:?}: {}",
                        self.name, address, RETRY_DELAY, e
                    );
                    tokio::time::sleep(RETRY_DELAY).await;
                    continue;
                }
            };
            if self.subscribe(&remote, &self.inbound).await {
                info!("Bridge {} connected to {}", self.name, address);
                self.notify(&local, "1").await;
                let local_closed = tokio::select! {
                    _ = self.forward(&local, &remote, &self.outbound, true) => true,
                    _ = self.forward(&remote, &local, &self.inbound, false) => false,
                };
                if local_closed {
                    // This broker is stopping
                    let _ = remote.disconnect().await;
                    return;
                }
                error!("Bridge {} lost its connection", self.name);
                self.notify(&local, "0").await;
            }
            let _ = remote.disconnect().await;
            tokio::time::sleep(RETRY_DELAY).await;
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
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{forwarded_topic, routes, Route};
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
}
