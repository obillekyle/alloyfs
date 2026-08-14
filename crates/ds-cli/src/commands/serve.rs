use std::path::PathBuf;
use std::sync::Arc;

use ds_agent::{AgentSession, ExportRegistry};
use ds_transport::{stdio, tcp, RequestHandler};

use crate::config::load_agent_config;

pub async fn run(
    addr: String,
    stdio_mode: bool,
    config: Option<PathBuf>,
    exports: Vec<String>,
) -> anyhow::Result<()> {
    let cfg = load_agent_config(config, &exports)?;
    let registry = Arc::new(ExportRegistry::from_config(&cfg)?);
    // One watcher per export; guards keep the OS watchers alive for the life
    // of the process.
    let mut _watch_guards = Vec::new();
    for export in registry.all() {
        let hub = export.events.clone();
        let name = export.name.clone();
        // A failed watcher degrades that export to no-events; it must never
        // take the whole agent down.
        match ds_agent::watch::spawn(export, hub, std::time::Duration::from_millis(250)) {
            Ok(guard) => _watch_guards.push(guard),
            Err(e) => tracing::warn!(export = name, error = %e, "file watching disabled"),
        }
    }
    // Frees locks/handles of clients that vanish without disconnecting
    // (heartbeats arrive every 10 s; 30 s of silence = dead).
    registry.spawn_lease_reaper(std::time::Duration::from_secs(30));
    if let Some(http) = cfg.agent.http_listen.clone() {
        let registry = registry.clone();
        let token = cfg.agent.http_token.clone();
        tokio::spawn(async move {
            if let Err(e) = ds_http::serve(&http, registry, token).await {
                tracing::error!(error = %e, "http api failed");
            }
        });
    }
    let name = format!("drive-sync/{}", env!("CARGO_PKG_VERSION"));
    if stdio_mode {
        // One session over our own stdin/stdout (the ssh exec channel).
        // stdout carries protocol frames; logging is stderr-only. No tcp_token
        // check: reaching this process already required an ssh login.
        stdio::serve(&name, Arc::new(AgentSession::new(registry))).await?;
    } else {
        let listen = cfg.agent.tcp_listen.unwrap_or(addr);
        let token = cfg.agent.tcp_token.clone();
        if token.is_none() && !ds_common::is_loopback_listen(&listen) {
            anyhow::bail!(
                "refusing to serve TCP on non-loopback {listen} without agent.tcp_token — \
                 anyone who can reach the port could mount every export"
            );
        }
        // Token-protected listeners require proto v3: older clients can't
        // decode AuthRequired, so they're turned away at the handshake.
        let min_proto = if token.is_some() {
            3
        } else {
            ds_proto::PROTO_VERSION_MIN
        };
        tcp::serve(&listen, name, min_proto, move || {
            Arc::new(AgentSession::with_token(registry.clone(), token.clone())) as Arc<dyn RequestHandler>
        })
        .await?;
    }
    Ok(())
}
