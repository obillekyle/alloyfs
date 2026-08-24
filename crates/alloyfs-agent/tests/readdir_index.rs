//! Readdir served from the v6 index.
//!
//! When an export's tree index is live, `Readdir` answers from it instead of
//! re-reading and re-stat'ing the whole directory per page. What has to hold
//! is that a client cannot tell the difference — same entries, same order,
//! same page boundaries, same versions, same excludes — except in speed.
//! These tests run the same directory through both paths (`tree_max_entries:
//! Some(0)` forces the export unindexed, so its Readdir is the disk path) and
//! compare.

use std::sync::Arc;
use std::time::SystemTime;

use alloyfs_agent::{AgentConfig, AgentSession, ExportConfig, ExportRegistry};
use alloyfs_proto::{
    DirEntry, ErrorCode, Frame, FrameCodec, RelPath, Request, Response, PROTO_VERSION_MAX, PROTO_VERSION_MIN,
};
use alloyfs_transport::{serve_connection, RequestHandler};
use futures::{SinkExt, StreamExt};
use tokio_util::codec::Framed;

fn registry(dir: &std::path::Path, tree_cap: Option<usize>, exclude: Vec<String>) -> Arc<ExportRegistry> {
    let mut cfg = AgentConfig::default();
    cfg.exports.insert(
        "test".into(),
        ExportConfig {
            path: dir.to_path_buf(),
            read_only: false,
            exclude,
            tree_max_entries: tree_cap,
            ..Default::default()
        },
    );
    Arc::new(ExportRegistry::from_config(&cfg).expect("export registry"))
}

/// A readdir cursor is `(generation << 32) | position` — the agent writes it
/// and the agent reads it back, so the encoding is private to ops.rs and
/// mirrored here rather than exported.
fn cursor_pos(cursor: u64) -> usize {
    (cursor & 0xffff_ffff) as usize
}

fn cursor_generation(cursor: u64) -> u64 {
    cursor >> 32
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
            client: "readdir".into(),
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

    /// The first `TreeToken` request is what builds the index; the token says
    /// whether it is live (nonzero) or the export is unindexed (0).
    async fn tree_token(&mut self) -> u64 {
        match self.call(Request::TreeToken).await {
            Ok(Response::TreeToken { token }) => token,
            other => panic!("expected TreeToken, got {other:?}"),
        }
    }

    async fn readdir_page(&mut self, path: &str, cursor: u64) -> (Vec<DirEntry>, Option<u64>) {
        match self
            .call(Request::Readdir {
                path: RelPath(path.into()),
                cursor,
            })
            .await
        {
            Ok(Response::Dir { entries, next_cursor }) => (entries, next_cursor),
            other => panic!("expected Dir, got {other:?}"),
        }
    }

    /// Every page of `path` in cursor order, plus each page's size, so tests
    /// can compare the paging SHAPE as well as the content.
    async fn readdir_all(&mut self, path: &str) -> (Vec<DirEntry>, Vec<usize>) {
        let mut all = Vec::new();
        let mut sizes = Vec::new();
        let mut cursor = 0u64;
        loop {
            let (page, next) = self.readdir_page(path, cursor).await;
            sizes.push(page.len());
            all.extend(page);
            match next {
                Some(n) => {
                    assert_eq!(cursor_pos(n), all.len(), "the cursor advances by entries served");
                    assert!(
                        cursor_generation(n) != 0,
                        "a listing that pages must name the snapshot it is paging"
                    );
                    if cursor != 0 {
                        assert_eq!(
                            cursor_generation(n),
                            cursor_generation(cursor),
                            "one scan pages one snapshot from end to end"
                        );
                    }
                    cursor = n;
                }
                None => break,
            }
        }
        (all, sizes)
    }
}

fn names(entries: &[DirEntry]) -> Vec<&str> {
    entries.iter().map(|e| e.name.as_str()).collect()
}

