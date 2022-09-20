use tokio::{net::TcpListener, signal};
use tracing::Level;
use tracing_subscriber::FmtSubscriber;

use mercurio_server::server;

#[tokio::main]
async fn main() -> mercurio_core::Result<()> {
    let subscriber = FmtSubscriber::builder()
        // all spans/events with a level higher than TRACE (e.g, debug, info, warn, etc.)
        // will be written to stdout.
        .with_max_level(Level::TRACE)
        // completes the builder.
        .finish();

    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");
    let listener = TcpListener::bind("127.0.0.1:1883").await?;
    server::run(listener, signal::ctrl_c()).await;

    Ok(())
}
