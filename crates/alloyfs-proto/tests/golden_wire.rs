//! Golden wire-format test: freezes the postcard payload encoding (no length
//! prefix) of every enum variant in alloyfs-proto. If any byte here changes, a
//! deployed peer speaking the old encoding can no longer talk to us — so a
//! failure means "decide about PROTO_VERSION", never "update the test blindly".

use std::time::{Duration, SystemTime};

use alloyfs_proto::{
    Attr, DirEntry, ErrorCode, EventKind, FileKind, Frame, FsEvent, LockKind, ManyRemove, ManySetattr,
    ManyWrite, OpenFlags, RelPath, Request, Response, TreeEntry, PROTO_VERSION_MAX, PROTO_VERSION_MIN,
};
use bytes::Bytes;

/// Fixed timestamp used everywhere a `SystemTime` appears.
/// 100ns-aligned: Windows SystemTime has 100ns resolution, so a finer
/// canonical value would encode differently per platform (the golden test
/// caught exactly that on its first cross-OS run).
fn t() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::new(1_700_000_000, 123_456_700)
}

fn path() -> RelPath {
    RelPath("dir/file.txt".into())
}

fn attr() -> Attr {
    Attr {
        kind: FileKind::File,
        size: 42,
        mtime: t(),
        ctime: t(),
        mode: 0o644,
        version: 8,
    }
}

fn flags() -> OpenFlags {
    OpenFlags {
        read: true,
        write: true,
        truncate: false,
        append: false,
        excl: true,
    }
}

fn req(body: Request) -> Frame {
    Frame::Request { id: 7, body }
}

fn ok(body: Response) -> Frame {
    Frame::Response {
        id: 7,
        body: Ok(body),
    }
}

fn err(code: ErrorCode) -> Frame {
    Frame::Response {
        id: 7,
        body: Err(code),
    }
}

fn ev(kind: EventKind) -> Frame {
    Frame::Events {
        batch: vec![FsEvent {
            seq: 55,
            kind,
            path: path(),
            new_version: Some(8),
            origin: Some(1),
        }],
    }
}

