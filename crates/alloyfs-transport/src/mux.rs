use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio_util::codec::{FramedRead, FramedWrite};

use alloyfs_proto::{
    ErrorCode, Frame, FrameCodec, FsEvent, Request, Response, PROTO_VERSION_MAX, PROTO_VERSION_MIN,
};

/// Backstop for a wedged-but-open peer: a request unanswered this long is
/// failed. The 10 s heartbeat catches most dead peers earlier; this bound
/// guarantees mount callbacks can never hang forever.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Liveness probing for `request_keepalive`: ping this often, and give the
/// peer this long to answer before declaring it wedged.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);
const KEEPALIVE_GRACE: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("connection closed")]
    Closed,
    #[error("request timed out after {}s", REQUEST_TIMEOUT.as_secs())]
    Timeout,
    #[error("handshake failed: {0}")]
    Handshake(String),
    #[error(transparent)]
    Proto(#[from] alloyfs_proto::ProtoError),
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
    /// Requests sent over this connection's lifetime. Observability: lets a
    /// test (or a diag) assert an answer came from a local cache by watching
    /// this NOT move — the strongest no-wire proof short of severing the
    /// connection, and unlike severing it leaves the connection usable.
    requests_sent: AtomicU64,
    tx: mpsc::Sender<Frame>,
    inflight: Arc<DashMap<u64, oneshot::Sender<Result<Response, ErrorCode>>>>,
    pings: Arc<DashMap<u64, oneshot::Sender<()>>>,
    events_tx: broadcast::Sender<Vec<FsEvent>>,
    closed_rx: watch::Receiver<bool>,
    /// The outgoing codec's zstd switch; see [`Self::enable_zstd`].
    zstd_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
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
        Self::establish_with_max(stream, client_name, PROTO_VERSION_MAX).await
    }

    /// [`Self::establish`] with the advertised ceiling lowered. Tests use it to
    /// negotiate an old-protocol session against a current peer: the handshake
    /// takes the min of both sides, so clamping the hello is enough to make a
    /// v9 agent answer as a v5 one — which is how "never send a variant the
    /// session's proto does not cover" gets exercised without an old binary.
    pub async fn establish_with_max<S>(
        stream: S,
        client_name: &str,
        proto_max: u16,
    ) -> Result<Arc<Self>, TransportError>
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        let (r, w) = tokio::io::split(stream);
        let mut reader = FramedRead::new(r, FrameCodec::default());
        let mut writer = FramedWrite::new(w, FrameCodec::default());

        writer
            .send(&Frame::Hello {
                proto_min: PROTO_VERSION_MIN,
                proto_max,
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
            return Err(TransportError::Handshake(format!(
                "server chose unsupported version {proto}"
            )));
        }
        // v3+: both sides may compress large frames from here on.
        writer.encoder_mut().compress = proto >= 3;
        // Kept out so zstd can be flipped AFTER the writer task owns the
        // codec — the caller's config isn't known here (see enable_zstd).
        let zstd_flag = writer.encoder().zstd.clone();

        let (tx, out_rx) = mpsc::channel::<Frame>(256);
        let inflight: Arc<DashMap<u64, oneshot::Sender<Result<Response, ErrorCode>>>> =
            Arc::new(DashMap::new());
        let pings: Arc<DashMap<u64, oneshot::Sender<()>>> = Arc::new(DashMap::new());
        let (events_tx, _) = broadcast::channel(256);
        let (closed_tx, closed_rx) = watch::channel(false);

        // Writer task: sole owner of the outgoing half. Batches flushes when
        // several frames are already queued (see writer::drain).
        tokio::spawn(crate::writer::drain(writer, out_rx));

        // Reader task: routes every incoming frame to its waiter.
        let conn = Arc::new(Self {
            next_id: AtomicU64::new(1),
            requests_sent: AtomicU64::new(0),
            tx,
            inflight: inflight.clone(),
            pings: pings.clone(),
            events_tx: events_tx.clone(),
            closed_rx,
            zstd_flag,
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
            // Stream ended: flip closed FIRST, then fail every waiter. The
            // order matters: request() re-checks is_closed() after inserting
            // its inflight entry, so any insert that lands after this clear
            // necessarily observes closed=true and bails instead of orphaning
            // an entry that would eat the full request timeout.
            let _ = closed_tx.send(true);
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

    /// Requests sent so far; see the field for what this is for.
    pub fn requests_sent(&self) -> u64 {
        self.requests_sent.load(Ordering::Relaxed)
    }

    /// Opt this side's OUTGOING large frames into zstd (v13). Returns
    /// whether it took: refused below v13, because the peer could not
    /// decode the variant — the caller never needs its own version check.
    /// Each direction decides independently; decode always accepts both
    /// algorithms regardless of this switch.
    pub fn enable_zstd(&self) -> bool {
        if self.proto < 13 {
            return false;
        }
        self.zstd_flag.store(true, Ordering::Relaxed);
        true
    }

    /// Send one request and await its response, bounded by REQUEST_TIMEOUT.
    /// Cancel-safe: dropping the future abandons the slot; a late response is
    /// discarded by the reader.
    pub async fn request(&self, body: Request) -> Result<Result<Response, ErrorCode>, TransportError> {
        self.request_with_deadline(body, REQUEST_TIMEOUT).await
    }

    /// Send a request and do NOT wait for its response.
    ///
    /// For requests whose reply the caller throws away. Waiting for one of
    /// those costs a full round trip to learn nothing, which is invisible on a
    /// loopback mount and dominates a remote one: at 60 ms RTT it is 60 ms per
    /// call, paid on a code path that had already written `let _ =`.
    ///
    /// Ordering is unaffected. The frame is handed to the writer in the same
    /// order as any other request, so the peer still processes it in sequence
    /// — this drops the WAIT, not the send, and a later request cannot
    /// overtake it.
    ///
    /// The inflight slot stays registered until the response lands, and the
    /// reader then discards it against a dropped receiver. That is the exact
    /// path a cancelled `request` already takes, which is why `request`
    /// documents itself as cancel-safe.
    pub async fn send_oneway(&self, body: Request) -> Result<(), TransportError> {
        self.begin_request(body).await.map(|_| ())
    }

    /// `request` with an explicit deadline (tests use short ones).
    pub async fn request_with_deadline(
        &self,
        body: Request,
        deadline: Duration,
    ) -> Result<Result<Response, ErrorCode>, TransportError> {
        let (id, done_rx) = self.begin_request(body).await?;
        match tokio::time::timeout(deadline, done_rx).await {
            Ok(done) => done.map_err(|_| TransportError::Closed),
            Err(_) => {
                self.inflight.remove(&id);
                Err(TransportError::Timeout)
            }
        }
    }

    /// A request with no fixed deadline, bounded by peer LIVENESS instead:
    /// while the wait runs, the peer is pinged every KEEPALIVE_INTERVAL and
    /// gets KEEPALIVE_GRACE to answer. Built for `Lock { wait: true }` — a
    /// legitimate lock wait can outlast any fixed timeout, but a wedged
    /// server still can't hang the caller forever, and connection death
    /// fails the wait immediately (teardown drops the inflight entry).
    pub async fn request_keepalive(
        &self,
        body: Request,
    ) -> Result<Result<Response, ErrorCode>, TransportError> {
        let (id, done_rx) = self.begin_request(body).await?;
        tokio::select! {
            done = done_rx => done.map_err(|_| TransportError::Closed),
            _ = self.liveness_lost() => {
                self.inflight.remove(&id);
                Err(TransportError::Timeout)
            }
        }
    }

    /// Resolves when the peer stops answering pings (never resolves while
    /// the peer stays responsive).
    async fn liveness_lost(&self) {
        loop {
            tokio::time::sleep(KEEPALIVE_INTERVAL).await;
            match tokio::time::timeout(KEEPALIVE_GRACE, self.ping()).await {
                Ok(Ok(_)) => {}                // alive (possibly busy) — keep waiting
                Ok(Err(_)) | Err(_) => return, // closed or unanswered ping
            }
        }
    }

    /// Shared request prologue: allocate an id, park the waiter in
    /// `inflight`, and send the frame — with the closed re-check that keeps
    /// teardown from orphaning the entry.
    async fn begin_request(
        &self,
        body: Request,
    ) -> Result<(u64, oneshot::Receiver<Result<Response, ErrorCode>>), TransportError> {
        if self.is_closed() {
            return Err(TransportError::Closed);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.requests_sent.fetch_add(1, Ordering::Relaxed);
        let (done_tx, done_rx) = oneshot::channel();
        self.inflight.insert(id, done_tx);
        // Re-check AFTER inserting: the reader flips closed before clearing
        // the map, so an insert that raced the teardown sees closed here and
        // fails fast instead of waiting out the whole deadline.
        if self.is_closed() {
            self.inflight.remove(&id);
            return Err(TransportError::Closed);
        }
        if self.tx.send(Frame::Request { id, body }).await.is_err() {
            self.inflight.remove(&id);
            return Err(TransportError::Closed);
        }
        Ok((id, done_rx))
    }

    /// Resolves once the connection has died (reader loop ended). For
    /// reconnect supervisors.
    pub async fn closed(&self) {
        let mut rx = self.closed_rx.clone();
        while !*rx.borrow() {
            if rx.changed().await.is_err() {
                return; // sender dropped ⇒ reader gone ⇒ closed
            }
        }
    }

    pub fn is_closed(&self) -> bool {
        *self.closed_rx.borrow()
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
