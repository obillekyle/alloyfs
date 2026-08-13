//! In-process loopback integration battery: real `AgentSession` and real
//! `RemoteFs` talking the full wire protocol over `tokio::io::duplex`.
//!
//! Every test runs on a multi-thread runtime because `RemoteFs` methods are
//! synchronous and `block_on` a captured runtime handle — on a current-thread
//! runtime that would deadlock. All `RemoteFs` calls go through
//! `harness::on_fs` (spawn_blocking) for the same reason.

mod harness;

use std::time::Duration;

use bytes::Bytes;
use ds_client::{ClientOptions, ROOT_INO};
use ds_proto::{ErrorCode, EventKind, FileKind, FsEvent, LockKind, OpenFlags, RelPath, Request, Response};
use ds_transport::TransportError;
use harness::{
    connect, connect_reconnectable, deadline_after, expect_event, lookup_path, mkfile, on_fs, patterned,
    read_all, remote_code, start_agent, wait_until, AgentOpts, Session,
};

fn rw() -> OpenFlags {
    OpenFlags {
        read: true,
        write: true,
        ..OpenFlags::default()
    }
}

fn ro() -> OpenFlags {
    OpenFlags {
        read: true,
        ..OpenFlags::default()
    }
}

/// Grab the raw event receiver BEFORE subscribing (pushes to a broadcast with
/// no receiver are dropped), then issue the Subscribe request.
async fn subscribe(s: &Session) -> tokio::sync::broadcast::Receiver<Vec<FsEvent>> {
    let conn = s.conn();
    let rx = conn.events();
    let resp = conn
        .request(Request::Subscribe { since_seq: None })
        .await
        .expect("transport")
        .expect("subscribe");
    assert!(matches!(resp, Response::Subscribed { .. }), "got {resp:?}");
    rx
}

// ---------------------------------------------------------------------- 1

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attach_getattr_readdir_versions() {
    let agent = start_agent(AgentOpts::default());
    std::fs::write(agent.dir.path().join("alpha.txt"), b"aaa").unwrap();
    std::fs::write(agent.dir.path().join("beta.txt"), b"bb").unwrap();

    let s = connect(&agent, ClientOptions::default()).await;
    let root = on_fs(&s.fs, |fs| fs.getattr(ROOT_INO)).await.unwrap();
    assert_eq!(root.kind, FileKind::Dir);

    let entries = on_fs(&s.fs, |fs| fs.readdir(ROOT_INO)).await.unwrap();
    for name in ["alpha.txt", "beta.txt"] {
        let (_, _, attr) = entries
            .iter()
            .find(|(n, _, _)| n == name)
            .unwrap_or_else(|| panic!("{name} missing from {entries:?}"));
        assert_eq!(attr.kind, FileKind::File);
        assert_eq!(attr.version, 0, "pre-seeded {name} must start at version 0");
    }

    mkfile(&s.fs, ROOT_INO, "gamma.txt", b"ggg").await;
    let entries = on_fs(&s.fs, |fs| fs.readdir(ROOT_INO)).await.unwrap();
    let (_, _, attr) = entries
        .iter()
        .find(|(n, _, _)| n == "gamma.txt")
        .expect("gamma.txt listed after create");
    assert!(attr.version > 0, "client-created file must have a bumped version");
}

// ---------------------------------------------------------------------- 2

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_write_read_roundtrip() {
    let agent = start_agent(AgentOpts::default());
    let s = connect(&agent, ClientOptions::default()).await;

    let data = patterned(4 * 1024);
    let ino = mkfile(&s.fs, ROOT_INO, "f.bin", &data).await;
    assert_eq!(read_all(&s.fs, ino).await, data);
    assert_eq!(std::fs::read(agent.dir.path().join("f.bin")).unwrap(), data);
}

// ---------------------------------------------------------------------- 3

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_chunk_write_read() {
    let agent = start_agent(AgentOpts::default());
    let s = connect(&agent, ClientOptions::default()).await;

    let data = patterned(300_000); // > 2 × DATA_CHUNK: exercises chunked write and read
    let ino = mkfile(&s.fs, ROOT_INO, "big.bin", &data).await;
    assert_eq!(read_all(&s.fs, ino).await, data);

    let mid = on_fs(&s.fs, move |fs| {
        let (fh, _) = fs.open(ino, ro())?;
        let out = fs.read(fh, 100_000, 100_000);
        fs.release(fh);
        out
    })
    .await
    .unwrap();
    assert_eq!(mid, data[100_000..200_000]);
    assert_eq!(std::fs::read(agent.dir.path().join("big.bin")).unwrap(), data);
}

