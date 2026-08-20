use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::SystemTime;

use crate::error::ErrorCode;

/// Protocol versions this build can speak. The handshake picks
/// `min(client_max, server_max)`; a peer whose range doesn't overlap ours is
/// rejected. Bump MAX when the protocol grows, MIN only on breaking changes.
///
/// v2: `Request::MountDefaults` / `Response::MountDefaults` — clients only
/// send it when the negotiated version is >= 2, so v1 peers never see the
/// (to them undecodable) new variants.
///
/// v3: `Request::Auth` + `ErrorCode::AuthRequired` (TCP shared-secret auth)
/// and `Frame::Compressed` (transparent large-frame compression). Both sides
/// send the new variants only when the negotiated version is >= 3.
///
/// v4: `Request::Symlink` / `Request::ReadLink` + `Response::Target`.
/// Symlinks could previously be read (resolved server-side) but never
/// created through a mount. Gated the same way: a v3 peer cannot decode
/// these variants, so they are sent only when the negotiated version is >= 4.
///
/// v5: `Response::WrittenAttr` — a write reply that carries the file's
/// post-write attributes. The gate is on the SERVER here rather than the
/// client, because it is the server that chooses the reply shape: it answers
/// with `Written` below v5 and `WrittenAttr` at v5+, and a v5 client accepts
/// either.
///
/// v6: `Request::Tree` / `Request::TreeToken` + `Response::Tree` /
/// `Response::TreeToken` — the server indexes an export and serves the whole
/// subtree in one exchange, plus a content-derived token for "has anything
/// changed at all". Gated on the client, which simply keeps using `Readdir`
/// against an older peer; an indexed export is an optimisation, never a
/// requirement.
///
/// v7: `Request::LockRange` / `UnlockRange` / `TestLock` +
/// `Response::LockStatus` — real POSIX byte-range advisory locks, replacing
/// the whole-file coarsening of `Lock`/`Unlock`. Gated on the client, which
/// falls back to the coarse pair against an older peer. The coarsening was
/// not merely imprecise: it applied to release as well as to acquire, so a
/// partial unlock dropped every lock the handle held, and SQLite performs
/// exactly that sequence on every read transaction.
///
/// v8: `Request::ReadMany` / `Response::Many` — the contents of many files in
/// one exchange, which is the tree's idea applied to bytes. Gated on the
/// client, which falls back to open+read per file against an older peer.
/// Measured before it was proposed: 200 files totalling 8 KB cost 35.9 s
/// against ~4 ms of actual transfer.
///
/// v9: `OpenRead`/`OpenedData` (open + first read, one exchange), `Setattr2`
/// (adds `readonly`, applied server-side against the current mode), and
/// `Attach2`/`Attached2` (attach + mount defaults + tree token in one).
/// All client-gated with per-feature fallbacks to the older verbs.
///
/// v10: `WriteMany` / `RemoveMany` / `SetattrMany` + `Response::ManyOutcome`
/// — the mutation-side siblings of `ReadMany`, fed by the client's write
/// batcher: a burst of small-file creates or deletes coalesces into one
/// exchange instead of two round trips per file. Client-gated; against an
/// older peer the batcher never engages and every mutation takes the classic
/// per-operation path.
///
/// v11: Windows attribute bits (Hidden, System) ride the HIGH bits of
/// `Attr::mode` (`MODE_WIN_*`), plus `Request::SetWinAttrs` to change them.
/// `Attr` itself cannot grow — it is embedded in every listing, tree page
/// and reply, so a new field would break every older peer's decode — and
/// the POSIX mode uses only the low bits, so the high bits are the one
/// extension point that costs nothing. Gated on BOTH sides: a server
/// attaches the bits only on v11+ sessions and strips them below; a client
/// sends `SetWinAttrs` only at v11+, and masks `MODE_WIN_MASK` off before
/// any value reaches a real chmod or st_mode. Servers store the bits
/// natively where the filesystem has them (a Windows agent uses the real
/// NTFS attributes) and in the export's `.alloyfs/winattrs.json` sidecar
/// where it does not (Linux exports) — the sidecar travels with the data,
/// and `.alloyfs` is server-excluded from every listing.
pub const PROTO_VERSION_MIN: u16 = 1;
pub const PROTO_VERSION_MAX: u16 = 11;

