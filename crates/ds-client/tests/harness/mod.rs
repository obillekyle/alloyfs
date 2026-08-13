//! In-process loopback harness: a real `AgentSession` served over one half of
//! a `tokio::io::duplex`, a real `RemoteFs` attached over the other half. No
//! sockets, no mounts — the full wire protocol, deterministic and fast.
//!
//! `RemoteFs` methods are synchronous and `block_on` a captured runtime
//! handle, so every call from a test must go through `on_fs` (spawn_blocking)
//! and every test must run on a multi-thread runtime.

#![allow(dead_code)]

use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use ds_agent::{watch, AgentConfig, AgentSession, Export, ExportConfig, ExportRegistry};
use ds_client::{ClientOptions, Dialer, FsError, RemoteFs, ROOT_INO};
use ds_proto::{Attr, ErrorCode, FsEvent, OpenFlags};
use ds_transport::{serve_connection, MuxConnection, RequestHandler};
use futures::future::BoxFuture;
use tokio::io::{AsyncRead, AsyncWrite, DuplexStream, ReadBuf};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

// ---------------------------------------------------------------- test agent

pub struct AgentOpts {
    pub read_only: bool,
    pub excludes: Vec<String>,
    pub watch: bool,
    pub debounce_ms: u64,
    /// The export's `client:` section — suggested mount defaults (proto v2).
    pub client_defaults: Option<ds_agent::ClientDefaults>,
}

impl Default for AgentOpts {
    fn default() -> Self {
        Self {
            read_only: false,
            excludes: Vec::new(),
            watch: false,
            debounce_ms: 100,
            client_defaults: None,
        }
    }
}

pub struct TestAgent {
    pub dir: tempfile::TempDir,
    pub registry: Arc<ExportRegistry>,
    pub export: Arc<Export>,
    _watch: Option<watch::WatchGuard>,
}

/// One export named "test" over a fresh tempdir. No lease reaper: tests that
/// need lock release use `Session::sever` for a deterministic disconnect.
pub fn start_agent(mut opts: AgentOpts) -> TestAgent {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let mut cfg = AgentConfig::default();
    cfg.exports.insert(
        "test".to_string(),
        ExportConfig {
            path: dir.path().to_path_buf(),
            read_only: opts.read_only,
            exclude: opts.excludes.clone(),
            client: opts.client_defaults.take(),
        },
    );
    let registry = Arc::new(ExportRegistry::from_config(&cfg).expect("export registry"));
    let export = registry.get("test").expect("export 'test'");
    let watch_guard = if opts.watch {
        Some(
            watch::spawn(
                export.clone(),
                export.events.clone(),
                Duration::from_millis(opts.debounce_ms),
            )
            .expect("watcher"),
        )
    } else {
        None
    };
    TestAgent {
        dir,
        registry,
        export,
        _watch: watch_guard,
    }
}

// ------------------------------------------------------------ severable pipe

type SeverState = Arc<Mutex<(bool, Vec<Waker>)>>;

/// Wraps ONE half of the duplex; both halves of a link share one state so a
/// single `sever()` kills the link in BOTH directions, like a broken socket:
/// reads yield EOF, writes fail with BrokenPipe.
///
/// Why both directions matter: severing only the server's read side leaves a
/// half-alive zombie — the server can still push frames to the client, the
/// client never observes EOF, so `closed()` never resolves and the reconnect
/// supervisor never dials. Server-side, the EOF walks `serve_connection`
/// into its clean-EOF path → `disconnected()` → locks/handles released.
pub struct Severable<S> {
    inner: S,
    state: SeverState,
}

#[derive(Clone)]
pub struct SeverHandle(SeverState);

impl SeverHandle {
    pub fn sever(&self) {
        let wakers = {
            let mut guard = self.0.lock().unwrap();
            guard.0 = true;
            std::mem::take(&mut guard.1)
        };
        for waker in wakers {
            waker.wake();
        }
    }
}

impl<S> Severable<S> {
    pub fn new(inner: S) -> (Self, SeverHandle) {
        let state: SeverState = Arc::new(Mutex::new((false, Vec::new())));
        let handle = SeverHandle(state.clone());
        (Self { inner, state }, handle)
    }

    /// A second wrapper sharing this one's sever state (the peer half).
    pub fn sibling(inner: S, handle: &SeverHandle) -> Self {
        Self {
            inner,
            state: handle.0.clone(),
        }
    }

    fn severed(&self) -> bool {
        self.state.lock().unwrap().0
    }

