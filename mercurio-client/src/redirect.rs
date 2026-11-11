//! Server References, which name the servers a broker sends its clients to.

/// A server a Server Reference names, and its port when it gives one.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Server {
    pub host: String,
    pub port: Option<u16>,
}

/// The servers `reference` names, in order: a space-separated list of
/// `host[:port]` entries (MQTT 5 section 4.11), IPv6 addresses in brackets.
/// An entry may start with a scheme such as `mqtts://`; it is kept only
/// when the scheme agrees with `tls`, so a redirection never trades TLS
/// for plain text, nor asks for TLS settings the client doesn't have.
/// Entries that can't be read are left out.
pub(crate) fn servers(reference: &str, tls: bool) -> Vec<Server> {
    reference
        .split_ascii_whitespace()
        .filter_map(|entry| server(entry, tls))
        .collect()
}

fn server(entry: &str, tls: bool) -> Option<Server> {
    let address = match entry.split_once("://") {
        Some((scheme, rest)) => {
            let secure = match scheme.to_ascii_lowercase().as_str() {
                "mqtts" | "ssl" | "tls" | "wss" => true,
                "mqtt" | "tcp" | "ws" => false,
                _ => return None,
            };
            if secure != tls {
                return None;
            }
            rest.split('/').next().unwrap_or_default()
        }
        None => entry,
    };
    let (host, port) = if let Some(rest) = address.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        match after {
            "" => (host, None),
            _ => (host, Some(after.strip_prefix(':')?)),
        }
    } else {
        match address.split_once(':') {
            // More than one colon: an IPv6 address without a port
            Some((_, port)) if port.contains(':') => (address, None),
            Some((host, port)) => (host, Some(port)),
            None => (address, None),
        }
    };
    if host.is_empty() {
        return None;
    }
    let port = match port {
        Some(port) => Some(port.parse().ok().filter(|port| *port != 0)?),
        None => None,
    };
    Some(Server {
        host: host.to_string(),
        port,
    })
}

#[cfg(test)]
mod tests {
    use super::{servers, Server};

    fn server(host: &str, port: Option<u16>) -> Server {
        Server {
            host: host.to_string(),
            port,
        }
    }

    #[test]
    fn a_reference_lists_hosts_with_optional_ports() {
        assert_eq!(
            servers(
                "a.example.com:1884  b.example.com [2001:db8::1]:8883 [::1] ::2",
                false
            ),
            [
                server("a.example.com", Some(1884)),
                server("b.example.com", None),
                server("2001:db8::1", Some(8883)),
                server("::1", None),
                server("::2", None),
            ]
        );
    }

    #[test]
    fn entries_that_cannot_be_read_are_left_out() {
        assert_eq!(
            servers("a:x :1883 [::1 b:0 c:70000 d:1883", false),
            [server("d", Some(1883))]
        );
        assert!(servers("", false).is_empty());
    }

    #[test]
    fn a_scheme_must_keep_tls_as_it_is() {
        let reference =
            "mqtt://plain:1883 mqtts://secure:8883 tcp://plain2 ssl://secure2/x ftp://odd";
        assert_eq!(
            servers(reference, true),
            [server("secure", Some(8883)), server("secure2", None)]
        );
        assert_eq!(
            servers(reference, false),
            [server("plain", Some(1883)), server("plain2", None)]
        );
    }
}
