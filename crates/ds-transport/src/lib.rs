//! How frames move: connection multiplexing and the concrete transports.
//!
//! The same `MuxConnection` (client side) and `serve_connection` (server side)
//! run over ANY ordered byte stream — a TCP socket today, an SSH child
//! process's stdin/stdout later. That is the whole point of this crate:
//! everything above it is transport-blind.

mod mux;
mod server;
pub mod stdio;
pub mod tcp;

pub use mux::{MuxConnection, TransportError};
pub use server::{serve_connection, EventPusher, RequestHandler};
