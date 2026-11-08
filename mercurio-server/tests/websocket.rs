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
    let config = ServerConfig {
        listeners: vec![ListenerConfig::new(
            websocket_listener,
            Transport::WebSocket,
        )],
        ..Default::default()
    };
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
    let config = ServerConfig {
        listeners: vec![ListenerConfig {
            http_dir: Some(dir.clone()),
            ..ListenerConfig::new(websocket_listener, Transport::WebSocket)
        }],
        ..Default::default()
    };
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
    let closed = timeout(Duration::from_secs(2), async {
        loop {
            match websocket.next().await {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return,
                Some(Ok(_)) => {}
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "still waiting for the rest of the message");
}
