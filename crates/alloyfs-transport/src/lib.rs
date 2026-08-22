//! How frames move: connection multiplexing and the concrete transports.
//!
//! The same `MuxConnection` (client side) and `serve_connection` (server side)
//! run over ANY ordered byte stream — a TCP socket today, an SSH child
//! process's stdin/stdout later. That is the whole point of this crate:
//! everything above it is transport-blind.

/// Initial capacity for a connection's codec buffers.
///
/// tokio-util starts a `Framed` at 8 KiB, which is a reasonable default for
/// a protocol of small messages and the wrong one here: a data frame is
/// `DATA_CHUNK` (128 KiB) plus framing, so every connection spent its first
/// reads growing the buffer by doubling — five reallocations and five
/// copies of a partially-filled frame, on every connection, forever.
/// Sized for one whole frame with room for its header.
pub(crate) const FRAME_BUFFER: usize = 132 * 1024;

mod mux;
mod server;
pub mod stdio;
pub mod tcp;
mod writer;

pub use mux::{MuxConnection, TransportError};
pub use server::{serve_connection, serve_connection_with, EventPusher, RequestHandler};
