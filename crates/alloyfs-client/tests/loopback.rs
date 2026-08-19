//! In-process loopback integration battery: real `AgentSession` and real
//! `RemoteFs` talking the full wire protocol over `tokio::io::duplex`.
//!
//! Every test runs on a multi-thread runtime because `RemoteFs` methods are
//! synchronous and `block_on` a captured runtime handle — on a current-thread
//! runtime that would deadlock. All `RemoteFs` calls go through
//! `harness::on_fs` (spawn_blocking) for the same reason.

mod harness;

use std::time::Duration;

use alloyfs_client::{ClientOptions, ROOT_INO};
use alloyfs_proto::{
    ErrorCode, EventKind, FileKind, FsEvent, LockKind, OpenFlags, RelPath, Request, Response,
    PROTO_VERSION_MAX,
};
use alloyfs_transport::TransportError;
use bytes::Bytes;
use harness::{
    connect, connect_raw_with_server_token, connect_reconnectable, deadline_after, expect_event, lookup_path,
    mkfile, on_fs, patterned, read_all, remote_code, start_agent, wait_until, AgentOpts, Session,
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
        Ok::<_, alloyfs_client::FsError>(())
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
        Ok::<_, alloyfs_client::FsError>(attr)
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

    // s1's raw write pins the now-stale version, and is REFUSED.
    //
    // This used to assert the opposite: the write landed and carried a
    // `conflict` flag. Telling a client its data was clobbered, after the
    // clobbering, is a notification rather than a safeguard — so a request
    // that pins a version now means "stop if this is not still true".
    let resp = s1
        .conn()
        .request(Request::Write {
            fh: fh1,
            offset: 0,
            data: Bytes::from_static(b"FINAL!"),
            expect_version: Some(v1),
        })
        .await
        .expect("transport");
    assert!(
        matches!(resp, Err(ErrorCode::Conflict)),
        "a stale expect_version must refuse the write, got {resp:?}"
    );
    assert_eq!(
        std::fs::read(agent.dir.path().join("w.txt")).unwrap(),
        b"second",
        "the refused write must not have touched the file"
    );

    // And without the pin, the old last-writer-wins behaviour is untouched —
    // every existing client sends None and must not start failing.
    let resp = s1
        .conn()
        .request(Request::Write {
            fh: fh1,
            offset: 0,
            data: Bytes::from_static(b"FINAL!"),
            expect_version: None,
        })
        .await
        .expect("transport")
        .expect("an unpinned write still wins");
    // Either write reply shape is fine here — what matters is that the write
    // was accepted and 6 bytes landed, not which protocol version answered.
    assert!(
        matches!(
            resp,
            Response::Written { n: 6, .. } | Response::WrittenAttr { n: 6, .. }
        ),
        "got {resp:?}"
    );
    assert_eq!(std::fs::read(agent.dir.path().join("w.txt")).unwrap(), b"FINAL!");
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
        cache_dir: data_dir.path().join("cache"),
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
        cache_dir: data_dir.path().join("cache"),
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
        cache_dir: data_dir.path().join("cache"),
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
        client_defaults: Some(alloyfs_agent::ClientDefaults {
            exclude: vec!["*.local".into()],
            pin: vec![],
            auto_cache_max: Some(alloyfs_common::SizeField::Bytes(1 << 20)),
            auto_cache_budget: Some(alloyfs_common::SizeField::Human("10M".into())),
        }),
        ..AgentOpts::default()
    });
    let content = patterned(64 * 1024);
    std::fs::write(agent.dir.path().join("big.bin"), &content).unwrap();
    let data_dir = tempfile::TempDir::new().unwrap();

    // No excludes, no sizes: everything below must come from the server.
    let opts = ClientOptions {
        data_dir: data_dir.path().to_path_buf(),
        cache_dir: data_dir.path().join("cache"),
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
        client_defaults: Some(alloyfs_agent::ClientDefaults {
            exclude: vec!["*.local".into()],
            pin: vec![],
            auto_cache_max: Some(alloyfs_common::SizeField::Bytes(1 << 20)),
            auto_cache_budget: None,
        }),
        ..AgentOpts::default()
    });
    let data_dir = tempfile::TempDir::new().unwrap();

    // Opted out: the suggestion is never even requested, so *.local is a
    // perfectly ordinary remote file.
    let opts = ClientOptions {
        data_dir: data_dir.path().to_path_buf(),
        cache_dir: data_dir.path().join("cache"),
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
        cache_dir: data_dir.path().join("cache"),
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

// --------------------------------------------------------------------- 24

/// Token-protected sessions serve NOTHING until Request::Auth presents the
/// right secret; wrong secrets are rejected without opening the gate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_token_gates_requests() {
    let agent = start_agent(AgentOpts::default());
    let conn = connect_raw_with_server_token(&agent, "sekrit").await;

    let attach = || Request::Attach {
        export: "test".into(),
    };
    assert_eq!(
        conn.request(attach()).await.expect("transport").unwrap_err(),
        ErrorCode::AuthRequired,
        "unauthenticated attach must be refused"
    );
    assert_eq!(
        conn.request(Request::Auth {
            token: "wrong".into()
        })
        .await
        .expect("transport")
        .unwrap_err(),
        ErrorCode::PermissionDenied,
        "bad token must be rejected"
    );
    assert_eq!(
        conn.request(attach()).await.expect("transport").unwrap_err(),
        ErrorCode::AuthRequired,
        "a failed auth must not open the gate"
    );
    conn.request(Request::Auth {
        token: "sekrit".into(),
    })
    .await
    .expect("transport")
    .expect("correct token accepted");
    conn.request(attach())
        .await
        .expect("transport")
        .expect("authenticated attach works");
}

