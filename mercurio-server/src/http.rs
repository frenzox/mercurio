//! Static files served by WebSocket listeners next to MQTT, such as a
//! browser client's page, as mosquitto's `http_dir` does.

use std::{
    io,
    path::{Component, Path, PathBuf},
    pin::Pin,
    task::{Context, Poll},
};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};

/// Longest request head read before giving up on the request
const MAX_HEAD: usize = 8192;

/// Reads the HTTP request at the start of `stream`. A WebSocket upgrade is
/// returned to be completed, with the bytes read so far replayed; any other
/// request is answered from `dir`, and `None` returned.
pub(crate) async fn serve_or_upgrade<S>(mut stream: S, dir: &Path) -> io::Result<Option<Rewind<S>>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut head = Vec::new();
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        if head.len() >= MAX_HEAD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request head too long",
            ));
        }
        let mut chunk = [0u8; 1024];
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        head.extend_from_slice(&chunk[..read]);
    }

    let text = String::from_utf8_lossy(&head).to_ascii_lowercase();
    let upgrade = text
        .lines()
        .any(|line| line.starts_with("upgrade:") && line.contains("websocket"));
    if upgrade {
        return Ok(Some(Rewind::new(stream, head)));
    }

    let response = respond(&String::from_utf8_lossy(&head), dir).await;
    stream.write_all(&response).await?;
    stream.shutdown().await?;
    Ok(None)
}

/// The response to a plain HTTP request.
async fn respond(head: &str, dir: &Path) -> Vec<u8> {
    let mut words = head.split_whitespace();
    let (method, target) = (words.next().unwrap_or(""), words.next().unwrap_or(""));
    if method != "GET" && method != "HEAD" {
        return status("405 Method Not Allowed");
    }
    let Some(path) = file_path(dir, target) else {
        return status("404 Not Found");
    };
    let Ok(body) = tokio::fs::read(&path).await else {
        return status("404 Not Found");
    };

    let mut response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        content_type(&path),
        body.len()
    )
    .into_bytes();
    if method == "GET" {
        response.extend_from_slice(&body);
    }
    response
}

/// The file `target` names under `dir`, `index.html` for directories.
/// `None` for targets that would leave `dir`.
fn file_path(dir: &Path, target: &str) -> Option<PathBuf> {
    let path = target.split(['?', '#']).next()?;
    let relative = Path::new(path.trim_start_matches('/'));
    if !relative
        .components()
        .all(|c| matches!(c, Component::Normal(_)))
    {
        return None;
    }
    let mut file = dir.join(relative);
    if path.ends_with('/') || file.is_dir() {
        file.push("index.html");
    }
    Some(file)
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html" | "htm") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript",
        Some("css") => "text/css",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("wasm") => "application/wasm",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn status(status: &str) -> Vec<u8> {
    format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").into_bytes()
}

/// A stream whose first bytes, already read from it, are read again.
pub(crate) struct Rewind<S> {
    stream: S,
    prefix: Vec<u8>,
    position: usize,
}

impl<S> Rewind<S> {
    pub(crate) fn new(stream: S, prefix: Vec<u8>) -> Self {
        Self {
            stream,
            prefix,
            position: 0,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Rewind<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.position < self.prefix.len() {
            let rest = &self.prefix[self.position..];
            let len = rest.len().min(buf.remaining());
            buf.put_slice(&rest[..len]);
            self.position += len;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Rewind<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::file_path;
    use std::path::Path;

    #[test]
    fn test_paths_stay_inside_the_directory() {
        let dir = Path::new("/srv/www");
        assert_eq!(
            file_path(dir, "/app.js?v=2"),
            Some(Path::new("/srv/www/app.js").into())
        );
        assert_eq!(
            file_path(dir, "/docs/"),
            Some(Path::new("/srv/www/docs/index.html").into())
        );
        assert_eq!(file_path(dir, "/../etc/passwd"), None);
        assert_eq!(file_path(dir, "/a/../../etc/passwd"), None);
        assert_eq!(
            file_path(dir, "//etc/passwd"),
            Some(Path::new("/srv/www/etc/passwd").into())
        );
    }
}
