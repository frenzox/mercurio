mod common;

use std::time::Duration;

use common::Peer;
use mercurio_client::{
    ClientError, ConnectOptions, DisconnectReason, Event, MqttClient, ReasonCode,
};
use mercurio_core::properties::ServerReference;
use mercurio_packets::connack::ConnAckProperties;
use mercurio_packets::disconnect::{DisconnectPacket, DisconnectProperties};
use mercurio_packets::ControlPacket;
use tokio::net::TcpListener;
use tokio::time::timeout;

async fn listen() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("127.0.0.1:{}", listener.local_addr().unwrap().port());
    (listener, address)
}

/// A port nothing listens on.
async fn closed_port() -> String {
    let (listener, address) = listen().await;
    drop(listener);
    address
}

fn port(address: &str) -> u16 {
    address.rsplit_once(':').unwrap().1.parse().unwrap()
}

/// Refuses the next connection with `reason`, naming `reference`.
async fn refuse(listener: &TcpListener, reason: ReasonCode, reference: &str) {
    let (mut peer, _) = Peer::accept(listener).await;
    peer.connack(
        false,
        reason,
        ConnAckProperties {
            server_reference: Some(ServerReference::new(reference.to_string())),
            ..Default::default()
        },
    )
    .await;
}

fn options(address: &str) -> ConnectOptions {
    ConnectOptions::new("127.0.0.1", port(address))
        .keep_alive(0)
        .connect_timeout(Duration::from_secs(2))
}

#[tokio::test]
async fn a_refusal_naming_other_servers_is_followed_to_the_first_that_answers() {
    let (a, a_address) = listen().await;
    let (b, b_address) = listen().await;
    let reference = format!("{} {b_address}", closed_port().await);
    let broker = tokio::spawn(async move {
        refuse(&a, ReasonCode::UseAnotherServer, &reference).await;
        Peer::accept_with(&b, false, Default::default()).await.0
    });
    let client = MqttClient::connect(options(&a_address).follow_redirects(1))
        .await
        .unwrap();
    let _peer = broker.await.unwrap();
    drop(client);
}

#[tokio::test]
async fn redirections_are_reported_unless_followed() {
    let (a, a_address) = listen().await;
    let (_b, b_address) = listen().await;
    tokio::spawn(async move { refuse(&a, ReasonCode::ServerMoved, &b_address).await });
    let error = MqttClient::connect(options(&a_address))
        .await
        .err()
        .unwrap();
    assert!(
        matches!(
            error,
            ClientError::Redirected {
                reason: ReasonCode::ServerMoved,
                ..
            }
        ),
        "{error:?}"
    );
}

#[tokio::test]
async fn redirections_stop_at_the_hop_limit() {
    for (hops, follows) in [(1, false), (2, true)] {
        let (a, a_address) = listen().await;
        let (b, b_address) = listen().await;
        let (c, c_address) = listen().await;
        tokio::spawn(async move {
            refuse(&a, ReasonCode::UseAnotherServer, &b_address).await;
            refuse(&b, ReasonCode::UseAnotherServer, &c_address).await;
            if follows {
                let _peer = Peer::accept_with(&c, false, Default::default()).await;
                std::future::pending::<()>().await;
            }
        });
        let result = MqttClient::connect(options(&a_address).follow_redirects(hops)).await;
        match follows {
            true => assert!(result.is_ok(), "{:?}", result.err()),
            false => assert!(matches!(result, Err(ClientError::Redirected { .. }))),
        }
    }
}

/// Connects through a refusal of `a` with `reason` to `b`, which then
/// drops the connection; returns which of the two the client comes back
/// to.
async fn reconnects_after(reason: ReasonCode) -> &'static str {
    let (a, a_address) = listen().await;
    let (b, b_address) = listen().await;
    let client = MqttClient::connect(
        options(&a_address)
            .follow_redirects(1)
            .reconnect(Duration::from_millis(50)),
    );
    let broker = async {
        refuse(&a, reason, &b_address).await;
        drop(Peer::accept_with(&b, false, Default::default()).await);
    };
    let (client, ()) = tokio::join!(client, broker);
    let client = client.unwrap();
    let back = tokio::select! {
        _ = Peer::accept_with(&a, false, Default::default()) => "a",
        _ = Peer::accept_with(&b, false, Default::default()) => "b",
    };
    drop(client);
    back
}

#[tokio::test]
async fn server_moved_lasts_and_use_another_server_does_not() {
    assert_eq!(reconnects_after(ReasonCode::ServerMoved).await, "b");
    assert_eq!(reconnects_after(ReasonCode::UseAnotherServer).await, "a");
}

#[tokio::test]
async fn a_disconnect_naming_another_server_moves_the_client_there() {
    let (a, a_address) = listen().await;
    let (b, b_address) = listen().await;
    let client = MqttClient::connect(options(&a_address).follow_redirects(1));
    let (client, (mut peer, _)) =
        tokio::join!(client, Peer::accept_with(&a, false, Default::default()));
    let client = client.unwrap();
    peer.write(ControlPacket::Disconnect(DisconnectPacket {
        reason: ReasonCode::UseAnotherServer,
        properties: Some(DisconnectProperties {
            server_reference: Some(ServerReference::new(b_address)),
            ..Default::default()
        }),
    }))
    .await;
    let _moved = Peer::accept_with(&b, false, Default::default()).await;
    let next = || async {
        timeout(Duration::from_secs(5), client.recv())
            .await
            .unwrap()
    };
    assert!(matches!(
        next().await,
        Some(Event::Disconnected {
            reason: DisconnectReason::ServerInitiated {
                reason: ReasonCode::UseAnotherServer,
                ..
            }
        })
    ));
    assert!(matches!(
        next().await,
        Some(Event::Reconnected {
            session_present: false,
            ..
        })
    ));
}

#[tokio::test]
async fn without_following_a_disconnect_naming_another_server_ends_the_client() {
    let (a, a_address) = listen().await;
    let client = MqttClient::connect(options(&a_address).reconnect(Duration::from_millis(50)));
    let (client, (mut peer, _)) =
        tokio::join!(client, Peer::accept_with(&a, false, Default::default()));
    let client = client.unwrap();
    peer.write(ControlPacket::Disconnect(DisconnectPacket {
        reason: ReasonCode::ServerMoved,
        properties: Some(DisconnectProperties {
            server_reference: Some(ServerReference::new("127.0.0.1:1".to_string())),
            ..Default::default()
        }),
    }))
    .await;
    assert!(matches!(
        timeout(Duration::from_secs(5), client.recv())
            .await
            .unwrap(),
        Some(Event::Disconnected { .. })
    ));
    assert!(timeout(Duration::from_secs(5), client.recv())
        .await
        .unwrap()
        .is_none());
}
