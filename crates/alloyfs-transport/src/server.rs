use std::sync::Arc;

use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use tokio_util::codec::{FramedRead, FramedWrite};

use alloyfs_proto::{ErrorCode, Frame, FrameCodec, Request, Response, PROTO_VERSION_MAX, PROTO_VERSION_MIN};

use crate::mux::TransportError;

/// Server→client push handle: lets the agent inject `Events` frames into a
/// session's outgoing stream, interleaved safely with responses.
#[derive(Clone)]
pub struct EventPusher {
    tx: mpsc::Sender<Frame>,
}

impl EventPusher {
    /// Push one batch. Returns false when the connection is gone — the
    /// caller should drop its subscription forwarding.
    pub async fn events(&self, batch: Vec<alloyfs_proto::FsEvent>) -> bool {
        self.tx.send(Frame::Events { batch }).await.is_ok()
    }
}

/// What the agent plugs into the transport layer: given one request, produce
/// one response. One handler instance exists per connection (= per session),
/// so implementations can hold per-session state (open handles, attach).
#[async_trait::async_trait]
pub trait RequestHandler: Send + Sync + 'static {
    async fn handle(&self, req: Request) -> Result<Response, ErrorCode>;

    /// Called once after the handshake with a handle for server-push frames.
    /// Default: ignore it (handlers that never push need nothing).
    async fn connected(&self, _push: EventPusher) {}

    /// Called when the connection ends for any reason (clean or not); the
    /// agent uses this to release handles/locks. Default: nothing.
    async fn disconnected(&self) {}
}

/// Server side of one connection: handshake, then dispatch loop.
///
/// Each request is handled in its own task so a slow disk read can't stall
/// the connection — responses go back through the shared writer channel in
/// completion order, and the client's mux matches them up by id.
pub async fn serve_connection<S>(
    stream: S,
    server_name: &str,
    handler: Arc<dyn RequestHandler>,
) -> Result<(), TransportError>
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    serve_connection_with(stream, server_name, handler, PROTO_VERSION_MIN).await
}

/// `serve_connection` that additionally requires the negotiated version to
/// reach `min_proto`. Token-protected TCP listeners pass 3: an older client
/// couldn't decode `AuthRequired` errors, so it's turned away at the
/// handshake with a version mismatch it CAN decode.
pub async fn serve_connection_with<S>(
    stream: S,
    server_name: &str,
    handler: Arc<dyn RequestHandler>,
    min_proto: u16,
) -> Result<(), TransportError>
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (r, w) = tokio::io::split(stream);
    let mut reader = FramedRead::new(r, FrameCodec::default());
    let mut writer = FramedWrite::new(w, FrameCodec::default());

    match reader.next().await {
        Some(Ok(Frame::Hello {
            proto_min,
            proto_max,
            client,
        })) => {
            let lo = proto_min.max(PROTO_VERSION_MIN).max(min_proto);
            let hi = proto_max.min(PROTO_VERSION_MAX);
            if lo > hi {
                let _ = writer
                    .send(&Frame::Response {
                        id: 0,
                        body: Err(ErrorCode::VersionMismatch),
                    })
                    .await;
                return Err(TransportError::Handshake(format!(
                    "no common version with {client} ({proto_min}..={proto_max})"
                )));
            }
            tracing::info!(client, proto = hi, "session connected");
            writer
                .send(&Frame::HelloAck {
                    proto: hi,
                    server: server_name.to_string(),
                })
                .await?;
            // v3+: both sides may compress large frames from here on.
            writer.encoder_mut().compress = hi >= 3;
        }
        Some(Ok(other)) => {
            return Err(TransportError::Handshake(format!(
                "expected Hello, got {other:?}"
            )))
        }
        Some(Err(e)) => return Err(e.into()),
        None => return Err(TransportError::Closed),
    }

    let (out_tx, mut out_rx) = mpsc::channel::<Frame>(256);
    handler.connected(EventPusher { tx: out_tx.clone() }).await;

    let write_task = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            if writer.send(&frame).await.is_err() {
                break;
            }
        }
    });

    let result = loop {
        match reader.next().await {
            Some(Ok(Frame::Request { id, body })) => {
                let handler = handler.clone();
                let out_tx = out_tx.clone();
                tokio::spawn(async move {
                    let body = handler.handle(body).await;
                    let _ = out_tx.send(Frame::Response { id, body }).await;
                });
            }
            Some(Ok(Frame::Ping { nonce })) => {
                let _ = out_tx.send(Frame::Pong { nonce }).await;
            }
            Some(Ok(other)) => {
                tracing::warn!(?other, "unexpected frame on server connection");
            }
            Some(Err(e)) => break Err(e.into()),
            None => break Ok(()), // clean EOF
        }
    };

    drop(out_tx);
    handler.disconnected().await;
    let _ = write_task.await;
    result
}
