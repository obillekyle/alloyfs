//! Diagnostics: ping, pipelined stress, and the NDJSON event tail.

use std::time::Instant;

use alloyfs_proto::{Request, Response, PROTO_RANGE};

use crate::urls::{connect_target, require_export, whoami};

pub async fn ping(url: String, count: u32, token: Option<String>, json: bool) -> anyhow::Result<()> {
    let (conn, _) = connect_target(&url, "alloyfs", "ping", token.as_deref()).await?;
    if !json {
        println!("connected to {}", conn.server_name);
        // Both halves of the compatibility question in one quotable line:
        // what this pair settled on, and what this build was willing to
        // speak. A peer that refuses to connect at all is diagnosed by
        // comparing two of these.
        println!(
            "protocol v{} negotiated (this build speaks {PROTO_RANGE})",
            conn.proto
        );
    }
    let mut rtts = Vec::with_capacity(count as usize);
    for i in 1..=count {
        let rtt = conn.ping().await?;
        let ms = rtt.as_secs_f64() * 1000.0;
        rtts.push(ms);
        if !json {
            println!("ping {i}: {ms:.3} ms");
        }
    }
    if json {
        // Milliseconds as numbers, not preformatted strings: a consumer that
        // wants three decimal places can round, and one that wants to
        // average cannot un-round.
        let sorted = {
            let mut v = rtts.clone();
            v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            v
        };
        emit(serde_json::json!({
            "url": url,
            "server": conn.server_name,
            "proto": conn.proto,
            "proto_range": PROTO_RANGE,
            "count": count,
            "rtt_ms": rtts,
            "min_ms": sorted.first(),
            "median_ms": sorted.get(sorted.len() / 2),
            "max_ms": sorted.last(),
        }))?;
    }
    Ok(())
}

/// One JSON document on stdout, pretty-printed and newline-terminated.
///
/// Pretty rather than compact because these are commands a person runs and
/// then pipes to `jq` — and `jq` reformats anyway, so the only reader who
/// notices is the one looking at raw output. Newline-terminated so a shell
/// prompt does not land on the closing brace.
fn emit(value: serde_json::Value) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(&value)?);
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

