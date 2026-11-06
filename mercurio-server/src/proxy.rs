//! The PROXY protocol, versions 1 and 2, which load balancers use to pass on
//! the address of the client they accepted the connection from.

use std::{
    io::{Error, ErrorKind, Result},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
};

use tokio::io::{AsyncRead, AsyncReadExt};

const V2_SIGNATURE: [u8; 12] = *b"\r\n\r\n\0\r\nQUIT\n";

/// Longest version 1 header, line ending included
const V1_MAX_LENGTH: usize = 107;

/// Reads the PROXY header at the start of `stream`, and nothing after it.
/// Returns the client's address, or `None` when the header carries none,
/// such as for the load balancer's own health checks.
pub(crate) async fn read_header<S: AsyncRead + Unpin>(
    stream: &mut S,
) -> Result<Option<SocketAddr>> {
    // Shorter than any version 1 header, so this never reads past one
    let mut start = [0u8; 12];
    stream.read_exact(&mut start).await?;
    if start == V2_SIGNATURE {
        return read_v2(stream).await;
    }
    if !start.starts_with(b"PROXY ") {
        return Err(invalid("not a PROXY protocol header"));
    }

    let mut line = start.to_vec();
    while !line.ends_with(b"\r\n") {
        if line.len() == V1_MAX_LENGTH {
            return Err(invalid("PROXY header too long"));
        }
        line.push(stream.read_u8().await?);
    }
    parse_v1(&line[..line.len() - 2])
}

fn parse_v1(line: &[u8]) -> Result<Option<SocketAddr>> {
    let line = std::str::from_utf8(line).map_err(|_| invalid("PROXY header is not text"))?;
    let fields: Vec<&str> = line.split(' ').collect();
    match fields[..] {
        ["PROXY", "UNKNOWN", ..] => Ok(None),
        ["PROXY", "TCP4" | "TCP6", source, _, port, _] => {
            let ip: IpAddr = source.parse().map_err(|_| invalid("bad source address"))?;
            let port: u16 = port.parse().map_err(|_| invalid("bad source port"))?;
            Ok(Some(SocketAddr::new(ip, port)))
        }
        _ => Err(invalid("malformed PROXY header")),
    }
}

async fn read_v2<S: AsyncRead + Unpin>(stream: &mut S) -> Result<Option<SocketAddr>> {
    let mut header = [0u8; 4];
    stream.read_exact(&mut header).await?;
    let [version_command, family, length @ ..] = header;
    if version_command >> 4 != 2 {
        return Err(invalid("unsupported PROXY protocol version"));
    }
    let mut addresses = vec![0u8; u16::from_be_bytes(length).into()];
    stream.read_exact(&mut addresses).await?;

    // LOCAL connections come from the load balancer itself
    if version_command & 0x0f == 0 {
        return Ok(None);
    }
    let source = match family >> 4 {
        1 if addresses.len() >= 12 => {
            let ip: [u8; 4] = addresses[..4].try_into().unwrap();
            let port = u16::from_be_bytes([addresses[8], addresses[9]]);
            SocketAddr::new(Ipv4Addr::from(ip).into(), port)
        }
        2 if addresses.len() >= 36 => {
            let ip: [u8; 16] = addresses[..16].try_into().unwrap();
            let port = u16::from_be_bytes([addresses[32], addresses[33]]);
            SocketAddr::new(Ipv6Addr::from(ip).into(), port)
        }
        // Unix sockets and unspecified families have no IP address
        _ => return Ok(None),
    };
    Ok(Some(source))
}

fn invalid(message: &str) -> Error {
    Error::new(ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::read_header;
    use tokio::io::AsyncReadExt;

    async fn read(bytes: &[u8]) -> (std::io::Result<Option<String>>, Vec<u8>) {
        let mut stream = bytes;
        let header = read_header(&mut stream)
            .await
            .map(|addr| addr.map(|a| a.to_string()));
        let mut rest = Vec::new();
        stream.read_to_end(&mut rest).await.unwrap();
        (header, rest)
    }

    #[tokio::test]
    async fn test_v1_headers_stop_at_the_line_end() {
        let (header, rest) = read(b"PROXY TCP4 192.0.2.1 198.51.100.1 56324 1883\r\n\x10").await;
        assert_eq!(header.unwrap().as_deref(), Some("192.0.2.1:56324"));
        assert_eq!(rest, b"\x10");

        let (header, rest) = read(b"PROXY TCP6 2001:db8::1 2001:db8::2 4000 1883\r\nx").await;
        assert_eq!(header.unwrap().as_deref(), Some("[2001:db8::1]:4000"));
        assert_eq!(rest, b"x");

        let (header, rest) = read(b"PROXY UNKNOWN\r\nx").await;
        assert_eq!(header.unwrap(), None);
        assert_eq!(rest, b"x");
    }

    #[tokio::test]
    async fn test_v2_headers() {
        let mut bytes = super::V2_SIGNATURE.to_vec();
        // PROXY command, TCP over IPv4, 12 bytes of addresses
        bytes.extend_from_slice(&[0x21, 0x11, 0x00, 0x0c]);
        bytes.extend_from_slice(&[192, 0, 2, 1, 198, 51, 100, 1, 0xdc, 0x04, 0x07, 0x5b]);
        bytes.push(0x10);
        let (header, rest) = read(&bytes).await;
        assert_eq!(header.unwrap().as_deref(), Some("192.0.2.1:56324"));
        assert_eq!(rest, b"\x10");

        // LOCAL command, no addresses
        let mut bytes = super::V2_SIGNATURE.to_vec();
        bytes.extend_from_slice(&[0x20, 0x00, 0x00, 0x00, 0x10]);
        let (header, rest) = read(&bytes).await;
        assert_eq!(header.unwrap(), None);
        assert_eq!(rest, b"\x10");
    }

    #[tokio::test]
    async fn test_other_data_is_refused() {
        let (header, _) = read(b"\x10\x0e\x00\x04MQTT\x05\x02\x00\x3c\x00\x00").await;
        assert!(header.is_err());
        let (header, _) = read(&[b'P'; 200]).await;
        assert!(header.is_err());
        let mut long = b"PROXY TCP4 ".to_vec();
        long.resize(200, b'1');
        let (header, _) = read(&long).await;
        assert!(header.is_err());
    }
}
