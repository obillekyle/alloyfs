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
    let state = Arc::new(AppState { registry, token });
    let app = Router::new()
        .route("/api/status", get(status))
        .route("/api/exports", get(exports))
        .route("/api/exports/{name}/browse", get(browse))
        .route("/api/exports/{name}/file", get(file_get).post(file_post))
        .route("/api/exports/{name}/mkdir", post(mkdir_post))
        .route("/api/exports/{name}/delete", post(delete_post))
        .route("/api/exports/{name}/events", get(events_sse))
        .layer(middleware::from_fn_with_state(state.clone(), auth))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(listen).await?;
    tracing::info!(%listen, "listening (http)");
    axum::serve(listener, app).await?;
    Ok(())
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
async fn file_get(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(q): Query<PathQuery>,
) -> Result<Response, StatusCode> {
    let export = state.registry.get(&name).ok_or(StatusCode::NOT_FOUND)?;
    let rel = RelPath(q.path);
    let full = export.resolve(&rel).map_err(code_to_status)?;
    let file = tokio::fs::File::open(&full)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    let md = file
        .metadata()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if md.is_dir() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let stream = tokio_util::io::ReaderStream::new(file);
    let body = axum::body::Body::from_stream(stream);
    Response::builder()
        .header("content-type", "application/octet-stream")
        .header("content-length", md.len())
        .body(body)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
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
        std::fs::write(&full, &body).map_err(|_| ErrorCode::Io)?;
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
        .map_err(|e| match e {
            ErrorCode::NotFound => StatusCode::NOT_FOUND,
            ErrorCode::PermissionDenied | ErrorCode::InvalidPath => StatusCode::FORBIDDEN,
            ErrorCode::NotADirectory => StatusCode::BAD_REQUEST,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        })?;
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
