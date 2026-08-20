//! The connection layer, over a `tokio::io::duplex` pipe.
//!
//! This crate had no tests of its own: it was covered only through whatever
//! the client and agent happened to exercise, which meant the things unique to
//! it — version negotiation, out-of-order correlation, what a peer sees when
//! the other end vanishes — were never asserted anywhere. A duplex pipe is
//! enough for all of it; nothing here needs a socket, a port, or a filesystem.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use alloyfs_proto::{
    Attr, ErrorCode, FileKind, FsEvent, RelPath, Request, Response, PROTO_VERSION_MAX, PROTO_VERSION_MIN,
};
use alloyfs_transport::{
    serve_connection, serve_connection_with, EventPusher, MuxConnection, RequestHandler,
};

// ------------------------------------------------------------------ handler

/// Answers `Getattr` with the number embedded in the path, after sleeping for
/// that many milliseconds. Both halves matter: the number proves which reply
/// belongs to which request, and the sleep makes the replies come back in the
/// opposite order to the requests.
#[derive(Default)]
struct Echo {
    /// Deliberately keep the `EventPusher` past disconnect, to exercise the
    /// serve loop's writer grace period.
    hoard_pusher: bool,
    connected: AtomicBool,
    disconnected: AtomicBool,
    handled: AtomicU32,
    push: std::sync::Mutex<Option<EventPusher>>,
}

fn attr_with_size(size: u64) -> Attr {
    Attr {
        kind: FileKind::File,
        size,
        mtime: SystemTime::UNIX_EPOCH,
        ctime: SystemTime::UNIX_EPOCH,
        mode: 0o644,
        version: 1,
    }
}

#[async_trait::async_trait]
impl RequestHandler for Echo {
    async fn handle(&self, req: Request) -> Result<Response, ErrorCode> {
        self.handled.fetch_add(1, Ordering::SeqCst);
        match req {
            Request::Getattr { path } => {
                let n: u64 = path.0.parse().map_err(|_| ErrorCode::InvalidPath)?;
                tokio::time::sleep(Duration::from_millis(n)).await;
                Ok(Response::Attr(attr_with_size(n)))
            }
            Request::Statfs => Err(ErrorCode::PermissionDenied),
            _ => Err(ErrorCode::Io),
        }
    }

    async fn connected(&self, push: EventPusher) {
        self.connected.store(true, Ordering::SeqCst);
        *self.push.lock().unwrap() = Some(push);
    }

    async fn disconnected(&self) {
        self.disconnected.store(true, Ordering::SeqCst);
        // The contract every handler is expected to honour: the pusher owns a
        // clone of the connection's outbound sender, and the serve loop drains
        // that channel before returning. Holding one here delays teardown to
        // the writer grace period; the agent releases its pusher for exactly
        // this reason.
        if !self.hoard_pusher {
            *self.push.lock().unwrap() = None;
        }
    }
}

/// Server on one end of a pipe, client on the other.
async fn connect(handler: Arc<Echo>) -> Arc<MuxConnection> {
    connect_with(handler, PROTO_VERSION_MIN).await.expect("handshake")
}

async fn connect_with(
    handler: Arc<Echo>,
    min_proto: u16,
) -> Result<Arc<MuxConnection>, alloyfs_transport::TransportError> {
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let h: Arc<dyn RequestHandler> = handler;
    tokio::spawn(async move {
        let _ = serve_connection_with(server_io, "test-agent", h, min_proto).await;
    });
    MuxConnection::establish(client_io, "test-client").await
}

// -------------------------------------------------------------- handshake

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handshake_negotiates_and_reports_the_server_name() {
    let handler = Arc::new(Echo::default());
    let conn = connect(handler.clone()).await;

    let attr = conn
        .request(Request::Getattr {
            path: RelPath("1".into()),
        })
        .await;
    assert!(matches!(attr, Ok(Ok(Response::Attr(_)))));
    // `connected` runs after the handshake, before any request is served.
    assert!(handler.connected.load(Ordering::SeqCst));
}

