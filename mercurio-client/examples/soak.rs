//! Endurance check: publishes numbered QoS 1 messages to a persistent
//! subscriber for a while, through broker restarts and failovers, and
//! reports any message the broker acknowledged but never delivered.
//!
//! ```text
//! cargo run --release --example soak -- <host:port>[,<host:port>...] <seconds> [rate]
//! ```
//!
//! Clients try the addresses in turn when reconnecting, so a standby
//! broker can be listed after the active one.

use std::{
    collections::HashSet,
    net::SocketAddr,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use mercurio_client::{ConnectOptions, Event, MqttClient, QoS};

const TOPIC: &str = "soak/sequence";

/// Connects through each address in turn, so a client reconnecting after a
/// failover reaches whichever broker serves.
fn options(addresses: &[SocketAddr], client_id: &str) -> ConnectOptions {
    let addresses = addresses.to_vec();
    let next = AtomicUsize::new(0);
    ConnectOptions::new("localhost", 0)
        .client_id(client_id)
        .reconnect(Duration::from_millis(500))
        .connector(move || {
            let mut last_error = None;
            for _ in 0..addresses.len() {
                let address = addresses[next.fetch_add(1, Ordering::Relaxed) % addresses.len()];
                match std::net::TcpStream::connect_timeout(&address, Duration::from_secs(2)) {
                    Ok(stream) => {
                        stream.set_nonblocking(true)?;
                        return tokio::net::TcpStream::from_std(stream);
                    }
                    Err(e) => last_error = Some(e),
                }
            }
            Err(last_error.expect("at least one address"))
        })
}

async fn connect(options: ConnectOptions) -> MqttClient {
    loop {
        match MqttClient::connect(options.clone()).await {
            Ok(client) => return client,
            Err(e) => {
                eprintln!("connect failed, retrying: {e:?}");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let addresses: Vec<SocketAddr> = args
        .next()
        .expect("addresses")
        .split(',')
        .map(|a| a.parse().expect("host:port"))
        .collect();
    let seconds: u64 = args.next().expect("seconds").parse().expect("seconds");
    let rate: u64 = args.next().map_or(200, |r| r.parse().expect("rate"));

    let received = Arc::new(Mutex::new((HashSet::<u64>::new(), 0u64)));
    let subscriber_options = options(&addresses, "soak-subscriber")
        .clean_start(false)
        .session_expiry(3600);
    let subscriber = connect(subscriber_options).await;
    subscriber
        .subscribe(&[(TOPIC, QoS::AtLeastOnce)])
        .await
        .expect("subscribe");
    let reconnects = Arc::new(AtomicU64::new(0));
    {
        let (received, reconnects) = (Arc::clone(&received), Arc::clone(&reconnects));
        tokio::spawn(async move {
            while let Some(event) = subscriber.recv().await {
                match event {
                    Event::Message { payload, .. } => {
                        let sequence = u64::from_be_bytes(payload[..8].try_into().unwrap());
                        let mut received = received.lock().unwrap();
                        if !received.0.insert(sequence) {
                            received.1 += 1;
                        }
                    }
                    Event::Reconnected { session_present } => {
                        reconnects.fetch_add(1, Ordering::Relaxed);
                        if !session_present {
                            eprintln!("subscriber's session was lost on reconnecting");
                        }
                    }
                    Event::Disconnected { .. } => {}
                }
            }
        });
    }

    let acknowledged = Arc::new(Mutex::new(Vec::<u64>::new()));
    let publisher = Arc::new(connect(options(&addresses, "soak-publisher")).await);
    let started = Instant::now();
    let deadline = started + Duration::from_secs(seconds);
    let in_flight = Arc::new(AtomicU64::new(0));
    let mut tick = tokio::time::interval(Duration::from_secs(1) / rate as u32);
    let mut report = Instant::now();
    let mut sequence = 0u64;
    while Instant::now() < deadline {
        tick.tick().await;
        let (task_publisher, task_acknowledged, task_in_flight) = (
            Arc::clone(&publisher),
            Arc::clone(&acknowledged),
            Arc::clone(&in_flight),
        );
        in_flight.fetch_add(1, Ordering::Relaxed);
        tokio::spawn(async move {
            // A publish made while disconnected waits for the reconnection
            if task_publisher
                .publish(TOPIC, sequence.to_be_bytes().to_vec(), QoS::AtLeastOnce)
                .await
                .is_ok()
            {
                task_acknowledged.lock().unwrap().push(sequence);
            }
            task_in_flight.fetch_sub(1, Ordering::Relaxed);
        });
        sequence += 1;

        if report.elapsed() >= Duration::from_secs(10) {
            report = Instant::now();
            let acked = acknowledged.lock().unwrap().len();
            let (got, duplicates) = {
                let received = received.lock().unwrap();
                (received.0.len(), received.1)
            };
            println!(
                "{:>5}s published {sequence} acknowledged {acked} received {got} duplicates {duplicates} in flight {} subscriber reconnects {}",
                started.elapsed().as_secs(),
                in_flight.load(Ordering::Relaxed),
                reconnects.load(Ordering::Relaxed),
            );
        }
    }

    // Let what is in flight arrive
    let grace = Instant::now() + Duration::from_secs(30);
    while in_flight.load(Ordering::Relaxed) > 0 && Instant::now() < grace {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    tokio::time::sleep(Duration::from_secs(5)).await;

    let acknowledged = acknowledged.lock().unwrap().clone();
    let received = received.lock().unwrap();
    let mut missing: Vec<u64> = acknowledged
        .iter()
        .filter(|s| !received.0.contains(s))
        .copied()
        .collect();
    missing.sort_unstable();
    println!(
        "done: published {sequence}, acknowledged {}, received {}, duplicates {}, acknowledged but never received {}",
        acknowledged.len(),
        received.0.len(),
        received.1,
        missing.len()
    );
    if !missing.is_empty() {
        println!(
            "missing: {:?}{}",
            &missing[..missing.len().min(20)],
            if missing.len() > 20 { " ..." } else { "" }
        );
    }
}
