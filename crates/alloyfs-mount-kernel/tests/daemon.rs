//! The daemon against a real server, with the kernel played by this file.
//!
//! A real `AgentSession` over a `tokio::io::duplex` pipe, a real `RemoteFs` on
//! the other end, and hand-built request frames byte-identical to what
//! `alloyfs_dev_read` hands userspace. Nothing here needs Linux, the module,
//! or root — so the whole opcode surface is covered by `cargo test` on any
//! host, and only the device plumbing is left for the in-guest stage 6 run.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use alloyfs_agent::{AgentConfig, AgentSession, ExportConfig, ExportRegistry};
use alloyfs_client::{ClientOptions, RemoteFs};
use alloyfs_mount_kernel::abi::{self, InHeader};
use alloyfs_mount_kernel::notify::Frames;
use alloyfs_mount_kernel::Server;
use alloyfs_proto::{EventKind, FsEvent, RelPath};
use alloyfs_transport::{serve_connection, MuxConnection, RequestHandler};

// ------------------------------------------------------------------ harness

struct Fixture {
    /// The export's backing directory: tests assert against it directly, and
    /// dropping it removes the tree.
    dir: tempfile::TempDir,
    server: Arc<Server>,
}

async fn fixture() -> Fixture {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let mut cfg = AgentConfig::default();
    cfg.exports.insert(
        "test".to_string(),
        ExportConfig {
            path: dir.path().to_path_buf(),
            read_only: false,
            exclude: Vec::new(),
            client: None,
        },
    );
    let registry = Arc::new(ExportRegistry::from_config(&cfg).expect("registry"));
    let (client_io, server_io) = tokio::io::duplex(1024 * 1024);
    let handler: Arc<dyn RequestHandler> = Arc::new(AgentSession::new(registry));
    tokio::spawn(async move {
        let _ = serve_connection(server_io, "test-agent", handler).await;
    });
    let conn = MuxConnection::establish(client_io, "test-client")
        .await
        .expect("handshake");
    let fs = RemoteFs::attach_with(conn, "test", ClientOptions::default())
        .await
        .expect("attach");
    Fixture {
        dir,
        server: Arc::new(Server::new(fs)),
    }
}

/// Build a request exactly as the kernel writes it into the daemon's buffer.
fn request(opcode: u32, unique: u64, nodeid: u64, offset: u64, size: u32, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&((abi::IN_HEADER_LEN + payload.len()) as u32).to_ne_bytes());
    v.extend_from_slice(&opcode.to_ne_bytes());
    v.extend_from_slice(&unique.to_ne_bytes());
    v.extend_from_slice(&nodeid.to_ne_bytes());
    v.extend_from_slice(&offset.to_ne_bytes());
    v.extend_from_slice(&size.to_ne_bytes());
    v.extend_from_slice(&0u32.to_ne_bytes()); // _pad
    v.extend_from_slice(payload);
    // Sanity: our own encoder must agree with the crate's decoder.
    let h = InHeader::parse(&v).expect("well-formed request");
    assert_eq!(h.unique, unique);
    v
}

struct Reply {
    error: i32,
    payload: Vec<u8>,
}

impl Reply {
    fn attr(&self) -> Attr {
        assert_eq!(self.error, 0, "expected success");
        assert_eq!(self.payload.len(), abi::ATTR_LEN);
        let p = &self.payload;
        Attr {
            nodeid: u64_at(p, 0),
            size: u64_at(p, 8),
            mtime_ns: u64_at(p, 16),
            mode: u32_at(p, 24),
            nlink: u32_at(p, 28),
        }
    }
}

#[derive(Debug)]
struct Attr {
    nodeid: u64,
    size: u64,
    mtime_ns: u64,
    mode: u32,
    nlink: u32,
}

