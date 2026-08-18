//! Version gating for the write reply: the agent may only answer with a
//! shape the peer it is talking to can decode.
//!
//! `Response::WrittenAttr` (v5) carries the attributes a client would
//! otherwise fetch with a follow-up Getattr. A v4 peer has no such variant —
//! postcard decodes by variant INDEX, so handing one to a v4 client does not
//! fail cleanly, it silently misreads the stream. These tests drive the wire
//! by hand because that is the only way to be a peer older than this build:
//! `MuxConnection` always offers the full range this crate compiles with.

use std::sync::Arc;

use alloyfs_agent::{AgentConfig, AgentSession, ExportConfig, ExportRegistry};
use alloyfs_proto::{
    ErrorCode, FileKind, Frame, FrameCodec, OpenFlags, RelPath, Request, Response, PROTO_VERSION_MAX,
    PROTO_VERSION_MIN,
};
use alloyfs_transport::{serve_connection, RequestHandler};
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use tokio::io::DuplexStream;
use tokio_util::codec::Framed;

/// One export named "test" over `dir`.
fn registry_for(dir: &tempfile::TempDir) -> Arc<ExportRegistry> {
    let mut cfg = AgentConfig::default();
    cfg.exports.insert(
        "test".into(),
        ExportConfig {
            path: dir.path().to_path_buf(),
            read_only: false,
            exclude: Vec::new(),
            ..Default::default()
        },
    );
    Arc::new(ExportRegistry::from_config(&cfg).expect("export registry"))
}

/// A hand-rolled client that can claim any `proto_max`, so the handshake can
/// be driven to a version older than this build's.
struct Peer {
    io: Framed<DuplexStream, FrameCodec>,
    proto: u16,
    next_id: u64,
}

impl Peer {
    /// Handshake offering `PROTO_VERSION_MIN..=proto_max` against a fresh
    /// session on `registry`.
    async fn connect(registry: &Arc<ExportRegistry>, proto_max: u16) -> Self {
        let (client_io, server_io) = tokio::io::duplex(256 * 1024);
        let handler: Arc<dyn RequestHandler> = Arc::new(AgentSession::new(registry.clone()));
        tokio::spawn(async move {
            let _ = serve_connection(server_io, "test-agent", handler).await;
        });
        let mut io = Framed::new(client_io, FrameCodec::default());
        let hello = Frame::Hello {
            proto_min: PROTO_VERSION_MIN,
            proto_max,
            client: "old-peer".into(),
        };
        io.send(&hello).await.expect("send hello");
        let proto = match io.next().await {
            Some(Ok(Frame::HelloAck { proto, .. })) => proto,
            other => panic!("expected HelloAck, got {other:?}"),
        };
        Self {
            io,
            proto,
            next_id: 0,
        }
    }

    async fn call(&mut self, body: Request) -> Result<Response, ErrorCode> {
        self.next_id += 1;
        let id = self.next_id;
        let frame = Frame::Request { id, body };
        self.io.send(&frame).await.expect("send request");
        match self.io.next().await {
            Some(Ok(Frame::Response { id: got, body })) => {
                assert_eq!(got, id, "the agent answered a different request");
                body
            }
            other => panic!("expected a Response frame, got {other:?}"),
        }
    }

    async fn ok(&mut self, body: Request) -> Response {
        self.call(body).await.expect("request refused")
    }

    /// Attach, create `name`, write `data` into it, and return the reply to
    /// the write plus the file's version-carrying attributes as a separate
    /// Getattr reports them — the round-trip v5 exists to remove.
    async fn attach_and_write(&mut self, name: &str, data: &'static [u8]) -> (Response, alloyfs_proto::Attr) {
        self.ok(Request::Attach {
            export: "test".into(),
        })
        .await;
        let flags = OpenFlags {
            read: true,
            write: true,
            excl: true,
            ..OpenFlags::default()
        };
        let path = RelPath(name.to_string());
        let fh = match self
            .ok(Request::Create {
                path: path.clone(),
                flags,
                mode: 0o644,
            })
            .await
        {
            Response::Opened { fh, .. } => fh,
            other => panic!("expected Opened, got {other:?}"),
        };
        let written = self
            .ok(Request::Write {
                fh,
                offset: 0,
                data: Bytes::from_static(data),
                expect_version: None,
            })
            .await;
        let polled = match self.ok(Request::Getattr { path }).await {
            Response::Attr(attr) => attr,
            other => panic!("expected Attr, got {other:?}"),
        };
        (written, polled)
    }
}