// --------------------------------------------------------------------- 25

/// A blocking lock wait dies immediately when the connection does — the
/// no-fixed-deadline keepalive path must not orphan the wait.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocking_lock_fails_fast_on_sever() {
    let agent = start_agent(AgentOpts::default());
    std::fs::write(agent.dir.path().join("l.txt"), b"lockme").unwrap();
    let s1 = connect(&agent, ClientOptions::default()).await;
    let s2 = connect(&agent, ClientOptions::default()).await;

    let fh1 = open_for_lock(&s1, "l.txt").await;
    let fh2 = open_for_lock(&s2, "l.txt").await;
    on_fs(&s1.fs, move |fs| fs.lock(fh1, LockKind::Exclusive, false))
        .await
        .unwrap();

    // s2 parks on a blocking wait, then its own link dies under it.
    let waiter = {
        let fs = s2.fs.clone();
        tokio::task::spawn_blocking(move || fs.lock(fh2, LockKind::Exclusive, true))
    };
    tokio::time::sleep(Duration::from_millis(300)).await; // let the wait park
    let started = std::time::Instant::now();
    s2.sever();
    let err = waiter.await.unwrap().unwrap_err();
    assert!(
        matches!(err, alloyfs_client::FsError::Transport(TransportError::Closed)),
        "got {err:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "wait outlived its connection: {:?}",
        started.elapsed()
    );
}

// --------------------------------------------------------------------- 26

/// Uncontended: a held lock survives sever + reconnect — the supervisor
/// replays it on the new session and the handle stays clean.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lock_survives_reconnect_uncontended() {
    let agent = start_agent(AgentOpts::default());
    std::fs::write(agent.dir.path().join("l.txt"), b"lockme").unwrap();
    let s1 = connect_reconnectable(&agent, ClientOptions::default()).await;
    let s2 = connect(&agent, ClientOptions::default()).await;

    let fh1 = open_for_lock(&s1, "l.txt").await;
    let fh2 = open_for_lock(&s2, "l.txt").await;
    on_fs(&s1.fs, move |fs| fs.lock(fh1, LockKind::Exclusive, false))
        .await
        .unwrap();

    let old_conn = s1.fs.conn();
    s1.sever();
    wait_until("supervisor swapped in a live connection", 10, || {
        let now = s1.fs.conn();
        (!std::sync::Arc::ptr_eq(&old_conn, &now) && !now.is_closed()).then_some(())
    })
    .await;

    // Replay settled before the swap: the contender must still be excluded.
    let err = on_fs(&s2.fs, move |fs| fs.lock(fh2, LockKind::Exclusive, false))
        .await
        .unwrap_err();
    assert_eq!(
        remote_code(err),
        ErrorCode::WouldBlock,
        "lock must still be held by s1"
    );

    // And s1's handle is not poisoned: I/O keeps working.
    on_fs(&s1.fs, move |fs| fs.write(fh1, 0, b"still mine"))
        .await
        .expect("uncontended replay must not poison the handle");

    // Release; the contender can now take it.
    on_fs(&s1.fs, move |fs| fs.unlock(fh1)).await.unwrap();
    on_fs(&s2.fs, move |fs| fs.lock(fh2, LockKind::Exclusive, false))
        .await
        .expect("lock must be free after s1 unlocks");
}

// --------------------------------------------------------------------- 27

/// Contended: a waiter grabs the lock during the reconnect gap; the replay
/// loses and the handle is poisoned — I/O fails loudly instead of running
/// without the mutual exclusion the app thinks it has.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lock_lost_to_contender_poisons_handle() {
    let agent = start_agent(AgentOpts::default());
    std::fs::write(agent.dir.path().join("l.txt"), b"lockme").unwrap();
    let s1 = connect_reconnectable(&agent, ClientOptions::default()).await;
    let s2 = connect(&agent, ClientOptions::default()).await;

    let fh1 = open_for_lock(&s1, "l.txt").await;
    let fh2 = open_for_lock(&s2, "l.txt").await;
    on_fs(&s1.fs, move |fs| fs.lock(fh1, LockKind::Exclusive, false))
        .await
        .unwrap();

    // s2 queues a blocking wait, guaranteeing it wins the instant the
    // server releases s1's dead session (FIFO waiters wake first).
    let contender = {
        let fs = s2.fs.clone();
        tokio::task::spawn_blocking(move || fs.lock(fh2, LockKind::Exclusive, true))
    };
    tokio::time::sleep(Duration::from_millis(300)).await; // park the waiter

    let old_conn = s1.fs.conn();
    s1.sever();
    contender.await.unwrap().expect("waiter must win the freed lock");
    wait_until("supervisor swapped in a live connection", 10, || {
        let now = s1.fs.conn();
        (!std::sync::Arc::ptr_eq(&old_conn, &now) && !now.is_closed()).then_some(())
    })
    .await;

    // s1's replay lost: the handle must be poisoned.
    let err = on_fs(&s1.fs, move |fs| fs.write(fh1, 0, b"not mine anymore"))
        .await
        .unwrap_err();
    assert_eq!(remote_code(err), ErrorCode::Io, "poisoned handle must fail EIO");
    let err = on_fs(&s1.fs, move |fs| fs.lock(fh1, LockKind::Exclusive, false))
        .await
        .unwrap_err();
    assert_eq!(
        remote_code(err),
        ErrorCode::Io,
        "poisoned handle must fail lock too"
    );
}

