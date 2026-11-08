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
use tokio_tungstenite::{
    tungstenite::{
        self,
        handshake::server::{ErrorResponse, Request, Response},
        http::HeaderValue,
        protocol::WebSocketConfig,
        Message,
    },
    WebSocketStream,
};

const SUBPROTOCOL_HEADER: &str = "Sec-WebSocket-Protocol";

/// Completes the WebSocket handshake on `stream`, selecting the `mqtt`
/// subprotocol when the client offers it [MQTT-6.0.0-4]. Messages larger
/// than `max_message_size` are refused before they are buffered whole.
pub(crate) async fn accept<S>(
    stream: S,
    max_message_size: usize,
) -> Result<WebSocketTransport<S>, tungstenite::Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // The error type is set by tungstenite's handshake callback
    #[allow(clippy::result_large_err)]
    let select_mqtt = |request: &Request, mut response: Response| {
        let offers_mqtt = request
            .headers()
            .get_all(SUBPROTOCOL_HEADER)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .any(|protocol| protocol.trim() == "mqtt");
        if offers_mqtt {
            response
                .headers_mut()
                .insert(SUBPROTOCOL_HEADER, HeaderValue::from_static("mqtt"));
        }
        Ok::<_, ErrorResponse>(response)
    };

    let config = WebSocketConfig::default()
        .max_message_size(Some(max_message_size))
        .max_frame_size(Some(max_message_size));
    let websocket =
        tokio_tungstenite::accept_hdr_async_with_config(stream, select_mqtt, Some(config)).await?;
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
                // The end of the stream
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