// ---------------------------------------------------------------------- 4

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rename_unlink_rmdir() {
    let agent = start_agent(AgentOpts::default());
    let s = connect(&agent, ClientOptions::default()).await;

    let (d_ino, _) = on_fs(&s.fs, |fs| fs.mkdir(ROOT_INO, "d", 0o755)).await.unwrap();
    mkfile(&s.fs, d_ino, "a.txt", b"one").await;
    mkfile(&s.fs, d_ino, "c.txt", b"three").await;

    on_fs(&s.fs, move |fs| fs.rename(d_ino, "a.txt", d_ino, "b.txt", false))
        .await
        .unwrap();
    let err = lookup_path(&s.fs, "d/a.txt").await.unwrap_err();
    assert_eq!(remote_code(err), ErrorCode::NotFound, "old name must be gone");
    let (_, attr) = lookup_path(&s.fs, "d/b.txt").await.unwrap();
    assert_eq!(attr.size, 3, "new name must resolve to the same file");

    let err = on_fs(&s.fs, move |fs| fs.rename(d_ino, "b.txt", d_ino, "c.txt", false))
        .await
        .unwrap_err();
    assert_eq!(
        remote_code(err),
        ErrorCode::AlreadyExists,
        "replace:false onto an existing name must fail"
    );

    let err = on_fs(&s.fs, |fs| fs.rmdir(ROOT_INO, "d")).await.unwrap_err();
    assert_eq!(remote_code(err), ErrorCode::NotEmpty);

    on_fs(&s.fs, move |fs| fs.unlink(d_ino, "b.txt")).await.unwrap();
    on_fs(&s.fs, move |fs| fs.unlink(d_ino, "c.txt")).await.unwrap();
    on_fs(&s.fs, |fs| fs.rmdir(ROOT_INO, "d")).await.unwrap();
    let err = lookup_path(&s.fs, "d").await.unwrap_err();
    assert_eq!(remote_code(err), ErrorCode::NotFound);
}

// ---------------------------------------------------------------------- 5

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_excl_semantics() {
    let agent = start_agent(AgentOpts::default());
    let s = connect(&agent, ClientOptions::default()).await;

    let excl = OpenFlags { excl: true, ..rw() };
    on_fs(&s.fs, move |fs| {
        let (_, fh, _) = fs.create(ROOT_INO, "x.txt", 0o644, excl)?;
        fs.write(fh, 0, b"12345")?;
        fs.release(fh);
        Ok::<_, ds_client::FsError>(())
    })
    .await
    .unwrap();

    let err = on_fs(&s.fs, move |fs| fs.create(ROOT_INO, "x.txt", 0o644, excl))
        .await
        .unwrap_err();
    assert_eq!(
        remote_code(err),
        ErrorCode::AlreadyExists,
        "O_EXCL on existing file"
    );

    // Non-excl, non-truncate create on an existing file: opens it, size kept.
    let attr = on_fs(&s.fs, |fs| {
        let (_, fh, attr) = fs.create(ROOT_INO, "x.txt", 0o644, rw())?;
        fs.release(fh);
        Ok::<_, ds_client::FsError>(attr)
    })
    .await
    .unwrap();
    assert_eq!(attr.size, 5, "truncate:false must preserve existing content");
}

// ---------------------------------------------------------------------- 6

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hardlink_semantics() {
    let agent = start_agent(AgentOpts::default());
    let s = connect(&agent, ClientOptions::default()).await;

    let (ino_a, fh_a) = on_fs(&s.fs, |fs| {
        let (ino, fh, _) = fs.create(ROOT_INO, "a.txt", 0o644, rw()).unwrap();
        (ino, fh)
    })
    .await;
    on_fs(&s.fs, move |fs| fs.link(ino_a, ROOT_INO, "b.txt"))
        .await
        .unwrap();

    let data = b"written through a, read through b".to_vec();
    let write_data = data.clone();
    on_fs(&s.fs, move |fs| fs.write(fh_a, 0, &write_data))
        .await
        .unwrap();

    let (ino_b, _) = lookup_path(&s.fs, "b.txt").await.unwrap();
    assert_eq!(read_all(&s.fs, ino_b).await, data, "link shares content");

    on_fs(&s.fs, move |fs| fs.release(fh_a)).await;
    on_fs(&s.fs, |fs| fs.unlink(ROOT_INO, "a.txt")).await.unwrap();
    let err = lookup_path(&s.fs, "a.txt").await.unwrap_err();
    assert_eq!(remote_code(err), ErrorCode::NotFound);
    assert_eq!(read_all(&s.fs, ino_b).await, data, "b survives unlinking a");
}

