//! Per-mount client configuration.

use std::path::PathBuf;
use std::sync::Arc;

use alloyfs_transport::MuxConnection;
use futures::future::BoxFuture;

/// Re-dial an equivalent connection after the current one dies. Built by the
/// CLI from the original mount url (tcp dial or ssh re-spawn).
pub type Dialer = Arc<dyn Fn() -> BoxFuture<'static, anyhow::Result<Arc<MuxConnection>>> + Send + Sync>;

/// Per-mount client behavior: local overlay excludes + auto-download cache +
/// optional reconnect. Default means every feature is off and RemoteFs
/// behaves exactly as a plain single-connection client.
#[derive(Clone)]
pub struct ClientOptions {
    pub excludes: Vec<String>,
    /// Durable per-host tree: the overlay lives here and losing it loses
    /// files that exist on no server.
    pub data_dir: PathBuf,
    /// Disposable per-host tree: blobs only, safe to delete at any time.
    /// Kept separate from `data_dir` so "clear the cache" can never reach
    /// the overlay.
    pub cache_dir: PathBuf,
    /// Directory name for this export within those trees.
    pub mount_key: String,
    /// None = no explicit choice: a server suggestion (v2+) applies, else
    /// the fallback below. `Some(0)` is an explicit OFF that beats both.
    pub auto_cache_max: Option<u64>,
    pub auto_cache_budget: Option<u64>,
    /// Used when neither the client nor the server chose a value. The
    /// library default is off/512M; the CLI mounts pass 2M/512M.
    pub auto_cache_max_fallback: u64,
    pub auto_cache_budget_fallback: u64,
    pub pins: Vec<String>,
    pub dialer: Option<Dialer>,
    /// Ignore the server's suggested client settings entirely.
    pub no_server_defaults: bool,
    /// Disable the v10 write batcher: every mutation blocks until the server
    /// has it, the pre-v10 contract. The batcher is on by default because its
    /// ack-early window is bounded to milliseconds and every barrier
    /// (fsync/flush/locks/rename/unmount) still blocks for the server — see
    /// batcher.rs for the exact promises.
    pub write_through: bool,
    /// Refuse to write over a file another machine changed since this handle
    /// last saw it. Off by default: it trades "your editor's save failed" for
    /// "your colleague's edit vanished", and which of those is worse depends
    /// entirely on what the mount is for.
    pub detect_conflicts: bool,
    /// Where this mount appears locally ("Y:", "/mnt/alloy"). Used only to
    /// rewrite symlink targets that point back into the mount; `None` skips
    /// that entirely, which is right for a client with no mountpoint (sync,
    /// the CLI diagnostics).
    pub mount_root: Option<String>,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            excludes: Vec::new(),
            data_dir: PathBuf::new(),
            cache_dir: PathBuf::new(),
            mount_key: String::new(),
            auto_cache_max: None,
            auto_cache_budget: None,
            auto_cache_max_fallback: 0, // library default: cache off
            auto_cache_budget_fallback: 512 * 1024 * 1024,
            pins: Vec::new(),
            dialer: None,
            no_server_defaults: false,
            write_through: false,
            detect_conflicts: false,
            mount_root: None,
        }
    }
}
