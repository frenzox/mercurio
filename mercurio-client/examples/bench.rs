//! Load generator for comparing brokers:
//!
//! ```text
//! cargo run --release --example bench -- <port> [fan-in-qos0|fan-in-qos1|fan-in-qos1-persistent|fan-out|latency|connections [count]]
//! ```
//!
//! Every scenario uses MQTT 5 clients on 127.0.0.1 and prints one line.

use std::{
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use mercurio_client::{ConnectOptions, Event, MqttClient, QoS};

async fn client(port: u16, id: String) -> MqttClient {
    connect(ConnectOptions::new("127.0.0.1", port).client_id(id)).await
}

async fn connect(options: ConnectOptions) -> MqttClient {
    MqttClient::connect(options.receive_maximum(1000))
        .await
        .expect("connect")
}

/// Receives `expected` messages, returning when the last one arrived.
async fn receive(client: &MqttClient, expected: u64) -> Instant {
    let mut received = 0;
    while received < expected {
        match tokio::time::timeout(Duration::from_secs(10), client.recv()).await {
            Ok(Some(Event::Message { .. })) => received += 1,
            Ok(Some(_)) => {}
            _ => panic!("only {received} of {expected} messages arrived"),
        }
    }
    Instant::now()
}

/// `publishers` clients each publish `count` messages to their own topic;
/// one subscriber receives them all.
/// With `persistent`, the subscriber's session outlives its connection, so
/// the broker stores what it sends it.
async fn fan_in(port: u16, qos: QoS, publishers: u64, count: u64, persistent: bool) {
    let options = ConnectOptions::new("127.0.0.1", port).client_id("bench-sub");
    let options = match persistent {
        true => options.session_expiry(3600),
        false => options,
    };
    let subscriber = connect(options).await;
    subscriber.subscribe(&[("bench/#", qos)]).await.unwrap();

    let started = Instant::now();
    let mut tasks = Vec::new();
    for p in 0..publishers {
        let publisher = client(port, format!("bench-pub-{p}")).await;
        tasks.push(tokio::spawn(async move {
            let topic = format!("bench/{p}");
            let payload = vec![b'x'; 64];
            for _ in 0..count {
                publisher
                    .publish(&topic, payload.clone(), qos)
                    .await
                    .unwrap();
            }
        }));
    }
    let done = receive(&subscriber, publishers * count).await;
    for task in tasks {
        task.await.unwrap();
    }
    let seconds = done.duration_since(started).as_secs_f64();
    println!(
        "fan-in QoS {}: {} messages in {:.2}s, {:.0} msg/s",
        qos as u8,
        publishers * count,
        seconds,
        (publishers * count) as f64 / seconds
    );
}

/// One publisher, `subscribers` clients receiving every message.
async fn fan_out(port: u16, subscribers: u64, count: u64) {
    let mut receivers = Vec::new();
    for s in 0..subscribers {
        let subscriber = client(port, format!("bench-fan-{s}")).await;
        subscriber
            .subscribe(&[("fan", QoS::AtMostOnce)])
            .await
            .unwrap();
        receivers.push(tokio::spawn(
            async move { receive(&subscriber, count).await },
        ));
    }
    let publisher = client(port, "bench-fan-pub".into()).await;
    let started = Instant::now();
    let payload = vec![b'x'; 64];
    for _ in 0..count {
        publisher
            .publish("fan", payload.clone(), QoS::AtMostOnce)
            .await
            .unwrap();
    }
    let mut done = started;
    for receiver in receivers {
        done = done.max(receiver.await.unwrap());
    }
    let seconds = done.duration_since(started).as_secs_f64();
    let deliveries = subscribers * count;
    println!(
        "fan-out QoS 0 to {subscribers}: {deliveries} deliveries in {seconds:.2}s, {:.0} msg/s",
        deliveries as f64 / seconds
    );
}

/// `count` clients connect and each subscribes to a topic of its own; then
/// one message is published to every topic. Prints how fast they connected
/// and pauses in between, so the broker's memory can be read.
async fn connections(port: u16, count: u64) {
    let started = Instant::now();
    let mut clients = Vec::with_capacity(count as usize);
    // A few hundred handshakes at a time, as a burst of thousands would
    // overflow the listen backlog
    for batch in (0..count).collect::<Vec<_>>().chunks(500) {
        let mut connecting = tokio::task::JoinSet::new();
        for &i in batch {
            connecting.spawn(async move {
                let client = client(port, format!("bench-conn-{i}")).await;
                client
                    .subscribe(&[(format!("conn/{i}").as_str(), QoS::AtMostOnce)])
                    .await
                    .unwrap();
                client
            });
        }
        clients.extend(connecting.join_all().await);
    }
    let seconds = started.elapsed().as_secs_f64();
    println!(
        "connections: {count} connected and subscribed in {seconds:.2}s, {:.0}/s",
        count as f64 / seconds
    );
    tokio::time::sleep(Duration::from_secs(5)).await;

    let publisher = client(port, "bench-conn-pub".into()).await;
    let started = Instant::now();
    for i in 0..count {
        publisher
            .publish(&format!("conn/{i}"), "x", QoS::AtMostOnce)
            .await
            .unwrap();
    }
    let mut receiving = tokio::task::JoinSet::new();
    for client in clients {
        receiving.spawn(async move {
            matches!(
                tokio::time::timeout(Duration::from_secs(30), client.recv()).await,
                Ok(Some(Event::Message { .. }))
            )
        });
    }
    let received = receiving
        .join_all()
        .await
        .into_iter()
        .filter(|&r| r)
        .count();
    println!(
        "connections: {received} of {count} received their message in {:.2}s",
        started.elapsed().as_secs_f64()
    );
}

fn now_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_micros() as u64
}