// ---------------------------------------------------------------------- 7

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_excludes_invisible() {
    let agent = start_agent(AgentOpts {
        excludes: vec!["secret/**".into(), "*.key".into()],
        ..AgentOpts::default()
    });
    std::fs::create_dir(agent.dir.path().join("secret")).unwrap();
    std::fs::write(agent.dir.path().join("secret").join("x.txt"), b"hidden").unwrap();
    std::fs::write(agent.dir.path().join("top.key"), b"hidden").unwrap();
    std::fs::write(agent.dir.path().join("visible.txt"), b"shown").unwrap();

    let s = connect(&agent, ClientOptions::default()).await;
    let names: Vec<String> = on_fs(&s.fs, |fs| fs.readdir(ROOT_INO))
        .await
        .unwrap()
        .into_iter()
        .map(|(n, _, _)| n)
        .collect();
    assert!(names.contains(&"visible.txt".to_string()), "listing: {names:?}");
    assert!(
        !names.contains(&"top.key".to_string()),
        "excluded file listed: {names:?}"
    );
    assert!(!names.contains(&"x.txt".to_string()));

    // "secret/**" hides the CONTENTS; the directory entry itself may list,
    // but it must appear empty and its children must not resolve.
    if names.contains(&"secret".to_string()) {
        let (secret_ino, _) = lookup_path(&s.fs, "secret").await.unwrap();
        let inside = on_fs(&s.fs, move |fs| fs.readdir(secret_ino)).await.unwrap();
        assert!(inside.is_empty(), "excluded children listed: {inside:?}");
    }
    let err = lookup_path(&s.fs, "secret/x.txt").await.unwrap_err();
    assert_eq!(remote_code(err), ErrorCode::NotFound);

    // Exactly NotFound — existence must not leak as PermissionDenied.
    let err = lookup_path(&s.fs, "top.key").await.unwrap_err();
    assert_eq!(remote_code(err), ErrorCode::NotFound);
}

// ---------------------------------------------------------------------- 8

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readonly_export() {
    let agent = start_agent(AgentOpts {
        read_only: true,
        ..AgentOpts::default()
    });
    std::fs::write(agent.dir.path().join("r.txt"), b"readable").unwrap();

    let s = connect(&agent, ClientOptions::default()).await;
    let err = on_fs(&s.fs, |fs| fs.create(ROOT_INO, "new.txt", 0o644, rw()))
        .await
        .unwrap_err();
    assert_eq!(remote_code(err), ErrorCode::ReadOnly);
    let err = on_fs(&s.fs, |fs| fs.mkdir(ROOT_INO, "newdir", 0o755))
        .await
        .unwrap_err();
    assert_eq!(remote_code(err), ErrorCode::ReadOnly);
    let err = on_fs(&s.fs, |fs| fs.unlink(ROOT_INO, "r.txt")).await.unwrap_err();
    assert_eq!(remote_code(err), ErrorCode::ReadOnly);

    let (ino, _) = lookup_path(&s.fs, "r.txt").await.unwrap();
    let err = on_fs(&s.fs, move |fs| fs.open(ino, rw())).await.unwrap_err();
    assert_eq!(
        remote_code(err),
        ErrorCode::ReadOnly,
        "write-open must be refused"
    );

    assert_eq!(read_all(&s.fs, ino).await, b"readable", "plain read still works");
}

// ---------------------------------------------------------------------- 9

