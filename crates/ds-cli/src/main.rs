//! drive-sync CLI: clap surface + dispatch. All behavior lives in
//! `commands/`; parsing/config/urls in their own modules.

mod commands;
mod config;
mod urls;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "drive-sync", version, about = "Cross-platform virtual drive service")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the agent, serving exports to clients.
    Serve {
        /// TCP listen address, e.g. 127.0.0.1:7440
        #[arg(long, default_value = "127.0.0.1:7440")]
        tcp: String,
        /// Serve one session over stdin/stdout instead of TCP (what
        /// `mount ssh://...` runs on the remote side).
        #[arg(long)]
        stdio: bool,
        /// Config file (.yml/.yaml preferred, .toml supported).
        #[arg(long)]
        config: Option<PathBuf>,
        /// Inline export, NAME=PATH (repeatable). Alternative to --config.
        #[arg(long = "export", value_name = "NAME=PATH")]
        exports: Vec<String>,
    },
    /// Mount an export as a local drive.
    /// URL: tcp://host:port/export or ssh://host/export
    Mount {
        url: String,
        /// Mountpoint: a directory on Linux, a drive letter (X:) on Windows.
        mountpoint: PathBuf,
        /// Remote drive-sync command for ssh:// urls.
        #[arg(long, default_value = "drive-sync")]
        remote_cmd: String,
        /// YAML mount config (exclude/pin/auto_cache_max/auto_cache_budget/
        /// data_dir). CLI flags override file values.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Local-only paths (gitignore-style glob, repeatable): stored on this
        /// machine, never sent to the server. e.g. --exclude node_modules
        #[arg(long = "exclude", value_name = "GLOB")]
        excludes: Vec<String>,
        /// Always fully cache matching files regardless of size (repeatable).
        #[arg(long = "pin", value_name = "GLOB")]
        pins: Vec<String>,
        /// Auto-download files up to this size ("2M", "512K", bytes; 0 = off).
        #[arg(long, value_name = "SIZE")]
        auto_cache_max: Option<String>,
        /// Total local cache budget (LRU eviction; pins exempt).
        #[arg(long, value_name = "SIZE")]
        auto_cache_budget: Option<String>,
        /// Local data directory (overlay + cache). Default: per-user app data.
        #[arg(long, value_name = "PATH")]
        data_dir: Option<PathBuf>,
        /// Ignore the server's suggested client settings (exclude/pin/cache
        /// sizes published by the export's `client:` config section).
        #[arg(long)]
        no_server_defaults: bool,
        /// Shared secret for token-protected TCP servers (agent.tcp_token).
        #[arg(long)]
        token: Option<String>,
    },
    /// Manage the local auto-download cache.
    Cache {
        #[command(subcommand)]
        cmd: CacheCmd,
    },
    /// Tail an export's live change events as NDJSON
    /// (url: tcp://host:port/export or ssh://host/export).
    Events {
        url: String,
        /// Resume from this sequence number (catch-up from the server's ring log).
        #[arg(long)]
        since: Option<u64>,
        /// Remote drive-sync command for ssh:// urls.
        #[arg(long, default_value = "drive-sync")]
        remote_cmd: String,
        /// Shared secret for token-protected TCP servers.
        #[arg(long)]
        token: Option<String>,
    },
    /// Measure round-trip latency to an agent (url: tcp://host:port or ssh://host).
    Ping {
        url: String,
        #[arg(long, default_value_t = 5)]
        count: u32,
        /// Shared secret for token-protected TCP servers.
        #[arg(long)]
        token: Option<String>,
    },
    /// Fire many concurrent pipelined requests at an agent and verify replies.
    Stress {
        url: String,
        #[arg(long, default_value_t = 1000)]
        count: u32,
        /// Shared secret for token-protected TCP servers.
        #[arg(long)]
        token: Option<String>,
    },
    /// Timed pipelined read of one remote file (no kernel mount involved).
    /// url: tcp://host:port/export or ssh://host/export.
    Bench {
        url: String,
        /// File path inside the export.
        path: String,
        /// Concurrent chunk requests (1 = serial; 16 = one readahead window).
        #[arg(long, default_value_t = 16)]
        depth: usize,
        #[arg(long, default_value = "drive-sync")]
        remote_cmd: String,
        /// Shared secret for token-protected TCP servers.
        #[arg(long)]
        token: Option<String>,
    },
}

#[derive(Subcommand)]
enum CacheCmd {
    /// Delete cached blobs for one mount url (SAFE while unmounted; never
    /// touches the overlay).
    Clear {
        /// Mount url incl. export (e.g. ssh://azure/projects), or --all.
        target: Option<String>,
        #[arg(long)]
        all: bool,
        #[arg(long, value_name = "PATH")]
        data_dir: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr) // stdout stays clean: --stdio transport uses it
        .init();

    match Cli::parse().command {
        Command::Serve {
            tcp,
            stdio,
            config,
            exports,
        } => commands::serve::run(tcp, stdio, config, exports).await,
        Command::Mount {
            url,
            mountpoint,
            remote_cmd,
            config,
            excludes,
            pins,
            auto_cache_max,
            auto_cache_budget,
            data_dir,
            no_server_defaults,
            token,
        } => {
            commands::mount::run(
                url,
                mountpoint,
                remote_cmd,
                config,
                excludes,
                pins,
                auto_cache_max,
                auto_cache_budget,
                data_dir,
                no_server_defaults,
                token,
            )
            .await
        }
        Command::Cache { cmd } => match cmd {
            CacheCmd::Clear {
                target,
                all,
                data_dir,
            } => commands::cache::clear(target, all, data_dir),
        },
        Command::Events {
            url,
            since,
            remote_cmd,
            token,
        } => commands::diag::events(url, since, remote_cmd, token).await,
        Command::Ping { url, count, token } => commands::diag::ping(url, count, token).await,
        Command::Stress { url, count, token } => commands::diag::stress(url, count, token).await,
        Command::Bench {
            url,
            path,
            depth,
            remote_cmd,
            token,
        } => commands::diag::bench(url, path, depth, remote_cmd, token).await,
    }
}