/// One canonical instance per variant, in a fixed order. Every field uses a
/// fixed value so the encodings below are stable byte-for-byte.
fn canonical() -> Vec<(&'static str, Frame)> {
    vec![
        // --- Frame variants not covered by the per-variant sweeps below ---
        (
            "hello",
            Frame::Hello {
                proto_min: PROTO_VERSION_MIN,
                proto_max: PROTO_VERSION_MAX,
                client: "golden".into(),
            },
        ),
        (
            "hello_ack",
            Frame::HelloAck {
                proto: PROTO_VERSION_MAX,
                server: "golden".into(),
            },
        ),
        ("ping", Frame::Ping { nonce: 7 }),
        ("pong", Frame::Pong { nonce: 7 }),
        // Fixed bytes rather than a real lz4 output: this freezes the
        // VARIANT encoding; the lz4 block format itself is frozen by the
        // compression roundtrip tests in alloyfs-proto/src/frame.rs.
        ("compressed", Frame::Compressed(Bytes::from_static(b"golden"))),
        // --- every Request variant ---
        (
            "req_attach",
            req(Request::Attach {
                export: "projects".into(),
            }),
        ),
        ("req_getattr", req(Request::Getattr { path: path() })),
        (
            "req_readdir",
            req(Request::Readdir {
                path: path(),
                cursor: 42,
            }),
        ),
        (
            "req_open",
            req(Request::Open {
                path: path(),
                flags: flags(),
            }),
        ),
        (
            "req_create",
            req(Request::Create {
                path: path(),
                flags: flags(),
                mode: 0o644,
            }),
        ),
        (
            "req_read",
            req(Request::Read {
                fh: 9,
                offset: 131_072,
                len: 42,
            }),
        ),
        (
            "req_write",
            req(Request::Write {
                fh: 9,
                offset: 131_072,
                data: Bytes::from_static(b"golden"),
                expect_version: Some(8),
            }),
        ),
        ("req_flush", req(Request::Flush { fh: 9 })),
        ("req_release", req(Request::Release { fh: 9 })),
        (
            "req_setattr",
            req(Request::Setattr {
                path: path(),
                size: Some(42),
                mtime: Some(t()),
                mode: Some(0o644),
            }),
        ),
        (
            "req_mkdir",
            req(Request::Mkdir {
                path: path(),
                mode: 0o644,
            }),
        ),
        ("req_unlink", req(Request::Unlink { path: path() })),
        ("req_rmdir", req(Request::Rmdir { path: path() })),
        (
            "req_rename",
            req(Request::Rename {
                from: path(),
                to: RelPath("dir/renamed.txt".into()),
                replace: true,
            }),
        ),
        (
            "req_lock",
            req(Request::Lock {
                fh: 9,
                kind: LockKind::Exclusive,
                wait: true,
            }),
        ),
        ("req_unlock", req(Request::Unlock { fh: 9 })),
        ("req_subscribe", req(Request::Subscribe { since_seq: Some(55) })),
        ("req_statfs", req(Request::Statfs)),
        (
            "req_link",
            req(Request::Link {
                target: path(),
                link: RelPath("dir/link.txt".into()),
            }),
        ),
        ("req_mount_defaults", req(Request::MountDefaults)),
        (
            "req_auth",
            req(Request::Auth {
                token: "golden-secret".into(),
            }),
        ),
        // --- every Response variant (as Ok) ---
        (
            "resp_attach_ok",
            ok(Response::AttachOk {
                export_id: 42,
                root_attr: attr(),
            }),
        ),
        ("resp_attr", ok(Response::Attr(attr()))),
        (
            "resp_dir",
            ok(Response::Dir {
                entries: vec![DirEntry {
                    name: "golden".into(),
                    attr: attr(),
                }],
                next_cursor: Some(42),
            }),
        ),
        ("resp_opened", ok(Response::Opened { fh: 9, attr: attr() })),
        ("resp_data", ok(Response::Data(Bytes::from_static(b"golden")))),
        (
            "resp_written",
            ok(Response::Written {
                n: 42,
                new_version: 8,
                conflict: true,
            }),
        ),
        (
            "resp_statfs",
            ok(Response::Statfs {
                block_size: 42,
                blocks: 42,
                blocks_free: 42,
            }),
        ),
        ("resp_subscribed", ok(Response::Subscribed { last_seq: 55 })),
        ("resp_ok", ok(Response::Ok)),
        (
            "resp_mount_defaults",
            ok(Response::MountDefaults {
                exclude: vec!["node_modules/**".into()],
                pin: vec!["*.lock".into()],
                auto_cache_max: Some(42),
                auto_cache_budget: None,
            }),
        ),
        (
            "req_symlink",
            req(Request::Symlink {
                // Relative on purpose: a target is opaque text, not a path we
                // validate, and the encoding must not care.
                target: "../elsewhere/file.txt".into(),
                link: path(),
            }),
        ),
        ("req_read_link", req(Request::ReadLink { path: path() })),
        (
            "resp_target",
            ok(Response::Target("../elsewhere/file.txt".into())),
        ),
        (
            "resp_written_attr",
            ok(Response::WrittenAttr { n: 42, attr: attr() }),
        ),
        (
            "req_tree",
            req(Request::Tree {
                path: path(),
                cursor: Some(64),
            }),
        ),
        ("req_tree_token", req(Request::TreeToken)),
        (
            "resp_tree",
            ok(Response::Tree {
                entries: vec![TreeEntry {
                    path: path(),
                    attr: attr(),
                }],
                next_cursor: Some(64),
                token: 0x0123_4567_89ab_cdef,
            }),
        ),
        (
            "resp_tree_token",
            ok(Response::TreeToken {
                token: 0x0123_4567_89ab_cdef,
            }),
        ),
        // --- v7: byte-range locks ---
        (
            "req_lock_range",
            req(Request::LockRange {
                fh: 9,
                owner: 0xfeed,
                kind: LockKind::Exclusive,
                start: 0x4000_0000,
                len: 1,
                wait: false,
            }),
        ),
        (
            "req_unlock_range",
            req(Request::UnlockRange {
                fh: 9,
                owner: 0xfeed,
                start: 0x4000_0002,
                len: 510,
            }),
        ),
        (
            "req_test_lock",
            req(Request::TestLock {
                fh: 9,
                owner: 0xfeed,
                kind: LockKind::Shared,
                start: 0x4000_0001,
                len: 1,
            }),
        ),
        ("resp_lock_status_free", ok(Response::LockStatus(None))),
        (
            "resp_lock_status_held",
            ok(Response::LockStatus(Some(alloyfs_proto::LockConflict {
                kind: LockKind::Exclusive,
                start: 0x4000_0001,
                len: 1,
                pid: 0,
            }))),
        ),
        // --- v8: bulk content fetch ---
        (
            "req_read_many",
            req(Request::ReadMany {
                paths: vec![path(), RelPath("dir/other.txt".into())],
                budget: 768 * 1024,
            }),
        ),
        (
            "resp_many",
            ok(Response::Many(vec![
                alloyfs_proto::ManyEntry::File {
                    attr: attr(),
                    data: bytes::Bytes::from_static(b"hi"),
                },
                alloyfs_proto::ManyEntry::Skipped(ErrorCode::TooLarge),
            ])),
        ),
        ("err_too_large", err(ErrorCode::TooLarge)),
        // --- v9: open+read, setattr2, attach2 ---
        (
            "req_open_read",
            req(Request::OpenRead {
                path: path(),
                flags: OpenFlags {
                    read: true,
                    ..Default::default()
                },
                len: 128 * 1024,
            }),
        ),
        (
            "req_setattr2",
            req(Request::Setattr2 {
                path: path(),
                size: Some(42),
                mtime: None,
                mode: None,
                readonly: Some(true),
            }),
        ),
        (
            "req_attach2",
            req(Request::Attach2 {
                export: "docs".into(),
            }),
        ),
        (
            "resp_opened_data",
            ok(Response::OpenedData {
                fh: 9,
                attr: attr(),
                data: bytes::Bytes::from_static(b"head"),
            }),
        ),
        (
            "resp_attached2",
            ok(Response::Attached2 {
                export_id: 7,
                root_attr: attr(),
                exclude: vec!["node_modules".into()],
                pin: vec![],
                auto_cache_max: Some(1 << 20),
                auto_cache_budget: None,
                tree_token: 0x0123_4567_89ab_cdef,
            }),
        ),
        (
            "req_write_many",
            req(Request::WriteMany {
                files: vec![ManyWrite {
                    path: path(),
                    mode: 0o666,
                    data: bytes::Bytes::from_static(b"body"),
                }],
            }),
        ),
        (
            "req_remove_many",
            req(Request::RemoveMany {
                entries: vec![ManyRemove {
                    path: path(),
                    dir: false,
                }],
            }),
        ),
        (
            "req_setattr_many",
            req(Request::SetattrMany {
                entries: vec![ManySetattr {
                    path: path(),
                    size: Some(42),
                    mtime: None,
                    mode: None,
                    readonly: Some(true),
                }],
            }),
        ),
        (
            "resp_many_outcome",
            ok(Response::ManyOutcome(vec![
                Ok(Some(attr())),
                Ok(None),
                Err(ErrorCode::NotFound),
            ])),
        ),
        // --- every ErrorCode (as Err) ---
        ("err_not_found", err(ErrorCode::NotFound)),
        ("err_permission_denied", err(ErrorCode::PermissionDenied)),
        ("err_already_exists", err(ErrorCode::AlreadyExists)),
        ("err_not_a_directory", err(ErrorCode::NotADirectory)),
        ("err_is_a_directory", err(ErrorCode::IsADirectory)),
        ("err_not_empty", err(ErrorCode::NotEmpty)),
        ("err_invalid_path", err(ErrorCode::InvalidPath)),
        ("err_bad_handle", err(ErrorCode::BadHandle)),
        ("err_read_only", err(ErrorCode::ReadOnly)),
        ("err_no_such_export", err(ErrorCode::NoSuchExport)),
        ("err_would_block", err(ErrorCode::WouldBlock)),
        ("err_conflict", err(ErrorCode::Conflict)),
        ("err_version_mismatch", err(ErrorCode::VersionMismatch)),
        ("err_not_attached", err(ErrorCode::NotAttached)),
        ("err_too_old", err(ErrorCode::TooOld)),
        ("err_io", err(ErrorCode::Io)),
        ("err_cross_device", err(ErrorCode::CrossDevice)),
        ("err_auth_required", err(ErrorCode::AuthRequired)),
        // --- every EventKind (inside an Events batch) ---
        ("event_created", ev(EventKind::Created)),
        ("event_modified", ev(EventKind::Modified)),
        ("event_attr_changed", ev(EventKind::AttrChanged)),
        ("event_removed", ev(EventKind::Removed)),
        (
            "event_renamed_from",
            ev(EventKind::RenamedFrom {
                to: RelPath("dir/renamed.txt".into()),
            }),
        ),
        ("event_resync_required", ev(EventKind::ResyncRequired)),
    ]
}