/// Open the file in this session and return the fh.
async fn open_for_lock(s: &Session, name: &'static str) -> u64 {
    let (ino, _) = lookup_path(&s.fs, name).await.unwrap();
    on_fs(&s.fs, move |fs| fs.open(ino, rw()).unwrap().0).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lock_exclusion_across_sessions() {
    let agent = start_agent(AgentOpts::default());
    std::fs::write(agent.dir.path().join("l.txt"), b"lockme").unwrap();
    let s1 = connect(&agent, ClientOptions::default()).await;
    let s2 = connect(&agent, ClientOptions::default()).await;

    let fh1 = open_for_lock(&s1, "l.txt").await;
    let fh2 = open_for_lock(&s2, "l.txt").await;

    on_fs(&s1.fs, move |fs| fs.lock(fh1, LockKind::Exclusive, false))
        .await
        .unwrap();
    for kind in [LockKind::Exclusive, LockKind::Shared] {
        let err = on_fs(&s2.fs, move |fs| fs.lock(fh2, kind, false))
            .await
            .unwrap_err();
        assert_eq!(
            remote_code(err),
            ErrorCode::WouldBlock,
            "{kind:?} vs held Exclusive"
        );
    }

    on_fs(&s1.fs, move |fs| fs.unlock(fh1)).await.unwrap();
    on_fs(&s2.fs, move |fs| fs.lock(fh2, LockKind::Exclusive, false))
        .await
        .unwrap();
}

// --------------------------------------------------------------------- 10

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lock_blocking_wait() {
    let agent = start_agent(AgentOpts::default());
    std::fs::write(agent.dir.path().join("l.txt"), b"lockme").unwrap();
    let s1 = connect(&agent, ClientOptions::default()).await;
    let s2 = connect(&agent, ClientOptions::default()).await;

    let fh1 = open_for_lock(&s1, "l.txt").await;
    let fh2 = open_for_lock(&s2, "l.txt").await;
    on_fs(&s1.fs, move |fs| fs.lock(fh1, LockKind::Exclusive, false))
        .await
        .unwrap();

    let fs2 = s2.fs.clone();
    let waiter = tokio::task::spawn_blocking(move || fs2.lock(fh2, LockKind::Exclusive, true));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !waiter.is_finished(),
        "wait:true must block while the lock is held"
    );

    on_fs(&s1.fs, move |fs| fs.unlock(fh1)).await.unwrap();
    wait_until("blocked lock granted after unlock", 5, || {
        waiter.is_finished().then_some(())
    })
    .await;
    waiter.await.expect("join").expect("lock granted");
}

// --------------------------------------------------------------------- 11

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lock_released_on_sever() {
    let agent = start_agent(AgentOpts::default());
    std::fs::write(agent.dir.path().join("l.txt"), b"lockme").unwrap();
    let s1 = connect(&agent, ClientOptions::default()).await;
    let s2 = connect(&agent, ClientOptions::default()).await;

    let fh1 = open_for_lock(&s1, "l.txt").await;
    let fh2 = open_for_lock(&s2, "l.txt").await;
    on_fs(&s1.fs, move |fs| fs.lock(fh1, LockKind::Exclusive, false))
        .await
        .unwrap();

    // Kill s1's connection server-side: disconnected() must free its locks.
    s1.sever();

    let fs2 = s2.fs.clone();
    let waiter = tokio::task::spawn_blocking(move || fs2.lock(fh2, LockKind::Exclusive, true));
    wait_until("lock granted after holder severed", 5, || {
        waiter.is_finished().then_some(())
    })
    .await;
    waiter
        .await
        .expect("join")
        .expect("lock granted after disconnect");
}

// --------------------------------------------------------------------- 12

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_conflict_last_writer_wins() {
    let agent = start_agent(AgentOpts::default());
    let s1 = connect(&agent, ClientOptions::default()).await;
    let s2 = connect(&agent, ClientOptions::default()).await;

    // s1 creates the file and keeps its (writable) fh + creation version.
    let (fh1, v1) = on_fs(&s1.fs, |fs| {
        let (_, fh, attr) = fs.create(ROOT_INO, "w.txt", 0o644, rw()).unwrap();
        (fh, attr.version)
    })
    .await;

    // s2 writes through its own handle, bumping the server-side version.
    let (ino2, _) = lookup_path(&s2.fs, "w.txt").await.unwrap();
    let fh2 = on_fs(&s2.fs, move |fs| fs.open(ino2, rw()).unwrap().0).await;
    on_fs(&s2.fs, move |fs| fs.write(fh2, 0, b"second"))
        .await
        .unwrap();

    // s1's raw write pins the now-stale version: flagged, but still applied.
    let resp = s1
        .conn()
        .request(Request::Write {
            fh: fh1,
            offset: 0,
            data: Bytes::from_static(b"FINAL!"),
            expect_version: Some(v1),
        })
        .await
        .expect("transport")
        .expect("write");
    match resp {
        Response::Written { conflict, n, .. } => {
            assert!(conflict, "stale expect_version must be reported as a conflict");
            assert_eq!(n, 6);
        }
        other => panic!("expected Written, got {other:?}"),
    }
    assert_eq!(
        std::fs::read(agent.dir.path().join("w.txt")).unwrap(),
        b"FINAL!",
        "last writer wins on disk"
    );
}

