//! drive-sync wire protocol: the types both ends of a connection agree on,
//! and the codec that turns them into length-prefixed bytes on the wire.
//!
//! This crate deliberately does NO networking or file I/O — it is pure data.
//! That keeps it testable on any platform and guarantees the agent and the
//! mount client can never disagree about what bytes mean.

mod error;
mod frame;
mod messages;

pub use error::{ErrorCode, ProtoError};
pub use frame::{FrameCodec, MAX_FRAME_LEN};
pub use messages::*;
