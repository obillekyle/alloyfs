//! Diagnostics: ping, pipelined stress, and the NDJSON event tail.

use std::time::Instant;

use alloyfs_proto::{Request, Response};

use crate::urls::{connect_target, require_export, whoami};

pub async fn ping(url: String, count: u32, token: Option<String>) -> anyhow::Result<()> {
    let (conn, _) = connect_target(&url, "alloyfs", "ping", token.as_deref()).await?;
    println!("connected to {} (proto v{})", conn.server_name, conn.proto);
    for i in 1..=count {
        let rtt = conn.ping().await?;
        println!("ping {i}: {:.3} ms", rtt.as_secs_f64() * 1000.0);
    }
    Ok(())
}

pub async fn stress(url: String, count: u32, token: Option<String>) -> anyhow::Result<()> {
    let (conn, _) = connect_target(&url, "alloyfs", "stress", token.as_deref()).await?;
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

/// Timed pipelined read of one remote file, bypassing the kernel mount: with
/// `--depth 1` it measures per-chunk RTT cost, with a deep window it measures
/// what the transport can actually carry. Comparing those two against a
/// through-mount copy of the same file pins which layer loses throughput.
pub async fn bench(
    url: String,
    path: String,
    depth: usize,
    remote_cmd: String,
    token: Option<String>,
) -> anyhow::Result<()> {
    use alloyfs_proto::{OpenFlags, RelPath, DATA_CHUNK};
    use futures::StreamExt;

    let (conn, export) = connect_target(&url, &remote_cmd, "bench", token.as_deref()).await?;
    let export = require_export(export, &url)?;
    match conn.request(Request::Attach { export }).await?? {
        Response::AttachOk { .. } => {}
        other => anyhow::bail!("unexpected attach reply: {other:?}"),
    }
    let flags = OpenFlags {
        read: true,
        ..OpenFlags::default()
    };
    let (fh, attr) = match conn
        .request(Request::Open {
            path: RelPath(path.clone()),
            flags,
        })
        .await??
    {
        Response::Opened { fh, attr } => (fh, attr),
        other => anyhow::bail!("unexpected open reply: {other:?}"),
    };
    let size = attr.size;
    anyhow::ensure!(size > 0, "{path} is empty — nothing to measure");

    let chunks: Vec<(u64, u32)> = (0..size)
        .step_by(DATA_CHUNK as usize)
        .map(|off| (off, ((size - off).min(DATA_CHUNK as u64)) as u32))
        .collect();
    let n = chunks.len();
    let start = Instant::now();
    let mut stream = futures::stream::iter(chunks.into_iter().map(|(offset, len)| {
        let conn = conn.clone();
        async move { conn.request(Request::Read { fh, offset, len }).await }
    }))
    .buffer_unordered(depth.max(1));
    let mut bytes = 0u64;
    while let Some(resp) = stream.next().await {
        match resp?? {
            Response::Data(data) => bytes += data.len() as u64,
            other => anyhow::bail!("unexpected read reply: {other:?}"),
        }
    }
    let dt = start.elapsed();
    let _ = conn.request(Request::Release { fh }).await;
    println!(
        "{bytes} bytes in {n} chunks, depth {depth}: {:.2} MB/s ({:.1} ms)",
        bytes as f64 / dt.as_secs_f64() / 1_000_000.0,
        dt.as_secs_f64() * 1000.0
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
    let fs = alloyfs_client::RemoteFs::attach(conn.clone(), &export).await?;
    let mut rx = conn.events();
    // The raw receiver above sees everything the pump subscribes to,
    // including the ring-log catch-up batches --since requests.
    let last_seq = fs.start_event_pump_since(since, |_| {}).await?;
    tracing::info!(last_seq, "subscribed; streaming events (NDJSON)");
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