/// The protocol range this build speaks, for `--version` and diagnostics —
/// "which wire version does this release talk" should not require reading
/// source. A literal rather than a formatted string because clap's version
/// output needs a `&'static str`; `proto_range_matches_the_constants` is what
/// keeps it from drifting away from the two constants above.
pub const PROTO_RANGE: &str = "1-11";

/// Read/write payloads are capped to this many bytes per request so one huge
/// file operation can never monopolize the connection (head-of-line blocking).
pub const DATA_CHUNK: u32 = 128 * 1024;

/// v11+: Windows attribute bits, carried in the HIGH bits of [`Attr::mode`].
///
/// The POSIX mode occupies the low 12 bits; these sit far above anything a
/// chmod accepts, and every consumer masks them off (`mode & !MODE_WIN_MASK`)
/// before a mode reaches an OS as a mode. Attached by servers only on v11+
/// sessions and stripped below, so an older peer never sees a bit it would
/// misread as permissions.
pub const MODE_WIN_HIDDEN: u32 = 1 << 20;
pub const MODE_WIN_SYSTEM: u32 = 1 << 21;
pub const MODE_WIN_MASK: u32 = MODE_WIN_HIDDEN | MODE_WIN_SYSTEM;

/// A path relative to an export root: UTF-8, `/`-separated, no leading `/`,
/// never containing `.` or `..` components, and never containing `\` or `:`.
///
/// Enforced in BOTH directions. The server validates what clients send, and
/// the `Deserialize` impl below validates every `RelPath` that arrives off the
/// wire — including the ones a *server* sends a client in events, listings and
/// tree entries. A client that trusts a path from its peer builds a local path
/// out of it (the sync engine joins components onto the sync root), so an
/// unvalidated `..` from a hostile or compromised agent walks straight out of
/// the tree it was supposed to stay in.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, PartialOrd, Ord)]
pub struct RelPath(pub String);

/// Validated on the way in, so no consumer has to remember to ask. postcard is
/// not self-describing and a malformed frame already drops the connection, so
/// failing here is consistent with how every other decode error is handled.
impl<'de> serde::Deserialize<'de> for RelPath {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let p = RelPath(String::deserialize(d)?);
        p.validate_wire()
            .map_err(|_| serde::de::Error::custom("path is not a valid RelPath"))?;
        Ok(p)
    }
}