// --------------------------------------------------------------------- 28

/// replace:true rename onto an existing file is atomic on both platforms
/// (std::fs::rename uses POSIX semantics on Windows since ~1.78).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rename_replace_atomic() {
    let agent = start_agent(AgentOpts::default());
    let s = connect(&agent, ClientOptions::default()).await;

    mkfile(&s.fs, ROOT_INO, "src.txt", b"the new content").await;
    mkfile(&s.fs, ROOT_INO, "dst.txt", b"about to be replaced").await;

    on_fs(&s.fs, |fs| {
        fs.rename(ROOT_INO, "src.txt", ROOT_INO, "dst.txt", true)
    })
    .await
    .expect("replace:true rename must succeed onto an existing target");
    assert_eq!(
        std::fs::read(agent.dir.path().join("dst.txt")).unwrap(),
        b"the new content"
    );
    assert!(!agent.dir.path().join("src.txt").exists());
}

// --------------------------------------------------------------------- 29

/// A session is bound to ONE export: attaching again to the same name is an
/// idempotent no-op, attaching to a different name is refused instead of
/// silently serving the wrong export.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_attach_refused() {
    let agent = start_agent(AgentOpts::default());
    let s = connect(&agent, ClientOptions::default()).await;

    let same = s
        .conn()
        .request(Request::Attach {
            export: "test".into(),
        })
        .await
        .expect("transport");
    assert!(
        matches!(same, Ok(Response::AttachOk { .. })),
        "same-export re-attach must stay idempotent: {same:?}"
    );

    let other = s
        .conn()
        .request(Request::Attach {
            export: "other".into(),
        })
        .await
        .expect("transport");
    assert_eq!(
        other.unwrap_err(),
        ErrorCode::AlreadyExists,
        "different-export attach must be refused"
    );
}

// --------------------------------------------------------------------- 30

/// WinFsp dispatcher threads and cache-manager read-ahead deliver
/// "sequential" streams slightly out of order and overlapping. The window
/// must tolerate that (serving correct bytes without collapsing); this
/// pattern used to clear the whole window on every deviation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readahead_tolerates_out_of_order_reads() {
    let agent = start_agent(AgentOpts::default());
    let chunk = 128 * 1024usize;
    let data = patterned(12 * chunk);
    std::fs::write(agent.dir.path().join("ooo.bin"), &data).unwrap();
    let s = connect(&agent, ClientOptions::default()).await;

    let (ino, _) = lookup_path(&s.fs, "ooo.bin").await.unwrap();
    let got = on_fs(&s.fs, move |fs| {
        let (fh, _) = fs.open(ino, ro())?;
        // Overlapped dispatch: mostly forward, occasionally a block early or
        // a re-read of the previous one — the shape WinFsp actually sends.
        let order = [0usize, 1, 3, 2, 4, 5, 5, 7, 6, 8, 9, 10, 11];
        let mut out = vec![0u8; 12 * chunk];
        for &b in &order {
            let piece = fs.read(fh, (b * chunk) as u64, chunk as u32)?;
            out[b * chunk..b * chunk + piece.len()].copy_from_slice(&piece);
        }
        fs.release(fh);
        Ok::<_, alloyfs_client::FsError>(out)
    })
    .await
    .unwrap();
    assert!(
        got == data,
        "out-of-order reads must still assemble the exact file"
    );
}

// --------------------------------------------------------------------- 31

/// Sub-chunk kernel reads (64 KiB paging I/O) hit each 128 KiB block twice;
/// the second half must come from the retained block, and above all must be
/// the RIGHT bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readahead_subchunk_strides() {
    let agent = start_agent(AgentOpts::default());
    let data = patterned(1024 * 1024);
    std::fs::write(agent.dir.path().join("strides.bin"), &data).unwrap();
    let s = connect(&agent, ClientOptions::default()).await;

    let (ino, _) = lookup_path(&s.fs, "strides.bin").await.unwrap();
    let got = on_fs(&s.fs, move |fs| {
        let (fh, _) = fs.open(ino, ro())?;
        let stride = 64 * 1024usize;
        let mut out = Vec::with_capacity(1024 * 1024);
        for i in 0..(1024 * 1024 / stride) {
            out.extend_from_slice(&fs.read(fh, (i * stride) as u64, stride as u32)?);
        }
        fs.release(fh);
        Ok::<_, alloyfs_client::FsError>(out)
    })
    .await
    .unwrap();
    assert!(got == data, "64K strides must reassemble the exact file");
}

