//! A peer cannot name its way out of the export.
//!
//! Two independent refusals now stand between a hostile path and the disk, and
//! this drives both. `RelPath::validate_wire` rejects a Windows drive
//! reference while the frame is still being decoded, which drops the
//! connection — fail-closed, and the reason a refusal here is not always an
//! error reply. `Export::resolve_new` rejects it again before any join, for
//! the paths that reach it by other routes.
//!
//! Driven by hand because the ordinary client never builds such a path. That
//! is the point: what matters is what happens when a peer that is not the
//! ordinary client sends one anyway.

use std::sync::Arc;

use alloyfs_agent::{AgentConfig, AgentSession, ExportConfig, ExportRegistry};
use alloyfs_proto::{Frame, FrameCodec, OpenFlags, RelPath, Request, PROTO_VERSION_MAX, PROTO_VERSION_MIN};
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

/// Attach on a FRESH connection and try to create `path`. True only if the
/// agent answered with a successful Create.
///
/// A fresh connection per attempt is required rather than tidy: a path refused
/// at decode takes the connection with it, so a shared one would report every
/// later attempt as refused regardless of merit.
async fn create_succeeds(registry: &Arc<ExportRegistry>, path: &str) -> bool {
    let (client_io, server_io) = tokio::io::duplex(256 * 1024);
    let handler: Arc<dyn RequestHandler> = Arc::new(AgentSession::new(registry.clone()));
    tokio::spawn(async move {
        let _ = serve_connection(server_io, "test-agent", handler).await;
    });
    let mut io = Framed::new(client_io, FrameCodec::default());
    io.send(&Frame::Hello {
        proto_min: PROTO_VERSION_MIN,
        proto_max: PROTO_VERSION_MAX,
        client: "escape-probe".into(),
    })
    .await
    .expect("send hello");
    match io.next().await {
        Some(Ok(Frame::HelloAck { .. })) => {}
        other => panic!("expected HelloAck, got {other:?}"),
    }
    io.send(&Frame::Request {
        id: 1,
        body: Request::Attach {
            export: "test".into(),
        },
    })
    .await
    .expect("send attach");
    match io.next().await {
        Some(Ok(Frame::Response { body: Ok(_), .. })) => {}
        other => panic!("attach was refused: {other:?}"),
    }
    let create = Frame::Request {
        id: 2,
        body: Request::Create {
            path: RelPath(path.to_string()),
            flags: OpenFlags {
                read: true,
                write: true,
                ..OpenFlags::default()
            },
            mode: 0o644,
        },
    };
    if io.send(&create).await.is_err() {
        return false;
    }
    matches!(io.next().await, Some(Ok(Frame::Response { body: Ok(_), .. })))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_drive_relative_path_cannot_escape_the_export() {
    let dir = tempfile::tempdir().expect("tempdir");
    let registry = registry_for(&dir);

    // `C:evil.txt` is the one that mattered. `PathBuf::join` REPLACES its
    // buffer when the argument carries a drive prefix, so spliced onto an
    // already-checked parent it resolves against drive C's working directory —
    // outside the export — while still reporting `is_absolute() == false`.
    // `a:b` is the same thing with a less obvious drive letter.
    for bad in [
        "C:evil.txt",
        "sub/C:evil.txt",
        "a:b",
        "../escape.txt",
        "x\\y",
        "/etc/passwd",
    ] {
        assert!(
            !create_succeeds(&registry, bad).await,
            "the agent accepted {bad:?}"
        );
    }

    // Nothing reached the disk under any spelling.
    let left: Vec<_> = std::fs::read_dir(dir.path())
        .expect("read export")
        .map(|e| e.expect("entry").file_name())
        .collect();
    assert!(left.is_empty(), "export should be empty, holds {left:?}");

    // ...and an ordinary name still works, so the rule is refusing the escape
    // rather than simply refusing.
    assert!(
        create_succeeds(&registry, "ordinary.txt").await,
        "an ordinary create was refused"
    );
    assert!(dir.path().join("ordinary.txt").is_file());
}

/// A colon is only dangerous when a single letter precedes it. Rejecting every
/// colon would take out ISO-timestamped filenames, which are ordinary on a
/// Linux export — and on the decode path that would mean dropping the
/// connection each time an event mentioned one.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ordinary_colon_is_still_a_legal_name() {
    let dir = tempfile::tempdir().expect("tempdir");
    let registry = registry_for(&dir);
    assert!(
        create_succeeds(&registry, "log-10:30:00.txt").await,
        "a timestamped filename was refused"
    );
}