/// Never called. Adding any variant to these enums fails compilation here,
/// forcing a conscious decision: add a golden vector for the new variant AND
/// decide whether PROTO_VERSION_MAX/MIN must move.
#[allow(dead_code)]
fn _variant_tripwire(
    frame: &Frame,
    request: &Request,
    response: &Response,
    code: &ErrorCode,
    event: &EventKind,
    file_kind: &FileKind,
    lock_kind: &LockKind,
) {
    match frame {
        Frame::Hello { .. } => {}
        Frame::HelloAck { .. } => {}
        Frame::Request { .. } => {}
        Frame::Response { .. } => {}
        Frame::Events { .. } => {}
        Frame::Ping { .. } => {}
        Frame::Pong { .. } => {}
        Frame::Compressed(..) => {} // v3: golden added, PROTO_VERSION_MAX bumped
    }
    match request {
        Request::Attach { .. } => {}
        Request::Getattr { .. } => {}
        Request::Readdir { .. } => {}
        Request::Open { .. } => {}
        Request::Create { .. } => {}
        Request::Read { .. } => {}
        Request::Write { .. } => {}
        Request::Flush { .. } => {}
        Request::Release { .. } => {}
        Request::Setattr { .. } => {}
        Request::Mkdir { .. } => {}
        Request::Unlink { .. } => {}
        Request::Rmdir { .. } => {}
        Request::Rename { .. } => {}
        Request::Lock { .. } => {}
        Request::Unlock { .. } => {}
        Request::Subscribe { .. } => {}
        Request::Statfs => {}
        Request::Link { .. } => {}
        Request::MountDefaults => {}  // v2: golden added, PROTO_VERSION_MAX bumped
        Request::Auth { .. } => {}    // v3: golden added, PROTO_VERSION_MAX bumped
        Request::Symlink { .. } => {} // v4: golden added, PROTO_VERSION_MAX bumped
        Request::ReadLink { .. } => {} // v4: golden added, PROTO_VERSION_MAX bumped
        Request::Tree { .. } => {}    // v6: golden added, PROTO_VERSION_MAX bumped
        Request::TreeToken => {}      // v6: golden added, PROTO_VERSION_MAX bumped
        Request::LockRange { .. } => {} // v7: golden added, PROTO_VERSION_MAX bumped
        Request::UnlockRange { .. } => {} // v7: golden added, PROTO_VERSION_MAX bumped
        Request::TestLock { .. } => {} // v7: golden added, PROTO_VERSION_MAX bumped
        Request::ReadMany { .. } => {} // v8: golden added, PROTO_VERSION_MAX bumped
        Request::OpenRead { .. } => {} // v9: golden added, PROTO_VERSION_MAX bumped
        Request::Setattr2 { .. } => {} // v9: golden added, PROTO_VERSION_MAX bumped
        Request::Attach2 { .. } => {} // v9: golden added, PROTO_VERSION_MAX bumped
        Request::WriteMany { .. } => {} // v10: golden added, PROTO_VERSION_MAX bumped
        Request::RemoveMany { .. } => {} // v10: golden added, PROTO_VERSION_MAX bumped
        Request::SetattrMany { .. } => {} // v10: golden added, PROTO_VERSION_MAX bumped
    }
    match response {
        Response::AttachOk { .. } => {}
        Response::Attr(..) => {}
        Response::Dir { .. } => {}
        Response::Opened { .. } => {}
        Response::Data(..) => {}
        Response::Written { .. } => {}
        Response::Statfs { .. } => {}
        Response::Subscribed { .. } => {}
        Response::Ok => {}
        Response::MountDefaults { .. } => {} // v2: golden added, PROTO_VERSION_MAX bumped
        Response::Target(..) => {}           // v4: golden added, PROTO_VERSION_MAX bumped
        Response::WrittenAttr { .. } => {}   // v5: golden added, PROTO_VERSION_MAX bumped
        Response::Tree { .. } => {}          // v6: golden added, PROTO_VERSION_MAX bumped
        Response::TreeToken { .. } => {}     // v6: golden added, PROTO_VERSION_MAX bumped
        Response::LockStatus(..) => {}       // v7: golden added, PROTO_VERSION_MAX bumped
        Response::Many(..) => {}             // v8: golden added, PROTO_VERSION_MAX bumped
        Response::OpenedData { .. } => {}    // v9: golden added, PROTO_VERSION_MAX bumped
        Response::Attached2 { .. } => {}     // v9: golden added, PROTO_VERSION_MAX bumped
        Response::ManyOutcome(..) => {}      // v10: golden added, PROTO_VERSION_MAX bumped
    }
    match code {
        ErrorCode::NotFound => {}
        ErrorCode::PermissionDenied => {}
        ErrorCode::AlreadyExists => {}
        ErrorCode::NotADirectory => {}
        ErrorCode::IsADirectory => {}
        ErrorCode::NotEmpty => {}
        ErrorCode::InvalidPath => {}
        ErrorCode::BadHandle => {}
        ErrorCode::ReadOnly => {}
        ErrorCode::NoSuchExport => {}
        ErrorCode::WouldBlock => {}
        ErrorCode::Conflict => {}
        ErrorCode::VersionMismatch => {}
        ErrorCode::NotAttached => {}
        ErrorCode::TooOld => {}
        ErrorCode::Io => {}
        ErrorCode::CrossDevice => {}
        ErrorCode::AuthRequired => {} // v3: golden added, PROTO_VERSION_MAX bumped
        ErrorCode::TooLarge => {}     // v8: golden added, PROTO_VERSION_MAX bumped
    }
    match event {
        EventKind::Created => {}
        EventKind::Modified => {}
        EventKind::AttrChanged => {}
        EventKind::Removed => {}
        EventKind::RenamedFrom { .. } => {}
        EventKind::ResyncRequired => {}
    }
    match file_kind {
        FileKind::File => {}
        FileKind::Dir => {}
        FileKind::Symlink => {}
    }
    match lock_kind {
        LockKind::Shared => {}
        LockKind::Exclusive => {}
    }
}