// --------------------------------------------------------------------- 32

/// A write invalidates retained blocks too: a sub-chunk re-read after a
/// write must see the new bytes, never a stale retained copy.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retained_blocks_invalidated_by_write() {
    let agent = start_agent(AgentOpts::default());
    let data = patterned(256 * 1024);
    std::fs::write(agent.dir.path().join("w.bin"), &data).unwrap();
    let s = connect(&agent, ClientOptions::default()).await;

    let (ino, _) = lookup_path(&s.fs, "w.bin").await.unwrap();
    on_fs(&s.fs, move |fs| {
        let (fh, _) = fs.open(ino, rw())?;
        let first = fs.read(fh, 0, 64 * 1024)?; // retains block 0
        assert_eq!(first[0], patterned(1)[0]);
        fs.write(fh, 0, b"XXXX")?; // clears the handle's window + retention
        let again = fs.read(fh, 0, 4)?;
        assert_eq!(
            &again, b"XXXX",
            "read-after-write must not serve a retained stale block"
        );
        fs.release(fh);
        Ok::<_, alloyfs_client::FsError>(())
    })
    .await
    .unwrap();
}

// --------------------------------------------------------------------- 21

/// `--detect-conflicts` end to end, through RemoteFs rather than raw frames.
///
/// Two mounts of one export. The second writes; the first then tries to save
/// over it and is stopped instead of silently winning.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn detect_conflicts_refuses_to_clobber() {
    let agent = start_agent(AgentOpts::default());
    let careful = connect(
        &agent,
        ClientOptions {
            detect_conflicts: true,
            ..ClientOptions::default()
        },
    )
    .await;
    let other = connect(&agent, ClientOptions::default()).await;

    let fh1 = on_fs(&careful.fs, |fs| {
        fs.create(ROOT_INO, "shared.txt", 0o644, rw()).unwrap().1
    })
    .await;

    // Someone else changes the file behind this handle's back.
    let (ino, _) = lookup_path(&other.fs, "shared.txt").await.unwrap();
    let fh2 = on_fs(&other.fs, move |fs| fs.open(ino, rw()).unwrap().0).await;
    on_fs(&other.fs, move |fs| fs.write(fh2, 0, b"theirs"))
        .await
        .unwrap();

    let err = on_fs(&careful.fs, move |fs| fs.write(fh1, 0, b"mine"))
        .await
        .expect_err("the write must be refused");
    assert!(
        matches!(err, alloyfs_client::FsError::Remote(ErrorCode::Conflict)),
        "expected a conflict, got {err:?}"
    );
    assert_eq!(
        std::fs::read(agent.dir.path().join("shared.txt")).unwrap(),
        b"theirs",
        "the refused write must not have touched the file"
    );
}

/// The same race, with the flag off: the write wins silently. This is the
/// default, and it must stay the default — turning it on for everyone would
/// make ordinary editors start failing saves on a shared mount.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_the_flag_the_last_writer_still_wins() {
    let agent = start_agent(AgentOpts::default());
    let s1 = connect(&agent, ClientOptions::default()).await;
    let s2 = connect(&agent, ClientOptions::default()).await;

    let fh1 = on_fs(&s1.fs, |fs| {
        fs.create(ROOT_INO, "shared.txt", 0o644, rw()).unwrap().1
    })
    .await;
    let (ino, _) = lookup_path(&s2.fs, "shared.txt").await.unwrap();
    let fh2 = on_fs(&s2.fs, move |fs| fs.open(ino, rw()).unwrap().0).await;
    on_fs(&s2.fs, move |fs| fs.write(fh2, 0, b"theirs"))
        .await
        .unwrap();

    on_fs(&s1.fs, move |fs| fs.write(fh1, 0, b"mine!!"))
        .await
        .expect("an unpinned write still wins");
    assert_eq!(
        std::fs::read(agent.dir.path().join("shared.txt")).unwrap(),
        b"mine!!"
    );
}

/// A write big enough to be chunked must not conflict with itself. Each chunk
/// bumps the server's version, so sending the same expectation for every chunk
/// would fail on the second one — the expectation has to advance.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_chunked_write_does_not_conflict_with_itself() {
    let agent = start_agent(AgentOpts::default());
    let s = connect(
        &agent,
        ClientOptions {
            detect_conflicts: true,
            ..ClientOptions::default()
        },
    )
    .await;

    let fh = on_fs(&s.fs, |fs| fs.create(ROOT_INO, "big.bin", 0o644, rw()).unwrap().1).await;

    // Several DATA_CHUNKs worth, so the client's write loop runs more than once.
    let payload = vec![b'z'; 3 * alloyfs_proto::DATA_CHUNK as usize + 17];
    let expected = payload.len();
    let wrote = on_fs(&s.fs, move |fs| fs.write(fh, 0, &payload))
        .await
        .expect("a chunked write must not conflict with itself");
    assert_eq!(wrote as usize, expected);
    assert_eq!(
        std::fs::metadata(agent.dir.path().join("big.bin")).unwrap().len() as usize,
        expected
    );
}

