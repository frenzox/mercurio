//! Bridges: forward topics between this broker and others.
//!
//! A bridge is a pair of MQTT 5 clients, one connected to this broker in
//! process and one to the remote broker. Each forwards what it receives
//! through the other. Subscriptions use No Local, so a message a bridge
//! forwarded is never sent back to it, and loops can't form even when a
//! topic goes both ways.

use std::{sync::Arc, time::Duration};

use mercurio_client::{ConnectOptions, Event, MqttClient, QoS, SubscriptionOptions};
use mercurio_server::server::LocalConnector;
use tracing::{error, info, warn};

use crate::config::{BridgeConfig, Direction};

const RETRY_DELAY: Duration = Duration::from_secs(5);

// ponytail: fixed, so the remote broker keeps inbound messages through a
// day of downtime; make it configurable if someone needs another value
const REMOTE_SESSION_EXPIRY: u32 = 24 * 60 * 60;

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

    let local_options = ConnectOptions::new("localhost", 0)
        .client_id(client_id.clone())
        .connector(move || local.connect());

    let mut remote_options = ConnectOptions::new(config.host.clone(), config.port)
        .client_id(client_id)
        .clean_start(false)
        .session_expiry(REMOTE_SESSION_EXPIRY)
        .reconnect(RETRY_DELAY)
        .tls(config.tls.enabled);
    if let Some(ca_path) = &config.tls.ca_path {
        remote_options = remote_options.ca_path(ca_path.clone());
    }
    if let Some(username) = &config.username {
        remote_options = remote_options.username(username.clone());
    }
    if let Some(password) = &config.password {
        remote_options = remote_options.password(password.clone());
    }

    let bridge = Bridge {
        name: config.name,
        outbound,
        inbound,
    };
    tokio::spawn(async move {
        loop {
            bridge.run(&local_options, &remote_options).await;
            tokio::time::sleep(RETRY_DELAY).await;
        }
    });
    Ok(())
}

struct Bridge {
    name: String,
    outbound: Vec<Route>,
    inbound: Vec<Route>,
}

impl Bridge {
    /// Forwards messages until either side's connection is lost for good.
    async fn run(&self, local_options: &ConnectOptions, remote_options: &ConnectOptions) {
        let local = Arc::new(self.connect("local", local_options).await);
        let remote = Arc::new(self.connect("remote", remote_options).await);
        if !(self.subscribe(&local, &self.outbound).await
            && self.subscribe(&remote, &self.inbound).await)
        {
            let _ = local.disconnect().await;
            let _ = remote.disconnect().await;
            return;
        }
        info!("Bridge {} connected", self.name);

        tokio::select! {
            _ = self.forward(&local, &remote, &self.outbound) => {}
            _ = self.forward(&remote, &local, &self.inbound) => {}
        }
        error!("Bridge {} lost its connection, restarting", self.name);
        let _ = local.disconnect().await;
        let _ = remote.disconnect().await;
    }

    async fn connect(&self, side: &str, options: &ConnectOptions) -> MqttClient {
        loop {
            match MqttClient::connect(options.clone()).await {
                Ok(client) => return client,
                Err(e) => warn!(
                    "Bridge {}: connecting to the {} broker failed, retrying in {:?}: {}",
                    self.name, side, RETRY_DELAY, e
                ),
            }
            tokio::time::sleep(RETRY_DELAY).await;
        }
    }

    /// Subscribes to every route, each tagged with its index plus one as
    /// subscription identifier, so `forward` knows which route a message
    /// came through.
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

    async fn forward(&self, from: &MqttClient, to: &MqttClient, routes: &[Route]) {
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
                    let route = subscription_ids
                        .first()
                        .and_then(|id| routes.get((*id as usize).checked_sub(1)?));
                    let Some(renamed) = route.and_then(|route| route.rename(&topic)) else {
                        continue;
                    };
                    let published = to
                        .publish_with_properties(&renamed, payload, qos, retain, properties)
                        .await;
                    if let Err(e) = published {
                        warn!("Bridge {}: forwarding `{}` failed: {}", self.name, topic, e);
                    }
                }
                Event::Reconnected { session_present } => {
                    info!("Bridge {} reconnected", self.name);
                    if !session_present {
                        self.subscribe(from, routes).await;
                    }
                }
                Event::Disconnected { reason } => {
                    warn!("Bridge {} disconnected: {:?}", self.name, reason);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{routes, Route};
    use crate::config::Config;
    use mercurio_client::QoS;

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
