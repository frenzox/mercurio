use std::time::Duration;

use tokio::{io::AsyncReadExt, net::TcpStream, time::timeout};

/// Reads a whole CONNACK, whatever properties it carries.
pub async fn read_connack(stream: &mut TcpStream) -> Vec<u8> {
    timeout(Duration::from_secs(2), async {
        let mut connack = vec![0u8; 2];
        stream.read_exact(&mut connack).await.unwrap();
        assert_eq!(connack[0], 0x20, "expected CONNACK");
        connack.resize(2 + connack[1] as usize, 0);
        stream.read_exact(&mut connack[2..]).await.unwrap();
        connack
    })
    .await
    .expect("no CONNACK")
}
