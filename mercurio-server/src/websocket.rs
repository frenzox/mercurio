//! MQTT over WebSocket. The MQTT byte stream travels in binary frames,
//! which may each hold any part of it [MQTT-6.0.0-2].

use std::{
    io,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    task::{ready, Context, Poll},
};

use bytes::Bytes;
use futures_util::{Sink, Stream};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_tungstenite::{
    tungstenite::{
        self,
        handshake::server::{ErrorResponse, Request, Response},
        http::{HeaderValue, StatusCode},
        protocol::WebSocketConfig,
        Message,
    },
    WebSocketStream,
};

const SUBPROTOCOL_HEADER: &str = "Sec-WebSocket-Protocol";

/// Completes the WebSocket handshake on `stream`, selecting the `mqtt`
/// subprotocol [MQTT-6.0.0-4]. A client that doesn't offer it is refused
/// with 400 Bad Request, and one whose `Origin` isn't among
/// `allowed_origins`, when given, with 403 Forbidden. Messages larger than
/// `max_message_size` are refused before they are buffered whole.
pub(crate) async fn accept<S>(
    stream: S,
    max_message_size: usize,
    allowed_origins: Option<&[String]>,
) -> Result<WebSocketTransport<S>, tungstenite::Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // The error type is set by tungstenite's handshake callback
    #[allow(clippy::result_large_err)]
    let select_mqtt = |request: &Request, mut response: Response| {
        if let (Some(allowed), Some(origin)) = (allowed_origins, request.headers().get("Origin")) {
            let allowed = origin
                .to_str()
                .is_ok_and(|origin| allowed.iter().any(|a| a.eq_ignore_ascii_case(origin)));
            if !allowed {
                return Err(refusal(StatusCode::FORBIDDEN, "origin not allowed"));
            }
        }
        let offers_mqtt = request
            .headers()
            .get_all(SUBPROTOCOL_HEADER)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .any(|protocol| protocol.trim() == "mqtt");
        if !offers_mqtt {
            return Err(refusal(
                StatusCode::BAD_REQUEST,
                "the mqtt subprotocol is required",
            ));
        }
        response
            .headers_mut()
            .insert(SUBPROTOCOL_HEADER, HeaderValue::from_static("mqtt"));
        Ok(response)
    };

    let config = WebSocketConfig::default()
        .max_message_size(Some(max_message_size))
        .max_frame_size(Some(max_message_size));
    let budget = ReadBudget {
        remaining: Arc::new(AtomicUsize::new(usize::MAX)),
        ceiling: max_message_size,
    };
    let stream = Budgeted {
        stream,
        budget: budget.clone(),
    };
    let websocket =
        tokio_tungstenite::accept_hdr_async_with_config(stream, select_mqtt, Some(config)).await?;
    Ok(WebSocketTransport {
        websocket,
        pending: Bytes::new(),
        budget,
    })
}

fn refusal(status: StatusCode, reason: &str) -> ErrorResponse {
    let mut response = ErrorResponse::new(Some(reason.to_string()));
    *response.status_mut() = status;
    response
}

/// Bytes a WebSocket client may still send, while its packets are held to
/// a smaller size than the WebSocket's own message limit, such as CONNECT
/// before the client is authenticated.
///
/// tungstenite buffers a whole message, up to the limit set at the
/// handshake, before handing it on, and tokio-tungstenite can't change that
/// limit afterwards. So the bytes read from the socket under the WebSocket
/// are counted instead, and reading past the budget fails.
#[derive(Clone)]
pub(crate) struct ReadBudget {
    /// `usize::MAX` when unbounded
    remaining: Arc<AtomicUsize>,
    /// The WebSocket's own message limit, past which no budget is needed
    ceiling: usize,
}

impl ReadBudget {
    /// Holds the client to packets of `max_packet_size`, from now on.
    /// Twice that many bytes may be read, which leaves room for framing.
    pub(crate) fn limit(&self, max_packet_size: usize) {
        let remaining = match max_packet_size >= self.ceiling {
            true => usize::MAX,
            false => max_packet_size.saturating_mul(2),
        };
        self.remaining.store(remaining, Ordering::Relaxed);
    }
}

/// The socket under a WebSocket, read within its `ReadBudget`.
struct Budgeted<S> {
    stream: S,
    budget: ReadBudget,
}

impl<S: AsyncRead + Unpin> AsyncRead for Budgeted<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let remaining = self.budget.remaining.load(Ordering::Relaxed);
        if remaining == usize::MAX {
            return Pin::new(&mut self.stream).poll_read(cx, buf);
        }
        if remaining == 0 {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the client sent more than its packet size limit allows",
            )));
        }
        // Never read past the budget, so pipelined packets wait in the
        // socket until the limit is lifted
        let mut limited = ReadBuf::new(buf.initialize_unfilled_to(remaining.min(buf.remaining())));
        ready!(Pin::new(&mut self.stream).poll_read(cx, &mut limited))?;
        let read = limited.filled().len();
        buf.advance(read);
        self.budget.remaining.fetch_sub(read, Ordering::Relaxed);
        Poll::Ready(Ok(()))
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Budgeted<S> {
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

/// Presents a WebSocket as the plain byte stream MQTT expects.
pub(crate) struct WebSocketTransport<S> {
    websocket: WebSocketStream<Budgeted<S>>,
    /// Received bytes not read yet
    pending: Bytes,
    budget: ReadBudget,
}

impl<S> WebSocketTransport<S> {
    pub(crate) fn read_budget(&self) -> ReadBudget {
        self.budget.clone()
    }
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
