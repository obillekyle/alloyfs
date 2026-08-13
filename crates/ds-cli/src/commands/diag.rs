//! Diagnostics: ping, pipelined stress, and the NDJSON event tail.

use std::time::Instant;

use ds_proto::{Request, Response};

use crate::urls::{connect_target, require_export, whoami};

pub async fn ping(url: String, count: u32, token: Option<String>) -> anyhow::Result<()> {
    let (conn, _) = connect_target(&url, "drive-sync", "ping", token.as_deref()).await?;
    println!("connected to {} (proto v{})", conn.server_name, conn.proto);
    for i in 1..=count {
        let rtt = conn.ping().await?;
        println!("ping {i}: {:.3} ms", rtt.as_secs_f64() * 1000.0);
    }
    Ok(())
}

pub async fn stress(url: String, count: u32, token: Option<String>) -> anyhow::Result<()> {
    let (conn, _) = connect_target(&url, "drive-sync", "stress", token.as_deref()).await?;
    let start = Instant::now();
    // Launch every request before awaiting any: true pipelining.
    let mut futs = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let conn = conn.clone();
        futs.push(tokio::spawn(async move { conn.request(Request::Statfs).await }));
    }
    let mut ok = 0u32;
    for f in futs {
        match f.await? {
            Ok(Ok(Response::Statfs { .. })) => ok += 1,
            other => anyhow::bail!("unexpected reply: {other:?}"),
        }
    }
    let dt = start.elapsed();
    println!(
        "{ok}/{count} pipelined requests OK in {:.1} ms ({:.0} req/s)",
        dt.as_secs_f64() * 1000.0,
        count as f64 / dt.as_secs_f64()
    );
    Ok(())
}

pub async fn events(
    url: String,
    since: Option<u64>,
    remote_cmd: String,
    token: Option<String>,
) -> anyhow::Result<()> {
    let (conn, export) = connect_target(&url, &remote_cmd, &whoami(), token.as_deref()).await?;
    let export = require_export(export, &url)?;
    let fs = ds_client::RemoteFs::attach(conn.clone(), &export).await?;
    let mut rx = conn.events();
    let last_seq = fs.start_event_pump(|_| {}).await?;
    tracing::info!(last_seq, "subscribed; streaming events (NDJSON)");
    let _ = since; // catch-up via --since arrives with reconnect support
    loop {
        match rx.recv().await {
            Ok(batch) => {
                for ev in batch {
                    println!("{}", serde_json::to_string(&ev)?);
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!(missed = n, "event stream lagged");
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }
    Ok(())
}
