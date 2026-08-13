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
pub const PROTO_VERSION_MIN: u16 = 1;
pub const PROTO_VERSION_MAX: u16 = 2;

/// Read/write payloads are capped to this many bytes per request so one huge
/// file operation can never monopolize the connection (head-of-line blocking).
pub const DATA_CHUNK: u32 = 128 * 1024;

/// A path relative to an export root: UTF-8, `/`-separated, no leading `/`,
/// never containing `.` or `..` components. The *server* is the enforcement
/// point (`validate`) — clients are untrusted by definition.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
pub struct RelPath(pub String);

impl RelPath {
    pub const ROOT: &'static str = "";

    pub fn validate(&self) -> Result<(), ErrorCode> {
        let s = &self.0;
        if s.starts_with('/') || s.contains('\\') {
            return Err(ErrorCode::InvalidPath);
        }
        if s.split('/')
            .any(|c| c.is_empty() && !s.is_empty() || c == "." || c == "..")
        {
            return Err(ErrorCode::InvalidPath);
        }
        Ok(())
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
    /// Set when the change was made through drive-sync by a connected session;
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
}