/// Round trips of QoS 1 messages published at `rate` per second.
async fn latency(port: u16, rate: u64, count: u64) {
    let subscriber = Arc::new(client(port, "bench-lat-sub".into()).await);
    subscriber
        .subscribe(&[("lat", QoS::AtLeastOnce)])
        .await
        .unwrap();
    let publisher = client(port, "bench-lat-pub".into()).await;

    let collector = tokio::spawn(async move {
        let mut samples = Vec::with_capacity(count as usize);
        while samples.len() < count as usize {
            if let Some(Event::Message { payload, .. }) = subscriber.recv().await {
                let sent = u64::from_be_bytes(payload[..8].try_into().unwrap());
                samples.push(now_micros() - sent);
            }
        }
        samples
    });
    let mut ticks = tokio::time::interval(Duration::from_micros(1_000_000 / rate));
    for _ in 0..count {
        ticks.tick().await;
        let payload = now_micros().to_be_bytes().to_vec();
        publisher
            .publish("lat", payload, QoS::AtLeastOnce)
            .await
            .unwrap();
    }
    let mut samples = collector.await.unwrap();
    samples.sort_unstable();
    let percentile = |p: f64| samples[((samples.len() - 1) as f64 * p) as usize];
    println!(
        "latency QoS 1 at {rate}/s: p50 {}us, p99 {}us, max {}us",
        percentile(0.5),
        percentile(0.99),
        samples[samples.len() - 1]
    );
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let port: u16 = args.next().expect("port").parse().expect("port number");
    match args.next().as_deref().unwrap_or("fan-in-qos0") {
        "fan-in-qos0" => fan_in(port, QoS::AtMostOnce, 10, 100_000, false).await,
        "fan-in-qos1" => fan_in(port, QoS::AtLeastOnce, 10, 20_000, false).await,
        "fan-in-qos1-persistent" => fan_in(port, QoS::AtLeastOnce, 10, 20_000, true).await,
        "fan-out" => fan_out(port, 50, 10_000).await,
        "latency" => latency(port, 1000, 10_000).await,
        "connections" => {
            let count = args.next().map_or(10_000, |n| n.parse().expect("count"));
            connections(port, count).await
        }
        other => panic!("unknown scenario {other}"),
    }
}
