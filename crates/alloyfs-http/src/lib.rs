//! HTTP surface on the agent: status, browse, file GET/POST, and a
//! Server-Sent-Events stream of file changes. Not a mount transport — an
//! observability and integration API (dashboards, scripts, CI).
//!
//! Authorization: when a token is configured, EVERY /api route requires
//! `Authorization: Bearer <token>` (constant-time comparison). Serving on a
//! non-loopback address without a token is refused at startup.

use std::convert::Infallible;
use std::sync::Arc;

use alloyfs_agent::ExportRegistry;
use alloyfs_proto::{ErrorCode, FileKind, RelPath};
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::Stream;
use serde::{Deserialize, Serialize};

/// Max POST body: files written over HTTP are capped here (mounts are the
/// path for bigger data).
const MAX_BODY: usize = 256 * 1024 * 1024;

struct AppState {
    registry: Arc<ExportRegistry>,
    token: Option<String>,
}

pub async fn serve(listen: &str, registry: Arc<ExportRegistry>, token: Option<String>) -> anyhow::Result<()> {
    anyhow::ensure!(
        token.is_some() || alloyfs_common::is_loopback_listen(listen),
        "http_listen {listen} is not loopback: set agent.http_token (refusing to serve an \
         unauthenticated API on the network)"
    );
    let app = router(registry, token);
    let listener = tokio::net::TcpListener::bind(listen).await?;
    tracing::info!(%listen, "listening (http)");
    axum::serve(listener, app).await?;
    Ok(())
}

/// The API as a `Router`, auth and body limit already layered on.
///
/// Split out of `serve` so it can be driven directly with
/// `tower::ServiceExt::oneshot` — testing these endpoints should not require
/// binding a port, and a bound port in a test is a race waiting to happen.
/// `serve` keeps the loopback/token safety check; this does not, because a
/// caller holding a `Router` has not yet decided where to expose it.
pub fn router(registry: Arc<ExportRegistry>, token: Option<String>) -> Router {
    let state = Arc::new(AppState { registry, token });
    Router::new()
        .route("/api/status", get(status))
        .route("/api/exports", get(exports))
        .route("/api/exports/{name}/browse", get(browse))
        .route("/api/exports/{name}/file", get(file_get).post(file_post))
        .route("/api/exports/{name}/mkdir", post(mkdir_post))
        .route("/api/exports/{name}/delete", post(delete_post))
        .route("/api/exports/{name}/events", get(events_sse))
        .layer(middleware::from_fn_with_state(state.clone(), auth))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .with_state(state)
}

use alloyfs_common::token_eq;

async fn auth(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let Some(expected) = &state.token else {
        return next.run(req).await; // loopback without token: open by choice
    };
    let ok = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|presented| token_eq(presented, expected));
    if ok {
        next.run(req).await
    } else {
        (StatusCode::UNAUTHORIZED, "missing or invalid bearer token\n").into_response()
    }
}