impl RelPath {
    /// The rules that hold on EVERY platform, checked on every `RelPath` that
    /// arrives off the wire — in both directions.
    ///
    /// Deliberately narrow: it rejects only what no legitimate path can
    /// contain, because a rule that fires on a legal filename is not a
    /// safety check, it is an outage. A server watching a Linux export emits
    /// events naming whatever files exist there, so a decode rule that
    /// rejected, say, every colon would drop the client's connection every
    /// time anyone touched `log-10:30:00.txt`, forever. Two things qualify:
    ///
    /// - **Traversal** — a leading `/`, an empty component, `.` or `..`. None
    ///   can appear in a filename on any supported platform, and `..` is what
    ///   walks a peer-supplied path out of the root it was meant to stay in.
    /// - **A Windows drive reference** — a component beginning `X:`.
    ///   `PathBuf::join` REPLACES its buffer when the argument carries a drive
    ///   prefix rather than appending under it, so `C:evil.txt` spliced onto a
    ///   checked parent resolves against drive C's working directory, outside
    ///   the export — while still reporting `is_absolute() == false`, so a
    ///   guard looking for absolute paths never sees it. Measured, not assumed.
    ///   Only a single ASCII letter before the colon forms a prefix (Rust's
    ///   own parser reads `a:b:c` that way too, and `log-10:30:00.txt` not at
    ///   all), which is why this rejects far less than "contains a colon".
    pub fn validate_wire(&self) -> Result<(), ErrorCode> {
        let s = &self.0;
        if s.starts_with('/') {
            return Err(ErrorCode::InvalidPath);
        }
        for c in s.split('/') {
            if (c.is_empty() && !s.is_empty()) || c == "." || c == ".." {
                return Err(ErrorCode::InvalidPath);
            }
            let b = c.as_bytes();
            if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
                return Err(ErrorCode::InvalidPath);
            }
        }
        Ok(())
    }

    /// What a SERVER additionally requires of a path a client sent it:
    /// `validate_wire`, plus no `\`. That extra rule keeps one spelling per
    /// path on a Windows server, where `\` is a separator; it costs a Linux
    /// export the ability to name a file with a backslash in it, which is the
    /// trade this has always made. It is not applied at decode, because the
    /// cost of being wrong there is a reconnect loop rather than an error.
    pub fn validate(&self) -> Result<(), ErrorCode> {
        if self.0.contains('\\') {
            return Err(ErrorCode::InvalidPath);
        }
        self.validate_wire()
    }

    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    pub fn join(&self, name: &str) -> RelPath {
        if self.0.is_empty() {
            RelPath(name.to_string())
        } else {
            RelPath(format!("{}/{}", self.0, name))
        }
    }

    /// (parent, file name); root has neither.
    pub fn split(&self) -> Option<(RelPath, &str)> {
        if self.is_root() {
            return None;
        }
        match self.0.rsplit_once('/') {
            Some((parent, name)) => Some((RelPath(parent.to_string()), name)),
            None => Some((RelPath(String::new()), &self.0)),
        }
    }
}

impl fmt::Debug for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RelPath({:?})", self.0)
    }
}

impl fmt::Display for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_root() {
            f.write_str("/")
        } else {
            f.write_str(&self.0)
        }
    }
}

/// File kind + metadata as the server reports it. Platform-specific fields
/// (uid/gid, Windows attributes) are synthesized client-side by each backend.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum FileKind {
    File,
    Dir,
    Symlink,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Attr {
    pub kind: FileKind,
    pub size: u64,
    pub mtime: SystemTime,
    pub ctime: SystemTime,
    /// Unix permission bits as reported by the server (0o644-style). The
    /// Windows backend maps read-only from the write bit.
    pub mode: u32,
    /// Server-side monotonic version of this file; bumped on every mutation.
    /// The client uses it for cache freshness and conflict detection.
    pub version: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    pub attr: Attr,
}

/// One entry of a `Tree` reply.
///
/// Carries a `path` where [`DirEntry`] carries a `name`, because a tree page
/// spans directories and a bare name would be ambiguous. Relative to the root
/// the `Tree` request named, so a client can rebase a subtree without
/// rewriting every entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreeEntry {
    pub path: RelPath,
    pub attr: Attr,
}

/// Open intent, kept deliberately smaller than POSIX's O_* zoo.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct OpenFlags {
    pub read: bool,
    pub write: bool,
    pub truncate: bool,
    pub append: bool,
    /// Creation must fail if the path already exists (O_EXCL). Without it, a
    /// Create that loses a race falls back to opening the existing file —
    /// POSIX open(O_CREAT) semantics.
    pub excl: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum LockKind {
    Shared,
    Exclusive,
}