// --------------------------------------------------------------------- 13

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn events_end_to_end() {
    let agent = start_agent(AgentOpts {
        watch: true,
        ..AgentOpts::default()
    });
    let s = connect(&agent, ClientOptions::default()).await;
    let mut rx = subscribe(&s).await;

    std::fs::write(agent.dir.path().join("note.txt"), b"hello").unwrap();
    let (ev, _) = expect_event(&mut rx, 10, |e| {
        e.path.0 == "note.txt" && matches!(e.kind, EventKind::Created | EventKind::Modified)
    })
    .await;
    assert_eq!(ev.path.0, "note.txt");

    std::fs::remove_file(agent.dir.path().join("note.txt")).unwrap();
    expect_event(&mut rx, 10, |e| {
        e.path.0 == "note.txt" && matches!(e.kind, EventKind::Removed)
    })
    .await;
}

// --------------------------------------------------------------------- 14

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn events_exclude_filtered() {
    let agent = start_agent(AgentOpts {
        watch: true,
        excludes: vec!["secret/**".into()],
        ..AgentOpts::default()
    });
    let s = connect(&agent, ClientOptions::default()).await;
    let mut rx = subscribe(&s).await;

    std::fs::create_dir(agent.dir.path().join("secret")).unwrap();
    std::fs::write(agent.dir.path().join("secret").join("hidden.txt"), b"shh").unwrap();
    std::fs::write(agent.dir.path().join("visible.txt"), b"hi").unwrap();

    // hidden.txt was touched FIRST: were it ever going to produce an event,
    // it would arrive before or alongside visible.txt's.
    let (_, seen) = expect_event(&mut rx, 10, |e| e.path.0 == "visible.txt").await;
    assert!(
        seen.iter().all(|e| !e.path.0.starts_with("secret/")),
        "excluded paths must never be event-broadcast: {seen:?}"
    );
}

// --------------------------------------------------------------------- 15

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn overlay_routing() {
    let agent = start_agent(AgentOpts::default());
    std::fs::write(agent.dir.path().join("remote.txt"), b"remote").unwrap();
    let data_dir = tempfile::TempDir::new().unwrap();

    let opts = ClientOptions {
        excludes: vec!["*.local".into()],
        data_dir: data_dir.path().to_path_buf(),
        mount_key: "t".into(),
        ..ClientOptions::default()
    };
    let s = connect(&agent, opts).await;

    let ino = mkfile(&s.fs, ROOT_INO, "notes.local", b"client only").await;

    let overlay_file = data_dir.path().join("overlay").join("t").join("notes.local");
    assert_eq!(std::fs::read(&overlay_file).unwrap(), b"client only");
    assert!(
        !agent.dir.path().join("notes.local").exists(),
        "overlay file must never materialize on the server"
    );
    assert_eq!(read_all(&s.fs, ino).await, b"client only");

    let names: Vec<String> = on_fs(&s.fs, |fs| fs.readdir(ROOT_INO))
        .await
        .unwrap()
        .into_iter()
        .map(|(n, _, _)| n)
        .collect();
    assert!(
        names.contains(&"notes.local".to_string()),
        "union listing: {names:?}"
    );
    assert!(
        names.contains(&"remote.txt".to_string()),
        "union listing: {names:?}"
    );

    // The server's namespace never even hears the name.
    let resp = s
        .conn()
        .request(Request::Getattr {
            path: RelPath("notes.local".into()),
        })
        .await
        .expect("transport");
    assert_eq!(resp.unwrap_err(), ErrorCode::NotFound);
}

// --------------------------------------------------------------------- 16

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn overlay_exdev_boundary() {
    let agent = start_agent(AgentOpts::default());
    std::fs::write(agent.dir.path().join("plain.txt"), b"remote").unwrap();
    let data_dir = tempfile::TempDir::new().unwrap();

    let opts = ClientOptions {
        excludes: vec!["*.local".into()],
        data_dir: data_dir.path().to_path_buf(),
        mount_key: "t".into(),
        ..ClientOptions::default()
    };
    let s = connect(&agent, opts).await;
    let (plain_ino, _) = lookup_path(&s.fs, "plain.txt").await.unwrap();

    // remote → overlay rename crosses the boundary.
    let err = on_fs(&s.fs, |fs| {
        fs.rename(ROOT_INO, "plain.txt", ROOT_INO, "plain.local", false)
    })
    .await
    .unwrap_err();
    assert_eq!(remote_code(err), ErrorCode::CrossDevice);

    // overlay → remote rename too.
    mkfile(&s.fs, ROOT_INO, "x.local", b"local").await;
    let err = on_fs(&s.fs, |fs| {
        fs.rename(ROOT_INO, "x.local", ROOT_INO, "x.txt", false)
    })
    .await
    .unwrap_err();
    assert_eq!(remote_code(err), ErrorCode::CrossDevice);

    // Hard links can't span the boundary either.
    let err = on_fs(&s.fs, move |fs| fs.link(plain_ino, ROOT_INO, "copy.local"))
        .await
        .unwrap_err();
    assert_eq!(remote_code(err), ErrorCode::CrossDevice);
}