fn code_to_status(e: ErrorCode) -> StatusCode {
    match e {
        ErrorCode::NotFound => StatusCode::NOT_FOUND,
        ErrorCode::PermissionDenied | ErrorCode::InvalidPath => StatusCode::FORBIDDEN,
        ErrorCode::NotADirectory | ErrorCode::IsADirectory => StatusCode::BAD_REQUEST,
        ErrorCode::AlreadyExists => StatusCode::CONFLICT,
        ErrorCode::NotEmpty => StatusCode::CONFLICT,
        ErrorCode::ReadOnly => StatusCode::FORBIDDEN,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

#[derive(Deserialize)]
struct PathQuery {
    path: String,
}

/// GET /api/exports/{name}/file?path=… — stream the file's bytes.
///
/// Speaks enough HTTP to serve the callers a plain GET invites: a single
/// `Range` for seeks and resumed downloads (the wire below was always
/// ranged; only this surface wasn't), a weak `ETag` + `Last-Modified` pair
/// so pollers get 304s, and a content-type from the extension so browsers
/// render media instead of downloading it. Every response carries
/// `nosniff` and a sandbox CSP — files people uploaded must render, but
/// never script against this origin — and `html` deliberately maps to
/// text/plain for the same reason.
async fn file_get(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(q): Query<PathQuery>,
    headers: HeaderMap,
) -> Result<Response, StatusCode> {
    use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};

    let export = state.registry.get(&name).ok_or(StatusCode::NOT_FOUND)?;
    let rel = RelPath(q.path);
    let full = export.resolve(&rel).map_err(code_to_status)?;
    let mut file = tokio::fs::File::open(&full)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    let md = file
        .metadata()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if md.is_dir() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let len = md.len();
    let mtime_ms = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis())
        .unwrap_or(0);
    // Weak by declaration and by construction (size + mtime, no hashing) —
    // exactly strong enough for revalidation.
    let etag = format!("W/\"{len:x}-{mtime_ms:x}\"");
    let base = |b: axum::http::response::Builder| {
        b.header("etag", etag.clone())
            .header("last-modified", httpdate_ms(mtime_ms))
            .header("accept-ranges", "bytes")
            .header("x-content-type-options", "nosniff")
            .header("content-security-policy", "sandbox")
    };
    if headers
        .get("if-none-match")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == etag))
    {
        return base(Response::builder().status(StatusCode::NOT_MODIFIED))
            .body(axum::body::Body::empty())
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR);
    }
    let ctype = content_type_for(&full);

    if let Some(spec) = headers.get("range").and_then(|v| v.to_str().ok()) {
        match parse_range(spec, len) {
            // Serve the slice: seek + take, still streamed.
            Ok(Some((start, end))) => {
                file.seek(std::io::SeekFrom::Start(start))
                    .await
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
                let stream = tokio_util::io::ReaderStream::new(file.take(end - start + 1));
                return base(Response::builder().status(StatusCode::PARTIAL_CONTENT))
                    .header("content-type", ctype)
                    .header("content-length", end - start + 1)
                    .header("content-range", format!("bytes {start}-{end}/{len}"))
                    .body(axum::body::Body::from_stream(stream))
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR);
            }
            // Well-formed but nothing satisfiable: the dedicated status,
            // with the total the client should retry from.
            Err(()) => {
                return Response::builder()
                    .status(StatusCode::RANGE_NOT_SATISFIABLE)
                    .header("content-range", format!("bytes */{len}"))
                    .body(axum::body::Body::empty())
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR);
            }
            // Malformed or multi-range: Range is advisory, the whole file
            // is a legal answer.
            Ok(None) => {}
        }
    }

    let stream = tokio_util::io::ReaderStream::new(file);
    base(Response::builder())
        .header("content-type", ctype)
        .header("content-length", len)
        .body(axum::body::Body::from_stream(stream))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

/// `bytes=a-b` → the closed interval to serve. `Ok(None)` means no usable
/// range — malformed or multi-range — and RFC 9110 makes Range advisory,
/// so the caller serves everything. `Err(())` is well-formed but
/// unsatisfiable: that one owes a 416.
fn parse_range(spec: &str, len: u64) -> Result<Option<(u64, u64)>, ()> {
    let Some(spec) = spec.strip_prefix("bytes=") else {
        return Ok(None);
    };
    if spec.contains(',') {
        return Ok(None);
    }
    let Some((a, b)) = spec.split_once('-') else {
        return Ok(None);
    };
    let (a, b) = (a.trim(), b.trim());
    if a.is_empty() {
        // Suffix form `-n`: the final n bytes.
        let n: u64 = match b.parse() {
            Ok(n) => n,
            Err(_) => return Ok(None),
        };
        if n == 0 || len == 0 {
            return Err(());
        }
        return Ok(Some((len.saturating_sub(n), len - 1)));
    }
    let start: u64 = match a.parse() {
        Ok(s) => s,
        Err(_) => return Ok(None),
    };
    if start >= len {
        return Err(());
    }
    let end = if b.is_empty() {
        len - 1
    } else {
        match b.parse::<u64>() {
            Ok(e) if e >= start => e.min(len - 1),
            _ => return Ok(None),
        }
    };
    Ok(Some((start, end)))
}

/// Extension → content-type for what a browser meaningfully renders;
/// everything else stays an opaque download. `html` is text/plain ON
/// PURPOSE: rendering user-uploaded markup on the API's origin would make
/// this a script host, and the CSP sandbox is belt to this suspender.
fn content_type_for(path: &std::path::Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("html" | "htm" | "txt" | "log" | "md") => "text/plain; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("json") => "application/json",
        Some("xml") => "application/xml",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("pdf") => "application/pdf",
        Some("mp4" | "m4v") => "video/mp4",
        Some("webm") => "video/webm",
        Some("mp3") => "audio/mpeg",
        Some("wav") => "audio/wav",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}

/// Milliseconds since the epoch → an RFC 9110 `IMF-fixdate`, computed the
/// boring way (civil-from-days) rather than pulling in a date crate for
/// one header.
fn httpdate_ms(ms: u128) -> String {
    let secs = (ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    // Howard Hinnant's civil_from_days, the standard closed form.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    const WDAY: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MON: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    format!(
        "{}, {:02} {} {} {:02}:{:02}:{:02} GMT",
        WDAY[days.rem_euclid(7) as usize],
        d,
        MON[(m - 1) as usize],
        y,
        sod / 3600,
        (sod / 60) % 60,
        sod % 60
    )
}