/// (name, lowercase hex of `postcard::to_stdvec(&frame)`) for every canonical
/// instance, in the same order. Regenerate with:
/// `cargo test -p alloyfs-proto print_goldens -- --ignored --nocapture`
#[rustfmt::skip]
const GOLDEN: &[(&str, &str)] = &[
    ("hello", "00010a06676f6c64656e"),
    ("hello_ack", "010a06676f6c64656e"),
    ("ping", "0507"),
    ("pong", "0607"),
    ("compressed", "0706676f6c64656e"),
    ("req_attach", "0207000870726f6a65637473"),
    ("req_getattr", "0207010c6469722f66696c652e747874"),
    ("req_readdir", "0207020c6469722f66696c652e7478742a"),
    ("req_open", "0207030c6469722f66696c652e7478740101000001"),
    ("req_create", "0207040c6469722f66696c652e7478740101000001a403"),
    ("req_read", "020705098080082a"),
    ("req_write", "0207060980800806676f6c64656e0108"),
    ("req_flush", "02070709"),
    ("req_release", "02070809"),
    ("req_setattr", "0207090c6469722f66696c652e747874012a0180e2cfaa06bc99ef3a01a403"),
    ("req_mkdir", "02070a0c6469722f66696c652e747874a403"),
    ("req_unlink", "02070b0c6469722f66696c652e747874"),
    ("req_rmdir", "02070c0c6469722f66696c652e747874"),
    ("req_rename", "02070d0c6469722f66696c652e7478740f6469722f72656e616d65642e74787401"),
    ("req_lock", "02070e090101"),
    ("req_unlock", "02070f09"),
    ("req_subscribe", "0207100137"),
    ("req_statfs", "020711"),
    ("req_link", "0207120c6469722f66696c652e7478740c6469722f6c696e6b2e747874"),
    ("req_mount_defaults", "020713"),
    ("req_auth", "0207140d676f6c64656e2d736563726574"),
    ("resp_attach_ok", "030700002a002a80e2cfaa06bc99ef3a80e2cfaa06bc99ef3aa40308"),
    ("resp_attr", "03070001002a80e2cfaa06bc99ef3a80e2cfaa06bc99ef3aa40308"),
    ("resp_dir", "030700020106676f6c64656e002a80e2cfaa06bc99ef3a80e2cfaa06bc99ef3aa40308012a"),
    ("resp_opened", "0307000309002a80e2cfaa06bc99ef3a80e2cfaa06bc99ef3aa40308"),
    ("resp_data", "0307000406676f6c64656e"),
    ("resp_written", "030700052a0801"),
    ("resp_statfs", "030700062a2a2a"),
    ("resp_subscribed", "0307000737"),
    ("resp_ok", "03070008"),
    ("resp_mount_defaults", "03070009010f6e6f64655f6d6f64756c65732f2a2a01062a2e6c6f636b012a00"),
    ("req_symlink", "020715152e2e2f656c736577686572652f66696c652e7478740c6469722f66696c652e747874"),
    ("req_read_link", "0207160c6469722f66696c652e747874"),
    ("resp_target", "0307000a152e2e2f656c736577686572652f66696c652e747874"),
    ("resp_written_attr", "0307000b2a002a80e2cfaa06bc99ef3a80e2cfaa06bc99ef3aa40308"),
    ("req_tree", "0207170c6469722f66696c652e7478740140"),
    ("req_tree_token", "020718"),
    ("resp_tree", "0307000c010c6469722f66696c652e747874002a80e2cfaa06bc99ef3a80e2cfaa06bc99ef3aa403080140ef9bafcdf8acd19101"),
    ("resp_tree_token", "0307000def9bafcdf8acd19101"),
    ("req_lock_range", "02071909edfd030180808080040100"),
    ("req_unlock_range", "02071a09edfd038280808004fe03"),
    ("req_test_lock", "02071b09edfd0300818080800401"),
    ("resp_lock_status_free", "0307000e00"),
    ("resp_lock_status_held", "0307000e010181808080040100"),
    ("req_read_many", "02071c020c6469722f66696c652e7478740d6469722f6f746865722e747874808030"),
    ("resp_many", "0307000f0200002a80e2cfaa06bc99ef3a80e2cfaa06bc99ef3aa403080268690112"),
    ("err_too_large", "03070112"),
    ("req_open_read", "02071d0c6469722f66696c652e7478740100000000808008"),
    ("req_setattr2", "02071e0c6469722f66696c652e747874012a00000101"),
    ("req_attach2", "02071f04646f6373"),
    ("resp_opened_data", "0307001009002a80e2cfaa06bc99ef3a80e2cfaa06bc99ef3aa403080468656164"),
    ("resp_attached2", "0307001107002a80e2cfaa06bc99ef3a80e2cfaa06bc99ef3aa40308010c6e6f64655f6d6f64756c6573000180804000ef9bafcdf8acd19101"),
    ("req_write_many", "020720010c6469722f66696c652e747874b60304626f6479"),
    ("req_remove_many", "020721010c6469722f66696c652e74787400"),
    ("req_setattr_many", "020722010c6469722f66696c652e747874012a00000101"),
    ("resp_many_outcome", "03070012030001002a80e2cfaa06bc99ef3a80e2cfaa06bc99ef3aa4030800000100"),
    ("err_not_found", "03070100"),
    ("err_permission_denied", "03070101"),
    ("err_already_exists", "03070102"),
    ("err_not_a_directory", "03070103"),
    ("err_is_a_directory", "03070104"),
    ("err_not_empty", "03070105"),
    ("err_invalid_path", "03070106"),
    ("err_bad_handle", "03070107"),
    ("err_read_only", "03070108"),
    ("err_no_such_export", "03070109"),
    ("err_would_block", "0307010a"),
    ("err_conflict", "0307010b"),
    ("err_version_mismatch", "0307010c"),
    ("err_not_attached", "0307010d"),
    ("err_too_old", "0307010e"),
    ("err_io", "0307010f"),
    ("err_cross_device", "03070110"),
    ("err_auth_required", "03070111"),
    ("event_created", "040137000c6469722f66696c652e74787401080101"),
    ("event_modified", "040137010c6469722f66696c652e74787401080101"),
    ("event_attr_changed", "040137020c6469722f66696c652e74787401080101"),
    ("event_removed", "040137030c6469722f66696c652e74787401080101"),
    ("event_renamed_from", "040137040f6469722f72656e616d65642e7478740c6469722f66696c652e74787401080101"),
    ("event_resync_required", "040137050c6469722f66696c652e74787401080101"),
];