// --------------------------------------------------------------------- 17

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn autocache_fresh_serve_write_invalidate() {
    let agent = start_agent(AgentOpts::default());
    let content = patterned(64 * 1024);
    std::fs::write(agent.dir.path().join("big.bin"), &content).unwrap();
    let data_dir = tempfile::TempDir::new().unwrap();

    let opts = ClientOptions {
        data_dir: data_dir.path().to_path_buf(),
        mount_key: "t".into(),
        auto_cache_max: Some(1 << 20),
        auto_cache_budget: Some(10 << 20),
        ..ClientOptions::default()
    };
    let s = connect(&agent, opts).await;

    // The walker runs at attach and pulls the whole (small) file down.
    let blob = data_dir.path().join("cache").join("t").join("big.bin");
    {
        let (blob, content) = (blob.clone(), content.clone());
        wait_until("walker caches big.bin", 10, move || {
            (std::fs::read(&blob).ok()? == content).then_some(())
        })
        .await;
    }

    let (ino, _) = lookup_path(&s.fs, "big.bin").await.unwrap();
    assert_eq!(
        read_all(&s.fs, ino).await,
        content,
        "cached read serves the right bytes"
    );

    // Write through the client, then read back on the SAME fh: the stale
    // blob must not answer.
    let new_content: Vec<u8> = content.iter().map(|b| b ^ 0xA5).collect();
    let fh = on_fs(&s.fs, move |fs| fs.open(ino, rw()).unwrap().0).await;
    {
        let new_content = new_content.clone();
        on_fs(&s.fs, move |fs| {
            fs.write(fh, 0, &new_content)?;
            fs.read(fh, 0, new_content.len() as u32)
                .map(|got| assert_eq!(got, new_content, "read-after-write must bypass the stale blob"))
        })
        .await
        .unwrap();
    }

    // Release queues a refetch; the blob converges on the new content.
    on_fs(&s.fs, move |fs| fs.release(fh)).await;
    wait_until("blob refetched after write+release", 10, move || {
        (std::fs::read(&blob).ok()? == new_content).then_some(())
    })
    .await;
}

// --------------------------------------------------------------------- 18

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readahead_sequential_correctness() {
    let agent = start_agent(AgentOpts::default());
    let data = patterned(2 * 1024 * 1024);
    std::fs::write(agent.dir.path().join("stream.bin"), &data).unwrap();
    let s = connect(&agent, ClientOptions::default()).await;

    let (ino, _) = lookup_path(&s.fs, "stream.bin").await.unwrap();
    let fh = on_fs(&s.fs, move |fs| fs.open(ino, ro()).unwrap().0).await;

    // Sequential stream in 256 KiB steps: after MIN_STREAK back-to-back reads
    // the window fills and later slices are served from prefetched blocks —
    // every byte must still be exact.
    let step = 256 * 1024usize;
    for i in 0..data.len() / step {
        let off = i * step;
        let got = on_fs(&s.fs, move |fs| fs.read(fh, off as u64, step as u32))
            .await
            .unwrap();
        assert!(
            got == data[off..off + step],
            "sequential step {i} at offset {off}"
        );
    }

    // Pattern break: scattered, DATA_CHUNK-unaligned reads on the SAME fh.
    // The window must clear on the break — no stale prefetched bytes.
    for &(off, len) in &[(100_001usize, 200_000usize), (1_700_003, 50_000), (37, 4_096)] {
        let got = on_fs(&s.fs, move |fs| fs.read(fh, off as u64, len as u32))
            .await
            .unwrap();
        assert!(got == data[off..off + len], "random read at {off}+{len}");
    }
    on_fs(&s.fs, move |fs| fs.release(fh)).await;
}