/// One coalesced change on the server's real filesystem.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsEvent {
    /// Per-export monotonic sequence number (gap-free within one batch run).
    pub seq: u64,
    pub kind: EventKind,
    pub path: RelPath,
    /// New version of the file, when the event implies one (Created/Modified).
    pub new_version: Option<u64>,
    /// Set when the change was made through alloyfs by a connected session;
    /// clients skip self-echo using this.
    pub origin: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EventKind {
    Created,
    Modified,
    AttrChanged,
    Removed,
    RenamedFrom {
        to: RelPath,
    },
    /// Event log rotated past this subscriber (or watcher overflow): caches
    /// can no longer be patched incrementally and must be flushed.
    ResyncRequired,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    Attach {
        export: String,
    },
    Getattr {
        path: RelPath,
    },
    Readdir {
        path: RelPath,
        cursor: u64,
    },
    Open {
        path: RelPath,
        flags: OpenFlags,
    },
    Create {
        path: RelPath,
        flags: OpenFlags,
        mode: u32,
    },
    Read {
        fh: u64,
        offset: u64,
        len: u32,
    },
    Write {
        fh: u64,
        offset: u64,
        data: Bytes,
        expect_version: Option<u64>,
    },
    Flush {
        fh: u64,
    },
    Release {
        fh: u64,
    },
    Setattr {
        path: RelPath,
        size: Option<u64>,
        mtime: Option<SystemTime>,
        mode: Option<u32>,
    },
    Mkdir {
        path: RelPath,
        mode: u32,
    },
    Unlink {
        path: RelPath,
    },
    Rmdir {
        path: RelPath,
    },
    Rename {
        from: RelPath,
        to: RelPath,
        replace: bool,
    },
    Lock {
        fh: u64,
        kind: LockKind,
        wait: bool,
    },
    Unlock {
        fh: u64,
    },
    Subscribe {
        since_seq: Option<u64>,
    },
    Statfs,
    /// Hard link: `link` becomes a second name for existing `target`. Only
    /// within one export — cross-device links are impossible by definition
    /// (two directory entries must share one inode). Appended last: postcard
    /// encodes variant indices, so old peers stay compatible.
    Link {
        target: RelPath,
        link: RelPath,
    },
    /// v2+: ask for the attached export's suggested client settings
    /// (overlay excludes, pins, cache sizes). Send ONLY when the negotiated
    /// protocol version is >= 2 — v1 peers cannot decode this variant.
    MountDefaults,
    /// v3+: authenticate a TCP session with the agent's shared secret
    /// (`agent.tcp_token`). Must precede every other request when the server
    /// requires it; ssh/stdio sessions are pre-authenticated by ssh itself.
    Auth {
        token: String,
    },
    /// v4+: create a symbolic link at `link` pointing at `target`.
    ///
    /// `target` is an opaque string, not a `RelPath`: a symlink's target is
    /// whatever text the creator chose and may be relative ("../sibling"),
    /// which is not a valid export-relative path and must not be validated as
    /// one. The server resolves it — relative to the link's own directory —
    /// and refuses anything landing outside the export, the same rule already
    /// applied when reading them.
    Symlink {
        target: String,
        link: RelPath,
    },
    /// v4+: read a symbolic link's target, verbatim as stored.
    ReadLink {
        path: RelPath,
    },
    /// v6+: the whole subtree under `path`, one reply instead of one round
    /// trip per directory.
    ///
    /// This exists because a mount is latency-bound. Walking an export the
    /// old way costs one `Readdir` per directory, and the client's cache
    /// walker measured 535 directories in 35.8 s against a 60 ms link — 535
    /// round trips, almost all of it waiting. The server holds the same
    /// information: enumerating 3601 entries with stat took 33 ms locally,
    /// and the result compressed to 44 KB. One round trip carries what used
    /// to take hundreds.
    ///
    /// Paginated on `cursor` because a big export cannot fit in one frame
    /// (`MAX_FRAME_LEN`), not because the client wants it piecemeal. An
    /// export the server declined to index answers `Unsupported`, and the
    /// client falls back to per-directory `Readdir`.
    Tree {
        path: RelPath,
        cursor: Option<u64>,
    },
    /// v6+: the export's current tree token — one cheap exchange that answers
    /// "has anything at all changed".
    ///
    /// Unlike an event sequence number, this is derived from the tree's
    /// CONTENT rather than from a session counter, so it is the same value
    /// after the agent restarts. That distinction is the whole point: an
    /// `ssh://` agent is spawned per connection and its sequence numbering
    /// begins again every mount, which makes a sequence useless for deciding
    /// whether a cache from an earlier mount is still good. A token is not.
    TreeToken,
    /// v7+: a POSIX byte-range advisory lock.
    ///
    /// The pre-v7 `Lock`/`Unlock` above coarsen every range to the whole
    /// file. That over-locks when taking, which is merely strict — and
    /// UNDER-locks when releasing, which is not: `Unlock` drops every lock the
    /// handle holds on the path, so an application holding two disjoint ranges
    /// and releasing one is left believing it still holds the other while the
    /// server holds nothing. SQLite performs exactly that sequence on every
    /// read transaction (read-lock PENDING, read-lock the shared range, unlock
    /// PENDING), which is why databases could not live on a mount.
    ///
    /// `len == 0` means "to the end of the file", matching `fcntl`'s
    /// `l_len == 0`.
    ///
    /// `owner` is the lock owner, distinct from `fh`: two handles sharing an
    /// owner never conflict with each other, which is what makes an upgrade an
    /// upgrade rather than a self-deadlock. FUSE supplies it as `lock_owner`.
    /// It is carried ALONGSIDE `fh` rather than replacing it — keying locks by
    /// handle gives open-file-description semantics, where closing one
    /// descriptor does not drop locks taken through another, and that is
    /// strictly better than POSIX's drop-all-on-any-close.
    LockRange {
        fh: u64,
        owner: u64,
        kind: LockKind,
        start: u64,
        len: u64,
        wait: bool,
    },
    /// v7+: release exactly `[start, start+len)` and nothing else. Splitting
    /// a held range in two is a normal outcome.
    UnlockRange {
        fh: u64,
        owner: u64,
        start: u64,
        len: u64,
    },
    /// v7+: `fcntl(F_GETLK)` — would this lock be granted, and if not, who
    /// blocks it?
    ///
    /// Answering from a client's local list is not an option: it would report
    /// "free" while another machine held the range, which is worse than the
    /// `ENOLCK`/`ENOSYS` it replaces. SQLite calls this whenever a `-journal`
    /// file exists and treats a failure as an I/O error, so its absence is why
    /// recovery and concurrent writes failed outright rather than degrading.
    TestLock {
        fh: u64,
        owner: u64,
        kind: LockKind,
        start: u64,
        len: u64,
    },
    /// v8+: the whole contents of several files in ONE exchange.
    ///
    /// The tree collapsed per-directory round trips; this does the same for
    /// bytes, and the numbers say it is the larger of the two. Measured:
    /// copying 200 files totalling 8 KB off a mount took **35.9 s** — 179 ms
    /// per file, about three round trips each — while the bytes themselves are
    /// roughly 4 ms of transfer at the link's rate. 99.99% of that was waiting.
    ///
    /// Deliberately by PATH, not by handle. Going through `Open` would put
    /// back one of the round trips this exists to remove, and a bulk consumer
    /// reads each file exactly once, so a handle buys it nothing.
    ///
    /// `budget` caps the bytes the reply may carry. The server stops once it
    /// is spent and answers with FEWER entries than were asked for; the client
    /// sees the short reply and asks for the remainder. No cursor is needed,
    /// because entries come back in request order — the reply is a prefix.
    ///
    /// Where this does NOT help: one large file is already at transport
    /// capacity, since readahead detects sequential access and keeps 32 blocks
    /// in flight (measured 415 MB/s loopback against a 479 ceiling). The whole
    /// gap is per-file fixed cost, which is why the unit here is the file.
    ReadMany {
        paths: Vec<RelPath>,
        budget: u32,
    },
    /// v9+: open a file and carry back its head in the same exchange.
    ///
    /// After ReadMany removed the per-file cost of bulk fetching, the residual
    /// per-file price on a mount was the open+first-read pair — two round
    /// trips for every small file an application touches. This folds them:
    /// the reply carries the handle, the attributes, and up to `len` bytes
    /// from offset 0 (capped at DATA_CHUNK server-side), which the client
    /// plants in its readahead so the following read never leaves the machine.
    ///
    /// `len == 0` asks for no data — the write-only and truncating opens,
    /// where head bytes would be read only to be discarded.
    OpenRead {
        path: RelPath,
        flags: OpenFlags,
        len: u32,
    },
    /// v9+: `Setattr` plus the one attribute Windows kept read-modify-writing.
    ///
    /// A NEW variant rather than a field on `Setattr` — postcard decodes by
    /// shape, so growing an existing variant would make a v8 peer misread the
    /// stream (the same reasoning recorded at `WrittenAttr`).
    ///
    /// `readonly` maps onto the write bits SERVER-side against the file's
    /// current mode, atomically. The WinFsp adapter used to do that mapping
    /// client-side as getattr-then-Setattr, which both cost a round trip and
    /// raced any concurrent chmod between the two.
    Setattr2 {
        path: RelPath,
        size: Option<u64>,
        mtime: Option<SystemTime>,
        mode: Option<u32>,
        readonly: Option<bool>,
    },
    /// v9+: everything a mount needs to become usable, in one exchange.
    ///
    /// `Attach` alone answers with the root attr; a real mount then asks
    /// `MountDefaults` (v2) and `TreeToken` (v6) before it can serve — three
    /// round trips of pure sequence. One reply carrying all three halves the
    /// measured time-to-first-file on a 60 ms link.
    Attach2 {
        export: String,
    },
    /// v10+: create-or-replace several WHOLE small files in ONE exchange —
    /// the write-side sibling of `ReadMany`.
    ///
    /// What it is for: write BURSTS. A create through the mount is two
    /// blocking round trips at minimum (create, then the write), and an
    /// application writing a hundred small files pays them serially —
    /// measured at 212 ms per file on a ~100 ms link, all of it waiting.
    /// The client's batcher coalesces such bursts and sends them here, so a
    /// hundred files cost one or two exchanges instead of two hundred.
    ///
    /// Whole files only, deliberately: an entry replaces the file's entire
    /// content (create-or-truncate + write + close in one server-side step),
    /// which is exactly the shape a small-file burst has. Anything written
    /// partially, randomly, or past the batcher's size cap takes the classic
    /// per-operation path instead.
    ///
    /// Entries apply IN ORDER, and outcomes come back per entry — one bad
    /// path must not poison the batch.
    WriteMany {
        files: Vec<ManyWrite>,
    },
    /// v10+: remove several entries in ONE exchange, in request order.
    ///
    /// The delete-side sibling of `WriteMany`, for `rm -r`-shaped bursts:
    /// removals were already a single round trip each, but a thousand of
    /// them is still a thousand round trips. Ordering matters and is
    /// preserved — a batch may carry children before their parent directory,
    /// or a removal followed (in a later batch) by a re-creation.
    RemoveMany {
        entries: Vec<ManyRemove>,
    },
    /// v10+: apply several metadata changes in ONE exchange, in order.
    ///
    /// The `Setattr2` shape, vectorised — what `chmod -R` and archive
    /// extraction (a timestamp restore per file) turn into.
    SetattrMany {
        entries: Vec<ManySetattr>,
    },
    /// v11+: change the Windows attribute bits (`MODE_WIN_*`) on one path.
    ///
    /// `set` and `clear` are masked INTENTS applied against the file's
    /// current bits server-side — the shape `Setattr2.readonly` uses, for
    /// the same reason: no read-modify-write race against a concurrent
    /// change. Answers `Response::Attr` with the post-change attributes,
    /// high bits included. A Windows server applies them to the real NTFS
    /// attributes; a Linux server persists them in the export's `.alloyfs`
    /// sidecar, which is invisible to every client and travels with the
    /// data.
    SetWinAttrs {
        path: RelPath,
        set: u32,
        clear: u32,
    },
}