mod hexfmt {
    pub fn encode(bytes: &[u8]) -> String {
        let mut s = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }

    pub fn decode(hex: &str) -> Vec<u8> {
        assert!(hex.len().is_multiple_of(2), "odd-length hex string");
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("invalid hex"))
            .collect()
    }
}

/// Prints the GOLDEN table body for pasting into this file.
#[test]
#[ignore]
fn print_goldens() {
    for (name, frame) in canonical() {
        let bytes = postcard::to_stdvec(&frame).expect("encode");
        println!("    (\"{name}\", \"{}\"),", hexfmt::encode(&bytes));
    }
}

#[test]
fn wire_format_is_frozen() {
    let canon = canonical();
    assert_eq!(
        GOLDEN.len(),
        canon.len(),
        "golden table has {} entries but canonical() produces {} — regenerate: \
         cargo test -p alloyfs-proto print_goldens -- --ignored --nocapture",
        GOLDEN.len(),
        canon.len()
    );
    for ((golden_name, expected_hex), (name, frame)) in GOLDEN.iter().zip(&canon) {
        assert_eq!(golden_name, name, "golden table order diverged from canonical()");
        let actual_hex = hexfmt::encode(&postcard::to_stdvec(frame).expect("encode"));
        assert_eq!(
            *expected_hex, actual_hex,
            "wire encoding of `{name}` changed\n  expected: {expected_hex}\n  actual:   {actual_hex}\n\
             wire format changed — if intentional, bump PROTO_VERSION_MAX (and MIN if breaking) in \
             crates/alloyfs-proto/src/messages.rs, then regenerate: \
             cargo test -p alloyfs-proto print_goldens -- --ignored --nocapture"
        );
    }
}

#[test]
fn goldens_decode() {
    for (name, hex) in GOLDEN {
        let bytes = hexfmt::decode(hex);
        postcard::from_bytes::<Frame>(&bytes)
            .unwrap_or_else(|e| panic!("golden `{name}` failed to decode as Frame: {e}"));
    }
}