// --------------------------------------------------------------------- 22

/// Symlinks, end to end: create through the client, read the target back,
/// and see it as a Symlink in a listing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn symlink_create_and_read() {
    let agent = start_agent(AgentOpts::default());
    let s = connect(&agent, ClientOptions::default()).await;
    mkfile(&s.fs, ROOT_INO, "real.txt", b"contents").await;

    let (ino, attr) = on_fs(&s.fs, |fs| fs.symlink(ROOT_INO, "link.txt", "real.txt"))
        .await
        .expect("symlink");
    assert_eq!(attr.kind, FileKind::Symlink, "a symlink must report as one");

    let target = on_fs(&s.fs, move |fs| fs.readlink(ino)).await.expect("readlink");
    assert_eq!(target, "real.txt");

    // And it is a real symlink on the server, not a copy.
    let md = std::fs::symlink_metadata(agent.dir.path().join("link.txt")).unwrap();
    assert!(md.file_type().is_symlink());
}

/// A relative target pointing within the export is fine, including one that
/// walks up out of a subdirectory and back down.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_relative_target_inside_the_export_is_allowed() {
    let agent = start_agent(AgentOpts::default());
    let s = connect(&agent, ClientOptions::default()).await;
    mkfile(&s.fs, ROOT_INO, "real.txt", b"contents").await;
    let (sub, _) = on_fs(&s.fs, |fs| fs.mkdir(ROOT_INO, "sub", 0o755)).await.unwrap();

    let (ino, _) = on_fs(&s.fs, move |fs| fs.symlink(sub, "up.txt", "../real.txt"))
        .await
        .expect("a target inside the export must be allowed");
    assert_eq!(
        on_fs(&s.fs, move |fs| fs.readlink(ino)).await.unwrap(),
        "../real.txt",
        "the target is stored verbatim, not rewritten"
    );
}

/// The export boundary. A symlink is the cheapest way to turn a read of an
/// exported directory into a read of the whole server, so a target that
/// escapes must be refused at creation — including a dangling one, since the
/// day that path appears the link becomes a hole.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_target_escaping_the_export_is_refused() {
    let agent = start_agent(AgentOpts::default());
    let s = connect(&agent, ClientOptions::default()).await;
    let (sub, _) = on_fs(&s.fs, |fs| fs.mkdir(ROOT_INO, "sub", 0o755)).await.unwrap();

    for (name, target) in [
        ("a", ".."),
        ("b", "../../etc/passwd"),
        ("c", "/etc/passwd"),
        ("d", "sub/../../outside"),
        ("e", "\\windows\\system32"),
        ("f", "C:\\Windows"),
    ] {
        let t = target.to_string();
        let err = on_fs(&s.fs, move |fs| fs.symlink(ROOT_INO, name, &t))
            .await
            .unwrap_err();
        assert_eq!(
            remote_code(err),
            ErrorCode::PermissionDenied,
            "target {target:?} was not refused"
        );
        assert!(
            !agent.dir.path().join(name).exists(),
            "target {target:?} was refused but a link was still created"
        );
    }

    // One level down, ".." is still inside — the check is about where it
    // lands, not how many times it walks up.
    on_fs(&s.fs, move |fs| fs.symlink(sub, "ok", ".."))
        .await
        .expect("landing on the export root is inside it");
}

/// A symlink into an excluded path would otherwise be a way to read exactly
/// what the export config says is invisible.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_target_inside_an_excluded_path_is_refused() {
    let agent = start_agent(AgentOpts {
        excludes: vec!["secrets/**".into()],
        ..AgentOpts::default()
    });
    std::fs::create_dir(agent.dir.path().join("secrets")).unwrap();
    std::fs::write(agent.dir.path().join("secrets/key.txt"), b"shh").unwrap();
    let s = connect(&agent, ClientOptions::default()).await;

    let err = on_fs(&s.fs, |fs| fs.symlink(ROOT_INO, "peek", "secrets/key.txt"))
        .await
        .unwrap_err();
    assert_eq!(remote_code(err), ErrorCode::PermissionDenied);
}

/// readlink on something that is not a link must say so rather than
/// inventing a target.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readlink_on_a_regular_file_is_refused() {
    let agent = start_agent(AgentOpts::default());
    let s = connect(&agent, ClientOptions::default()).await;
    let ino = mkfile(&s.fs, ROOT_INO, "real.txt", b"contents").await;

    let err = on_fs(&s.fs, move |fs| fs.readlink(ino)).await.unwrap_err();
    assert_eq!(remote_code(err), ErrorCode::InvalidPath);
}

// --------------------------------------------------------------------- 33