/// One `WriteMany` entry: a whole file, created or replaced.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManyWrite {
    pub path: RelPath,
    /// Mode for a CREATED file; an existing file keeps its own (the same
    /// contract `Create` has — the server's umask decides, see the mount's
    /// 0o666 note).
    pub mode: u32,
    pub data: Bytes,
}

/// One `RemoveMany` entry. `dir` picks rmdir semantics over unlink — the
/// server must not guess from the path's current kind, because the batch may
/// describe a state the filesystem has already left.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManyRemove {
    pub path: RelPath,
    pub dir: bool,
}

/// One `SetattrMany` entry: `Setattr2`'s fields, per path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManySetattr {
    pub path: RelPath,
    pub size: Option<u64>,
    pub mtime: Option<SystemTime>,
    pub mode: Option<u32>,
    pub readonly: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    AttachOk {
        export_id: u32,
        root_attr: Attr,
    },
    Attr(Attr),
    Dir {
        entries: Vec<DirEntry>,
        next_cursor: Option<u64>,
    },
    Opened {
        fh: u64,
        attr: Attr,
    },
    Data(Bytes),
    /// Pre-v5 write reply: byte count and new version, no attributes. Still
    /// the only reply a v4-or-older peer can decode, so it stays.
    Written {
        n: u32,
        new_version: u64,
        conflict: bool,
    },
    Statfs {
        block_size: u32,
        blocks: u64,
        blocks_free: u64,
    },
    Subscribed {
        last_seq: u64,
    },
    Ok,
    /// v2+: the export's suggested client config. Lists are suggestions the
    /// client unions with its own; sizes apply only where the client didn't
    /// set an explicit value. Appended last (see `Request::MountDefaults`).
    MountDefaults {
        exclude: Vec<String>,
        pin: Vec<String>,
        auto_cache_max: Option<u64>,
        auto_cache_budget: Option<u64>,
    },
    /// v4+: a symlink's target, exactly as stored on the server. Appended
    /// last (see `Request::ReadLink`).
    Target(String),
    /// v5+: `Written` plus the file's attributes as they stand after the
    /// write. Every mount backend followed a write with a Getattr to learn
    /// exactly these three fields (size, mtime, version), so carrying them
    /// here removes a network round-trip per write.
    ///
    /// A NEW variant rather than fields on `Written`, and not only because
    /// variants are append-only: postcard is not self-describing, so one
    /// variant index cannot decode to two different shapes. Changing
    /// `Written` would mean a v4 peer misreading the trailing attribute bytes
    /// as whatever came next in the stream. `attr.version` carries what
    /// `Written::new_version` did; `conflict` is not repeated (it has been
    /// permanently false since a version mismatch started refusing the write
    /// outright, and no client reads it).
    WrittenAttr {
        n: u32,
        attr: Attr,
    },
    /// v6+: a page of the subtree, plus the token the whole page describes.
    ///
    /// `token` rides along so a client never has to ask twice: having walked
    /// the pages, it knows both the contents and the exact state they are
    /// contents OF. A token that changes between pages means the export moved
    /// underneath the read, and the client restarts rather than stitching two
    /// states into one tree.
    Tree {
        entries: Vec<TreeEntry>,
        next_cursor: Option<u64>,
        token: u64,
    },
    /// v6+: the export's tree token. 0 means the export is not indexed —
    /// too large for the configured cap, or indexing failed — and the client
    /// should keep using `Readdir`.
    TreeToken {
        token: u64,
    },
    /// v7+: the answer to `TestLock`. `None` means the range is free for the
    /// requested kind — i.e. `F_GETLK` reports `F_UNLCK`.
    LockStatus(Option<LockConflict>),
    /// v8+: files served by `ReadMany`, in request order.
    ///
    /// SHORTER than the request when the budget ran out. That is the whole
    /// paging mechanism: a reply is always a PREFIX of what was asked for, so
    /// the client knows exactly which paths are outstanding without a cursor.
    Many(Vec<ManyEntry>),
    /// v9+: `Opened` plus the head of the file. `data` is empty when the
    /// request asked for none, when the file is empty, or when it is not a
    /// regular file; it is the FIRST `min(len, DATA_CHUNK, size)` bytes
    /// otherwise, read under the same open handle being returned.
    OpenedData {
        fh: u64,
        attr: Attr,
        data: Bytes,
    },
    /// v9+: the `Attach2` answer — attach, mount defaults, and the tree token
    /// in one. `defaults` mirrors `Response::MountDefaults`; `tree_token` is 0
    /// for an unindexed export, exactly as `TreeToken` reports it.
    Attached2 {
        export_id: u32,
        root_attr: Attr,
        exclude: Vec<String>,
        pin: Vec<String>,
        auto_cache_max: Option<u64>,
        auto_cache_budget: Option<u64>,
        tree_token: u64,
    },
    /// v10+: per-entry outcomes for the bulk mutations (`WriteMany`,
    /// `RemoveMany`, `SetattrMany`), in request order and always the SAME
    /// length as the request — a bulk mutation is applied entirely, entry by
    /// entry, and each entry's fate is reported rather than the batch's.
    ///
    /// `Ok(Some(attr))` where the operation produces attributes (writes,
    /// setattrs — the client patches its caches from them, exactly as the
    /// single-op replies allow); `Ok(None)` for removals; `Err` for the
    /// entries the server refused, which must not poison their neighbours.
    ManyOutcome(Vec<Result<Option<Attr>, ErrorCode>>),
}

