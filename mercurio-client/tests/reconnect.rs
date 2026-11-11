use std::time::Duration;

use mercurio_client::{
    ClientError, ConnectOptions, DisconnectReason, Event, MqttClient, ProtocolVersion, QoS,
    ReasonCode,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};

/// Reads one packet whose remaining length fits in a byte.
async fn read_packet(socket: &mut TcpStream) -> Vec<u8> {
    let mut header = [0u8; 2];
    socket.read_exact(&mut header).await.unwrap();
    let mut body = vec![0u8; header[1] as usize];
    socket.read_exact(&mut body).await.unwrap();
    [header.to_vec(), body].concat()
}

async fn accept(listener: &TcpListener, session_present: bool) -> TcpStream {
    let (mut socket, _) = listener.accept().await.unwrap();
    assert_eq!(read_packet(&mut socket).await[0], 0x10);
    socket
        .write_all(&[0x20, 0x03, session_present as u8, 0x00, 0x00])
        .await
        .unwrap();
    socket
}

/// Drops the connection with a QoS 1 message unacknowledged and a QoS 2
/// message released but not completed, then returns what the client
/// sends after reconnecting.
async fn broker(listener: TcpListener, session_present: bool) -> Vec<Vec<u8>> {
    let mut socket = accept(&listener, false).await;
    let qos1 = read_packet(&mut socket).await;
    assert_eq!(qos1[0], 0x32);
    let qos2 = read_packet(&mut socket).await;
    assert_eq!(qos2[0], 0x34);
    socket
        .write_all(&[0x50, 0x02, qos2[5], qos2[6]])
        .await
        .unwrap();
    assert_eq!(read_packet(&mut socket).await[0], 0x62);
    drop(socket);

    let mut socket = accept(&listener, session_present).await;
    let mut resent = vec![read_packet(&mut socket).await];
    if session_present {
        resent.push(read_packet(&mut socket).await);
        socket
            .write_all(&[0x70, 0x02, qos2[5], qos2[6]])
            .await
            .unwrap();
    }
    socket
        .write_all(&[0x40, 0x02, qos1[5], qos1[6]])
        .await
        .unwrap();
    let mut sink = [0u8; 64];
    while socket.read(&mut sink).await.unwrap_or(0) > 0 {}
    resent
}

async fn publish_across_reconnect(session_present: bool) -> (Vec<Vec<u8>>, Vec<Event>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let broker = tokio::spawn(broker(listener, session_present));

    let client = MqttClient::connect(
        ConnectOptions::new("127.0.0.1", port)
            .client_id("c")
            .clean_start(false)
            .keep_alive(0)
            .reconnect(Duration::from_millis(50)),
    )
    .await
    .unwrap();
    let (a, b) = timeout(Duration::from_secs(3), async {
        tokio::join!(
            client.publish("a", "1", QoS::AtLeastOnce),
            client.publish("a", "2", QoS::ExactlyOnce)
        )
    })
    .await
    .expect("publishes never completed");
    a.unwrap();
    b.unwrap();

    let events = vec![client.recv().await.unwrap(), client.recv().await.unwrap()];
    client.disconnect().await.unwrap();
    (broker.await.unwrap(), events)
}

#[tokio::test]
async fn resumed_session_gets_unacknowledged_packets_again() {
    let (resent, events) = publish_across_reconnect(true).await;

    // The QoS 2 PUBREL again, and the QoS 1 PUBLISH with DUP set
    assert_eq!(resent[0][0], 0x62, "{resent:02x?}");
    assert_eq!(resent[1][0], 0x3a, "{resent:02x?}");
    assert!(matches!(
        events[0],
        Event::Disconnected {
            reason: DisconnectReason::ConnectionClosed
        }
    ));
    assert!(matches!(
        events[1],
        Event::Reconnected {
            session_present: true
        }
    ));
}

#[tokio::test]
async fn lost_session_starts_unacknowledged_messages_over() {
    let (resent, events) = publish_across_reconnect(false).await;

    // A fresh PUBLISH; the released QoS 2 message is done
    assert_eq!(resent, vec![resent[0].clone()]);
    assert_eq!(resent[0][0], 0x32, "{resent:02x?}");
    assert!(matches!(
        events[1],
        Event::Reconnected {
            session_present: false
        }
    ));
}

#[tokio::test]
async fn without_reconnect_the_client_stays_disconnected() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let broker = tokio::spawn(async move {
        drop(accept(&listener, false).await);
        timeout(Duration::from_millis(500), listener.accept())
            .await
            .is_err()
    });

    let client = MqttClient::connect(ConnectOptions::new("127.0.0.1", port))
        .await
        .unwrap();
    assert!(matches!(
        client.recv().await,
        Some(Event::Disconnected { .. })
    ));
    assert!(broker.await.unwrap(), "client reconnected");
}