// --------------------------------------------------------------------- 19

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconnect_read_survives() {
    let agent = start_agent(AgentOpts::default());
    let content = patterned(48 * 1024);
    std::fs::write(agent.dir.path().join("r.bin"), &content).unwrap();
    let s = connect_reconnectable(&agent, ClientOptions::default()).await;

    let (ino, _) = lookup_path(&s.fs, "r.bin").await.unwrap();
    let fh = on_fs(&s.fs, move |fs| fs.open(ino, ro()).unwrap().0).await;
    let len = content.len() as u32;
    let got = on_fs(&s.fs, move |fs| fs.read(fh, 0, len)).await.unwrap();
    assert!(got == content, "read before disconnect");

    let old_conn = s.fs.conn();
    s.sever();

    // A read issued in the dead window can stall for the full REQUEST_TIMEOUT
    // (30 s): its waiter registers after the dying reader already failed all
    // inflight requests, so nothing fails it early. Wait for the supervisor's
    // swap first (new connection object, not closed), then prove the SAME
    // kernel fh works again (re-dialed, re-attached, handle re-opened with a
    // fresh server_fh translation).
    wait_until("supervisor swapped in a live connection", 10, || {
        let now = s.fs.conn();
        (!std::sync::Arc::ptr_eq(&old_conn, &now) && !now.is_closed()).then_some(())
    })
    .await;

    let deadline = deadline_after(10);
    loop {
        match on_fs(&s.fs, move |fs| fs.read(fh, 0, len)).await {
            Ok(data) => {
                assert!(data == content, "reconnected read returned wrong bytes");
                break;
            }
            Err(err) => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "read did not recover within 10s of reconnect (last: {err:?})"
                );
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
    }
    on_fs(&s.fs, move |fs| fs.release(fh)).await;
}

// --------------------------------------------------------------------- 20

/// Accumulate pump batches into `captured` until `done(captured)`; false on
/// deadline or pump end.
async fn recv_until(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<Vec<FsEvent>>,
    captured: &mut Vec<FsEvent>,
    secs: u64,
    done: impl Fn(&[FsEvent]) -> bool,
) -> bool {
    let deadline = deadline_after(secs);
    while !done(captured) {
        let now = std::time::Instant::now();
        if now >= deadline {
            return false;
        }
        match tokio::time::timeout(deadline - now, rx.recv()).await {
            Ok(Some(batch)) => captured.extend(batch),
            Ok(None) | Err(_) => return false,
        }
    }
    true
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconnect_events_resume() {
    let agent = start_agent(AgentOpts {
        watch: true,
        ..AgentOpts::default()
    });
    let s = connect_reconnectable(&agent, ClientOptions::default()).await;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<FsEvent>>();
    s.fs.start_event_pump(move |batch| {
        let _ = tx.send(batch.to_vec());
    })
    .await
    .unwrap();

    // A arrives over the live stream.
    std::fs::write(agent.dir.path().join("a.txt"), b"first").unwrap();
    let mut captured: Vec<FsEvent> = Vec::new();
    assert!(
        recv_until(&mut rx, &mut captured, 10, |evs| evs
            .iter()
            .any(|e| e.path.0 == "a.txt"))
        .await,
        "a.txt event before disconnect: {captured:?}"
    );

    // Kill the link; the change to B lands only in the server-side ring log.
    s.sever();
    tokio::time::sleep(Duration::from_millis(500)).await;
    std::fs::write(agent.dir.path().join("b.txt"), b"second").unwrap();

    // After reconnect the pump resubscribes with since_seq and the ring-log
    // catchup (or the live stream, if B flushes late) must deliver b.txt.
    // The supervisor's epoch bump can race the pump's Closed observation; if
    // the pump missed this cycle, another sever forces the next one — the
    // ring log still holds b.txt, so catchup delivers it regardless.
    let overall = deadline_after(15);
    loop {
        if recv_until(&mut rx, &mut captured, 3, |evs| {
            evs.iter().any(|e| e.path.0 == "b.txt")
        })
        .await
        {
            break;
        }
        assert!(
            std::time::Instant::now() < overall,
            "b.txt never delivered after reconnect; captured: {captured:?}"
        );
        s.sever();
    }

    let a = captured.iter().position(|e| e.path.0 == "a.txt").unwrap();
    let b = captured.iter().position(|e| e.path.0 == "b.txt").unwrap();
    assert!(a < b, "a.txt must precede b.txt: {captured:?}");
}

// --------------------------------------------------------------------- 21

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_timeout() {
    let agent = start_agent(AgentOpts::default());
    std::fs::write(agent.dir.path().join("l.txt"), b"lockme").unwrap();
    let s1 = connect(&agent, ClientOptions::default()).await;
    let s2 = connect(&agent, ClientOptions::default()).await;

    let fh1 = open_for_lock(&s1, "l.txt").await;
    let fh2 = open_for_lock(&s2, "l.txt").await;
    on_fs(&s1.fs, move |fs| fs.lock(fh1, LockKind::Exclusive, false))
        .await
        .unwrap();

    // A wait:true lock on a held file is a request the server legitimately
    // never answers — exactly what the client-side deadline is for.
    let started = std::time::Instant::now();
    let err = s2
        .conn()
        .request_with_deadline(
            Request::Lock {
                fh: fh2,
                kind: LockKind::Exclusive,
                wait: true,
            },
            Duration::from_millis(300),
        )
        .await
        .unwrap_err();
    let waited = started.elapsed();
    assert!(matches!(err, TransportError::Timeout), "got {err:?}");
    assert!(
        waited >= Duration::from_millis(250),
        "timeout fired too early: {waited:?}"
    );
    assert!(
        waited < Duration::from_millis(1500),
        "deadline not honored (~300ms expected): {waited:?}"
    );
}

// --------------------------------------------------------------------- 22

/// The export's `client:` section reaches a default-configured client at
/// attach: overlay excludes, pins, and cache sizes all come from the server.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_defaults_negotiated() {
    let agent = start_agent(AgentOpts {
        client_defaults: Some(ds_agent::ClientDefaults {
            exclude: vec!["*.local".into()],
            pin: vec![],
            auto_cache_max: Some(ds_common::SizeField::Bytes(1 << 20)),
            auto_cache_budget: Some(ds_common::SizeField::Human("10M".into())),
        }),
        ..AgentOpts::default()
    });
    let content = patterned(64 * 1024);
    std::fs::write(agent.dir.path().join("big.bin"), &content).unwrap();
    let data_dir = tempfile::TempDir::new().unwrap();

    // No excludes, no sizes: everything below must come from the server.
    let opts = ClientOptions {
        data_dir: data_dir.path().to_path_buf(),
        mount_key: "t".into(),
        ..ClientOptions::default()
    };
    let s = connect(&agent, opts).await;

    // Server-suggested exclude activates the overlay.
    mkfile(&s.fs, ROOT_INO, "notes.local", b"client only").await;
    let overlay_file = data_dir.path().join("overlay").join("t").join("notes.local");
    assert_eq!(std::fs::read(&overlay_file).unwrap(), b"client only");
    assert!(
        !agent.dir.path().join("notes.local").exists(),
        "server-suggested exclude must keep the file off the server"
    );

    // Server-suggested cache sizes enable the auto-cache walker.
    let blob = data_dir.path().join("cache").join("t").join("big.bin");
    wait_until(
        "walker caches big.bin via server-suggested sizes",
        10,
        move || (std::fs::read(&blob).ok()? == content).then_some(()),
    )
    .await;
}

