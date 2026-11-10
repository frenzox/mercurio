//! Credential checks share one queue per process, so this file holds a
//! single test: another one running alongside would share the queue too.

use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use mercurio_server::{
    plugin::{BoxFuture, ClientInfo, Credentials, Decision, Plugin},
    server::ServerConfig,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Semaphore,
    time::timeout,
};

/// Holds every password check until `release` is closed, counting the
/// checks of user `late` apart from the others.
struct Gate {
    held: AtomicUsize,
    late: AtomicUsize,
    release: Semaphore,
}

impl Plugin for Gate {
    fn name(&self) -> &str {
        "gate"
    }

    fn authenticate<'a>(
        &'a self,
        _client: &'a ClientInfo,
        credentials: &'a Credentials<'a>,
    ) -> BoxFuture<'a, Decision> {
        Box::pin(async move {
            match credentials.username {
                "late" => self.late.fetch_add(1, Ordering::SeqCst),
                _ => self.held.fetch_add(1, Ordering::SeqCst),
            };
            let _ = self.release.acquire().await;
            Decision::Deny
        })
    }
}

async fn start_broker(gate: Arc<Gate>, connect_timeout: Duration) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut config = ServerConfig::default();
    config.plugins = vec![gate];
    config.plugin_timeout = Duration::from_secs(30);
    config.limits.connect_timeout = connect_timeout;
    config.sys_interval = None;
    tokio::spawn(mercurio_server::server::run_with_config(
        listener,
        config,
        std::future::pending::<()>(),
    ));
    addr
}

/// An MQTT 3.1.1 CONNECT logging in as `user`.
async fn log_in(addr: SocketAddr, user: &str) -> TcpStream {
    let mut payload = Vec::new();
    for field in [user, user, "secret"] {
        payload.extend((field.len() as u16).to_be_bytes());
        payload.extend(field.as_bytes());
    }
    let mut connect = vec![0x10, 10 + payload.len() as u8];
    connect.extend([0x00, 0x04, b'M', b'Q', b'T', b'T', 0x04, 0xc2, 0x00, 0x3c]);
    connect.extend(payload);
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(&connect).await.unwrap();
    stream
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_login_queued_past_its_connect_timeout_is_dropped_unchecked() {
    let gate = Arc::new(Gate {
        held: AtomicUsize::new(0),
        late: AtomicUsize::new(0),
        release: Semaphore::new(0),
    });

    // A patient broker whose logins take every place in the queue
    let patient = start_broker(Arc::clone(&gate), Duration::from_secs(30)).await;
    let places = std::thread::available_parallelism().map_or(4, |n| n.get());
    let mut holders = Vec::new();
    for i in 0..places {
        holders.push(log_in(patient, &format!("holder{i}")).await);
    }
    timeout(Duration::from_secs(5), async {
        while gate.held.load(Ordering::SeqCst) < places {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the queue never filled");

    // A hasty broker's client waits behind them, past its connect timeout
    let hasty = start_broker(Arc::clone(&gate), Duration::from_millis(200)).await;
    let mut late = log_in(hasty, "late").await;
    let read = timeout(Duration::from_secs(2), late.read(&mut [0u8; 1]))
        .await
        .expect("a queued login outlived its connect timeout");
    assert!(matches!(read, Ok(0) | Err(_)), "got a CONNACK");

    // Places freed later don't go to it
    gate.release.close();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(gate.late.load(Ordering::SeqCst), 0);
}