/// POST /api/exports/{name}/file?path=… — create or overwrite with the body.
async fn file_post(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(q): Query<PathQuery>,
    body: bytes::Bytes,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let export = state.registry.get(&name).ok_or(StatusCode::NOT_FOUND)?;
    if export.read_only {
        return Err(StatusCode::FORBIDDEN);
    }
    let rel = RelPath(q.path);
    let full = export.resolve_new(&rel).map_err(code_to_status)?;
    let written = body.len();
    // HTTP mutations are deliberately origin-LESS: origin tagging exists only
    // so a mounted session doesn't hear its own writes echoed back, and an
    // HTTP client has no session or subscription to suppress. Every mount
    // SHOULD see these changes via the watcher — that's correct, not a gap.
    let version = tokio::task::spawn_blocking(move || -> Result<u64, ErrorCode> {
        // Land atomically: a concurrent GET must stream either the old or
        // the new content, never a half-written truncation — which is what
        // writing in place handed readers. Same-directory tmp + rename,
        // the sidecar idiom; unique per request so racing posts to one
        // path each land whole (last rename wins, none interleave). The
        // watcher sees a dotfile create + rename pair; coalescing folds it.
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = format!(
            ".{}.{}.alloyfs-post",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let tmp = full.with_file_name(match full.file_name().and_then(|n| n.to_str()) {
            Some(n) => format!(".{n}{unique}"),
            None => unique,
        });
        let landed = std::fs::write(&tmp, &body).and_then(|()| std::fs::rename(&tmp, &full));
        if landed.is_err() {
            let _ = std::fs::remove_file(&tmp);
            return Err(ErrorCode::Io);
        }
        Ok(export.bump(&rel))
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    .map_err(code_to_status)?;
    Ok(Json(
        serde_json::json!({ "written": written, "version": version }),
    ))
}

/// POST /api/exports/{name}/mkdir?path=…
async fn mkdir_post(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(q): Query<PathQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let export = state.registry.get(&name).ok_or(StatusCode::NOT_FOUND)?;
    if export.read_only {
        return Err(StatusCode::FORBIDDEN);
    }
    let rel = RelPath(q.path);
    let full = export.resolve_new(&rel).map_err(code_to_status)?;
    tokio::task::spawn_blocking(move || -> Result<(), ErrorCode> {
        std::fs::create_dir(&full).map_err(|e| match e.kind() {
            std::io::ErrorKind::AlreadyExists => ErrorCode::AlreadyExists,
            _ => ErrorCode::Io,
        })?;
        export.bump(&rel);
        Ok(())
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    .map_err(code_to_status)?;
    Ok(Json(serde_json::json!({ "created": true })))
}

/// POST /api/exports/{name}/delete?path=… — file or EMPTY directory.
async fn delete_post(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(q): Query<PathQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let export = state.registry.get(&name).ok_or(StatusCode::NOT_FOUND)?;
    if export.read_only {
        return Err(StatusCode::FORBIDDEN);
    }
    let rel = RelPath(q.path);
    let full = export.resolve(&rel).map_err(code_to_status)?;
    tokio::task::spawn_blocking(move || -> Result<(), ErrorCode> {
        let md = std::fs::symlink_metadata(&full).map_err(|_| ErrorCode::NotFound)?;
        if md.is_dir() {
            std::fs::remove_dir(&full).map_err(|e| match e.kind() {
                std::io::ErrorKind::DirectoryNotEmpty => ErrorCode::NotEmpty,
                _ => ErrorCode::Io,
            })?;
        } else {
            std::fs::remove_file(&full).map_err(|_| ErrorCode::Io)?;
        }
        export.bump(&rel);
        Ok(())
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    .map_err(code_to_status)?;
    Ok(Json(serde_json::json!({ "deleted": true })))
}

#[derive(Serialize)]
struct ExportInfo {
    name: String,
    root: String,
    read_only: bool,
    last_seq: u64,
}

fn export_info(e: &alloyfs_agent::Export) -> ExportInfo {
    ExportInfo {
        name: e.name.clone(),
        root: e.root.display().to_string(),
        read_only: e.read_only,
        last_seq: e.events.last_seq(),
    }
}

async fn status(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "exports": state.registry.all().iter().map(|e| export_info(e)).collect::<Vec<_>>(),
    }))
}

async fn exports(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(
        state
            .registry
            .all()
            .iter()
            .map(|e| export_info(e))
            .collect::<Vec<_>>(),
    )
}

#[derive(Deserialize)]
struct BrowseQuery {
    #[serde(default)]
    path: String,
}

#[derive(Serialize)]
struct BrowseEntry {
    name: String,
    kind: &'static str,
    size: u64,
    mtime_ms: u128,
    version: u64,
}

async fn browse(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(q): Query<BrowseQuery>,
) -> Result<Json<Vec<BrowseEntry>>, StatusCode> {
    let export = state.registry.get(&name).ok_or(StatusCode::NOT_FOUND)?;
    let rel = RelPath(q.path);
    let entries = tokio::task::spawn_blocking(move || export.browse(&rel))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .map_err(code_to_status)?;
    Ok(Json(
        entries
            .into_iter()
            .map(|e| BrowseEntry {
                name: e.name,
                kind: match e.attr.kind {
                    FileKind::Dir => "dir",
                    FileKind::Symlink => "symlink",
                    FileKind::File => "file",
                },
                size: e.attr.size,
                mtime_ms: e
                    .attr
                    .mtime
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis(),
                version: e.attr.version,
            })
            .collect(),
    ))
}

/// SSE stream of change events. `Last-Event-ID` (standard SSE reconnect
/// header) maps to the server's ring log for catch-up; too-old resumes get a
/// `resync` event instead of silently missing changes.
async fn events_sse(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, StatusCode> {
    let export = state.registry.get(&name).ok_or(StatusCode::NOT_FOUND)?;
    let since = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());

    let (catchup, rx) = match export.events.subscribe(since) {
        Ok(pair) => pair,
        Err(ErrorCode::TooOld) => {
            let (catchup, rx) = export
                .events
                .subscribe(None)
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            drop(catchup);
            let resync = futures::stream::once(async {
                Ok(Event::default()
                    .event("resync")
                    .data("event history expired; refetch state"))
            });
            let live = live_stream(rx);
            return Ok(Sse::new(Box::pin(futures::StreamExt::chain(resync, live))
                as std::pin::Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>)
            .keep_alive(KeepAlive::default()));
        }
        Err(_) => return Err(StatusCode::INTERNAL_SERVER_ERROR),
    };

    let catchup_stream = futures::stream::iter(catchup.into_iter().map(|ev| Ok(fs_event(&ev))));
    let stream = futures::StreamExt::chain(catchup_stream, live_stream(rx));
    Ok(
        Sse::new(Box::pin(stream)
            as std::pin::Pin<
                Box<dyn Stream<Item = Result<Event, Infallible>> + Send>,
            >)
        .keep_alive(KeepAlive::default()),
    )
}

fn fs_event(ev: &alloyfs_proto::FsEvent) -> Event {
    Event::default()
        .id(ev.seq.to_string())
        .event("fs")
        .data(serde_json::to_string(ev).unwrap_or_default())
}

fn live_stream(
    rx: tokio::sync::broadcast::Receiver<Vec<alloyfs_proto::FsEvent>>,
) -> impl Stream<Item = Result<Event, Infallible>> + Send {
    use futures::StreamExt;
    tokio_stream::wrappers::BroadcastStream::new(rx)
        .filter_map(|item| async move {
            match item {
                Ok(batch) => {
                    let events: Vec<Result<Event, Infallible>> =
                        batch.iter().map(|ev| Ok(fs_event(ev))).collect();
                    Some(futures::stream::iter(events))
                }
                // Lagged: tell the client to resync rather than lose data silently.
                Err(_) => Some(futures::stream::iter(vec![Ok(Event::default()
                    .event("resync")
                    .data("stream lagged; refetch state"))])),
            }
        })
        .flatten()
}

#[cfg(test)]
mod range_tests {
    use super::*;

    #[test]
    fn ranges_parse_clamp_and_refuse() {
        // Plain, open-ended, suffix — all against a 100-byte file.
        assert_eq!(parse_range("bytes=0-49", 100), Ok(Some((0, 49))));
        assert_eq!(parse_range("bytes=50-", 100), Ok(Some((50, 99))));
        assert_eq!(parse_range("bytes=-10", 100), Ok(Some((90, 99))));
        // An end past EOF clamps; a start past EOF is the 416 case.
        assert_eq!(parse_range("bytes=90-500", 100), Ok(Some((90, 99))));
        assert_eq!(parse_range("bytes=100-", 100), Err(()));
        assert_eq!(parse_range("bytes=-0", 100), Err(()));
        assert_eq!(parse_range("bytes=0-", 0), Err(()));
        // Advisory means ignorable: malformed, inverted, multi-range, and
        // other units all serve the whole file rather than erroring.
        assert_eq!(parse_range("bytes=5-2", 100), Ok(None));
        assert_eq!(parse_range("bytes=a-b", 100), Ok(None));
        assert_eq!(parse_range("bytes=0-1,5-6", 100), Ok(None));
        assert_eq!(parse_range("items=0-1", 100), Ok(None));
    }

    #[test]
    fn httpdate_matches_known_instants() {
        assert_eq!(httpdate_ms(0), "Thu, 01 Jan 1970 00:00:00 GMT");
        // 2026-08-21 12:34:56 UTC — a Friday; the civil-from-days math and
        // the weekday offset pin each other.
        assert_eq!(httpdate_ms(1_787_315_696_000), "Fri, 21 Aug 2026 12:34:56 GMT");
    }
}
