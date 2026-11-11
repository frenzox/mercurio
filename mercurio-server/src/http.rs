//! Static files served by WebSocket listeners next to MQTT, such as a
//! browser client's page, as mosquitto's `http_dir` does.

use std::{
    io,
    path::{Component, Path, PathBuf},
    pin::Pin,
    task::{Context, Poll},
};

use tokio::{
    fs::File,
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf},
    sync::Semaphore,
    time::Duration,
};

/// Longest request head read before giving up on the request
const MAX_HEAD: usize = 8192;

/// How long answering one request may take, the file sent included. Kept
/// apart from the connect deadline, which only covers reading the request.
pub(crate) const RESPONSE_TIMEOUT: Duration = Duration::from_secs(60);

/// Responses one listener sends at once; further requests are answered
/// with Service Unavailable.
pub(crate) const MAX_RESPONSES: usize = 32;

/// The files a listener serves, and the responses it may send at once.
pub(crate) struct Files {
    pub dir: PathBuf,
    pub responses: Semaphore,
}

impl Files {
    pub(crate) fn new(dir: PathBuf) -> Self {
        Files {
            dir,
            responses: Semaphore::new(MAX_RESPONSES),
        }
    }
}

/// The request at the start of a WebSocket listener's connection.
pub(crate) enum Request<S> {
    /// A WebSocket upgrade to complete, with the bytes read so far replayed
    Upgrade(Rewind<S>),
    /// A plain HTTP request, with its head, to answer from the files
    Files(S, String),
}

/// Reads the HTTP request head at the start of `stream`.
pub(crate) async fn read_request<S>(mut stream: S) -> io::Result<Request<S>>
where
    S: AsyncRead + Unpin,
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

    let text = String::from_utf8_lossy(&head).into_owned();
    let upgrade = text
        .to_ascii_lowercase()
        .lines()
        .any(|line| line.starts_with("upgrade:") && line.contains("websocket"));
    if upgrade {
        return Ok(Request::Upgrade(Rewind::new(stream, head)));
    }
    Ok(Request::Files(stream, text))
}

/// Answers the plain HTTP request `head` from `files`, if the listener may
/// send another response.
pub(crate) async fn respond<S>(mut stream: S, head: &str, files: &Files) -> io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    let Ok(_response) = files.responses.try_acquire() else {
        stream.write_all(&status("503 Service Unavailable")).await?;
        return stream.shutdown().await;
    };
    let mut words = head.split_whitespace();
    let (method, target) = (words.next().unwrap_or(""), words.next().unwrap_or(""));
    let found = match method {
        "GET" | "HEAD" => find(&files.dir, target).await,
        _ => Found::Status("405 Method Not Allowed"),
    };
    match found {
        Found::File(file, path, length) => {
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {length}\r\n\
                 Connection: close\r\n\r\n",
                content_type(&path),
            );
            stream.write_all(header.as_bytes()).await?;
            if method == "GET" {
                tokio::io::copy(&mut file.take(length), &mut stream).await?;
            }
        }
        Found::Redirect(location) => {
            let response = format!(
                "HTTP/1.1 301 Moved Permanently\r\nLocation: {location}\r\n\
                 Content-Length: 0\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(response.as_bytes()).await?;
        }
        Found::Status(code) => stream.write_all(&status(code)).await?,
    }
    stream.shutdown().await
}

/// What a request's target names.
enum Found {
    /// A file, its path and length
    File(File, PathBuf, u64),
    /// A directory named without its trailing slash: where it is
    Redirect(String),
    Status(&'static str),
}

async fn find(dir: &Path, target: &str) -> Found {
    let not_found = Found::Status("404 Not Found");
    let Some(path) = file_path(dir, target) else {
        return not_found;
    };
    let Ok(metadata) = tokio::fs::metadata(&path).await else {
        return not_found;
    };
    if metadata.is_dir() {
        return match redirect(target) {
            Some(location) => Found::Redirect(location),
            None => not_found,
        };
    }
    if !metadata.is_file() {
        return not_found;
    }
    match File::open(&path).await {
        Ok(file) => Found::File(file, path, metadata.len()),
        Err(_) => not_found,
    }
}

/// Where a directory named by `target` is, with its trailing slash and
/// query. Rebuilt from the path's segments, so `//host` can't send the
/// client to another host. None when `target` had the slash already.
fn redirect(target: &str) -> Option<String> {
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (target, None),
    };
    if path.ends_with('/') {
        return None;
    }
    let mut location = String::from("/");
    for segment in path.split('/').filter(|s| !s.is_empty()) {
        location.push_str(segment);
        location.push('/');
    }
    if let Some(query) = query {
        location.push('?');
        location.push_str(query);
    }
    Some(location)
}

/// The file `target` names under `dir`, `index.html` for targets ending in
/// a slash. `None` for targets that would leave `dir`.
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
    if path.ends_with('/') {
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
    use super::{file_path, redirect};
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

    #[test]
    fn test_redirects_stay_on_this_host() {
        assert_eq!(redirect("/docs").as_deref(), Some("/docs/"));
        assert_eq!(redirect("/a//b?x=1").as_deref(), Some("/a/b/?x=1"));
        assert_eq!(
            redirect("//evil.example").as_deref(),
            Some("/evil.example/")
        );
        assert_eq!(redirect("").as_deref(), Some("/"));
        assert_eq!(redirect("/docs/"), None);
    }
}