/// One file in a `ReadMany` reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ManyEntry {
    /// The file, whole, with the attributes it had when read.
    File { attr: Attr, data: Bytes },
    /// Not served, and why. `TooLarge` means it did not fit a bulk reply and
    /// should be read the ordinary chunked way; the rest (gone, excluded, not
    /// a regular file) mean skip it. A per-entry error rather than a failed
    /// request, because one unreadable file must not cost the whole batch.
    Skipped(ErrorCode),
}

/// v7+: the lock that would block a `TestLock`, in `fcntl(F_GETLK)`'s terms.
///
/// `pid` is reported as 0 when unknown, which it always is across a network:
/// the holder is a handle on another machine and its process id means nothing
/// here. SQLite ignores `l_pid`, and no correct caller can do otherwise on a
/// shared filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LockConflict {
    pub kind: LockKind,
    pub start: u64,
    pub len: u64,
    pub pid: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Frame {
    /// First frame from the client.
    Hello {
        proto_min: u16,
        proto_max: u16,
        client: String,
    },
    /// Server's reply choosing the version; connection is then "up".
    HelloAck {
        proto: u16,
        server: String,
    },
    Request {
        id: u64,
        body: Request,
    },
    Response {
        id: u64,
        body: Result<Response, ErrorCode>,
    },
    /// Server push; not correlated with any request.
    Events {
        batch: Vec<FsEvent>,
    },
    Ping {
        nonce: u64,
    },
    Pong {
        nonce: u64,
    },
    /// v3+: an entire postcard-encoded `Frame`, lz4-block-compressed (with
    /// prepended uncompressed size). Senders use it for large compressible
    /// frames when the negotiated version is >= 3; nesting is forbidden.
    Compressed(Bytes),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `PROTO_RANGE` is printed by `--version` and `alloyfs ping`, where a
    /// wrong answer is worse than none: it is what a bug report quotes when
    /// two peers refuse to talk.
    #[test]
    fn proto_range_matches_the_constants() {
        assert_eq!(
            PROTO_RANGE,
            format!("{PROTO_VERSION_MIN}-{PROTO_VERSION_MAX}"),
            "PROTO_RANGE must be regenerated when either constant moves"
        );
    }
}
