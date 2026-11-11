use std::{net::SocketAddr, path::PathBuf, time::Duration};

use mercurio_server::server::{ListenerConfig, ServerConfig, Transport};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::{sleep, timeout},
};

/// Larger than what loopback sockets buffer, so serving it waits on the
/// client reading
const LARGE: usize = 32 * 1024 * 1024;

fn site(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mercurio-{name}-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("docs")).unwrap();
    std::fs::write(dir.join("docs/index.html"), "<h1>docs</h1>").unwrap();
    std::fs::write(dir.join("large.bin"), vec![7u8; LARGE]).unwrap();
    dir
}

async fn serve(dir: PathBuf, connect_timeout: Duration) -> SocketAddr {
    let websocket_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = websocket_listener.local_addr().unwrap();
    let mut config = ServerConfig::default();
    config.limits.connect_timeout = connect_timeout;
    let mut listener = ListenerConfig::new(websocket_listener, Transport::WebSocket);
    listener.http_dir = Some(dir);
    config.listeners = vec![listener];
    tokio::spawn(mercurio_server::server::run_with_config(
        TcpListener::bind("127.0.0.1:0").await.unwrap(),
        config,
        std::future::pending::<()>(),
    ));
    addr
}

async fn request(addr: SocketAddr, target: &str) -> TcpStream {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let request = format!("GET {target} HTTP/1.1\r\nHost: x\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    stream
}

async fn response(mut stream: TcpStream) -> Vec<u8> {
    let mut response = Vec::new();
    timeout(Duration::from_secs(10), stream.read_to_end(&mut response))
        .await
        .expect("response not finished")
        .unwrap();
    response
}

/// Sending a file can take longer than connecting may: a slow reader still
/// gets all of it.
#[tokio::test]
async fn files_outlast_the_connect_deadline() {
    let dir = site("slow-reader");
    let addr = serve(dir.clone(), Duration::from_millis(200)).await;

    let stream = request(addr, "/large.bin").await;
    sleep(Duration::from_millis(500)).await;
    let response = response(stream).await;
    let head_end = response.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    assert!(response.starts_with(b"HTTP/1.1 200 OK"));
    assert_eq!(response.len() - head_end, LARGE);
    std::fs::remove_dir_all(dir).unwrap();
}

/// Without the slash, the page's relative links would resolve against the
/// parent directory.
#[tokio::test]
async fn directories_redirect_to_their_slash_form() {
    let dir = site("redirect");
    let addr = serve(dir.clone(), Duration::from_secs(10)).await;

    let redirect = String::from_utf8(response(request(addr, "/docs?v=1").await).await).unwrap();
    assert!(redirect.starts_with("HTTP/1.1 301"), "{redirect}");
    assert!(
        redirect.contains("\r\nLocation: /docs/?v=1\r\n"),
        "{redirect}"
    );

    // A path that looks like another host is not sent there
    let redirect = String::from_utf8(response(request(addr, "//docs").await).await).unwrap();
    assert!(redirect.contains("\r\nLocation: /docs/\r\n"), "{redirect}");

    let page = String::from_utf8(response(request(addr, "/docs/").await).await).unwrap();
    assert!(page.starts_with("HTTP/1.1 200 OK"), "{page}");
    assert!(page.ends_with("<h1>docs</h1>"));
    std::fs::remove_dir_all(dir).unwrap();
}

/// Clients that never read their file hold a response each; past the
/// limit, others are told to come back later instead of piling up.
#[tokio::test]
async fn concurrent_responses_are_limited() {
    let dir = site("busy");
    let addr = serve(dir.clone(), Duration::from_secs(10)).await;

    let mut held = Vec::new();
    for _ in 0..32 {
        held.push(request(addr, "/large.bin").await);
    }
    sleep(Duration::from_millis(200)).await;
    let busy = String::from_utf8(response(request(addr, "/docs/").await).await).unwrap();
    assert!(busy.starts_with("HTTP/1.1 503"), "{busy}");

    drop(held);
    sleep(Duration::from_millis(200)).await;
    let page = String::from_utf8(response(request(addr, "/docs/").await).await).unwrap();
    assert!(page.starts_with("HTTP/1.1 200 OK"), "{page}");
    std::fs::remove_dir_all(dir).unwrap();
}
