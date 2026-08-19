//! `ReadMany`: many whole files in one exchange.
//!
//! The behaviour worth pinning is not "it returns the bytes" but what happens
//! at the edges — a budget that runs out mid-batch, a file too big to ever
//! fit, and a path that is not there. Each has a wrong answer that looks
//! plausible: serving nothing and spinning, failing the whole batch for one
//! bad file, or quietly dropping the remainder.

use std::sync::Arc;

use alloyfs_agent::{AgentConfig, AgentSession, ExportConfig, ExportRegistry};
use alloyfs_proto::{
    ErrorCode, Frame, FrameCodec, ManyEntry, RelPath, Request, Response, PROTO_VERSION_MAX, PROTO_VERSION_MIN,
};
use alloyfs_transport::{serve_connection, RequestHandler};
use futures::{SinkExt, StreamExt};
use tokio_util::codec::Framed;

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
            client: "bulk".into(),
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
        self.io
            .send(&Frame::Request { id, body })
            .await
            .expect("send request");
        match self.io.next().await {
            Some(Ok(Frame::Response { id: got, body })) => {
                assert_eq!(got, id, "the agent answered a different request");
                body
            }
            other => panic!("expected a Response frame, got {other:?}"),
        }
    }

    async fn read_many(&mut self, paths: &[&str], budget: u32) -> Vec<ManyEntry> {
        let paths = paths.iter().map(|p| RelPath((*p).into())).collect();
        match self.call(Request::ReadMany { paths, budget }).await {
            Ok(Response::Many(entries)) => entries,
            other => panic!("expected Many, got {other:?}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn many_files_come_back_in_one_exchange() {
    let dir = tempfile::tempdir().expect("tempdir");
    for i in 0..5 {
        std::fs::write(dir.path().join(format!("f{i}.txt")), format!("body-{i}")).unwrap();
    }
    let registry = registry_for(&dir);
    let mut peer = Peer::connect(&registry).await;

    let names: Vec<String> = (0..5).map(|i| format!("f{i}.txt")).collect();
    let refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
    let entries = peer.read_many(&refs, 768 * 1024).await;

    assert_eq!(entries.len(), 5, "all five should fit one reply");
    for (i, entry) in entries.iter().enumerate() {
        match entry {
            ManyEntry::File { attr, data } => {
                assert_eq!(&data[..], format!("body-{i}").as_bytes());
                assert_eq!(attr.size, data.len() as u64);
            }
            other => panic!("entry {i} was not a file: {other:?}"),
        }
    }
}

/// A reply is always a PREFIX of the request, so a client can tell exactly
/// what is outstanding without a cursor. Asking for a budget smaller than the
/// batch is what forces the server to stop early.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_spent_budget_truncates_the_reply_to_a_prefix() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Four files of 1000 bytes against a budget of 2500: two fit, the third
    // would exceed it.
    for i in 0..4 {
        std::fs::write(dir.path().join(format!("b{i}.bin")), vec![b'x'; 1000]).unwrap();
    }
    let registry = registry_for(&dir);
    let mut peer = Peer::connect(&registry).await;

    let names: Vec<String> = (0..4).map(|i| format!("b{i}.bin")).collect();
    let refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
    let entries = peer.read_many(&refs, 2500).await;

    assert!(
        entries.len() < 4 && !entries.is_empty(),
        "expected a short-but-nonempty reply, got {}",
        entries.len()
    );
    // Whatever came back is the FIRST n, in order.
    for (i, entry) in entries.iter().enumerate() {
        match entry {
            ManyEntry::File { data, .. } => {
                assert_eq!(data.len(), 1000, "entry {i}");
            }
            other => panic!("entry {i} was not a file: {other:?}"),
        }
    }

    // ...and the remainder is servable on the next ask, which is the whole
    // paging contract.
    let rest = peer.read_many(&refs[entries.len()..], 2500).await;
    assert!(!rest.is_empty(), "the remainder should be servable");
}

/// A file too big for ANY bulk reply must not stall the batch. Answering it
/// with nothing would leave a client asking for the same thing forever, so it
/// comes back `TooLarge` and is read the ordinary chunked way instead.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_larger_than_the_budget_is_reported_not_omitted() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("big.bin"), vec![b'x'; 20_000]).unwrap();
    std::fs::write(dir.path().join("small.txt"), b"tiny").unwrap();
    let registry = registry_for(&dir);
    let mut peer = Peer::connect(&registry).await;

    // Budget below the big file's size, so it can never fit.
    let entries = peer.read_many(&["big.bin", "small.txt"], 4096).await;

    assert_eq!(entries.len(), 2, "both entries should be accounted for");
    assert!(
        matches!(entries[0], ManyEntry::Skipped(ErrorCode::TooLarge)),
        "expected TooLarge, got {:?}",
        entries[0]
    );
    match &entries[1] {
        ManyEntry::File { data, .. } => assert_eq!(&data[..], b"tiny"),
        other => panic!("the small file should still be served: {other:?}"),
    }
}

/// One missing or unreadable path must not cost the whole batch — the entry
/// carries the error and the rest are served.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_path_is_skipped_and_the_rest_are_served() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("here.txt"), b"present").unwrap();
    std::fs::create_dir(dir.path().join("adir")).unwrap();
    let registry = registry_for(&dir);
    let mut peer = Peer::connect(&registry).await;

    let entries = peer
        .read_many(&["gone.txt", "here.txt", "adir"], 768 * 1024)
        .await;

    assert_eq!(entries.len(), 3);
    assert!(
        matches!(entries[0], ManyEntry::Skipped(ErrorCode::NotFound)),
        "missing path: {:?}",
        entries[0]
    );
    match &entries[1] {
        ManyEntry::File { data, .. } => assert_eq!(&data[..], b"present"),
        other => panic!("existing file should be served: {other:?}"),
    }
    assert!(
        matches!(entries[2], ManyEntry::Skipped(_)),
        "a directory is not a file: {:?}",
        entries[2]
    );
}