    /// Park on the sever flag: re-check under the lock (a sever between the
    /// caller's check and here must not be missed) and register the waker.
    /// Returns true when severed.
    fn park(&self, cx: &mut Context<'_>) -> bool {
        let mut guard = self.state.lock().unwrap();
        if guard.0 {
            return true;
        }
        guard.1.push(cx.waker().clone());
        false
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Severable<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.severed() {
            // Report EOF by leaving the buffer untouched.
            return Poll::Ready(Ok(()));
        }
        match Pin::new(&mut self.inner).poll_read(cx, buf) {
            Poll::Pending => {
                if self.park(cx) {
                    return Poll::Ready(Ok(()));
                }
                Poll::Pending
            }
            ready => ready,
        }
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Severable<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if self.severed() {
            return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()));
        }
        match Pin::new(&mut self.inner).poll_write(cx, buf) {
            Poll::Pending => {
                if self.park(cx) {
                    return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()));
                }
                Poll::Pending
            }
            ready => ready,
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        if self.severed() {
            return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()));
        }
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

// -------------------------------------------------------------- client session

/// Tracks the SeverHandle of the CURRENTLY live server link. Reconnectable
/// sessions replace the handle on every dial.
type SeverSlot = Arc<Mutex<Option<SeverHandle>>>;

/// A connected client. Deliberately does NOT hold an `Arc<MuxConnection>`:
/// holding the first connection forever would keep its event broadcast
/// channel open after a reconnect, so the event pump would never observe
/// `Closed` and never resubscribe. Take snapshots via `conn()` instead.
pub struct Session {
    pub fs: Arc<RemoteFs>,
    sever: SeverSlot,
    _server: Option<JoinHandle<()>>,
}

impl Session {
    /// Snapshot of the live connection (changes across reconnects).
    pub fn conn(&self) -> Arc<MuxConnection> {
        self.fs.conn()
    }

    /// Force the CURRENT server link to see EOF (simulated dead client).
    pub fn sever(&self) {
        self.sever
            .lock()
            .unwrap()
            .as_ref()
            .expect("session has a live link")
            .sever();
    }
}

/// Spawn one server link: fresh `AgentSession` served over the severable
/// server half of a new duplex pair. The returned client half shares the
/// same sever state, so one `sever()` kills both directions; the slot
/// remembers the new link's handle.
fn spawn_server_link(
    registry: &Arc<ExportRegistry>,
    slot: &SeverSlot,
) -> (Severable<DuplexStream>, JoinHandle<()>) {
    let (client_io, server_io): (DuplexStream, DuplexStream) = tokio::io::duplex(1024 * 1024);
    let (server_half, sever) = Severable::new(server_io);
    let client_half = Severable::sibling(client_io, &sever);
    *slot.lock().unwrap() = Some(sever);
    let handler: Arc<dyn RequestHandler> = Arc::new(AgentSession::new(registry.clone()));
    let server = tokio::spawn(async move {
        let _ = serve_connection(server_half, "test-agent", handler).await;
    });
    (client_half, server)
}

/// Full loopback stack: duplex pipe, `serve_connection` on the (severable)
/// server half, `MuxConnection` + `RemoteFs::attach_with` on the client half.
/// No dialer: a severed link stays dead.
pub async fn connect(agent: &TestAgent, opts: ClientOptions) -> Session {
    let slot: SeverSlot = Arc::new(Mutex::new(None));
    let (client_io, server) = spawn_server_link(&agent.registry, &slot);
    let conn = MuxConnection::establish(client_io, "test-client")
        .await
        .expect("handshake");
    let fs = RemoteFs::attach_with(conn, "test", opts).await.expect("attach");
    Session {
        fs,
        sever: slot,
        _server: Some(server),
    }
}

/// Like `connect`, but wires a `Dialer` into the options so the reconnect
/// supervisor can re-dial after `Session::sever()`: every dial builds a fresh
/// duplex pair + fresh `AgentSession` against the SAME registry, and the
/// session's sever slot tracks the newest link.
pub async fn connect_reconnectable(agent: &TestAgent, mut opts: ClientOptions) -> Session {
    let slot: SeverSlot = Arc::new(Mutex::new(None));
    let registry = agent.registry.clone();
    let dial_slot = slot.clone();
    let dialer: Dialer = Arc::new(move || {
        let registry = registry.clone();
        let slot = dial_slot.clone();
        Box::pin(async move {
            let (client_io, _server) = spawn_server_link(&registry, &slot);
            let conn = MuxConnection::establish(client_io, "test-client").await?;
            Ok(conn)
        }) as BoxFuture<'static, anyhow::Result<Arc<MuxConnection>>>
    });

    let initial = dialer().await.expect("initial dial");
    opts.dialer = Some(dialer);
    let fs = RemoteFs::attach_with(initial, "test", opts)
        .await
        .expect("attach");
    Session {
        fs,
        sever: slot,
        _server: None,
    }
}