/// A write's reply carries the file's new attributes (protocol v5), so the
/// stat every backend does straight after a write is served from memory
/// instead of costing a second round-trip.
///
/// Proved by severing the link before asking: `getattr` consults the cache
/// before it touches the connection, so getting an answer at all means it
/// never went to the wire — and that answer has to be the POST-write one.
/// Both older behaviours fail here: dropping the entry leaves nothing to
/// answer from, and keeping it unchanged answers with the pre-write size.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_write_reply_refreshes_the_attr_cache_instead_of_costing_a_getattr() {
    let agent = start_agent(AgentOpts::default());
    let s = connect(&agent, ClientOptions::default()).await;
    assert_eq!(
        s.conn().proto,
        PROTO_VERSION_MAX,
        "two peers from one build negotiate the newest version"
    );

    let (ino, fh) = on_fs(&s.fs, |fs| {
        let (ino, fh, attr) = fs.create(ROOT_INO, "w.txt", 0o644, rw()).expect("create");
        assert_eq!(attr.size, 0, "the cached attr starts at the empty file");
        (ino, fh)
    })
    .await;

    let (n, written) = on_fs(&s.fs, move |fs| fs.write_at(fh, 0, b"golden"))
        .await
        .expect("write");
    assert_eq!(n, 6);
    let written = written.expect("a v5 peer must carry the attributes on the write reply");
    assert_eq!(written.size, 6);

    s.sever();
    let cached = on_fs(&s.fs, move |fs| fs.getattr(ino))
        .await
        .expect("the post-write attr must be cached, with no server left to ask");
    assert_eq!(cached.size, 6, "the cached attr must be the post-write one");
    assert_eq!(cached.version, written.version);
}

// ------------------------------------------------------- v6: the tree index

/// Ask for the whole tree, following pages, and return (paths, token).
async fn fetch_tree(s: &Session, root: &str) -> (Vec<String>, u64) {
    let conn = s.conn();
    let mut paths = Vec::new();
    let mut cursor = None;
    let mut token;
    loop {
        let resp = conn
            .request(Request::Tree {
                path: RelPath(root.into()),
                cursor,
            })
            .await
            .expect("transport")
            .expect("tree");
        let Response::Tree {
            entries,
            next_cursor,
            token: t,
        } = resp
        else {
            panic!("expected a Tree reply, got {resp:?}");
        };
        token = t;
        paths.extend(entries.into_iter().map(|e| e.path.0));
        match next_cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    (paths, token)
}

async fn tree_token(s: &Session) -> u64 {
    let conn = s.conn();
    match conn
        .request(Request::TreeToken)
        .await
        .expect("transport")
        .expect("tree token")
    {
        Response::TreeToken { token } => token,
        other => panic!("expected TreeToken, got {other:?}"),
    }
}

/// The whole point of the index: one exchange returns what a directory-by-
/// directory walk would need one round trip per directory to discover.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_tree_returns_a_whole_subtree_at_once() {
    let agent = start_agent(AgentOpts::default());
    let root = agent.dir.path();
    std::fs::create_dir_all(root.join("a/b/c")).unwrap();
    std::fs::write(root.join("top.txt"), b"t").unwrap();
    std::fs::write(root.join("a/one.txt"), b"1").unwrap();
    std::fs::write(root.join("a/b/two.txt"), b"22").unwrap();
    std::fs::write(root.join("a/b/c/three.txt"), b"333").unwrap();

    let s = connect(&agent, ClientOptions::default()).await;
    let (mut paths, token) = fetch_tree(&s, "").await;
    paths.sort();
    assert_eq!(
        paths,
        [
            "a",
            "a/b",
            "a/b/c",
            "a/b/c/three.txt",
            "a/b/two.txt",
            "a/one.txt",
            "top.txt"
        ],
        "four directories deep, one exchange"
    );
    assert_ne!(token, 0, "an indexed export reports a non-zero token");

    // A subtree query is scoped, and does not re-list its ancestors.
    let (sub, _) = fetch_tree(&s, "a/b").await;
    let mut sub = sub;
    sub.sort();
    assert_eq!(sub, ["a/b/c", "a/b/c/three.txt", "a/b/two.txt"]);
}

