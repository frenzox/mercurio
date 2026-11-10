//! Logging to the local syslog daemon, through its `/dev/log` socket.

use std::{
    io,
    os::unix::net::UnixDatagram,
    sync::{Arc, Mutex},
};

use tracing::{Level, Metadata};
use tracing_subscriber::fmt::MakeWriter;

/// Makes one writer per log line, sent as one datagram when dropped.
pub struct Syslog {
    socket: Arc<Socket>,
    /// `mercuriod[<pid>]: `, the tag syslog daemons expect
    tag: String,
}

/// A datagram socket connected to the syslog daemon's, connected again when
/// the daemon restarts and binds a new one.
struct Socket {
    path: String,
    connected: Mutex<UnixDatagram>,
}

impl Socket {
    fn connect(path: &str) -> io::Result<UnixDatagram> {
        let socket = UnixDatagram::unbound()?;
        socket.connect(path)?;
        Ok(socket)
    }

    fn send(&self, datagram: &[u8]) -> io::Result<()> {
        let mut connected = self.connected.lock().unwrap_or_else(|e| e.into_inner());
        if connected.send(datagram).is_ok() {
            return Ok(());
        }
        *connected = Self::connect(&self.path)?;
        connected.send(datagram).map(|_| ())
    }
}

impl Syslog {
    pub fn connect(path: &str) -> io::Result<Self> {
        Ok(Self {
            socket: Arc::new(Socket {
                path: path.to_string(),
                connected: Mutex::new(Socket::connect(path)?),
            }),
            tag: format!("mercuriod[{}]: ", std::process::id()),
        })
    }

    fn line(&self, level: Level) -> Line {
        // RFC 3164 priority: the daemon facility (3) times 8, plus severity
        let severity = match level {
            Level::ERROR => 3,
            Level::WARN => 4,
            Level::INFO => 6,
            _ => 7,
        };
        let mut buffer = format!("<{}>", 3 * 8 + severity).into_bytes();
        buffer.extend_from_slice(self.tag.as_bytes());
        Line {
            socket: Arc::clone(&self.socket),
            start: buffer.len(),
            buffer,
        }
    }
}

pub struct Line {
    socket: Arc<Socket>,
    buffer: Vec<u8>,
    /// Where the message starts, after the priority and tag
    start: usize,
}

impl io::Write for Line {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for Line {
    fn drop(&mut self) {
        while self.buffer.last() == Some(&b'\n') {
            self.buffer.pop();
        }
        // The formatter pads levels, as in " INFO"
        while self.buffer.get(self.start) == Some(&b' ') {
            self.buffer.remove(self.start);
        }
        // Nowhere to report a failure to log
        let _ = self.socket.send(&self.buffer);
    }
}

impl<'a> MakeWriter<'a> for Syslog {
    type Writer = Line;

    fn make_writer(&'a self) -> Line {
        self.line(Level::INFO)
    }

    fn make_writer_for(&'a self, meta: &Metadata<'_>) -> Line {
        self.line(*meta.level())
    }
}

#[cfg(test)]
mod tests {
    use super::Syslog;
    use std::{io::Write, os::unix::net::UnixDatagram};
    use tracing::Level;

    #[test]
    fn test_lines_carry_priority_and_tag() {
        let path = std::env::temp_dir().join(format!("mercuriod-syslog-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let receiver = UnixDatagram::bind(&path).unwrap();
        let syslog = Syslog::connect(path.to_str().unwrap()).unwrap();

        let mut line = syslog.line(Level::WARN);
        writeln!(line, " WARN disk almost full").unwrap();
        drop(line);

        let mut buffer = [0u8; 256];
        let len = receiver.recv(&mut buffer).unwrap();
        let expected = format!(
            "<28>mercuriod[{}]: WARN disk almost full",
            std::process::id()
        );
        assert_eq!(std::str::from_utf8(&buffer[..len]).unwrap(), expected);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn test_logging_resumes_after_the_daemon_restarts() {
        let path =
            std::env::temp_dir().join(format!("mercuriod-syslog-restart-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let receiver = UnixDatagram::bind(&path).unwrap();
        let syslog = Syslog::connect(path.to_str().unwrap()).unwrap();
        writeln!(syslog.line(Level::INFO), "before").unwrap();
        let mut buffer = [0u8; 256];
        receiver.recv(&mut buffer).unwrap();

        // The daemon restarts, binding a new socket at the same path
        drop(receiver);
        std::fs::remove_file(&path).unwrap();
        let receiver = UnixDatagram::bind(&path).unwrap();
        receiver
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();

        writeln!(syslog.line(Level::INFO), "after").unwrap();
        let len = receiver.recv(&mut buffer).expect("the line was lost");
        assert!(std::str::from_utf8(&buffer[..len])
            .unwrap()
            .ends_with("after"));
        std::fs::remove_file(&path).unwrap();
    }
}
