//! Two sessions writing one file — the collision no lock catches on Windows.
//!
//! The registry has its own unit tests; this drives the real thing, through
//! two connections and real `Open` requests, because the interesting part is
//! whether the agent's session and handle lifecycle actually reaches it.

use std::sync::Arc;

use alloyfs_agent::{AgentConfig, AgentSession, ExportConfig, ExportRegistry};
use alloyfs_proto::{
    ErrorCode, Frame, FrameCodec, OpenFlags, RelPath, Request, Response, PROTO_VERSION_MAX, PROTO_VERSION_MIN,
};
use alloyfs_transport::{serve_connection, RequestHandler};
use futures::{SinkExt, StreamExt};
use tokio_util::codec::Framed;

struct Peer {
    io: Framed<tokio::io::DuplexStream, FrameCodec>,
    next_id: u64,
}

impl Peer {
    async fn connect(registry: &Arc<ExportRegistry>) -> Self {
        let (client_io, server_io) = tokio::io::duplex(1024 * 1024);
        let handler: Arc<dyn RequestHandler> = Arc::new(AgentSession::new(registry.clone()));
        tokio::spawn(async move {
            let _ = serve_connection(server_io, "test-agent", handler).await;
        });
        let mut io = Framed::new(client_io, FrameCodec::default());
        io.send(&Frame::Hello {
            proto_min: PROTO_VERSION_MIN,
            proto_max: PROTO_VERSION_MAX,
            client: "writer-conflict".into(),
        })
        .await
        .expect("hello");
        match io.next().await {
            Some(Ok(Frame::HelloAck { .. })) => {}
            other => panic!("expected HelloAck, got {other:?}"),
        }
        let mut peer = Self { io, next_id: 0 };
        peer.call(Request::Attach {
            export: "test".into(),
        })
        .await
        .expect("attach");
        peer
    }

    async fn call(&mut self, body: Request) -> Result<Response, ErrorCode> {
        self.next_id += 1;
        let id = self.next_id;
        self.io.send(&Frame::Request { id, body }).await.expect("send");
        match self.io.next().await {
            Some(Ok(Frame::Response { id: got, body })) => {
                assert_eq!(got, id);
                body
            }
            other => panic!("expected a Response frame, got {other:?}"),
        }
    }

    async fn open_write(&mut self, path: &str) -> u64 {
        match self
            .call(Request::Open {
                path: RelPath(path.into()),
                flags: OpenFlags {
                    read: true,
                    write: true,
                    ..Default::default()
                },
            })
            .await
        {
            Ok(Response::Opened { fh, .. }) => fh,
            other => panic!("open {path}: {other:?}"),
        }
    }
}

fn registry_with(dir: &std::path::Path) -> Arc<ExportRegistry> {
    let mut cfg = AgentConfig::default();
    cfg.exports.insert(
        "test".into(),
        ExportConfig {
            path: dir.to_path_buf(),
            ..Default::default()
        },
    );
    Arc::new(ExportRegistry::from_config(&cfg).expect("registry"))
}

/// The whole point: two SESSIONS with one file open for writing is visible to
/// the agent, and one session with it open twice is not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_sessions_writing_one_file_are_visible_to_the_agent() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("db.sqlite"), b"x").unwrap();
    std::fs::write(dir.path().join("other.txt"), b"y").unwrap();
    let registry = registry_with(dir.path());
    let export = registry.get("test").expect("export");

    let mut a = Peer::connect(&registry).await;
    let mut b = Peer::connect(&registry).await;

    let fh_a = a.open_write("db.sqlite").await;
    assert!(
        export.writers.contended().is_empty(),
        "one writer is never contention"
    );

    // The same session opening it again is ordinary — its own machine orders
    // those correctly, and reporting it would train people to ignore the
    // warning that matters.
    let fh_a2 = a.open_write("db.sqlite").await;
    assert!(
        export.writers.contended().is_empty(),
        "a second handle in ONE session must not register as contention"
    );

    // A different session is the case nothing serialises on Windows.
    let fh_b = b.open_write("db.sqlite").await;
    let contended = export.writers.contended();
    assert_eq!(contended.len(), 1, "the collision is seen: {contended:?}");
    assert_eq!(contended[0].0, RelPath("db.sqlite".into()));
    assert_eq!(contended[0].1.len(), 2, "two distinct sessions");

    // A different FILE is not a conflict.
    b.open_write("other.txt").await;
    assert_eq!(
        export.writers.contended().len(),
        1,
        "an unrelated file must not join the report"
    );

    // Closing the foreign writer clears it; the first session still holds two.
    b.call(Request::Release { fh: fh_b }).await.expect("release");
    assert!(
        export.writers.contended().is_empty(),
        "one session left is not contention"
    );

    a.call(Request::Release { fh: fh_a }).await.expect("release");
    a.call(Request::Release { fh: fh_a2 }).await.expect("release");
}

/// A read-only open is not a writer, however many sessions hold one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readers_are_never_reported_as_conflicting() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("shared.txt"), b"data").unwrap();
    let registry = registry_with(dir.path());
    let export = registry.get("test").expect("export");

    let mut a = Peer::connect(&registry).await;
    let mut b = Peer::connect(&registry).await;
    for peer in [&mut a, &mut b] {
        let resp = peer
            .call(Request::Open {
                path: RelPath("shared.txt".into()),
                flags: OpenFlags {
                    read: true,
                    ..Default::default()
                },
            })
            .await;
        assert!(matches!(resp, Ok(Response::Opened { .. })), "{resp:?}");
    }
    assert!(
        export.writers.contended().is_empty(),
        "two readers are not a conflict"
    );
}