/// The token is what a client compares against a cache it kept from a previous
/// mount, so it must be stable while nothing changes and move when something
/// does — including through a path the client never asks about.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_token_tracks_the_export() {
    let agent = start_agent(AgentOpts {
        // The second half of this test changes a file WITHOUT going through
        // alloyfs, so the watcher has to be the thing that notices.
        watch: true,
        ..AgentOpts::default()
    });
    std::fs::write(agent.dir.path().join("one.txt"), b"1").unwrap();

    let s = connect(&agent, ClientOptions::default()).await;
    let first = tree_token(&s).await;
    assert_ne!(first, 0);
    assert_eq!(first, tree_token(&s).await, "stable while nothing changes");

    // Polled rather than read once: the watcher path is asynchronous, and the
    // harness's `wait_until` takes a synchronous probe while asking for a token
    // means a request.
    async fn token_changing_from(s: &Session, old: u64, what: &str) -> u64 {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let t = tree_token(s).await;
            if t != old {
                return t;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "{what} did not move the token"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    // A write THROUGH the mount moves it.
    mkfile(&s.fs, ROOT_INO, "two.txt", b"22").await;
    let after_create = token_changing_from(&s, first, "creating a file").await;

    // And so does one made behind alloyfs's back, since the watcher feeds the
    // same index.
    std::fs::write(agent.dir.path().join("three.txt"), b"333").unwrap();
    token_changing_from(&s, after_create, "a change made directly on the server").await;
}

/// An export past the cap is not an error: the client is told so with token 0
/// and carries on with `Readdir`. Reporting a failure would push a decision
/// onto every caller for something that is only ever an optimisation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unindexed_export_reports_zero_rather_than_failing() {
    let agent = start_agent(AgentOpts {
        tree_max_entries: Some(1),
        ..AgentOpts::default()
    });
    std::fs::write(agent.dir.path().join("a.txt"), b"a").unwrap();
    std::fs::write(agent.dir.path().join("b.txt"), b"b").unwrap();
    std::fs::write(agent.dir.path().join("c.txt"), b"c").unwrap();

    let s = connect(&agent, ClientOptions::default()).await;
    assert_eq!(tree_token(&s).await, 0, "over the cap reads as unindexed");
    let (paths, token) = fetch_tree(&s, "").await;
    assert_eq!(token, 0);
    assert!(paths.is_empty(), "no entries, but no error either");

    // Readdir still works, which is the entire point of the fallback.
    let entries = on_fs(&s.fs, |fs| fs.readdir(ROOT_INO)).await.unwrap();
    assert_eq!(entries.len(), 3);
}

// ------------------------------------------------- two clients, one export

/// Two mounts of the same export must see each other. This is the property the
/// whole origin/echo mechanism exists to provide, and until now nothing pinned
/// it: `events_end_to_end` uses one client and edits the disk directly, and
/// every other two-client test is about locking.
///
/// The failure it guards against is silent — a second client would simply go
/// on serving stale attributes, with nothing logged and nothing erroring.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_clients_write_reaches_another() {
    let agent = start_agent(AgentOpts {
        watch: true,
        ..AgentOpts::default()
    });
    std::fs::write(agent.dir.path().join("shared.txt"), b"first").unwrap();

    let a = connect(&agent, ClientOptions::default()).await;
    let b = connect(&agent, ClientOptions::default()).await;

    // B runs the event pump, which is what a real mount does and what applies
    // events to the caches. A bare `subscribe` only opens a receiver — it
    // delivers events to the test while leaving B's caches untouched, which
    // would leave the TTL to do the work and prove nothing.
    let (b_tx, mut b_events) = tokio::sync::mpsc::unbounded_channel::<Vec<FsEvent>>();
    b.fs.start_event_pump(move |batch| {
        let _ = b_tx.send(batch.to_vec());
    })
    .await
    .unwrap();

    // B learns the file the ordinary way, which also caches its attributes —
    // so a stale answer later is a real possibility rather than a hypothetical.
    let (b_ino, before) = lookup_path(&b.fs, "shared.txt").await.unwrap();
    assert_eq!(before.size, 5);

    // A writes through its own mount.
    let (a_ino, _) = lookup_path(&a.fs, "shared.txt").await.unwrap();
    let fh = on_fs(&a.fs, move |fs| fs.open(a_ino, rw()).unwrap().0).await;
    on_fs(&a.fs, move |fs| fs.write(fh, 0, b"second-and-longer").unwrap()).await;
    on_fs(&a.fs, move |fs| fs.release(fh)).await;

    // B is told.
    let mut captured: Vec<FsEvent> = Vec::new();
    assert!(
        recv_until(&mut b_events, &mut captured, 10, |evs| evs
            .iter()
            .any(|e| e.path.0 == "shared.txt"))
        .await,
        "B never received an event for A's write; saw {captured:?}"
    );

    // And the event is not merely delivered — B's cached attributes give way
    // to it. Polled rather than read once, because the invalidation travels
    // through the watcher and the coalescer's debounce.
    //
    // The deadline is deliberately shorter than ATTR_TTL (5 s). Allowing
    // longer would have made this pass whether or not the event did anything:
    // the cached attribute expires on its own, and a re-fetch would report the
    // new size for reasons that have nothing to do with propagation. Timing it
    // out below the TTL is what makes a pass mean "the event invalidated it".
    let started = std::time::Instant::now();
    loop {
        let attr = on_fs(&b.fs, move |fs| fs.getattr(b_ino)).await.unwrap();
        if attr.size == 17 {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "B still reports size {} after A wrote 17 bytes; at 5 s the TTL \
             would have refreshed it anyway and this test would prove nothing",
            attr.size
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the update arrived only after ATTR_TTL could have supplied it"
    );

    // The bytes themselves, not just the size.
    assert_eq!(read_all(&b.fs, b_ino).await, b"second-and-longer");
}