/// Fetch an export's whole tree in as few round trips as the server allows,
/// and report what it cost.
///
/// A diagnostic rather than a feature: it answers "is this export indexed, and
/// what does one exchange actually buy here" without needing a mount. The
/// comparison it exists to make is against `Readdir`, which needs one round
/// trip per directory — so the directory count in the output is roughly the
/// number of round trips this replaced.
pub async fn tree(url: String, remote_cmd: String, token: Option<String>, json: bool) -> anyhow::Result<()> {
    let (conn, export) = connect_target(&url, &remote_cmd, &whoami(), token.as_deref()).await?;
    let export = require_export(export, &url)?;
    conn.request(alloyfs_proto::Request::Attach { export }).await??;

    if conn.proto < 6 {
        anyhow::bail!(
            "the agent speaks protocol {} — the tree index arrived in v6",
            conn.proto
        );
    }

    let started = std::time::Instant::now();
    let mut cursor = None;
    let mut requests = 0usize;
    let mut files = 0usize;
    let mut dirs = 0usize;
    let mut bytes = 0u64;
    let mut token_seen = 0u64;
    loop {
        let resp = conn
            .request(alloyfs_proto::Request::Tree {
                path: alloyfs_proto::RelPath(String::new()),
                cursor,
            })
            .await??;
        let alloyfs_proto::Response::Tree {
            entries,
            next_cursor,
            token,
        } = resp
        else {
            anyhow::bail!("expected a Tree reply, got {resp:?}");
        };
        requests += 1;
        if token == 0 {
            if json {
                // `indexed: false` rather than an error: not being indexed is
                // a legitimate configuration, and a monitoring script should
                // be able to see it without parsing a message.
                return emit(serde_json::json!({ "url": url, "indexed": false }));
            }
            println!("{url}: not indexed — clients fall back to per-directory readdir");
            println!("  (the export is past the agent's tree_max_entries, or indexing failed)");
            return Ok(());
        }
        // A token that moves between pages means the export changed underneath
        // the read; the pages would describe two different trees stitched into
        // one, which is worse than not answering.
        if token_seen != 0 && token != token_seen {
            anyhow::bail!("the export changed while being read (token {token_seen} -> {token}); retry");
        }
        token_seen = token;
        for e in &entries {
            match e.attr.kind {
                alloyfs_proto::FileKind::Dir => dirs += 1,
                _ => {
                    files += 1;
                    bytes += e.attr.size;
                }
            }
        }
        match next_cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    let ms = started.elapsed().as_secs_f64() * 1000.0;
    if json {
        return emit(serde_json::json!({
            "url": url,
            "indexed": true,
            // A string: the token is a u64 whose top bit is routinely set,
            // and JSON numbers are doubles in most consumers — JavaScript
            // would silently round it and then compare two tokens as equal
            // when they are not, which is the one thing a token is for.
            "token": format!("{token_seen:#018x}"),
            "entries": dirs + files,
            "directories": dirs,
            "files": files,
            "bytes": bytes,
            "requests": requests,
            "elapsed_ms": ms,
        }));
    }
    println!("{url}");
    println!("  token       {token_seen:#018x}");
    println!(
        "  entries     {} ({dirs} directories, {files} files)",
        dirs + files
    );
    println!("  bytes       {bytes}");
    println!("  requests    {requests}");
    println!("  elapsed     {ms:.1} ms");
    println!("  readdir would have needed ~{dirs} round trips for the same listing");
    Ok(())
}

/// Small-file fetch, both strategies, alternating in ONE process.
///
/// The question this answers is what `ReadMany` bought, and the honest way to
/// ask it is to run both against the same files over the same connection in
/// the same minute. Stat latency on the machine this was written for drifts
/// ~50% intraday, so a number from a previous build is not comparable to one
/// from this build — but two numbers taken seconds apart are, and alternating
/// them means neither strategy gets the good half of a drift.
///
/// A CPU-bound control runs alongside for the same reason: if it moves between
/// rounds, the machine moved and the round is suspect.
pub async fn bulk(
    url: String,
    dir: String,
    rounds: usize,
    remote_cmd: String,
    token: Option<String>,
    json: bool,
) -> anyhow::Result<()> {
    use alloyfs_proto::{ManyEntry, OpenFlags, RelPath, DATA_CHUNK};

    let (conn, export) = connect_target(&url, &remote_cmd, "bulk", token.as_deref()).await?;
    let export = require_export(export, &url)?;
    match conn.request(Request::Attach { export }).await?? {
        Response::AttachOk { .. } => {}
        other => anyhow::bail!("unexpected attach reply: {other:?}"),
    }

    // The file list, and the sizes, come from one listing — the same thing the
    // walker has in hand when it decides what to fetch.
    let mut files: Vec<(RelPath, u64)> = Vec::new();
    let mut cursor = 0u64;
    loop {
        match conn
            .request(Request::Readdir {
                path: RelPath(dir.clone()),
                cursor,
            })
            .await??
        {
            Response::Dir { entries, next_cursor } => {
                for e in entries {
                    if e.attr.kind == alloyfs_proto::FileKind::File {
                        let p = if dir.is_empty() {
                            RelPath(e.name.clone())
                        } else {
                            RelPath(format!("{dir}/{}", e.name))
                        };
                        files.push((p, e.attr.size));
                    }
                }
                match next_cursor {
                    Some(c) => cursor = c,
                    None => break,
                }
            }
            other => anyhow::bail!("unexpected readdir reply: {other:?}"),
        }
    }
    anyhow::ensure!(!files.is_empty(), "{dir} has no files to measure");
    let total_bytes: u64 = files.iter().map(|(_, s)| s).sum();
    if !json {
        println!(
            "{} files, {total_bytes} bytes total, {rounds} round(s) of each strategy\n",
            files.len()
        );
    }

    // Per-file: Open, Read the whole thing, Release. What the walker did before
    // v8, and what any consumer without ReadMany still does.
    let per_file = |conn: std::sync::Arc<alloyfs_transport::MuxConnection>, files: Vec<(RelPath, u64)>| async move {
        let mut bytes = 0u64;
        for (path, size) in files {
            let flags = OpenFlags {
                read: true,
                ..OpenFlags::default()
            };
            let fh = match conn.request(Request::Open { path, flags }).await {
                Ok(Ok(Response::Opened { fh, .. })) => fh,
                _ => continue,
            };
            let mut off = 0u64;
            while off < size {
                let len = ((size - off).min(DATA_CHUNK as u64)) as u32;
                match conn.request(Request::Read { fh, offset: off, len }).await {
                    Ok(Ok(Response::Data(d))) => {
                        bytes += d.len() as u64;
                        off += d.len().max(1) as u64;
                    }
                    _ => break,
                }
            }
            let _ = conn.request(Request::Release { fh }).await;
        }
        bytes
    };

    // ReadMany: as many whole files per exchange as the budget allows, the
    // reply a prefix of the request.
    let bulk_fetch = |conn: std::sync::Arc<alloyfs_transport::MuxConnection>, files: Vec<(RelPath, u64)>| async move {
        const BUDGET: u32 = 768 * 1024;
        let mut bytes = 0u64;
        let mut remaining: Vec<RelPath> = files.into_iter().map(|(p, _)| p).collect();
        while !remaining.is_empty() {
            let entries = match conn
                .request(Request::ReadMany {
                    paths: remaining.clone(),
                    budget: BUDGET,
                })
                .await
            {
                Ok(Ok(Response::Many(e))) if !e.is_empty() => e,
                _ => break,
            };
            let served = entries.len().min(remaining.len());
            for entry in entries {
                if let ManyEntry::File { data, .. } = entry {
                    bytes += data.len() as u64;
                }
            }
            remaining.drain(..served);
        }
        bytes
    };

    // Per-file again, but four at a time — what the walker ACTUALLY did before
    // v8 (FILE_CONCURRENCY = 4). Without this the comparison flatters
    // ReadMany, because a serial loop is not the thing it replaced.
    let per_file_conc = |conn: std::sync::Arc<alloyfs_transport::MuxConnection>,
                         files: Vec<(RelPath, u64)>| async move {
        use futures::StreamExt;
        let bytes = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        futures::stream::iter(files.into_iter().map(|(path, size)| {
            let conn = conn.clone();
            let bytes = bytes.clone();
            async move {
                let flags = OpenFlags {
                    read: true,
                    ..OpenFlags::default()
                };
                let fh = match conn.request(Request::Open { path, flags }).await {
                    Ok(Ok(Response::Opened { fh, .. })) => fh,
                    _ => return,
                };
                let mut off = 0u64;
                while off < size {
                    let len = ((size - off).min(DATA_CHUNK as u64)) as u32;
                    match conn.request(Request::Read { fh, offset: off, len }).await {
                        Ok(Ok(Response::Data(d))) => {
                            bytes.fetch_add(d.len() as u64, std::sync::atomic::Ordering::Relaxed);
                            off += d.len().max(1) as u64;
                        }
                        _ => break,
                    }
                }
                let _ = conn.request(Request::Release { fh }).await;
            }
        }))
        .buffer_unordered(4)
        .collect::<Vec<_>>()
        .await;
        bytes.load(std::sync::atomic::Ordering::Relaxed)
    };

    // A fixed amount of arithmetic, timed each round. It has nothing to do
    // with the filesystem, so if it moves, the machine did.
    let control = || {
        let start = Instant::now();
        let mut acc = 0u64;
        for i in 0..40_000_000u64 {
            acc = acc.wrapping_add(i ^ (acc >> 7));
        }
        std::hint::black_box(acc);
        start.elapsed().as_secs_f64() * 1000.0
    };

    let mut serial_ms = Vec::new();
    let mut conc_ms = Vec::new();
    let mut bulk_ms = Vec::new();
    let mut controls: Vec<(f64, f64)> = Vec::new();
    for round in 1..=rounds {
        let ctl_a = control();

        let start = Instant::now();
        let a_bytes = per_file(conn.clone(), files.clone()).await;
        let a = start.elapsed().as_secs_f64() * 1000.0;
        serial_ms.push(a);

        let start = Instant::now();
        let c_bytes = per_file_conc(conn.clone(), files.clone()).await;
        let c = start.elapsed().as_secs_f64() * 1000.0;
        conc_ms.push(c);

        let start = Instant::now();
        let b_bytes = bulk_fetch(conn.clone(), files.clone()).await;
        let b = start.elapsed().as_secs_f64() * 1000.0;
        bulk_ms.push(b);

        let ctl_b = control();
        // The control belongs in the output whichever form it takes: a round
        // whose control moved is a round where the MACHINE moved, and a
        // consumer that cannot see that will read drift as a result.
        controls.push((ctl_a, ctl_b));
        if !json {
            println!(
                "round {round}: serial {a:9.1} ms ({a_bytes} B) | 4-way {c:8.1} ms ({c_bytes} B) \
                 | ReadMany {b:7.1} ms ({b_bytes} B) | control {ctl_a:.0}/{ctl_b:.0} ms"
            );
        }
    }

    let median = |mut v: Vec<f64>| {
        v.sort_by(|x, y| x.partial_cmp(y).unwrap());
        v[v.len() / 2]
    };
    let a = median(serial_ms.clone());
    let c = median(conc_ms.clone());
    let b = median(bulk_ms.clone());
    if json {
        return emit(serde_json::json!({
            "url": url,
            "dir": dir,
            "rounds": rounds,
            "files": files.len(),
            // The size of the workload, which the human header carries and
            // the JSON would otherwise drop: a timing without it is not
            // comparable to a timing from another directory.
            "bytes": total_bytes,
            "serial_ms": serial_ms,
            "concurrent_4_ms": conc_ms,
            "readmany_ms": bulk_ms,
            // Two per round, taken before and after: they bracket the round,
            // so a gap between them is drift that happened DURING it.
            "control_ms": controls.iter().map(|(x, y)| vec![*x, *y]).collect::<Vec<_>>(),
            "median": {
                "serial_ms": a,
                "concurrent_4_ms": c,
                "readmany_ms": b,
                "serial_over_readmany": a / b.max(0.001),
                "concurrent_over_readmany": c / b.max(0.001),
            },
        }));
    }
    println!("\nmedians over {rounds} round(s):");
    println!("  per-file, serial     {a:9.1} ms   ({:.1}x)", a / b.max(0.001));
    println!(
        "  per-file, 4 at a time{c:9.1} ms   ({:.1}x)  <- what the walker actually did",
        c / b.max(0.001)
    );
    println!("  ReadMany             {b:9.1} ms   (1.0x)");
    Ok(())
}