/// The load-bearing equivalence: a directory big enough to page must list
/// identically through the index and through the disk — same names, same
/// order, same page boundaries, same cursors, same attrs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paging_matches_between_index_and_disk() {
    let dir = tempfile::tempdir().expect("tempdir");
    // More entries than one page (READDIR_PAGE = 1024), plus a subdirectory
    // with children so the direct-child filter is exercised: its children
    // must appear under it and never in the parent's pages.
    for i in 0..1100 {
        std::fs::write(dir.path().join(format!("f{i:04}.txt")), b"x").unwrap();
    }
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    for i in 0..3 {
        std::fs::write(dir.path().join("sub").join(format!("in{i}")), b"y").unwrap();
    }

    let indexed = registry(dir.path(), None, vec![]);
    let mut ip = Peer::connect(&indexed).await;
    assert_ne!(ip.tree_token().await, 0, "the index must be live");
    let (from_index, index_sizes) = ip.readdir_all("").await;

    // Same directory, indexing off: this Readdir is the disk path.
    let disk = registry(dir.path(), Some(0), vec![]);
    let mut dp = Peer::connect(&disk).await;
    assert_eq!(dp.tree_token().await, 0, "cap 0 must leave the export unindexed");
    let (from_disk, disk_sizes) = dp.readdir_all("").await;

    assert!(index_sizes.len() >= 2, "the directory must actually page");
    assert_eq!(index_sizes, disk_sizes, "page boundaries must agree");
    assert_eq!(from_index.len(), 1101, "1100 files and one subdirectory");
    assert_eq!(
        names(&from_index),
        names(&from_disk),
        "names and order must agree"
    );
    for (a, b) in from_index.iter().zip(&from_disk) {
        assert_eq!(a.attr, b.attr, "attrs for {:?} must agree", a.name);
    }

    // A non-root directory answers through the index's prefix scan; it has to
    // match the disk too, and carry exactly its own children.
    let (sub_index, _) = ip.readdir_all("sub").await;
    let (sub_disk, _) = dp.readdir_all("sub").await;
    assert_eq!(names(&sub_index), ["in0", "in1", "in2"]);
    assert_eq!(names(&sub_index), names(&sub_disk));
}

/// The version-0 trap: the index stores every attr with version 0 (they come
/// from plain stats), and autocache freshness checks treat "either side 0" as
/// unknowable. An index-served page must overlay the live per-path version.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn index_served_entries_carry_live_versions() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("bumped.txt"), b"a").unwrap();
    std::fs::write(dir.path().join("untouched.txt"), b"b").unwrap();
    let registry = registry(dir.path(), None, vec![]);
    let mut peer = Peer::connect(&registry).await;
    assert_ne!(peer.tree_token().await, 0, "the index must be live");

    // Any mutation through the agent bumps the path's version.
    let bumped = match peer
        .call(Request::Setattr {
            path: RelPath("bumped.txt".into()),
            size: None,
            mtime: Some(SystemTime::now()),
            mode: None,
        })
        .await
    {
        Ok(Response::Attr(a)) => a.version,
        other => panic!("expected Attr, got {other:?}"),
    };
    assert_ne!(bumped, 0, "a setattr must bump the version");

    let (entries, next) = peer.readdir_page("", 0).await;
    assert_eq!(next, None);
    let version_of = |name: &str| {
        entries
            .iter()
            .find(|e| e.name == name)
            .unwrap_or_else(|| panic!("{name} missing from the listing"))
            .attr
            .version
    };
    assert_eq!(
        version_of("bumped.txt"),
        bumped,
        "the live version must be overlaid on the index's stored 0"
    );
    assert_eq!(version_of("untouched.txt"), 0, "no mutation, no version");
}

