//! v12 `GetattrMany`: per-path verdicts, request order, and exact
//! equivalence with the lone `Getattr` each entry replaces — the same
//! contract the v10 bulk mutations pinned for their singles.

use std::sync::Arc;

use alloyfs_agent::{AgentConfig, AgentSession, ExportConfig, ExportRegistry};
use alloyfs_proto::{
    ErrorCode, Frame, FrameCodec, RelPath, Request, Response, PROTO_VERSION_MAX, PROTO_VERSION_MIN,
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
        let (client_io, server_io) = tokio::io::duplex(4 * 1024 * 1024);
        let handler: Arc<dyn RequestHandler> = Arc::new(AgentSession::new(registry.clone()));
        tokio::spawn(async move {
            let _ = serve_connection(server_io, "test-agent", handler).await;
        });
        let mut io = Framed::new(client_io, FrameCodec::default());
        io.send(&Frame::Hello {
            proto_min: PROTO_VERSION_MIN,
            proto_max: PROTO_VERSION_MAX,
            client: "getattr-many".into(),
        })
        .await
        .expect("send hello");
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
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bulk_verdicts_match_the_lone_getattrs_in_order() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("a.txt"), b"alpha").unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    std::fs::write(dir.path().join("sub").join("b.txt"), b"beta!!").unwrap();

    let mut cfg = AgentConfig::default();
    cfg.exports.insert(
        "test".into(),
        ExportConfig {
            path: dir.path().to_path_buf(),
            ..Default::default()
        },
    );
    let registry = Arc::new(ExportRegistry::from_config(&cfg).expect("registry"));
    let mut peer = Peer::connect(&registry).await;

    // The lone answers first — the truth the bulk must reproduce.
    let lone = |p: &str| Request::Getattr {
        path: RelPath(p.into()),
    };
    let a = match peer.call(lone("a.txt")).await {
        Ok(Response::Attr(at)) => at,
        other => panic!("lone a.txt: {other:?}"),
    };
    let b = match peer.call(lone("sub/b.txt")).await {
        Ok(Response::Attr(at)) => at,
        other => panic!("lone sub/b.txt: {other:?}"),
    };
    let ghost = peer.call(lone("ghost.txt")).await;
    assert!(matches!(ghost, Err(ErrorCode::NotFound)), "got {ghost:?}");

    // One exchange, mixed verdicts, request order, same length.
    let out = match peer
        .call(Request::GetattrMany {
            paths: vec![
                RelPath("a.txt".into()),
                RelPath("ghost.txt".into()),
                RelPath("sub".into()),
                RelPath("sub/b.txt".into()),
            ],
        })
        .await
    {
        Ok(Response::ManyOutcome(out)) => out,
        other => panic!("expected ManyOutcome, got {other:?}"),
    };
    assert_eq!(out.len(), 4, "same length as the request");
    assert_eq!(out[0], Ok(Some(a)), "a.txt matches its lone Getattr");
    assert_eq!(out[1], Err(ErrorCode::NotFound), "ghost errs exactly as alone");
    match &out[2] {
        Ok(Some(at)) => assert_eq!(at.kind, alloyfs_proto::FileKind::Dir, "sub is a dir"),
        other => panic!("sub: {other:?}"),
    }
    assert_eq!(out[3], Ok(Some(b)), "nested path matches its lone Getattr");
}