fn u32_at(b: &[u8], off: usize) -> u32 {
    u32::from_ne_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn u64_at(b: &[u8], off: usize) -> u64 {
    let mut v = [0u8; 8];
    v.copy_from_slice(&b[off..off + 8]);
    u64::from_ne_bytes(v)
}

/// Serve one request off the async runtime (the server blocks on RPCs) and
/// decode the response frame the way `alloyfs_dev_write` would.
async fn call(fx: &Fixture, req: Vec<u8>) -> Reply {
    let server = fx.server.clone();
    let unique = InHeader::parse(&req).unwrap().unique;
    let frame = tokio::task::spawn_blocking(move || server.handle(&req))
        .await
        .expect("serve")
        .expect("a well-formed request always produces a frame");
    assert_eq!(
        frame.len(),
        u32_at(&frame, 0) as usize,
        "header length must cover the frame"
    );
    assert!(frame.len() >= abi::OUT_HEADER_LEN);
    assert_eq!(u64_at(&frame, 8), unique, "the reply must echo unique");
    Reply {
        error: i32::from_ne_bytes([frame[4], frame[5], frame[6], frame[7]]),
        payload: frame[abi::OUT_HEADER_LEN..].to_vec(),
    }
}

/// Decode a READDIR payload into (name, nodeid, next-cursor, type).
fn dirents(payload: &[u8]) -> Vec<(String, u64, u64, u32)> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + abi::DIRENT_LEN <= payload.len() {
        let nodeid = u64_at(payload, pos);
        let off = u64_at(payload, pos + 8);
        let namelen = u32_at(payload, pos + 16) as usize;
        let ty = u32_at(payload, pos + 20);
        let entry = abi::dirent_size(namelen);
        assert!(pos + entry <= payload.len(), "entry runs past the payload");
        let name =
            String::from_utf8(payload[pos + abi::DIRENT_LEN..pos + abi::DIRENT_LEN + namelen].to_vec())
                .expect("utf-8 name");
        out.push((name, nodeid, off, ty));
        pos += entry;
    }
    assert_eq!(pos, payload.len(), "payload must be a whole number of entries");
    out
}

fn create_payload(mode: u32, name: &str) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&mode.to_ne_bytes());
    p.extend_from_slice(&0u32.to_ne_bytes());
    p.extend_from_slice(name.as_bytes());
    p
}

