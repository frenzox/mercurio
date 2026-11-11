//! MQTT over WebSocket. The MQTT byte stream travels in binary frames,
//! which may each hold any part of it [MQTT-6.0.0-2].

use std::{
    io,
    pin::Pin,
    task::{ready, Context, Poll},
};

use bytes::Bytes;
use futures_util::{Sink, Stream};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::time::timeout;
use tokio_tungstenite::{
    tungstenite::{
        client::IntoClientRequest, http::HeaderValue, protocol::WebSocketConfig, Message,
    },
    WebSocketStream,
};
use tracing::info;

use crate::error::ClientError;
use crate::options::ConnectOptions;

/// Opens the WebSocket at `path` over `stream`, asking for the `mqtt`
/// subprotocol [MQTT-6.0.0-3], within the connect timeout. A broker that
/// doesn't select it is refused.
pub(crate) async fn handshake<S>(
    options: &ConnectOptions,
    stream: S,
    path: &str,
) -> Result<WebSocketTransport<S>, ClientError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let scheme = if options.tls.enabled { "wss" } else { "ws" };
    let url = format!("{scheme}://{}:{}{path}", options.host, options.port);
    info!("Opening WebSocket {}", url);
    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(|e| ClientError::InvalidArgument(format!("WebSocket URL `{url}`: {e}")))?;
    request
        .headers_mut()
        .insert("Sec-WebSocket-Protocol", HeaderValue::from_static("mqtt"));
    // A broker's message holds at most a packet or so; larger ones are
    // refused before they are buffered whole
    let limit = options.incoming_packet_limit() as usize;
    let config = WebSocketConfig::default()
        .max_message_size(Some(limit))
        .max_frame_size(Some(limit));
    let (websocket, _) = timeout(
        options.connect_timeout,
        tokio_tungstenite::client_async_with_config(request, stream, Some(config)),
    )
    .await
    .map_err(|_| ClientError::Timeout)?
    .map_err(|e| {
        ClientError::ConnectionFailed(match e {
            tokio_tungstenite::tungstenite::Error::Io(e) => e,
            e => io::Error::other(e),
        })
    })?;
    Ok(WebSocketTransport {
        websocket,
        pending: Bytes::new(),
    })
}

/// Presents a WebSocket as the plain byte stream MQTT expects.
pub(crate) struct WebSocketTransport<S> {
    websocket: WebSocketStream<S>,
    /// Received bytes not read yet
    pending: Bytes,
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for WebSocketTransport<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            if !self.pending.is_empty() {
                let len = self.pending.len().min(buf.remaining());
                buf.put_slice(&self.pending.split_to(len));
                return Poll::Ready(Ok(()));
            }

            match ready!(Pin::new(&mut self.websocket).poll_next(cx)) {
                Some(Ok(Message::Binary(data))) => self.pending = data,
                // MQTT is only carried in binary frames [MQTT-6.0.0-1]
                Some(Ok(Message::Text(_))) => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "MQTT over WebSocket must use binary frames",
                    )))
                }
                Some(Ok(Message::Close(_))) | None => return Poll::Ready(Ok(())),
                // Pings are answered by the WebSocket layer itself
                Some(Ok(_)) => {}
                Some(Err(e)) => return Poll::Ready(Err(io::Error::other(e))),
            }
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for WebSocketTransport<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut websocket = Pin::new(&mut self.websocket);
        ready!(websocket.as_mut().poll_ready(cx)).map_err(io::Error::other)?;
        websocket
            .start_send(Message::Binary(Bytes::copy_from_slice(buf)))
            .map_err(io::Error::other)?;
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.websocket)
            .poll_flush(cx)
            .map_err(io::Error::other)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.websocket)
            .poll_close(cx)
            .map_err(io::Error::other)
    }
}

#[cfg(test)]
mod tests {
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;
    use tokio::net::{TcpListener, TcpStream};
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

    use super::*;

    #[tokio::test]
    async fn the_client_asks_for_mqtt_at_its_path_and_sends_binary_frames() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut asked = None;
            // The error type is set by tungstenite's handshake callback
            #[allow(clippy::result_large_err)]
            let callback = |request: &Request, mut response: Response| {
                asked = Some((
                    request.uri().path().to_string(),
                    request.headers().get("Sec-WebSocket-Protocol").cloned(),
                ));
                response
                    .headers_mut()
                    .insert("Sec-WebSocket-Protocol", HeaderValue::from_static("mqtt"));
                Ok(response)
            };
            let mut websocket = tokio_tungstenite::accept_hdr_async(stream, callback)
                .await
                .unwrap();
            let frame = websocket.next().await.unwrap().unwrap();
            (asked.unwrap(), frame)
        });

        let options = ConnectOptions::new("127.0.0.1", address.port());
        let stream = TcpStream::connect(address).await.unwrap();
        let mut transport = handshake(&options, stream, "/ws/mqtt").await.unwrap();
        transport.write_all(&[0xc0, 0x00]).await.unwrap();
        transport.flush().await.unwrap();

        let ((path, protocol), frame) = server.await.unwrap();
        assert_eq!(path, "/ws/mqtt");
        assert_eq!(protocol.unwrap(), "mqtt");
        assert_eq!(frame, Message::Binary(Bytes::from_static(&[0xc0, 0x00])));
    }

    #[tokio::test]
    async fn a_server_not_selecting_mqtt_is_refused() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let _ = tokio_tungstenite::accept_async(stream).await;
        });
        let options = ConnectOptions::new("127.0.0.1", address.port());
        let stream = TcpStream::connect(address).await.unwrap();
        let result = handshake(&options, stream, "/mqtt").await;
        assert!(
            matches!(result, Err(ClientError::ConnectionFailed(_))),
            "{:?}",
            result.err()
        );
    }
}