/// Excluded names must be invisible through BOTH paths. The index never
/// contains them (the walk filters, and the watcher's Coalescer drops their
/// events), but that property has to be observable from the wire, not just
/// true in the data structure.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn excluded_names_never_appear_in_either_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("visible.txt"), b"v").unwrap();
    std::fs::write(dir.path().join("hidden.secret"), b"h").unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    std::fs::write(dir.path().join("sub").join("also.secret"), b"h").unwrap();
    std::fs::write(dir.path().join("sub").join("plain.txt"), b"p").unwrap();

    for cap in [None, Some(0)] {
        let registry = registry(dir.path(), cap, vec!["*.secret".into()]);
        let mut peer = Peer::connect(&registry).await;
        let token = peer.tree_token().await;
        assert_eq!(token != 0, cap.is_none(), "cap {cap:?} decides which path serves");

        let (root, _) = peer.readdir_all("").await;
        assert_eq!(names(&root), ["sub", "visible.txt"], "cap {cap:?}");
        let (sub, _) = peer.readdir_all("sub").await;
        assert_eq!(names(&sub), ["plain.txt"], "cap {cap:?}");
    }
}

/// A directory big enough to page parks a listing snapshot after its first
/// page (continuation pages slice it instead of rebuilding the world). A
/// mutation racing that pagination must reach the NEXT listing, while the
/// in-flight one completes on what it started with.
///
/// The next listing is the easy half: a new scan never consults a snapshot.
/// The other half used to be the reverse — the version funnel dropped the
/// snapshot, so the continuation rebuilt and spliced the create in. This
/// test asserted exactly that, and had to pick a name sorting LAST to do it,
/// because a rebuild is indexed by the OLD position and any shift in the
/// range ahead of the cursor loses entries. See
/// `a_delete_mid_listing_does_not_skip_untouched_entries` for the same shape
/// without the dodge.
///
/// The DISK path is where this is observable: the index path can't show it
/// in a watcherless rig, since agent-mediated creates only reach the index
/// through the event feed, which is the watcher's — pinned by
/// `a_live_index_is_what_answers_readdir`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mutation_mid_listing_reaches_the_next_listing() {
    let dir = tempfile::tempdir().expect("tempdir");
    for i in 0..1100 {
        std::fs::write(dir.path().join(format!("f{i:04}.txt")), b"x").unwrap();
    }
    let registry = registry(dir.path(), Some(0), vec![]);
    let mut peer = Peer::connect(&registry).await;
    assert_eq!(
        peer.tree_token().await,
        0,
        "cap 0 must leave the export unindexed"
    );

    let (page0, next) = peer.readdir_page("", 0).await;
    let cursor = next.expect("1100 entries must page");

    // Through the agent, so the funnel (bump) sees it. The name sorts last:
    // the continuation's remaining range is not shifted by the insert, so
    // the completed listing can be asserted exactly.
    let created = peer
        .call(Request::Create {
            path: RelPath("zz-mid-listing.txt".into()),
            flags: alloyfs_proto::OpenFlags {
                read: true,
                write: true,
                ..Default::default()
            },
            mode: 0o644,
        })
        .await;
    assert!(matches!(created, Ok(Response::Opened { .. })), "got {created:?}");

    // The in-flight continuation completes on the listing it STARTED. The
    // create landed after the first page was served, and POSIX leaves an
    // entry added during a scan unspecified — so completing consistently is
    // a choice, and it is the one that lets the position in the cursor keep
    // meaning something.
    let mut rest = Vec::new();
    let mut c = cursor;
    loop {
        let (page, next) = peer.readdir_page("", c).await;
        rest.extend(page);
        match next {
            Some(n) => c = n,
            None => break,
        }
    }
    assert_eq!(
        page0.len() + rest.len(),
        1100,
        "the scan completes on the snapshot it started"
    );
    assert!(
        !rest.iter().any(|e| e.name == "zz-mid-listing.txt"),
        "an entry created after the scan began is not spliced into it"
    );

    // And a fresh listing agrees end to end.
    let (all, _) = peer.readdir_all("").await;
    assert_eq!(all.len(), 1101);
    assert!(all.iter().any(|e| e.name == "zz-mid-listing.txt"));
}