#[tokio::test]
async fn resent_messages_respect_a_lowered_receive_maximum() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let broker = tokio::spawn(async move {
        let mut socket = accept(&listener, false).await;
        let first = read_packet(&mut socket).await;
        let second = read_packet(&mut socket).await;
        drop(socket);

        // Session kept, but now only one message may be in flight
        let (mut socket, _) = listener.accept().await.unwrap();
        read_packet(&mut socket).await;
        socket
            .write_all(&[0x20, 0x06, 0x01, 0x00, 0x03, 0x21, 0x00, 0x01])
            .await
            .unwrap();
        assert_eq!(
            read_packet(&mut socket).await[..7],
            [0x3a, first[1], 0x00, 0x01, b'a', first[5], first[6]]
        );
        let mut early = [0u8; 1];
        assert!(
            timeout(Duration::from_millis(300), socket.read(&mut early))
                .await
                .is_err(),
            "second message resent beyond Receive Maximum"
        );
        socket
            .write_all(&[0x40, 0x02, first[5], first[6]])
            .await
            .unwrap();
        assert_eq!(
            read_packet(&mut socket).await[..7],
            [0x3a, second[1], 0x00, 0x01, b'a', second[5], second[6]]
        );
        socket
            .write_all(&[0x40, 0x02, second[5], second[6]])
            .await
            .unwrap();
        let mut sink = [0u8; 64];
        while socket.read(&mut sink).await.unwrap_or(0) > 0 {}
    });

    let client = MqttClient::connect(
        ConnectOptions::new("127.0.0.1", port)
            .client_id("c")
            .clean_start(false)
            .keep_alive(0)
            .reconnect(Duration::from_millis(50)),
    )
    .await
    .unwrap();
    let (a, b) = timeout(Duration::from_secs(3), async {
        tokio::join!(
            client.publish("a", "1", QoS::AtLeastOnce),
            client.publish("a", "2", QoS::AtLeastOnce)
        )
    })
    .await
    .expect("publishes never completed");
    a.unwrap();
    b.unwrap();
    client.disconnect().await.unwrap();
    broker.await.unwrap();
}

/// Drops the first connection, refuses the reconnections with each of
/// `refusals` in turn, then takes the client back.
async fn refusing_broker(
    listener: TcpListener,
    protocol: ProtocolVersion,
    refusals: Vec<u8>,
) -> TcpStream {
    let connack = |code| match protocol {
        ProtocolVersion::V5 => vec![0x20, 0x03, 0x00, code, 0x00],
        _ => vec![0x20, 0x02, 0x00, code],
    };
    let (mut socket, _) = listener.accept().await.unwrap();
    read_packet(&mut socket).await;
    socket.write_all(&connack(0)).await.unwrap();
    drop(socket);
    for code in refusals {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_packet(&mut socket).await;
        socket.write_all(&connack(code)).await.unwrap();
    }
    let (mut socket, _) = listener.accept().await.unwrap();
    read_packet(&mut socket).await;
    socket.write_all(&connack(0)).await.unwrap();
    socket
}

async fn events_until_none(client: &MqttClient) -> Vec<Event> {
    let mut events = Vec::new();
    loop {
        match timeout(Duration::from_secs(5), client.recv()).await {
            Ok(Some(event)) => events.push(event),
            Ok(None) => return events,
            Err(_) => panic!("the client neither stopped nor sent more: {events:?}"),
        }
    }
}

#[tokio::test]
async fn transient_refusals_are_retried() {
    for (protocol, refusals) in [
        (ProtocolVersion::V5, vec![0x88, 0x89, 0x97, 0x9f]),
        // Server unavailable
        (ProtocolVersion::V3_1_1, vec![0x03]),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let broker = tokio::spawn(refusing_broker(listener, protocol, refusals));
        let client = MqttClient::connect(
            ConnectOptions::new("127.0.0.1", port)
                .protocol_version(protocol)
                .client_id("c")
                .keep_alive(0)
                .reconnect(Duration::from_millis(10)),
        )
        .await
        .unwrap();
        assert!(matches!(
            client.recv().await,
            Some(Event::Disconnected { .. })
        ));
        let event = timeout(Duration::from_secs(5), client.recv()).await;
        assert!(
            matches!(event, Ok(Some(Event::Reconnected { .. }))),
            "{protocol:?}: {event:?}"
        );
        let _socket = broker.await.unwrap();
    }
}

#[tokio::test]
async fn a_permanent_refusal_stops_the_client_with_its_reason() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    // Not authorized, then a connection the client must not come back for
    let _broker = tokio::spawn(refusing_broker(listener, ProtocolVersion::V5, vec![0x87]));
    let client = MqttClient::connect(
        ConnectOptions::new("127.0.0.1", port)
            .client_id("c")
            .keep_alive(0)
            .reconnect(Duration::from_millis(10)),
    )
    .await
    .unwrap();

    let events = events_until_none(&client).await;
    assert!(
        matches!(events[0], Event::Disconnected { .. }),
        "{events:?}"
    );
    let Event::Stopped { error } = &events[1] else {
        panic!("no final event: {events:?}");
    };
    assert!(
        matches!(
            **error,
            ClientError::ConnectionRefused(ReasonCode::NotAuthorized)
        ),
        "{error:?}"
    );
    assert_eq!(events.len(), 2, "{events:?}");
}