// ------------------------------------------------------------------- helpers

/// Run one closure against the sync `RemoteFs` facade off the async runtime.
/// Never call `RemoteFs` methods directly from async test code — they
/// `block_on` and would deadlock a worker thread.
pub async fn on_fs<T, F>(fs: &Arc<RemoteFs>, f: F) -> T
where
    T: Send + 'static,
    F: FnOnce(&RemoteFs) -> T + Send + 'static,
{
    let fs = fs.clone();
    tokio::task::spawn_blocking(move || f(&fs))
        .await
        .expect("on_fs: blocking task panicked")
}

fn deadline_mult() -> u64 {
    std::env::var("DS_TEST_DEADLINE_MULT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1)
        .max(1)
}

/// Deadline `secs` (× DS_TEST_DEADLINE_MULT) from now, for hand-rolled polls.
pub fn deadline_after(secs: u64) -> std::time::Instant {
    std::time::Instant::now() + Duration::from_secs(secs * deadline_mult())
}

/// Poll `probe` every 25 ms until it yields, or panic after `secs`
/// (× DS_TEST_DEADLINE_MULT) naming `what`.
pub async fn wait_until<T>(what: &str, secs: u64, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = deadline_after(secs);
    loop {
        if let Some(v) = probe() {
            return v;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "wait_until timed out after {secs}s: {what}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Drain event batches until one event satisfies `pred`; panic on deadline.
/// Returns the matching event plus EVERYTHING received up to and including
/// its batch, so callers can also assert on what must NOT have arrived.
pub async fn expect_event(
    rx: &mut broadcast::Receiver<Vec<FsEvent>>,
    secs: u64,
    pred: impl Fn(&FsEvent) -> bool,
) -> (FsEvent, Vec<FsEvent>) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs * deadline_mult());
    let mut seen: Vec<FsEvent> = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(batch)) => {
                seen.extend(batch);
                if let Some(hit) = seen.iter().find(|e| pred(e)) {
                    return (hit.clone(), seen);
                }
            }
            Ok(Err(e)) => panic!("event stream ended while waiting: {e} (seen: {seen:?})"),
            Err(_) => panic!("expect_event timed out after {secs}s (seen: {seen:?})"),
        }
    }
}

/// Deterministic non-repeating-ish byte pattern.
pub fn patterned(len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i.wrapping_mul(31).wrapping_add(i >> 8) % 251) as u8)
        .collect()
}

/// Unwrap the remote error code (panics on transport errors).
pub fn remote_code(err: FsError) -> ErrorCode {
    match err {
        FsError::Remote(code) => code,
        other => panic!("expected remote error, got {other:?}"),
    }
}

/// Resolve "a/b/c" component-by-component from the root.
pub async fn lookup_path(fs: &Arc<RemoteFs>, path: &str) -> Result<(u64, Attr), FsError> {
    let path = path.to_string();
    on_fs(fs, move |fs| {
        let mut ino = ROOT_INO;
        let mut attr = fs.root_attr;
        for comp in path.split('/').filter(|c| !c.is_empty()) {
            let (next, a) = fs.lookup(ino, comp)?;
            ino = next;
            attr = a;
        }
        Ok((ino, attr))
    })
    .await
}

/// Create a file with `data` under `parent_ino`, close it, return its ino.
pub async fn mkfile(fs: &Arc<RemoteFs>, parent_ino: u64, name: &str, data: &[u8]) -> u64 {
    let name = name.to_string();
    let data = data.to_vec();
    on_fs(fs, move |fs| {
        let flags = OpenFlags {
            read: true,
            write: true,
            ..OpenFlags::default()
        };
        let (ino, fh, _attr) = fs.create(parent_ino, &name, 0o644, flags).expect("create");
        if !data.is_empty() {
            fs.write(fh, 0, &data).expect("write");
        }
        fs.release(fh);
        ino
    })
    .await
}

/// Open read-only, read the whole file (size from the open's fresh attr),
/// release.
pub async fn read_all(fs: &Arc<RemoteFs>, ino: u64) -> Vec<u8> {
    on_fs(fs, move |fs| {
        let flags = OpenFlags {
            read: true,
            ..OpenFlags::default()
        };
        let (fh, attr) = fs.open(ino, flags).expect("open for read");
        let data = if attr.size == 0 {
            Vec::new()
        } else {
            fs.read(fh, 0, attr.size as u32).expect("read")
        };
        fs.release(fh);
        data
    })
    .await
}