/// Deleting one entry mid-scan must not take an unrelated entry with it.
///
/// Pages are addressed by POSITION. Dropping the snapshot behind a running
/// scan meant the next page rebuilt a listing one entry SHORTER and applied
/// the old position to it, so whatever had shifted down into that slot was
/// stepped straight over — never served, never reported missing. Here the
/// scan has already been handed positions 0..1024 when position 0 is
/// removed; every later name shifts down one, and the entry that lands on
/// 1024 is `f1024.txt`, which nothing touched.
///
/// POSIX permits a scan to miss what is added or removed while it runs. It
/// does not permit it to miss what sat still. `find -delete` and a shell
/// loop over a directory are both this exact interleaving.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_delete_mid_listing_does_not_skip_untouched_entries() {
    let dir = tempfile::tempdir().expect("tempdir");
    for i in 0..1100 {
        std::fs::write(dir.path().join(format!("f{i:04}.txt")), b"x").unwrap();
    }
    let registry = registry(dir.path(), Some(0), vec![]);
    let mut peer = Peer::connect(&registry).await;

    let (page0, next) = peer.readdir_page("", 0).await;
    assert_eq!(page0.len(), 1024, "a full first page is what sets up the shift");
    let cursor = next.expect("1100 entries must page");

    // Through the agent, so the version funnel sees it — the funnel is what
    // used to discard the snapshot this scan is paging.
    let removed = peer
        .call(Request::Unlink {
            path: RelPath("f0000.txt".into()),
        })
        .await;
    assert!(matches!(removed, Ok(Response::Ok)), "got {removed:?}");

    let mut rest = Vec::new();
    let mut c = cursor;
    loop {
        let (page, next) = peer.readdir_page("", c).await;
        rest.extend(page);
        match next {
            Some(n) => c = n,
            None => break,
        }
    }

    assert!(
        rest.iter().any(|e| e.name == "f1024.txt"),
        "f1024.txt shifted into the position the cursor names; it was never touched \
         and must still be served"
    );
    assert_eq!(
        page0.len() + rest.len(),
        1100,
        "the scan completes on its own snapshot: every entry once, none dropped"
    );

    // The delete is not hidden — it is simply not retrofitted into a scan
    // that started before it.
    let (all, _) = peer.readdir_all("").await;
    assert_eq!(all.len(), 1099);
    assert!(!all.iter().any(|e| e.name == "f0000.txt"));
}

/// Pin WHICH path answers. No watcher runs in these tests, so a file created
/// behind the agent's back is the one observable difference: the disk path
/// would list it, a live index cannot know it yet. (In production the watcher
/// folds such a change into the index within its debounce; what is asserted
/// here is the routing, not the freshness contract.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_live_index_is_what_answers_readdir() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("a.txt"), b"a").unwrap();
    let registry = registry(dir.path(), None, vec![]);
    let mut peer = Peer::connect(&registry).await;
    assert_ne!(peer.tree_token().await, 0, "the index must be live");

    std::fs::write(dir.path().join("b.txt"), b"b").unwrap();

    // Getattr goes to disk and proves the file exists...
    let got = peer
        .call(Request::Getattr {
            path: RelPath("b.txt".into()),
        })
        .await;
    assert!(matches!(got, Ok(Response::Attr(_))), "got {got:?}");
    // ...so a listing without it can only have come from the index.
    let (entries, _) = peer.readdir_page("", 0).await;
    assert_eq!(names(&entries), ["a.txt"], "the index, not the disk, answered");

    // Everything the index cannot answer exactly still falls through to the
    // disk path and errors exactly as before.
    let missing = peer
        .call(Request::Readdir {
            path: RelPath("nope".into()),
            cursor: 0,
        })
        .await;
    assert!(
        matches!(missing, Err(ErrorCode::NotFound)),
        "a directory the index has never heard of: {missing:?}"
    );
    let not_dir = peer
        .call(Request::Readdir {
            path: RelPath("a.txt".into()),
            cursor: 0,
        })
        .await;
    assert!(not_dir.is_err(), "listing a file must still fail: {not_dir:?}");
}

