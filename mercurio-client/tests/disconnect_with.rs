use std::time::Duration;

use mercurio_client::{ClientError, ConnectOptions, MqttClient, ProtocolVersion, ReasonCode};
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

/// An MQTT 5 CONNACK accepting the client, with these properties.
fn connack(session_present: bool, properties: &[u8]) -> Vec<u8> {
    let mut packet = vec![
        0x20,
        3 + properties.len() as u8,
        session_present as u8,
        0x00,
    ];
    packet.push(properties.len() as u8);
    packet.extend_from_slice(properties);
    packet
}

/// Accepts a connection, answers its CONNECT with `connack` and returns
/// the CONNECT and the connection.
async fn accept(listener: &TcpListener, connack: &[u8]) -> (Vec<u8>, TcpStream) {
    let (mut socket, _) = listener.accept().await.unwrap();
    let connect = read_packet(&mut socket).await;
    assert_eq!(connect[0], 0x10);
    socket.write_all(connack).await.unwrap();
    (connect, socket)
}

#[tokio::test]
async fn disconnect_with_sends_the_reason_and_session_expiry() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let broker = tokio::spawn(async move {
        let (_, mut socket) = accept(&listener, &connack(false, &[])).await;
        read_packet(&mut socket).await
    });
    let client = MqttClient::connect(
        ConnectOptions::new("127.0.0.1", port)
            .client_id("c")
            .keep_alive(0)
            .session_expiry(3600),
    )
    .await
    .unwrap();
    client
        .disconnect_with(ReasonCode::DisconnectWithWillMessage, Some(0))
        .await
        .unwrap();
    let disconnect = timeout(Duration::from_secs(2), broker)
        .await
        .unwrap()
        .unwrap();
    // Reason 0x04, then the Session Expiry Interval property set to 0
    assert_eq!(
        disconnect,
        [0xe0, 0x07, 0x04, 0x05, 0x11, 0x00, 0x00, 0x00, 0x00]
    );
}

#[tokio::test]
async fn disconnect_with_refuses_what_the_client_may_not_send() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let broker = tokio::spawn(async move {
        let (_, mut socket) = accept(&listener, &connack(false, &[])).await;
        read_packet(&mut socket).await
    });
    let client = MqttClient::connect(
        ConnectOptions::new("127.0.0.1", port)
            .client_id("c")
            .keep_alive(0),
    )
    .await
    .unwrap();
    // A reason only a broker sends
    let result = client
        .disconnect_with(ReasonCode::ServerShuttingDown, None)
        .await;
    assert!(
        matches!(result, Err(ClientError::InvalidArgument(_))),
        "{result:?}"
    );
    // A session that ends with the connection can't be extended at the end
    // [MQTT-3.14.2-2]
    let result = client
        .disconnect_with(ReasonCode::NormalDisconnection, Some(60))
        .await;
    assert!(
        matches!(result, Err(ClientError::InvalidArgument(_))),
        "{result:?}"
    );
    // Still connected: a plain disconnect goes out
    client.disconnect().await.unwrap();
    assert_eq!(broker.await.unwrap()[0], 0xe0);
}

#[tokio::test]
async fn mqtt_3_has_no_disconnect_reasons() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_packet(&mut socket).await;
        socket.write_all(&[0x20, 0x02, 0x00, 0x00]).await.unwrap();
        let _ = read_packet(&mut socket).await;
    });
    let client = MqttClient::connect(
        ConnectOptions::new("127.0.0.1", port)
            .client_id("c")
            .keep_alive(0)
            .protocol_version(ProtocolVersion::V3_1_1),
    )
    .await
    .unwrap();
    let result = client
        .disconnect_with(ReasonCode::DisconnectWithWillMessage, None)
        .await;
    assert!(
        matches!(result, Err(ClientError::NotSupported(_))),
        "{result:?}"
    );
    client
        .disconnect_with(ReasonCode::NormalDisconnection, None)
        .await
        .unwrap();
}
