mod common;

use std::{net::SocketAddr, time::Duration};

use bytes::{Bytes, BytesMut};
use futures_util::{SinkExt, StreamExt};
use mercurio_core::protocol::ProtocolVersion;
use mercurio_packets::ControlPacket;
use mercurio_server::server::{ListenerConfig, ServerConfig, Transport};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};
use tokio_tungstenite::{
    client_async,
    tungstenite::{client::IntoClientRequest, http::HeaderValue, Message},
    WebSocketStream,
};

/// Starts a broker serving plain MQTT and MQTT over WebSocket.
async fn start_broker() -> (SocketAddr, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let websocket_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addrs = (
        listener.local_addr().unwrap(),
        websocket_listener.local_addr().unwrap(),
    );
    let mut config = ServerConfig::default();
    config.listeners = vec![ListenerConfig::new(
        websocket_listener,
        Transport::WebSocket,
    )];
    tokio::spawn(mercurio_server::server::run_with_config(
        listener,
        config,
        std::future::pending::<()>(),
    ));
    addrs
}

async fn websocket_connect(addr: SocketAddr) -> WebSocketStream<TcpStream> {
    let mut request = format!("ws://{addr}/mqtt").into_client_request().unwrap();
    request
        .headers_mut()
        .insert("Sec-WebSocket-Protocol", HeaderValue::from_static("mqtt"));
    let stream = TcpStream::connect(addr).await.unwrap();
    let (websocket, response) = client_async(request, stream).await.unwrap();
    assert_eq!(
        response.headers().get("Sec-WebSocket-Protocol").unwrap(),
        "mqtt"
    );
    websocket
}

/// Reads frames until a whole MQTT packet arrived.
async fn next_packet(
    websocket: &mut WebSocketStream<TcpStream>,
    buffer: &mut BytesMut,
) -> ControlPacket {
    loop {
        if ControlPacket::check(buffer).is_ok() {
            return ControlPacket::parse_with_version(buffer, ProtocolVersion::V5).unwrap();
        }
        let frame = timeout(Duration::from_secs(2), websocket.next())
            .await
            .expect("timed out waiting for broker")
            .expect("connection closed")
            .unwrap();
        if let Message::Binary(data) = frame {
            buffer.extend_from_slice(&data);
        }
    }
}

const CONNECT: [u8; 16] = [
    0x10, 0x0e, 0x00, 0x04, b'M', b'Q', b'T', b'T', 0x05, 0x02, 0x00, 0x3c, 0x00, 0x00, 0x01, b'w',
];

#[tokio::test]
async fn websocket_clients_talk_to_tcp_clients() {
    let (tcp_addr, websocket_addr) = start_broker().await;

    let mut websocket = websocket_connect(websocket_addr).await;
    let mut buffer = BytesMut::new();
    websocket
        .send(Message::Binary(Bytes::from_static(&CONNECT)))
        .await
        .unwrap();
    assert!(matches!(
        next_packet(&mut websocket, &mut buffer).await,
        ControlPacket::ConnAck(_)
    ));

    // SUBSCRIBE to "t", split across two frames: frames don't have to
    // match packet boundaries
    let subscribe = [0x82, 0x07, 0x00, 0x01, 0x00, 0x00, 0x01, b't', 0x00];
    for part in [&subscribe[..4], &subscribe[4..]] {
        websocket
            .send(Message::Binary(Bytes::copy_from_slice(part)))
            .await
            .unwrap();
    }
    assert!(matches!(
        next_packet(&mut websocket, &mut buffer).await,
        ControlPacket::SubAck(_)
    ));

    let mut publisher = TcpStream::connect(tcp_addr).await.unwrap();
    let mut connect = CONNECT;
    connect[15] = b'p';
    publisher.write_all(&connect).await.unwrap();
    common::read_connack(&mut publisher).await;
    publisher
        .write_all(&[0x30, 0x06, 0x00, 0x01, b't', 0x00, b'h', b'i'])
        .await
        .unwrap();

    let ControlPacket::Publish(publish) = next_packet(&mut websocket, &mut buffer).await else {
        panic!("Expected Publish");
    };
    assert_eq!(publish.payload, Some(Bytes::from_static(b"hi")));
}