/// A server that demands a version this client cannot speak must refuse at the
/// handshake rather than accept the connection and fail later. Token-protected
/// listeners rely on exactly this: a v1 client cannot decode `AuthRequired`,
/// so it is turned away before it can be confused by the answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_floor_above_the_client_refuses_the_connection() {
    let handler = Arc::new(Echo::default());
    let res = connect_with(handler, PROTO_VERSION_MAX + 1).await;
    assert!(
        res.is_err(),
        "a client that cannot reach the server's floor must not get a usable connection"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_floor_the_client_can_reach_is_accepted() {
    let handler = Arc::new(Echo::default());
    let conn = connect_with(handler, PROTO_VERSION_MAX)
        .await
        .expect("the client speaks up to PROTO_VERSION_MAX");
    assert!(!conn.is_closed());
}

// ------------------------------------------------------- multiplexing

/// The point of the mux: replies are matched by id, not by arrival order.
/// These three requests are answered in reverse, and each caller still gets
/// its own answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replies_are_correlated_not_ordered() {
    let handler = Arc::new(Echo::default());
    let conn = connect(handler.clone()).await;

    let slow = {
        let c = conn.clone();
        tokio::spawn(async move {
            c.request(Request::Getattr {
                path: RelPath("120".into()),
            })
            .await
        })
    };
    let mid = {
        let c = conn.clone();
        tokio::spawn(async move {
            c.request(Request::Getattr {
                path: RelPath("60".into()),
            })
            .await
        })
    };
    let fast = {
        let c = conn.clone();
        tokio::spawn(async move {
            c.request(Request::Getattr {
                path: RelPath("1".into()),
            })
            .await
        })
    };

    for (task, want) in [(slow, 120u64), (mid, 60), (fast, 1)] {
        match task.await.expect("join").expect("transport").expect("no error") {
            Response::Attr(a) => assert_eq!(a.size, want, "reply went to the wrong caller"),
            other => panic!("unexpected response: {other:?}"),
        }
    }
    assert_eq!(handler.handled.load(Ordering::SeqCst), 3);
}

/// An error is a normal reply, not a transport failure: the connection has to
/// survive it and keep serving.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_error_reply_does_not_kill_the_connection() {
    let handler = Arc::new(Echo::default());
    let conn = connect(handler).await;

    let err = conn.request(Request::Statfs).await.expect("transport ok");
    assert!(matches!(err, Err(ErrorCode::PermissionDenied)));

    let after = conn
        .request(Request::Getattr {
            path: RelPath("2".into()),
        })
        .await;
    assert!(
        matches!(after, Ok(Ok(Response::Attr(_)))),
        "connection died on an error reply"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ping_round_trips() {
    let handler = Arc::new(Echo::default());
    let conn = connect(handler).await;
    conn.ping().await.expect("pong");
}

// ------------------------------------------------------------- write batching

/// The writer batches flushes when frames are queued back-to-back; this pins
/// down what batching must NOT change. Server→client: 200 event batches
/// pushed without a pause between them are exactly the burst the writer
/// coalesces, and events are the one frame kind whose ORDER the transport
/// promises (responses are correlated by id instead) — so all 200 must arrive,
/// in sequence, none stuck in a buffer waiting for a flush that never comes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pushed_burst_arrives_complete_and_in_order() {
    const N: u64 = 200;
    let handler = Arc::new(Echo::default());
    let conn = connect(handler.clone()).await;
    let mut rx = conn.events();

    let push = handler.push.lock().unwrap().clone().expect("connected() ran");
    tokio::spawn(async move {
        for seq in 0..N {
            let batch = vec![FsEvent {
                seq,
                kind: alloyfs_proto::EventKind::Created,
                path: RelPath(format!("f{seq}")),
                new_version: None,
                origin: None,
            }];
            if !push.events(batch).await {
                panic!("connection died mid-burst");
            }
        }
    });

    for want in 0..N {
        let got = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("burst frame arrived")
            .expect("no broadcast lag for 200 buffered events");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].seq, want, "batched writes must not reorder frames");
    }
}

/// Client→server direction of the same burst: 200 requests issued
/// concurrently pile up in the outbound channel faster than the writer can
/// drain them, and every one must still get ITS OWN reply. The replies also
/// come back as a server-side burst, so both writers batch here.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_request_burst_resolves_every_caller() {
    const N: u64 = 200;
    let handler = Arc::new(Echo::default());
    let conn = connect(handler.clone()).await;

    let tasks: Vec<_> = (0..N)
        .map(|n| {
            let c = conn.clone();
            tokio::spawn(async move {
                // The Echo handler answers with the number in the path, so a
                // misrouted reply is detectable, not just a hang.
                c.request(Request::Getattr {
                    path: RelPath(n.to_string()),
                })
                .await
            })
        })
        .collect();

    for (n, task) in tasks.into_iter().enumerate() {
        match task.await.expect("join").expect("transport").expect("no error") {
            Response::Attr(a) => assert_eq!(a.size, n as u64, "reply went to the wrong caller"),
            other => panic!("unexpected response: {other:?}"),
        }
    }
    assert_eq!(handler.handled.load(Ordering::SeqCst), N as u32);
}