/// The other half of the same mechanism: a client is NOT told about its own
/// writes. It already applied them synchronously, so an echo would be a
/// redundant invalidation that throws away a cache entry known to be correct.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_is_not_told_about_its_own_write() {
    let agent = start_agent(AgentOpts {
        watch: true,
        ..AgentOpts::default()
    });
    let a = connect(&agent, ClientOptions::default()).await;
    let b = connect(&agent, ClientOptions::default()).await;
    let mut a_events = subscribe(&a).await;
    let mut b_events = subscribe(&b).await;

    mkfile(&a.fs, ROOT_INO, "mine.txt", b"x").await;

    // B hears about it...
    expect_event(&mut b_events, 10, |e| e.path.0 == "mine.txt").await;

    // ...and by the time it has, A has had every chance to and did not. Using
    // B's delivery as the barrier is what makes this a real assertion rather
    // than a race with a short sleep.
    let mut echoed = Vec::new();
    while let Ok(batch) = a_events.try_recv() {
        echoed.extend(batch.into_iter().filter(|e| e.path.0 == "mine.txt"));
    }
    assert!(
        echoed.is_empty(),
        "a client must not be echoed its own write, got {echoed:?}"
    );
}

// ---------------------------------------------------- the directory cache

/// A repeat listing inside DIR_TTL is answered locally. Proven by severing the
/// connection between the two calls: the second can only succeed from cache.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_repeat_readdir_is_answered_locally() {
    let agent = start_agent(AgentOpts::default());
    std::fs::write(agent.dir.path().join("a.txt"), b"a").unwrap();
    std::fs::write(agent.dir.path().join("b.txt"), b"bb").unwrap();

    let s = connect(&agent, ClientOptions::default()).await;
    let first = on_fs(&s.fs, |fs| fs.readdir(ROOT_INO)).await.unwrap();
    assert_eq!(first.len(), 2);

    s.sever();
    let second = on_fs(&s.fs, |fs| fs.readdir(ROOT_INO))
        .await
        .expect("a live listing must not need the server");
    let names: Vec<&str> = second.iter().map(|(n, _, _)| n.as_str()).collect();
    assert_eq!(names.len(), 2);
    assert!(names.contains(&"a.txt") && names.contains(&"b.txt"));
}

/// The listing is COMPLETE, so it answers both directions without the server:
/// a name it holds resolves, a name it lacks is NotFound — not a round trip,
/// and after the sever, not an error either.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_live_listing_answers_lookups_both_ways() {
    let agent = start_agent(AgentOpts::default());
    std::fs::write(agent.dir.path().join("real.txt"), b"here").unwrap();

    let s = connect(&agent, ClientOptions::default()).await;
    on_fs(&s.fs, |fs| fs.readdir(ROOT_INO)).await.unwrap();
    s.sever();

    let (_, attr) = on_fs(&s.fs, |fs| fs.lookup(ROOT_INO, "real.txt"))
        .await
        .expect("a listed name resolves from the listing");
    assert_eq!(attr.size, 4);

    // This is the desktop.ini / resolver-probe case: a miss must be a local
    // NotFound, where it used to be a 60 ms round trip per probe, forever.
    let err = on_fs(&s.fs, |fs| fs.lookup(ROOT_INO, "desktop.ini.missing"))
        .await
        .expect_err("an unlisted name is refused locally");
    assert_eq!(remote_code(err), ErrorCode::NotFound);
}

/// Our own mutations bust the listing synchronously — the server strips
/// self-origin events, so nothing else would.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_local_create_invalidates_the_listing() {
    let agent = start_agent(AgentOpts::default());
    std::fs::write(agent.dir.path().join("old.txt"), b"o").unwrap();

    let s = connect(&agent, ClientOptions::default()).await;
    let before = on_fs(&s.fs, |fs| fs.readdir(ROOT_INO)).await.unwrap();
    assert_eq!(before.len(), 1);

    mkfile(&s.fs, ROOT_INO, "new.txt", b"n").await;
    let after = on_fs(&s.fs, |fs| fs.readdir(ROOT_INO)).await.unwrap();
    let names: Vec<&str> = after.iter().map(|(n, _, _)| n.as_str()).collect();
    assert!(
        names.contains(&"new.txt"),
        "the listing must show our own create immediately, got {names:?}"
    );
}

/// Another client's change arrives through the pump and busts the listing —
/// within a deadline well under DIR_TTL, so a pass means the event did it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_remote_change_invalidates_the_listing() {
    let agent = start_agent(AgentOpts {
        watch: true,
        ..AgentOpts::default()
    });
    std::fs::write(agent.dir.path().join("seed.txt"), b"s").unwrap();

    let s = connect(&agent, ClientOptions::default()).await;
    s.fs.start_event_pump(|_| {}).await.unwrap();
    let before = on_fs(&s.fs, |fs| fs.readdir(ROOT_INO)).await.unwrap();
    assert_eq!(before.len(), 1);

    std::fs::write(agent.dir.path().join("from-elsewhere.txt"), b"x").unwrap();
    let started = std::time::Instant::now();
    loop {
        let now = on_fs(&s.fs, |fs| fs.readdir(ROOT_INO)).await.unwrap();
        if now.iter().any(|(n, _, _)| n == "from-elsewhere.txt") {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "still serving the stale listing; at DIR_TTL(5s) it would refresh anyway and prove nothing"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
