//! Mount-agnostic client core.
//!
//! `RemoteFs` is a *synchronous* facade over the async connection, because
//! both fuser (Linux) and WinFsp (Windows) call us back on their own plain
//! threads — each call parks its thread on the async runtime via `block_on`.
//! Platform mount backends translate their callback dialect into these calls;
//! nothing in here knows what platform it's on.

mod autocache;
mod events;
mod inode;
mod overlay;
mod readahead;
mod remote_fs;
pub mod sync;
mod walker;

pub use inode::{InodeTable, ROOT_INO};
pub use remote_fs::{ClientOptions, Dialer, FsError, RemoteFs};
pub use sync::{ConflictPolicy, SyncEngine, SyncOptions, SyncStats};
