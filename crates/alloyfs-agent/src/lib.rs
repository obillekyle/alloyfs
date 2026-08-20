//! The server side of alloyfs: exports real folders to remote clients.
//!
//! One `ExportRegistry` lives for the whole process; one `AgentSession` (the
//! `RequestHandler` implementation) lives per client connection and holds
//! that client's open file handles.

mod config;
mod fsutil;
mod listings;
mod locks;
mod ops;
pub mod tree;
pub mod watch;
mod winattr;

pub use config::{AgentConfig, ClientDefaults, ExportConfig};
pub use ops::{AgentSession, Export, ExportRegistry, MountDefaults};
pub use watch::EventHub;
