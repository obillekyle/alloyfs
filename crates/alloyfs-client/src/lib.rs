//! Mount-agnostic client core.
//!
//! `RemoteFs` is a *synchronous* facade over the async connection, because
//! both fuser (Linux) and WinFsp (Windows) call us back on their own plain
//! threads — each call parks its thread on the async runtime via `block_on`.
//! Platform mount backends translate their callback dialect into these calls;
//! nothing in here knows what platform it's on.

mod autocache;
mod error;
mod events;
mod inode;
mod metacache;
mod options;
mod overlay;
mod readahead;
mod remote_fs;
mod symlink;
pub mod sync;
mod walker;

pub use error::{posix_errno, FsError};
pub use inode::{InodeTable, ROOT_INO};
pub use options::{ClientOptions, Dialer};
pub use remote_fs::RemoteFs;
pub use sync::{ConflictPolicy, SyncEngine, SyncOptions, SyncStats};
