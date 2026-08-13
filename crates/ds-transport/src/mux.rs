use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::codec::{FramedRead, FramedWrite};

use ds_proto::{ErrorCode, Frame, FrameCodec, FsEvent, Request, Response, PROTO_VERSION_MAX, PROTO_VERSION_MIN};

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("connection closed")]
    Closed,
    #[error("handshake failed: {0}")]
    Handshake(String),
    #[error(transparent)]
    Proto(#[from] ds_proto::ProtoError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Client side of one connection: many concurrent callers share one stream.
///
/// A writer task owns the outgoing half (all frames funnel through an mpsc
/// channel), a reader task owns the incoming half and routes each frame:
/// `Response` → the oneshot waiting in `inflight`, `Events` → the broadcast
/// channel, `Pong` → the ping table. This is exactly a promise-map over a
/// socket — request N can resolve before request N-1.
pub struct MuxConnection {
    next_id: AtomicU64,
    tx: mpsc::Sender<Frame>,
    inflight: Arc<DashMap<u64, oneshot::Sender<Result<Response, ErrorCode>>>>,
    pings: Arc<DashMap<u64, oneshot::Sender<()>>>,
    events_tx: broadcast::Sender<Vec<FsEvent>>,
    pub server_name: String,
    pub proto: u16,
}

impl MuxConnection {
    /// Perform the client handshake on `stream`, then spawn the reader/writer
    /// tasks and return the ready-to-use connection.
    pub async fn establish<S>(stream: S, client_name: &str) -> Result<Arc<Self>, TransportError>
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        let (r, w) = tokio::io::split(stream);
        let mut reader = FramedRead::new(r, FrameCodec);
        let mut writer = FramedWrite::new(w, FrameCodec);

        writer
            .send(&Frame::Hello {
                proto_min: PROTO_VERSION_MIN,
                proto_max: PROTO_VERSION_MAX,
                client: client_name.to_string(),
            })
            .await?;
        let (proto, server_name) = match reader.next().await {
            Some(Ok(Frame::HelloAck { proto, server })) => (proto, server),
            Some(Ok(other)) => return Err(TransportError::Handshake(format!("unexpected frame {other:?}"))),
            Some(Err(e)) => return Err(e.into()),
            None => return Err(TransportError::Closed),
        };
        if !(PROTO_VERSION_MIN..=PROTO_VERSION_MAX).contains(&proto) {
            return Err(TransportError::Handshake(format!("server chose unsupported version {proto}")));
        }

        let (tx, mut out_rx) = mpsc::channel::<Frame>(256);
        let inflight: Arc<DashMap<u64, oneshot::Sender<Result<Response, ErrorCode>>>> = Arc::new(DashMap::new());
        let pings: Arc<DashMap<u64, oneshot::Sender<()>>> = Arc::new(DashMap::new());
        let (events_tx, _) = broadcast::channel(256);

        // Writer task: sole owner of the outgoing half.
        tokio::spawn(async move {
            while let Some(frame) = out_rx.recv().await {
                if writer.send(&frame).await.is_err() {
                    break;
                }
            }
        });

        // Reader task: routes every incoming frame to its waiter.
        let conn = Arc::new(Self {
            next_id: AtomicU64::new(1),
            tx,
            inflight: inflight.clone(),
            pings: pings.clone(),
            events_tx: events_tx.clone(),
            server_name,
            proto,
        });
        tokio::spawn(async move {
            while let Some(frame) = reader.next().await {
                match frame {
                    Ok(Frame::Response { id, body }) => {
                        if let Some((_, waiter)) = inflight.remove(&id) {
                            let _ = waiter.send(body);
                        } else {
                            tracing::warn!(id, "response for unknown request id");
                        }
                    }
                    Ok(Frame::Events { batch }) => {
                        // Only fails when nobody subscribes — fine to ignore.
                        let _ = events_tx.send(batch);
                    }
                    Ok(Frame::Pong { nonce }) => {
                        if let Some((_, waiter)) = pings.remove(&nonce) {
                            let _ = waiter.send(());
                        }
                    }
                    Ok(other) => tracing::warn!(?other, "unexpected frame on client connection"),
                    Err(e) => {
                        tracing::error!(error = %e, "connection codec error");
                        break;
                    }
                }
            }
            // Stream ended: fail every waiter so callers see Closed, not a hang.
            inflight.clear();
            pings.clear();
        });

        // Heartbeat: one ping every 10 s keeps the server's lease for this
        // session alive (and notices a dead peer). Weak reference so the
        // pinger never keeps a dropped connection alive by itself.
        let weak = Arc::downgrade(&conn);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(10));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let Some(conn) = weak.upgrade() else { break };
                if conn.ping().await.is_err() {
                    break;
                }
            }
        });

        Ok(conn)
    }

    /// Send one request and await its response. Cancel-safe: dropping the
    /// future abandons the slot; a late response is discarded by the reader.
    pub async fn request(&self, body: Request) -> Result<Result<Response, ErrorCode>, TransportError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (done_tx, done_rx) = oneshot::channel();
        self.inflight.insert(id, done_tx);
        if self.tx.send(Frame::Request { id, body }).await.is_err() {
            self.inflight.remove(&id);
            return Err(TransportError::Closed);
        }
        done_rx.await.map_err(|_| TransportError::Closed)
    }

    /// Round-trip a Ping and return the measured latency.
    pub async fn ping(&self) -> Result<Duration, TransportError> {
        let nonce = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (done_tx, done_rx) = oneshot::channel();
        self.pings.insert(nonce, done_tx);
        let start = Instant::now();
        if self.tx.send(Frame::Ping { nonce }).await.is_err() {
            self.pings.remove(&nonce);
            return Err(TransportError::Closed);
        }
        done_rx.await.map_err(|_| TransportError::Closed)?;
        Ok(start.elapsed())
    }

    /// Subscribe to server-pushed event batches.
    pub fn events(&self) -> broadcast::Receiver<Vec<FsEvent>> {
        self.events_tx.subscribe()
    }
}
