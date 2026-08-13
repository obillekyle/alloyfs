//! Plain-TCP transport: the simplest way frames move between machines.

use std::sync::Arc;

use tokio::net::{TcpListener, TcpStream};

use crate::mux::{MuxConnection, TransportError};
use crate::server::RequestHandler;

pub async fn connect(addr: &str, client_name: &str) -> Result<Arc<MuxConnection>, TransportError> {
    let stream = TcpStream::connect(addr).await?;
    stream.set_nodelay(true)?; // latency matters more than tiny-packet overhead
    MuxConnection::establish(stream, client_name).await
}

/// Accept loop: one spawned `serve_connection` per client. `make_handler` is
/// called per connection so each session gets its own handler/state.
pub async fn serve<F>(addr: &str, server_name: String, make_handler: F) -> Result<(), TransportError>
where
    F: Fn() -> Arc<dyn RequestHandler> + Send + Sync + 'static,
{
    let listener = TcpListener::bind(addr).await?;
    tracing::info!(%addr, "listening (tcp)");
    loop {
        let (stream, peer) = listener.accept().await?;
        stream.set_nodelay(true)?;
        let handler = make_handler();
        let server_name = server_name.clone();
        tokio::spawn(async move {
            tracing::info!(%peer, "connection accepted");
            if let Err(e) = crate::server::serve_connection(stream, &server_name, handler).await {
                tracing::warn!(%peer, error = %e, "connection ended with error");
            } else {
                tracing::info!(%peer, "connection closed");
            }
        });
    }
}
