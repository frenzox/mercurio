use std::time::Duration;

use mercurio_client::{ConnectOptions, DisconnectReason, Event, MqttClient, QoS};
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

    // The QoS 1 PUBLISH again with DUP set, then the QoS 2 PUBREL, in the
    // order they were first sent
    assert_eq!(resent[0][0], 0x3a, "{resent:02x?}");
    assert_eq!(resent[1][0], 0x62, "{resent:02x?}");
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