// ------------------------------------------------------------------- reads

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn root_getattr_describes_a_directory() {
    let fx = fixture().await;
    // This is the request mount(2) itself makes: fill_super refuses a root
    // that is not a directory.
    let a = call(&fx, request(abi::OP_GETATTR, 1, abi::ROOT_NODEID, 0, 0, b""))
        .await
        .attr();
    assert_eq!(a.nodeid, abi::ROOT_NODEID);
    assert_eq!(a.mode & abi::S_IFMT, abi::S_IFDIR);
    assert_eq!(a.nlink, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lookup_hits_and_misses() {
    let fx = fixture().await;
    std::fs::write(fx.dir.path().join("one.txt"), b"first file").unwrap();

    let a = call(
        &fx,
        request(abi::OP_LOOKUP, 2, abi::ROOT_NODEID, 0, 0, b"one.txt"),
    )
    .await
    .attr();
    assert_eq!(a.size, 10);
    assert!(a.mtime_ns > 0, "the kernel stats out of this field");
    assert_eq!(a.mode & abi::S_IFMT, abi::S_IFREG);
    assert!(a.nodeid > abi::ROOT_NODEID, "children get their own nodeids");

    let miss = call(
        &fx,
        request(abi::OP_LOOKUP, 3, abi::ROOT_NODEID, 0, 0, b"nope.txt"),
    )
    .await;
    assert_eq!(miss.error, -abi::ENOENT);
    assert!(miss.payload.is_empty());

    // A nodeid the daemon never handed out.
    let bogus = call(&fx, request(abi::OP_GETATTR, 4, 999_999, 0, 0, b"")).await;
    assert_eq!(bogus.error, -abi::ENOENT);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn names_the_kernel_should_never_send_are_refused() {
    let fx = fixture().await;
    for bad in [&b"a/b"[..], b".", b"..", b""] {
        let r = call(&fx, request(abi::OP_LOOKUP, 5, abi::ROOT_NODEID, 0, 0, bad)).await;
        assert_eq!(r.error, -abi::EINVAL, "name {bad:?} must be rejected");
    }
    let long = vec![b'x'; abi::MAX_NAME + 1];
    let r = call(&fx, request(abi::OP_LOOKUP, 6, abi::ROOT_NODEID, 0, 0, &long)).await;
    assert_eq!(r.error, -abi::ENAMETOOLONG);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_opcode_is_enosys() {
    let fx = fixture().await;
    let r = call(&fx, request(4242, 7, abi::ROOT_NODEID, 0, 0, b"")).await;
    assert_eq!(r.error, -abi::ENOSYS);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readdir_pages_with_a_resumable_cursor() {
    let fx = fixture().await;
    for name in ["a.txt", "b.txt"] {
        std::fs::write(fx.dir.path().join(name), b"x").unwrap();
    }
    std::fs::create_dir(fx.dir.path().join("sub")).unwrap();

    // The kernel emits "." and ".." itself, so its first cursor is 2.
    let r = call(
        &fx,
        request(
            abi::OP_READDIR,
            8,
            abi::ROOT_NODEID,
            2,
            abi::MAX_PAYLOAD as u32,
            b"",
        ),
    )
    .await;
    assert_eq!(r.error, 0);
    let mut entries = dirents(&r.payload);
    assert_eq!(entries.len(), 3);
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(entries[0].0, "a.txt");
    assert_eq!(entries[2].0, "sub");
    assert_eq!(entries[2].3, abi::DT_DIR);
    assert_eq!(entries[0].3, abi::DT_REG);

    // Resuming from the last entry's cursor must yield nothing — an empty
    // payload is how the kernel learns the directory ended.
    let last = dirents(&r.payload).last().unwrap().2;
    let end = call(
        &fx,
        request(
            abi::OP_READDIR,
            9,
            abi::ROOT_NODEID,
            last,
            abi::MAX_PAYLOAD as u32,
            b"",
        ),
    )
    .await;
    assert_eq!(end.error, 0);
    assert!(end.payload.is_empty());

    // Resuming from the FIRST entry's cursor yields exactly the rest.
    let first = dirents(&r.payload)[0].2;
    let rest = call(
        &fx,
        request(
            abi::OP_READDIR,
            10,
            abi::ROOT_NODEID,
            first,
            abi::MAX_PAYLOAD as u32,
            b"",
        ),
    )
    .await;
    assert_eq!(dirents(&rest.payload).len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_serves_ranges_and_eof() {
    let fx = fixture().await;
    std::fs::write(fx.dir.path().join("one.txt"), b"first file").unwrap();
    let ino = call(
        &fx,
        request(abi::OP_LOOKUP, 11, abi::ROOT_NODEID, 0, 0, b"one.txt"),
    )
    .await
    .attr()
    .nodeid;

    let whole = call(&fx, request(abi::OP_READ, 12, ino, 0, 4096, b"")).await;
    assert_eq!(whole.error, 0);
    assert_eq!(whole.payload, b"first file");

    let tail = call(&fx, request(abi::OP_READ, 13, ino, 6, 4, b"")).await;
    assert_eq!(tail.payload, b"file");

    let past = call(&fx, request(abi::OP_READ, 14, ino, 100, 4096, b"")).await;
    assert_eq!(past.error, 0);
    assert!(
        past.payload.is_empty(),
        "reading past EOF is a short read, not an error"
    );

    // Repeated reads reuse one cached handle rather than opening per request.
    for _ in 0..5 {
        assert_eq!(
            call(&fx, request(abi::OP_READ, 15, ino, 0, 4096, b""))
                .await
                .payload,
            b"first file"
        );
    }
}

// --------------------------------------------------------------- mutations

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_write_and_read_back() {
    let fx = fixture().await;
    let created = call(
        &fx,
        request(
            abi::OP_CREATE,
            20,
            abi::ROOT_NODEID,
            0,
            0,
            &create_payload(abi::S_IFREG | 0o644, "new.txt"),
        ),
    )
    .await
    .attr();
    assert_eq!(created.size, 0);
    assert_eq!(created.mode & abi::S_IFMT, abi::S_IFREG);
    assert!(
        fx.dir.path().join("new.txt").exists(),
        "the file must exist on the server"
    );

    let w = call(
        &fx,
        request(abi::OP_WRITE, 21, created.nodeid, 0, 0, b"written locally"),
    )
    .await;
    assert_eq!(w.error, 0);
    assert_eq!(w.payload.len(), abi::WRITE_OUT_LEN);
    assert_eq!(u32_at(&w.payload, 0), 15, "the reply reports what was written");
    assert_eq!(
        std::fs::read(fx.dir.path().join("new.txt")).unwrap(),
        b"written locally"
    );

    // The size the next stat() sees must be the post-write one.
    let a = call(&fx, request(abi::OP_GETATTR, 22, created.nodeid, 0, 0, b""))
        .await
        .attr();
    assert_eq!(a.size, 15);

    // Writing at an offset (an append, as the kernel issues it).
    let w2 = call(&fx, request(abi::OP_WRITE, 23, created.nodeid, 15, 0, b"!")).await;
    assert_eq!(u32_at(&w2.payload, 0), 1);
    let back = call(&fx, request(abi::OP_READ, 24, created.nodeid, 0, 4096, b"")).await;
    assert_eq!(back.payload, b"written locally!");

    // Creating it again opens what is there, exactly as open(O_CREAT) without
    // O_EXCL must — and above all does NOT truncate it. (The C test daemon
    // answers EEXIST here; it can afford to, because the kernel only sends
    // CREATE for a dentry it believes is negative. Racing that belief is
    // precisely the case where POSIX says "open the existing file".)
    let again = call(
        &fx,
        request(
            abi::OP_CREATE,
            25,
            abi::ROOT_NODEID,
            0,
            0,
            &create_payload(abi::S_IFREG | 0o644, "new.txt"),
        ),
    )
    .await
    .attr();
    assert_eq!(again.nodeid, created.nodeid, "the same file keeps its nodeid");
    assert_eq!(again.size, 16);
    assert_eq!(
        std::fs::read(fx.dir.path().join("new.txt")).unwrap(),
        b"written locally!"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mkdir_rmdir_unlink_and_rename() {
    let fx = fixture().await;
    let dir = call(
        &fx,
        request(
            abi::OP_MKDIR,
            30,
            abi::ROOT_NODEID,
            0,
            0,
            &create_payload(abi::S_IFDIR | 0o755, "newdir"),
        ),
    )
    .await
    .attr();
    assert_eq!(dir.mode & abi::S_IFMT, abi::S_IFDIR);
    assert!(fx.dir.path().join("newdir").is_dir());

    // A non-empty directory must refuse to go.
    std::fs::write(fx.dir.path().join("newdir/inner.txt"), b"hi").unwrap();
    let busy = call(&fx, request(abi::OP_RMDIR, 31, abi::ROOT_NODEID, 0, 0, b"newdir")).await;
    assert_eq!(busy.error, -abi::ENOTEMPTY);

    let gone = call(&fx, request(abi::OP_UNLINK, 32, dir.nodeid, 0, 0, b"inner.txt")).await;
    assert_eq!(gone.error, 0);
    assert!(gone.payload.is_empty());
    let empty = call(&fx, request(abi::OP_RMDIR, 33, abi::ROOT_NODEID, 0, 0, b"newdir")).await;
    assert_eq!(empty.error, 0);
    assert!(!fx.dir.path().join("newdir").exists());

    // Rename, including across directories.
    std::fs::write(fx.dir.path().join("from.txt"), b"movable").unwrap();
    std::fs::create_dir(fx.dir.path().join("dest")).unwrap();
    let dest = call(&fx, request(abi::OP_LOOKUP, 34, abi::ROOT_NODEID, 0, 0, b"dest"))
        .await
        .attr()
        .nodeid;
    let mut p = Vec::new();
    p.extend_from_slice(&dest.to_ne_bytes());
    p.extend_from_slice(&(8u16).to_ne_bytes()); // "from.txt"
    p.extend_from_slice(&(9u16).to_ne_bytes()); // "moved.txt"
    p.extend_from_slice(&0u32.to_ne_bytes());
    p.extend_from_slice(b"from.txtmoved.txt");
    let r = call(&fx, request(abi::OP_RENAME, 35, abi::ROOT_NODEID, 0, 0, &p)).await;
    assert_eq!(r.error, 0);
    assert_eq!(
        std::fs::read(fx.dir.path().join("dest/moved.txt")).unwrap(),
        b"movable"
    );

    // A rename payload whose lengths do not fit its own names is rejected
    // before anything is touched.
    let mut bad = Vec::new();
    bad.extend_from_slice(&dest.to_ne_bytes());
    bad.extend_from_slice(&(200u16).to_ne_bytes());
    bad.extend_from_slice(&(200u16).to_ne_bytes());
    bad.extend_from_slice(&0u32.to_ne_bytes());
    bad.extend_from_slice(b"short");
    let r = call(&fx, request(abi::OP_RENAME, 36, abi::ROOT_NODEID, 0, 0, &bad)).await;
    assert_eq!(r.error, -abi::EINVAL);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setattr_truncates_and_chmods() {
    let fx = fixture().await;
    std::fs::write(fx.dir.path().join("f.txt"), b"0123456789").unwrap();
    let ino = call(&fx, request(abi::OP_LOOKUP, 40, abi::ROOT_NODEID, 0, 0, b"f.txt"))
        .await
        .attr()
        .nodeid;

    let mut p = Vec::new();
    p.extend_from_slice(&abi::SETATTR_SIZE.to_ne_bytes());
    p.extend_from_slice(&0u32.to_ne_bytes());
    p.extend_from_slice(&4u64.to_ne_bytes());
    p.extend_from_slice(&0u64.to_ne_bytes());
    let a = call(&fx, request(abi::OP_SETATTR, 41, ino, 0, 0, &p))
        .await
        .attr();
    assert_eq!(a.size, 4);
    assert_eq!(std::fs::read(fx.dir.path().join("f.txt")).unwrap(), b"0123");

    let mut p = Vec::new();
    p.extend_from_slice(&abi::SETATTR_MODE.to_ne_bytes());
    p.extend_from_slice(&0o600u32.to_ne_bytes());
    p.extend_from_slice(&0u64.to_ne_bytes());
    p.extend_from_slice(&0u64.to_ne_bytes());
    let a = call(&fx, request(abi::OP_SETATTR, 42, ino, 0, 0, &p))
        .await
        .attr();
    assert_eq!(
        a.mode & abi::S_IFMT,
        abi::S_IFREG,
        "chmod must not change the file type"
    );

    // A truncated setattr payload cannot be trusted.
    let r = call(&fx, request(abi::OP_SETATTR, 43, ino, 0, 0, b"\x01\x00")).await;
    assert_eq!(r.error, -abi::EINVAL);
}

// ----------------------------------------------------------- notifications

/// Collects frames instead of writing them to the device.
#[derive(Clone, Default)]
struct Collector(Arc<Mutex<Vec<Vec<u8>>>>);

impl Frames for Collector {
    fn write_frame(&self, buf: &[u8]) -> std::io::Result<()> {
        self.0.lock().unwrap().push(buf.to_vec());
        Ok(())
    }
}

struct Note {
    code: i32,
    parent: u64,
    parent2: u64,
    size: u64,
    isdir: bool,
    name: String,
    name2: String,
}

/// Decode a notification frame the way `alloyfs_notify_from_daemon` does.
fn decode_note(frame: &[u8]) -> Note {
    assert_eq!(frame.len(), u32_at(frame, 0) as usize);
    assert_eq!(u64_at(frame, 8), 0, "notifications carry unique == 0");
    let p = &frame[abi::OUT_HEADER_LEN..];
    assert!(p.len() >= abi::NOTIFY_ENTRY_LEN);
    let namelen = u16::from_ne_bytes([p[28], p[29]]) as usize;
    let name2len = u16::from_ne_bytes([p[30], p[31]]) as usize;
    assert_eq!(abi::NOTIFY_ENTRY_LEN + namelen + name2len, p.len());
    Note {
        code: i32::from_ne_bytes([frame[4], frame[5], frame[6], frame[7]]),
        parent: u64_at(p, 0),
        parent2: u64_at(p, 8),
        size: u64_at(p, 16),
        isdir: u32_at(p, 24) & abi::NOTIFY_F_ISDIR != 0,
        name: String::from_utf8(p[32..32 + namelen].to_vec()).unwrap(),
        name2: String::from_utf8(p[32 + namelen..].to_vec()).unwrap(),
    }
}

fn event(seq: u64, kind: EventKind, path: &str) -> FsEvent {
    FsEvent {
        seq,
        kind,
        path: RelPath(path.to_string()),
        new_version: None,
        origin: None,
    }
}

/// Run one batch through the notification thread and collect what it wrote.
async fn notify_batch(fx: &Fixture, batch: Vec<FsEvent>) -> Vec<Note> {
    let (sink, rx) = alloyfs_mount_kernel::notify::channel();
    let frames = Collector::default();
    let stop = Arc::new(AtomicBool::new(false));
    let (server, sink_frames, stop2) = (fx.server.clone(), frames.clone(), stop.clone());
    let thread =
        std::thread::spawn(move || alloyfs_mount_kernel::notify::run(rx, server, sink_frames, stop2));
    sink.push(&batch);
    drop(sink); // the thread ends once the queue is drained and closed
    thread.join().expect("notify thread");
    let out = frames.0.lock().unwrap().clone();
    out.iter().map(|f| decode_note(f)).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_events_become_kernel_notifications() {
    let fx = fixture().await;
    // The kernel only notifies on cached paths, and our parent nodeids come
    // from the inode table — so walk the root first, as a mount would.
    call(
        &fx,
        request(
            abi::OP_READDIR,
            50,
            abi::ROOT_NODEID,
            2,
            abi::MAX_PAYLOAD as u32,
            b"",
        ),
    )
    .await;

    std::fs::write(fx.dir.path().join("fresh.txt"), b"hello").unwrap();
    std::fs::create_dir(fx.dir.path().join("freshdir")).unwrap();
    let notes = notify_batch(
        &fx,
        vec![
            event(1, EventKind::Created, "fresh.txt"),
            event(2, EventKind::Created, "freshdir"),
        ],
    )
    .await;
    assert_eq!(notes.len(), 2);
    assert_eq!(notes[0].code, abi::NOTIFY_CREATE);
    assert_eq!(notes[0].parent, abi::ROOT_NODEID);
    assert_eq!(notes[0].name, "fresh.txt");
    assert!(!notes[0].isdir);
    assert!(notes[1].isdir, "a directory that appeared must set ISDIR");

    // Modify carries the NEW size — the kernel writes it straight into i_size.
    std::fs::write(fx.dir.path().join("fresh.txt"), b"a-much-longer-body").unwrap();
    let notes = notify_batch(&fx, vec![event(3, EventKind::Modified, "fresh.txt")]).await;
    assert_eq!(notes[0].code, abi::NOTIFY_MODIFY);
    assert_eq!(notes[0].size, 18);

    let notes = notify_batch(&fx, vec![event(4, EventKind::AttrChanged, "fresh.txt")]).await;
    assert_eq!(notes[0].code, abi::NOTIFY_ATTRIB);

    // A rename becomes one paired notification, not a delete plus a create.
    std::fs::rename(
        fx.dir.path().join("fresh.txt"),
        fx.dir.path().join("freshdir/moved.txt"),
    )
    .unwrap();
    let notes = notify_batch(
        &fx,
        vec![event(
            5,
            EventKind::RenamedFrom {
                to: RelPath("freshdir/moved.txt".into()),
            },
            "fresh.txt",
        )],
    )
    .await;
    assert_eq!(notes[0].code, abi::NOTIFY_RENAME);
    assert_eq!(notes[0].parent, abi::ROOT_NODEID);
    assert_eq!(notes[0].name, "fresh.txt");
    assert_eq!(notes[0].name2, "moved.txt");
    assert_ne!(notes[0].parent2, 0, "the destination directory must resolve");
    assert_ne!(notes[0].parent2, notes[0].parent);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deletes_report_isdir_from_what_the_kernel_was_told() {
    let fx = fixture().await;
    std::fs::create_dir(fx.dir.path().join("gonedir")).unwrap();
    std::fs::write(fx.dir.path().join("gone.txt"), b"x").unwrap();
    // The kernel learned about both through a listing; that is the same moment
    // the daemon learned their kinds.
    call(
        &fx,
        request(
            abi::OP_READDIR,
            60,
            abi::ROOT_NODEID,
            2,
            abi::MAX_PAYLOAD as u32,
            b"",
        ),
    )
    .await;
    std::fs::remove_dir(fx.dir.path().join("gonedir")).unwrap();
    std::fs::remove_file(fx.dir.path().join("gone.txt")).unwrap();

    let notes = notify_batch(
        &fx,
        vec![
            event(6, EventKind::Removed, "gonedir"),
            event(7, EventKind::Removed, "gone.txt"),
        ],
    )
    .await;
    assert_eq!(notes.len(), 2);
    assert_eq!(notes[0].code, abi::NOTIFY_DELETE);
    assert!(notes[0].isdir, "a removed directory still needs FS_ISDIR");
    assert!(!notes[1].isdir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn events_the_kernel_cannot_use_are_dropped() {
    let fx = fixture().await;
    let notes = notify_batch(
        &fx,
        vec![
            // Nothing has ever looked inside "deep/", so its nodeid is unknown
            // — exactly the case the kernel would skip anyway.
            event(8, EventKind::Created, "deep/inner.txt"),
            // A whole-tree resync has no name to address.
            event(9, EventKind::ResyncRequired, "whatever"),
        ],
    )
    .await;
    assert!(notes.is_empty(), "got {} unexpected notifications", notes.len());
    // And the fs is still usable afterwards.
    let a = call(&fx, request(abi::OP_GETATTR, 70, abi::ROOT_NODEID, 0, 0, b""))
        .await
        .attr();
    assert_eq!(a.mode & abi::S_IFMT, abi::S_IFDIR);
}

/// A remote change must reach the next READ, not just the next GETATTR.
///
/// The daemon keeps one open server handle per nodeid so that a sequential
/// read is not an open/close per 128 KiB. That cache has to be dropped when
/// the file changes underneath it, or every read after the first goes on
/// serving the bytes the handle was opened over. Stage 6 caught this as
/// "new size visible / new contents [first file]": the notification carried
/// the right size, the kernel wrote it into i_size, and the data behind it
/// was stale — the worst shape for a bug, because `ls` looks correct.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_remote_modify_invalidates_the_cached_read_handle() {
    let fx = fixture().await;
    std::fs::write(fx.dir.path().join("one.txt"), b"first file").unwrap();
    let ino = call(
        &fx,
        request(abi::OP_LOOKUP, 80, abi::ROOT_NODEID, 0, 0, b"one.txt"),
    )
    .await
    .attr()
    .nodeid;

    // The first read is what opens and caches the handle.
    let first = call(&fx, request(abi::OP_READ, 81, ino, 0, 4096, b"")).await;
    assert_eq!(first.payload, b"first file");

    std::fs::write(fx.dir.path().join("one.txt"), b"a-much-longer-body").unwrap();
    let notes = notify_batch(&fx, vec![event(82, EventKind::Modified, "one.txt")]).await;
    assert_eq!(notes[0].size, 18, "the notification must carry the new size");

    let after = call(&fx, request(abi::OP_READ, 83, ino, 0, 4096, b"")).await;
    assert_eq!(
        String::from_utf8_lossy(&after.payload),
        "a-much-longer-body",
        "read served bytes from the handle opened before the change"
    );
}

/// The same trap, reached by renaming rather than rewriting: the destination
/// inherits the source's nodeid, so a handle cached under it survives the
/// move and answers reads of the new name.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_remote_rename_invalidates_the_cached_read_handle() {
    let fx = fixture().await;
    std::fs::write(fx.dir.path().join("one.txt"), b"first file").unwrap();
    let ino = call(
        &fx,
        request(abi::OP_LOOKUP, 90, abi::ROOT_NODEID, 0, 0, b"one.txt"),
    )
    .await
    .attr()
    .nodeid;
    assert_eq!(
        call(&fx, request(abi::OP_READ, 91, ino, 0, 4096, b""))
            .await
            .payload,
        b"first file"
    );

    std::fs::write(fx.dir.path().join("one.txt"), b"a-much-longer-body").unwrap();
    std::fs::rename(fx.dir.path().join("one.txt"), fx.dir.path().join("renamed.txt")).unwrap();
    notify_batch(
        &fx,
        vec![event(
            92,
            EventKind::RenamedFrom {
                to: RelPath("renamed.txt".to_string()),
            },
            "one.txt",
        )],
    )
    .await;

    let ino2 = call(
        &fx,
        request(abi::OP_LOOKUP, 93, abi::ROOT_NODEID, 0, 0, b"renamed.txt"),
    )
    .await
    .attr()
    .nodeid;
    let after = call(&fx, request(abi::OP_READ, 94, ino2, 0, 4096, b"")).await;
    assert_eq!(
        String::from_utf8_lossy(&after.payload),
        "a-much-longer-body",
        "the new name served the handle cached for the old one"
    );
}