#[tokio::test]
async fn text_frames_close_the_connection() {
    let (_, websocket_addr) = start_broker().await;
    let mut websocket = websocket_connect(websocket_addr).await;

    websocket
        .send(Message::Text("not mqtt".into()))
        .await
        .unwrap();

    // Nothing but the end of the connection comes back
    let next = timeout(Duration::from_secs(2), async {
        loop {
            match websocket.next().await {
                Some(Ok(Message::Binary(_))) => panic!("unexpected MQTT data"),
                Some(Ok(_)) => continue,
                _ => break,
            }
        }
    })
    .await;
    assert!(next.is_ok(), "connection still open");
}

#[tokio::test]
async fn websocket_listeners_serve_files_and_mqtt() {
    let dir = std::env::temp_dir().join(format!("mercurio-http-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("index.html"), "<h1>hello</h1>").unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let websocket_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = websocket_listener.local_addr().unwrap();
    let mut config = ServerConfig::default();
    config.listeners = vec![{
        let mut value = ListenerConfig::new(websocket_listener, Transport::WebSocket);
        value.http_dir = Some(dir.clone());
        value
    }];
    tokio::spawn(mercurio_server::server::run_with_config(
        listener,
        config,
        std::future::pending::<()>(),
    ));

    let get = |target: &'static str| async move {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let request = format!("GET {target} HTTP/1.1\r\nHost: x\r\n\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = String::new();
        timeout(Duration::from_secs(2), stream.read_to_string(&mut response))
            .await
            .unwrap()
            .unwrap();
        response
    };
    let page = get("/").await;
    assert!(page.starts_with("HTTP/1.1 200 OK"), "{page}");
    assert!(page.ends_with("<h1>hello</h1>"));
    assert!(get("/../secret").await.starts_with("HTTP/1.1 404"));

    // The same listener still speaks MQTT over WebSocket
    let mut websocket = websocket_connect(addr).await;
    let mut buffer = BytesMut::new();
    websocket
        .send(Message::Binary(Bytes::from_static(&CONNECT)))
        .await
        .unwrap();
    assert!(matches!(
        next_packet(&mut websocket, &mut buffer).await,
        ControlPacket::ConnAck(_)
    ));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The status a WebSocket upgrade with `headers` is refused with, if any.
async fn upgrade_status(addr: SocketAddr, headers: &[(&'static str, &'static str)]) -> Option<u16> {
    let mut request = format!("ws://{addr}/mqtt").into_client_request().unwrap();
    for (name, value) in headers {
        request
            .headers_mut()
            .insert(*name, HeaderValue::from_static(value));
    }
    let stream = TcpStream::connect(addr).await.unwrap();
    match client_async(request, stream).await {
        Ok(_) => None,
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
            Some(response.status().as_u16())
        }
        Err(e) => panic!("{e}"),
    }
}

/// Without the subprotocol the client isn't speaking MQTT [MQTT-6.0.0-3],
/// and a browser page couldn't tell the broker apart from another server.
#[tokio::test]
async fn upgrades_without_the_mqtt_subprotocol_are_refused() {
    let (_, websocket_addr) = start_broker().await;
    assert_eq!(upgrade_status(websocket_addr, &[]).await, Some(400));
    let other = [("Sec-WebSocket-Protocol", "chat")];
    assert_eq!(upgrade_status(websocket_addr, &other).await, Some(400));
    let both = [("Sec-WebSocket-Protocol", "chat, mqtt")];
    assert_eq!(upgrade_status(websocket_addr, &both).await, None);
}

/// A page from another site can open a WebSocket to an intranet broker;
/// its browser says where the page came from.
#[tokio::test]
async fn only_allowed_origins_may_upgrade() {
    let websocket_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = websocket_listener.local_addr().unwrap();
    let mut listener = ListenerConfig::new(websocket_listener, Transport::WebSocket);
    listener.allowed_origins = Some(vec!["https://app.example".into()]);
    let mut config = ServerConfig::default();
    config.listeners = vec![listener];
    tokio::spawn(mercurio_server::server::run_with_config(
        TcpListener::bind("127.0.0.1:0").await.unwrap(),
        config,
        std::future::pending::<()>(),
    ));

    let mqtt = ("Sec-WebSocket-Protocol", "mqtt");
    let from = |origin| [mqtt, ("Origin", origin)];
    assert_eq!(
        upgrade_status(addr, &from("https://evil.example")).await,
        Some(403)
    );
    assert_eq!(
        upgrade_status(addr, &from("https://app.example")).await,
        None
    );
    assert_eq!(
        upgrade_status(addr, &from("HTTPS://APP.example")).await,
        None
    );
    // Clients other than browsers send no Origin
    assert_eq!(upgrade_status(addr, &[mqtt]).await, None);
}

/// Frames are reassembled before the MQTT packet size is known, so a
/// message growing past the largest packet is refused as it arrives,
/// before the rest of it is sent.
#[tokio::test]
async fn messages_larger_than_the_largest_packet_are_refused() {
    use tokio_tungstenite::tungstenite::protocol::frame::{coding::Data, coding::OpCode, Frame};

    let (_, websocket_addr) = start_broker().await;
    let mut websocket = websocket_connect(websocket_addr).await;

    // Twice the default maximum packet size, in a message never finished
    let fragment = Frame::message(
        vec![0u8; 2 * 1024 * 1024],
        OpCode::Data(Data::Binary),
        false,
    );
    let _ = websocket.send(Message::Frame(fragment)).await;
    assert!(
        closed(&mut websocket).await,
        "still waiting for the rest of the message"
    );
}

/// Whether the broker closes `websocket` within two seconds.
async fn closed(websocket: &mut WebSocketStream<TcpStream>) -> bool {
    timeout(Duration::from_secs(2), async {
        loop {
            match websocket.next().await {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return,
                Some(Ok(_)) => {}
            }
        }
    })
    .await
    .is_ok()
}

/// Before CONNECT is accepted, a client gets as little room as on TCP,
/// not a whole message as large as the largest packet; once connected, it
/// gets the rest.
#[tokio::test]
async fn messages_before_connect_get_the_connect_limit() {
    use tokio_tungstenite::tungstenite::protocol::frame::{coding::Data, coding::OpCode, Frame};

    let (_, websocket_addr) = start_broker().await;
    let mut websocket = websocket_connect(websocket_addr).await;
    // Larger than a CONNECT may be, smaller than a packet, never finished
    let fragment = Frame::message(vec![0u8; 512 * 1024], OpCode::Data(Data::Binary), false);
    let _ = websocket.send(Message::Frame(fragment)).await;
    assert!(
        closed(&mut websocket).await,
        "still waiting for the rest of the message"
    );

    let mut websocket = websocket_connect(websocket_addr).await;
    let mut buffer = BytesMut::new();
    websocket
        .send(Message::Binary(Bytes::from_static(&CONNECT)))
        .await
        .unwrap();
    assert!(matches!(
        next_packet(&mut websocket, &mut buffer).await,
        ControlPacket::ConnAck(_)
    ));
    // SUBSCRIBE to "t", then a 512 KiB PUBLISH on it comes back
    let subscribe = [0x82, 0x07, 0x00, 0x01, 0x00, 0x00, 0x01, b't', 0x00];
    websocket
        .send(Message::Binary(Bytes::copy_from_slice(&subscribe)))
        .await
        .unwrap();
    assert!(matches!(
        next_packet(&mut websocket, &mut buffer).await,
        ControlPacket::SubAck(_)
    ));
    let payload = 512 * 1024;
    let length = 3 + 1 + payload;
    let mut publish = vec![0x30];
    let mut rest = length;
    loop {
        let byte = (rest % 128) as u8;
        rest /= 128;
        publish.push(if rest > 0 { byte | 0x80 } else { byte });
        if rest == 0 {
            break;
        }
    }
    publish.extend_from_slice(&[0x00, 0x01, b't', 0x00]);
    publish.resize(publish.len() + payload, 7);
    websocket
        .send(Message::Binary(Bytes::from(publish)))
        .await
        .unwrap();
    let ControlPacket::Publish(received) = next_packet(&mut websocket, &mut buffer).await else {
        panic!("Expected Publish");
    };
    assert_eq!(received.payload.unwrap().len(), payload);
}