/// A symlink chain cannot walk the create path out of the export.
///
/// `Export::resolve` canonicalizes and checks containment, so every READ is
/// safe. `resolve_new` — the create/mkdir/rename-to path — canonicalized only
/// to test EXCLUDES and returned the un-resolved path, which `create` then
/// `open(2)`s, following the link.
///
/// `symlink_lands_inside` is purely lexical, and that is what makes it
/// reachable. `up/..` cancels on paper, while the kernel resolves `up` to the
/// export root and then `..` to its PARENT. Each hop adds one level, so a
/// chain reaches any depth:
///
///     sub/up   -> ".."             lands ""    on paper; really the root
///     sub/up2  -> "up/.."          lands "sub" on paper; really the parent
///     sub/esc  -> "up2/secret.txt" lands under sub on paper; really outside
///
/// `docs/deployment/security.md` promises this is refused. It was not.
///
/// Unix-only: it needs real symlinks, and creating one on Windows requires
/// Developer Mode or elevation.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_symlink_chain_cannot_escape_the_export_on_create() {
    let outside = tempfile::TempDir::new().expect("outside");
    let secret = outside.path().join("secret.txt");
    std::fs::write(&secret, b"ORIGINAL SECRET").unwrap();

    // The export is a directory INSIDE `outside`, so `..` from it reaches the
    // secret's directory — the ordinary shape of an export under a home dir.
    let export_root = outside.path().join("export");
    std::fs::create_dir(&export_root).unwrap();
    std::fs::create_dir(export_root.join("sub")).unwrap();

    let mut cfg = AgentConfig::default();
    cfg.exports.insert(
        "test".into(),
        ExportConfig {
            path: export_root.clone(),
            read_only: false,
            exclude: Vec::new(),
            ..Default::default()
        },
    );
    let registry = Arc::new(ExportRegistry::from_config(&cfg).expect("registry"));

    // Build the chain on disk directly — whether the agent's `symlink` handler
    // would accept each hop is a separate question; this test is about what
    // `resolve_new` does once such a link EXISTS. A pre-existing symlink out
    // of the export, made by any means, is the same situation.
    std::os::unix::fs::symlink("..", export_root.join("sub").join("up")).unwrap();
    std::os::unix::fs::symlink("up/..", export_root.join("sub").join("up2")).unwrap();
    std::os::unix::fs::symlink("up2/secret.txt", export_root.join("sub").join("esc")).unwrap();

    let escaped = create_succeeds(&registry, "sub/esc").await;

    let after = std::fs::read(&secret).unwrap_or_default();
    assert_eq!(
        after, b"ORIGINAL SECRET",
        "create() followed a symlink out of the export and truncated a file the \
         client can never legitimately name"
    );
    assert!(
        !escaped,
        "create() on a symlink that resolves outside the export must be refused"
    );
}

/// The same guarantee for a link that points straight out, with no chain.
///
/// The chain above is what makes it reachable through the agent's own
/// `symlink` handler; this is what happens when the export simply contains a
/// symlink somebody made another way — far more common.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_plain_symlink_out_of_the_export_is_refused_on_create() {
    let outside = tempfile::TempDir::new().expect("outside");
    let secret = outside.path().join("secret.txt");
    std::fs::write(&secret, b"ORIGINAL SECRET").unwrap();

    let export_root = outside.path().join("export");
    std::fs::create_dir(&export_root).unwrap();
    std::os::unix::fs::symlink(&secret, export_root.join("esc")).unwrap();

    let mut cfg = AgentConfig::default();
    cfg.exports.insert(
        "test".into(),
        ExportConfig {
            path: export_root.clone(),
            read_only: false,
            exclude: Vec::new(),
            ..Default::default()
        },
    );
    let registry = Arc::new(ExportRegistry::from_config(&cfg).expect("registry"));

    let escaped = create_succeeds(&registry, "esc").await;
    assert_eq!(
        std::fs::read(&secret).unwrap_or_default(),
        b"ORIGINAL SECRET",
        "create() truncated a file outside the export"
    );
    assert!(!escaped, "must be refused");
}