/// A symlink must list identically through both paths, and must list the
/// TARGET's attrs — the contract `getattr` already keeps.
///
/// The two paths used to disagree here. The index path followed the link with
/// a bare `fs::metadata`; the disk path carried a comment claiming
/// `DirEntry::metadata` follows symlinks, which std documents it does not — so
/// it served the link's own attrs and nobody noticed, because the difference
/// only shows on an export where indexing happened to be on or off.
///
/// Unix-only: creating a symlink on Windows needs Developer Mode or an
/// elevated process, which a test must not require. The code under test is
/// platform-independent, and the Linux leg of the gate runs this.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_symlink_lists_its_target_through_both_paths() {
    let dir = tempfile::tempdir().expect("tempdir");
    // 4096 bytes, so the target's size cannot be confused with the link's own
    // (a link's size is the byte length of its target path — here, 8).
    std::fs::write(dir.path().join("real.bin"), vec![7u8; 4096]).unwrap();
    std::os::unix::fs::symlink("real.bin", dir.path().join("link.bin")).unwrap();

    let indexed = registry(dir.path(), None, vec![]);
    let mut ip = Peer::connect(&indexed).await;
    assert_ne!(ip.tree_token().await, 0, "the index must be live");
    let (from_index, _) = ip.readdir_all("").await;

    let disk = registry(dir.path(), Some(0), vec![]);
    let mut dp = Peer::connect(&disk).await;
    assert_eq!(dp.tree_token().await, 0, "cap 0 must leave the export unindexed");
    let (from_disk, _) = dp.readdir_all("").await;

    let pick = |v: &[DirEntry], n: &str| v.iter().find(|e| e.name == n).cloned().expect("entry");
    let li = pick(&from_index, "link.bin");
    let ld = pick(&from_disk, "link.bin");

    assert_eq!(li.attr, ld.attr, "a link must list the same through both paths");
    assert_eq!(
        li.attr.size, 4096,
        "the listing must report the target's size, as getattr does"
    );
    assert_eq!(
        li.attr.kind,
        pick(&from_index, "real.bin").attr.kind,
        "a followed link lists as what it points at"
    );
}

/// A symlink pointing OUT of the export must not publish its target's attrs.
///
/// This is why the follow goes through `resolve` and not `fs::metadata`:
/// resolve canonicalizes and refuses anything landing outside the root. The
/// index path used to follow raw, so the size and mtime of a file the client
/// can never open — and that the export boundary exists to hide — were served
/// in the listing.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_link_out_of_the_export_lists_as_itself() {
    let outside = tempfile::tempdir().expect("outside");
    let secret = outside.path().join("secret.bin");
    std::fs::write(&secret, vec![9u8; 8192]).unwrap();

    let dir = tempfile::tempdir().expect("tempdir");
    std::os::unix::fs::symlink(&secret, dir.path().join("escape.bin")).unwrap();

    for cap in [None, Some(0)] {
        let reg = registry(dir.path(), cap, vec![]);
        let mut peer = Peer::connect(&reg).await;
        peer.tree_token().await;
        let (entries, _) = peer.readdir_all("").await;
        let e = entries
            .iter()
            .find(|e| e.name == "escape.bin")
            .expect("the link still lists");
        assert_ne!(
            e.attr.size, 8192,
            "cap {cap:?}: the size of a file outside the export must not be served"
        );
        assert_eq!(
            e.attr.kind,
            alloyfs_proto::FileKind::Symlink,
            "cap {cap:?}: an unfollowable link lists as a link"
        );
    }
}

/// A dangling link still lists, with its own attrs.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dangling_link_still_lists() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::os::unix::fs::symlink("nothing-here", dir.path().join("broken")).unwrap();

    for cap in [None, Some(0)] {
        let reg = registry(dir.path(), cap, vec![]);
        let mut peer = Peer::connect(&reg).await;
        peer.tree_token().await;
        let (entries, _) = peer.readdir_all("").await;
        let e = entries
            .iter()
            .find(|e| e.name == "broken")
            .unwrap_or_else(|| panic!("cap {cap:?}: a broken link must still list"));
        assert_eq!(e.attr.kind, alloyfs_proto::FileKind::Symlink);
    }
}
