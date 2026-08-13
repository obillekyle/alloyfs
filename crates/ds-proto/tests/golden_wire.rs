//! Golden wire-format test: freezes the postcard payload encoding (no length
//! prefix) of every enum variant in ds-proto. If any byte here changes, a
//! deployed peer speaking the old encoding can no longer talk to us — so a
//! failure means "decide about PROTO_VERSION", never "update the test blindly".

use std::time::{Duration, SystemTime};

use bytes::Bytes;
use ds_proto::{
    Attr, DirEntry, ErrorCode, EventKind, FileKind, Frame, FsEvent, LockKind, OpenFlags, RelPath, Request,
    Response, PROTO_VERSION_MAX, PROTO_VERSION_MIN,
};

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
        Request::MountDefaults => {} // v2: golden added, PROTO_VERSION_MAX bumped
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
/// `cargo test -p ds-proto print_goldens -- --ignored --nocapture`
#[rustfmt::skip]
const GOLDEN: &[(&str, &str)] = &[
    // hello/hello_ack embed PROTO_VERSION_MAX — they legitimately move when
    // the protocol version bumps (v2: MountDefaults negotiation).
    ("hello", "00010206676f6c64656e"),
    ("hello_ack", "010206676f6c64656e"),
    ("ping", "0507"),
    ("pong", "0607"),
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
         cargo test -p ds-proto print_goldens -- --ignored --nocapture",
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
             crates/ds-proto/src/messages.rs, then regenerate: \
             cargo test -p ds-proto print_goldens -- --ignored --nocapture"
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
