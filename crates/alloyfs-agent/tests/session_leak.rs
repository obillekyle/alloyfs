//! Does a real AgentSession let serve_connection finish when the peer leaves?
use std::sync::Arc;
use std::time::Duration;

use alloyfs_agent::{AgentConfig, AgentSession, ExportConfig, ExportRegistry};
use alloyfs_proto::{Frame, FrameCodec, PROTO_VERSION_MAX, PROTO_VERSION_MIN};
use alloyfs_transport::{serve_connection, RequestHandler};
use futures::{SinkExt, StreamExt};
use tokio_util::codec::Framed;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serve_connection_returns_after_the_peer_disconnects() {
    let dir = tempfile::TempDir::new().unwrap();
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
    let registry = Arc::new(ExportRegistry::from_config(&cfg).unwrap());
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let h: Arc<dyn RequestHandler> = Arc::new(AgentSession::new(registry));
    let served = tokio::spawn(async move {
        let _ = serve_connection(server_io, "agent", h).await;
    });
    {
        let mut f = Framed::new(client_io, FrameCodec::default());
        let hello = Frame::Hello {
            proto_min: PROTO_VERSION_MIN,
            proto_max: PROTO_VERSION_MAX,
            client: "probe".into(),
        };
        f.send(&hello).await.unwrap();
        assert!(matches!(f.next().await, Some(Ok(Frame::HelloAck { .. }))));
    }
    tokio::time::timeout(Duration::from_secs(5), served)
        .await
        .expect("serve_connection must return once the peer is gone")
        .unwrap();
}