/// A peer that stops at v4 must negotiate v4 and be answered in the v4 shape
/// — the whole point of not raising PROTO_VERSION_MIN.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_v4_peer_negotiates_and_writes_exactly_as_before() {
    let dir = tempfile::TempDir::new().unwrap();
    let registry = registry_for(&dir);
    let mut peer = Peer::connect(&registry, 4).await;
    assert_eq!(
        peer.proto, 4,
        "the handshake must settle on the older peer's ceiling"
    );

    let (written, polled) = peer.attach_and_write("v4.txt", b"golden").await;
    let Response::Written {
        n,
        new_version,
        conflict,
    } = written
    else {
        panic!("a v4 peer must be answered with `Written`, got {written:?}");
    };
    assert_eq!(n, 6);
    assert!(new_version > 0, "a write always bumps the version");
    assert!(!conflict);

    // Everything the v4 client can only learn by asking again.
    assert_eq!(polled.size, 6);
    assert_eq!(polled.version, new_version);
    assert_eq!(
        std::fs::read(dir.path().join("v4.txt")).unwrap(),
        b"golden",
        "the bytes must reach the disk regardless of the reply shape"
    );
}

/// At v5 the same write answers with the attributes attached — and they must
/// be the SAME answer the follow-up Getattr gives, or the round-trip has been
/// traded for a lie.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_v5_peer_gets_the_follow_up_getattr_in_the_write_reply() {
    let dir = tempfile::TempDir::new().unwrap();
    let registry = registry_for(&dir);
    let mut peer = Peer::connect(&registry, PROTO_VERSION_MAX).await;
    assert_eq!(peer.proto, 5);

    let (written, polled) = peer.attach_and_write("v5.txt", b"golden").await;
    let Response::WrittenAttr { n, attr } = written else {
        panic!("a v5 peer must be answered with `WrittenAttr`, got {written:?}");
    };
    assert_eq!(n, 6);
    assert_eq!(attr.size, 6);
    assert_eq!(attr.kind, FileKind::File);
    assert!(attr.version > 0);

    // mtime is excluded on purpose: the carried value comes from a stat of
    // the still-open write handle, and Windows may not have flushed the
    // timestamp to the FCB yet. Size and version — what freshness and
    // conflict detection are actually decided by — must match exactly.
    assert_eq!(polled.size, attr.size);
    assert_eq!(polled.version, attr.version);
    assert_eq!(polled.kind, attr.kind);
}

/// A peer offering a range that ends below this build's floor has nothing in
/// common with it and must be turned away at the handshake rather than left
/// to misparse a v5 stream.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_peer_below_the_floor_is_refused() {
    let dir = tempfile::TempDir::new().unwrap();
    let registry = registry_for(&dir);
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let handler: Arc<dyn RequestHandler> = Arc::new(AgentSession::new(registry.clone()));
    tokio::spawn(async move {
        let _ = serve_connection(server_io, "test-agent", handler).await;
    });
    let mut io = Framed::new(client_io, FrameCodec::default());
    let hello = Frame::Hello {
        proto_min: 0,
        proto_max: PROTO_VERSION_MIN - 1,
        client: "ancient".into(),
    };
    io.send(&hello).await.expect("send hello");
    assert!(
        matches!(
            io.next().await,
            Some(Ok(Frame::Response {
                body: Err(ErrorCode::VersionMismatch),
                ..
            }))
        ),
        "a peer with no common version must be told so"
    );
}