// ------------------------------------------------------------- push + teardown

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pushed_events_reach_the_client() {
    let handler = Arc::new(Echo::default());
    let conn = connect(handler.clone()).await;
    let mut rx = conn.events();

    let push = handler.push.lock().unwrap().clone().expect("connected() ran");
    let batch = vec![FsEvent {
        seq: 7,
        kind: alloyfs_proto::EventKind::Created,
        path: RelPath("new.txt".into()),
        new_version: None,
        origin: None,
    }];
    assert!(push.events(batch).await, "the connection is still up");

    let got = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("an event should arrive")
        .expect("channel open");
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].seq, 7);
    assert_eq!(got[0].path.0, "new.txt");
}

/// When the peer's stream closes the server must run `disconnected()`, because
/// that is what releases the session's handles and locks — without it a dead
/// client's exclusive lock is held until the 30 s lease reaper notices.
///
/// The client side here is hand-rolled rather than a `MuxConnection`, and
/// deliberately so: `MuxConnection` hands its stream to background tasks and
/// has no `Drop`, so dropping the handle does NOT close the connection. Using
/// one here would test that quirk instead of the server's cleanup path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_server_cleans_up_when_the_peer_stream_closes() {
    assert_teardown(Arc::new(Echo::default()), Duration::from_secs(5)).await;
}

/// And it cleans up even when the handler does NOT release its pusher.
///
/// This is the regression guard for a real deadlock: the agent kept its
/// `EventPusher` in a `OnceLock` it never cleared, so the outbound channel
/// never closed, so the serve loop never returned, so the handler was never
/// dropped — one wedged task and one leaked socket write half for every client
/// that disconnected. Here the grace period breaks the cycle instead, which is
/// why the budget is longer than WRITER_DRAIN_GRACE rather than shorter.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_handler_that_keeps_its_pusher_cannot_wedge_the_connection() {
    let hoarder = Arc::new(Echo {
        hoard_pusher: true,
        ..Echo::default()
    });
    assert_teardown(hoarder, Duration::from_secs(20)).await;
}

async fn assert_teardown(handler: Arc<Echo>, budget: Duration) {
    use alloyfs_proto::{Frame, FrameCodec};
    use futures::{SinkExt, StreamExt};
    use tokio_util::codec::Framed;

    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let h: Arc<dyn RequestHandler> = handler.clone();
    let served = tokio::spawn(async move {
        let _ = serve_connection(server_io, "test-agent", h).await;
    });

    {
        let mut framed = Framed::new(client_io, FrameCodec::default());
        let hello = Frame::Hello {
            proto_min: PROTO_VERSION_MIN,
            proto_max: PROTO_VERSION_MAX,
            client: "hand-rolled".into(),
        };
        framed.send(&hello).await.expect("send hello");
        match framed.next().await {
            Some(Ok(Frame::HelloAck { proto, .. })) => {
                assert!((PROTO_VERSION_MIN..=PROTO_VERSION_MAX).contains(&proto));
            }
            other => panic!("expected HelloAck, got {other:?}"),
        }
    } // the stream itself is dropped here, so the pipe really does close

    tokio::time::timeout(budget, served)
        .await
        .expect("the serve loop must end when the peer's stream closes")
        .expect("join");
    assert!(
        handler.disconnected.load(Ordering::SeqCst),
        "disconnected() is what releases this session's handles and locks"
    );
}

/// A request issued after the peer is gone must fail rather than hang forever
/// waiting for a reply that cannot come.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_request_after_the_peer_leaves_fails_promptly() {
    let handler = Arc::new(Echo::default());
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let h: Arc<dyn RequestHandler> = handler;
    let server = tokio::spawn(async move {
        let _ = serve_connection(server_io, "test-agent", h).await;
    });
    let conn = MuxConnection::establish(client_io, "test-client")
        .await
        .expect("handshake");

    server.abort();
    let _ = server.await;
    conn.closed().await;

    let res = tokio::time::timeout(
        Duration::from_secs(5),
        conn.request(Request::Getattr {
            path: RelPath("1".into()),
        }),
    )
    .await
    .expect("must not hang");
    assert!(
        res.is_err(),
        "a request on a dead connection has to report failure"
    );
    assert!(conn.is_closed());
}