// --------------------------------------------------------------------- 23

/// `no_server_defaults` (and explicit client values) beat the suggestion.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_defaults_opt_out_and_precedence() {
    let agent = start_agent(AgentOpts {
        client_defaults: Some(ds_agent::ClientDefaults {
            exclude: vec!["*.local".into()],
            pin: vec![],
            auto_cache_max: Some(ds_common::SizeField::Bytes(1 << 20)),
            auto_cache_budget: None,
        }),
        ..AgentOpts::default()
    });
    let data_dir = tempfile::TempDir::new().unwrap();

    // Opted out: the suggestion is never even requested, so *.local is a
    // perfectly ordinary remote file.
    let opts = ClientOptions {
        data_dir: data_dir.path().to_path_buf(),
        mount_key: "t".into(),
        no_server_defaults: true,
        ..ClientOptions::default()
    };
    let s = connect(&agent, opts).await;
    mkfile(&s.fs, ROOT_INO, "notes.local", b"remote after all").await;
    assert_eq!(
        std::fs::read(agent.dir.path().join("notes.local")).unwrap(),
        b"remote after all",
        "opt-out: file must land on the server"
    );
    drop(s);

    // Explicit Some(0) beats the server's Some(1M): cache stays off, so the
    // walker never materializes a blob dir for this mount.
    let content = patterned(16 * 1024);
    std::fs::write(agent.dir.path().join("small.bin"), &content).unwrap();
    let opts = ClientOptions {
        data_dir: data_dir.path().to_path_buf(),
        mount_key: "t2".into(),
        auto_cache_max: Some(0),
        ..ClientOptions::default()
    };
    let s = connect(&agent, opts).await;
    let (ino, _) = lookup_path(&s.fs, "small.bin").await.unwrap();
    assert_eq!(read_all(&s.fs, ino).await, content);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !data_dir
            .path()
            .join("cache")
            .join("t2")
            .join("small.bin")
            .exists(),
        "explicit auto_cache_max=0 must beat the server suggestion"
    );
}
