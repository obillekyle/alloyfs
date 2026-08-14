//! Stdio transport: the protocol over a child process's stdin/stdout.
//!
//! This is how `mount ssh://azure/projects X:` works with zero server setup:
//! we spawn `ssh azure alloyfs serve --stdio` and the SSH exec channel IS
//! the byte stream — authentication, encryption, and process lifetime all
//! come from the user's existing ssh config. The server side is the mirror
//! image: this process's own stdin/stdout, which is why the agent logs
//! strictly to stderr.

use std::process::Stdio;
use std::sync::Arc;

use tokio::process::Command;

use crate::mux::{MuxConnection, TransportError};
use crate::server::RequestHandler;

/// Client side: spawn `program args...` and establish a connection over its
/// stdio. The child's stderr passes through to ours (ssh errors stay visible).
pub async fn connect_command(
    program: &str,
    args: &[String],
    client_name: &str,
) -> Result<Arc<MuxConnection>, TransportError> {
    tracing::info!(program, ?args, "spawning transport process");
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()?;
    let stdin = child.stdin.take().expect("stdin piped");
    let stdout = child.stdout.take().expect("stdout piped");

    // The child must outlive this function: park it in a reaper task. When
    // the connection drops, stdin closes, the remote agent sees EOF and
    // exits, and wait() completes.
    tokio::spawn(async move {
        match child.wait().await {
            Ok(status) => tracing::info!(%status, "transport process ended"),
            Err(e) => tracing::warn!(error = %e, "transport process wait failed"),
        }
    });

    MuxConnection::establish(tokio::io::join(stdout, stdin), client_name).await
}

/// Server side: serve exactly one session over this process's own stdio,
/// returning when the peer disconnects.
pub async fn serve(server_name: &str, handler: Arc<dyn RequestHandler>) -> Result<(), TransportError> {
    crate::server::serve_connection(
        tokio::io::join(tokio::io::stdin(), tokio::io::stdout()),
        server_name,
        handler,
    )
    .await
}