#[tokio::test]
async fn a_resent_message_now_too_large_fails_and_frees_its_slot() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let broker = tokio::spawn(async move {
        let mut socket = accept(&listener, false).await;
        assert_eq!(read_packet(&mut socket).await[0], 0x32);
        drop(socket);

        // Session kept, but now Maximum Packet Size 50 and Receive
        // Maximum 1
        let (mut socket, _) = listener.accept().await.unwrap();
        read_packet(&mut socket).await;
        socket
            .write_all(&[
                0x20, 0x0b, 0x01, 0x00, 0x08, 0x27, 0x00, 0x00, 0x00, 0x32, 0x21, 0x00, 0x01,
            ])
            .await
            .unwrap();
        let small = read_packet(&mut socket).await;
        assert_eq!(small[0], 0x32, "{small:02x?}");
        socket
            .write_all(&[0x40, 0x02, small[5], small[6]])
            .await
            .unwrap();
        socket
    });

    let client = MqttClient::connect(
        ConnectOptions::new("127.0.0.1", port)
            .client_id("c")
            .clean_start(false)
            .keep_alive(0)
            .reconnect(Duration::from_millis(50)),
    )
    .await
    .unwrap();
    let large = timeout(
        Duration::from_secs(3),
        client.publish("a", vec![0u8; 100], QoS::AtLeastOnce),
    )
    .await
    .expect("the oversized resend never failed");
    assert!(
        matches!(large, Err(ClientError::PacketTooLarge)),
        "{large:?}"
    );
    timeout(
        Duration::from_secs(3),
        client.publish("a", "small", QoS::AtLeastOnce),
    )
    .await
    .expect("the slot of the oversized message stayed taken")
    .unwrap();
    let _socket = broker.await.unwrap();
}

#[tokio::test]
async fn publishes_queued_while_disconnected_follow_the_new_limits() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let broker = tokio::spawn(async move {
        drop(accept(&listener, false).await);
        // Now Maximum QoS 1 and no retained messages
        let (mut socket, _) = listener.accept().await.unwrap();
        read_packet(&mut socket).await;
        socket
            .write_all(&[0x20, 0x07, 0x00, 0x00, 0x04, 0x24, 0x01, 0x25, 0x00])
            .await
            .unwrap();
        let mut sent = [0u8; 1];
        timeout(Duration::from_millis(500), socket.read(&mut sent))
            .await
            .is_err()
    });

    let client = MqttClient::connect(
        ConnectOptions::new("127.0.0.1", port)
            .client_id("c")
            .keep_alive(0)
            .reconnect(Duration::from_millis(200)),
    )
    .await
    .unwrap();
    assert!(matches!(
        client.recv().await,
        Some(Event::Disconnected { .. })
    ));
    let (qos2, retained) = timeout(Duration::from_secs(3), async {
        tokio::join!(
            client.publish("a", "1", QoS::ExactlyOnce),
            client.publish_with_retain("a", "2", QoS::AtLeastOnce, true)
        )
    })
    .await
    .expect("the queued publishes never completed");
    assert!(
        matches!(qos2, Err(ClientError::NotSupported(_))),
        "{qos2:?}"
    );
    assert!(
        matches!(retained, Err(ClientError::NotSupported(_))),
        "{retained:?}"
    );
    assert!(broker.await.unwrap(), "an unsupported publish was sent");
}

#[tokio::test]
async fn reconnection_backs_off() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let broker = tokio::spawn(async move {
        drop(accept(&listener, false).await);
        // Each attempt fails before CONNACK
        let mut attempts = Vec::new();
        for _ in 0..6 {
            drop(listener.accept().await.unwrap());
            attempts.push(std::time::Instant::now());
        }
        attempts
    });
    let _client = MqttClient::connect(
        ConnectOptions::new("127.0.0.1", port)
            .client_id("c")
            .keep_alive(0)
            .reconnect(Duration::from_millis(20))
            .reconnect_max(Duration::from_millis(200)),
    )
    .await
    .unwrap();

    let attempts = timeout(Duration::from_secs(5), broker)
        .await
        .expect("the client stopped retrying")
        .unwrap();
    let gaps: Vec<Duration> = attempts.windows(2).map(|w| w[1] - w[0]).collect();
    // Doubling from 20 ms, jittered, up to the 200 ms cap
    assert!(gaps[0] < Duration::from_millis(120), "{gaps:?}");
    assert!(gaps[3] >= Duration::from_millis(100), "{gaps:?}");
    assert!(
        gaps.iter().all(|g| *g < Duration::from_millis(300)),
        "{gaps:?}"
    );
}
