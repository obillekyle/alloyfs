//! The HTTP API, driven through the `Router` rather than a socket.
//!
//! `tower::ServiceExt::oneshot` feeds a request straight into the router, so
//! these tests need no port, no bind, and have no chance of colliding with a
//! parallel test or a real agent. The bearer-token cases matter most: this is
//! the one surface where a mistake exposes every export over the network.

use std::sync::Arc;

use alloyfs_agent::{AgentConfig, ExportConfig, ExportRegistry};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

// ------------------------------------------------------------------ harness

struct Api {
    dir: tempfile::TempDir,
    registry: Arc<ExportRegistry>,
    token: Option<String>,
}

fn api(token: Option<&str>) -> Api {
    let dir = tempfile::TempDir::new().expect("tempdir");
    std::fs::write(dir.path().join("one.txt"), b"first file").unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    std::fs::write(dir.path().join("sub/two.txt"), b"nested").unwrap();
    // An excluded path, to prove the API honours server-side excludes rather
    // than reimplementing its own view of the export.
    std::fs::write(dir.path().join("secret.key"), b"do not serve").unwrap();

    let mut cfg = AgentConfig::default();
    cfg.exports.insert(
        "test".to_string(),
        ExportConfig {
            path: dir.path().to_path_buf(),
            read_only: false,
            exclude: vec!["*.key".to_string()],
            client: None,
        },
    );
    let registry = Arc::new(ExportRegistry::from_config(&cfg).expect("registry"));
    Api {
        dir,
        registry,
        token: token.map(str::to_string),
    }
}

impl Api {
    /// One request through the router. `auth` is the bearer token to present.
    async fn send(&self, method: &str, uri: &str, auth: Option<&str>, body: Body) -> (StatusCode, String) {
        let app = alloyfs_http::router(self.registry.clone(), self.token.clone());
        let mut req = Request::builder().method(method).uri(uri);
        if let Some(t) = auth {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        let resp = app.oneshot(req.body(body).unwrap()).await.expect("router");
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .expect("body");
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    async fn get(&self, uri: &str) -> (StatusCode, String) {
        let auth = self.token.clone();
        self.send("GET", uri, auth.as_deref(), Body::empty()).await
    }

    async fn post(&self, uri: &str, body: &str) -> (StatusCode, String) {
        let auth = self.token.clone();
        self.send("POST", uri, auth.as_deref(), Body::from(body.to_string()))
            .await
    }
}

// --------------------------------------------------------------------- auth

/// The security boundary. A token-protected API must refuse anything without
/// a correct bearer token, and must refuse it the same way whether the header
/// is missing, malformed, or simply wrong.
#[tokio::test]
async fn a_protected_api_refuses_every_wrong_token() {
    let api = api(Some("s3cret"));

    for (label, header) in [
        ("no header", None),
        ("empty bearer", Some("")),
        ("wrong token", Some("nope")),
        ("prefix of the real token", Some("s3cre")),
        ("real token plus a suffix", Some("s3cretx")),
    ] {
        let (status, _) = api.send("GET", "/api/status", header, Body::empty()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{label} was accepted");
    }

    let (status, _) = api
        .send("GET", "/api/status", Some("s3cret"), Body::empty())
        .await;
    assert_eq!(status, StatusCode::OK, "the correct token must be accepted");
}

/// Auth is a layer over the whole router, so it has to cover every route —
/// not just the one that happened to be tested.
#[tokio::test]
async fn auth_covers_every_route() {
    let api = api(Some("s3cret"));
    for uri in [
        "/api/status",
        "/api/exports",
        "/api/exports/test/browse?path=",
        "/api/exports/test/file?path=one.txt",
    ] {
        let (status, _) = api.send("GET", uri, None, Body::empty()).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{uri} was reachable unauthenticated"
        );
    }
    for uri in [
        "/api/exports/test/mkdir?path=x",
        "/api/exports/test/delete?path=x",
    ] {
        let (status, _) = api.send("POST", uri, None, Body::empty()).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{uri} was reachable unauthenticated"
        );
    }
}

/// No token configured = deliberately open, which `serve` only permits on
/// loopback. Worth pinning so nobody "fixes" it into a 401 and breaks every
/// local dashboard.
#[tokio::test]
async fn an_untokened_api_is_open() {
    let api = api(None);
    let (status, _) = api.send("GET", "/api/status", None, Body::empty()).await;
    assert_eq!(status, StatusCode::OK);
}

// ------------------------------------------------------------------- reads

#[tokio::test]
async fn status_and_exports_list_the_export() {
    let api = api(None);

    let (status, body) = api.get("/api/status").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("\"version\""),
        "status must report a version: {body}"
    );
    assert!(body.contains("test"), "status must name the export: {body}");

    let (status, body) = api.get("/api/exports").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("test"));
}

#[tokio::test]
async fn browse_lists_a_directory_and_honours_excludes() {
    let api = api(None);

    let (status, body) = api.get("/api/exports/test/browse?path=").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("one.txt"));
    assert!(body.contains("sub"));
    assert!(
        !body.contains("secret.key"),
        "an excluded path must be invisible to the API too: {body}"
    );

    let (status, body) = api.get("/api/exports/test/browse?path=sub").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("two.txt"));
}

#[tokio::test]
async fn file_get_returns_contents_and_404s_for_missing() {
    let api = api(None);

    let (status, body) = api.get("/api/exports/test/file?path=one.txt").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "first file");

    let (status, _) = api.get("/api/exports/test/file?path=nope.txt").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Excluded paths report NotFound, never Forbidden: existence must not leak.
    let (status, _) = api.get("/api/exports/test/file?path=secret.key").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn an_unknown_export_is_not_found() {
    let api = api(None);
    let (status, _) = api.get("/api/exports/nosuch/browse?path=").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Path traversal has to be refused by the same hardening the wire protocol
/// uses, not by a second implementation that can drift from it.
#[tokio::test]
async fn traversal_out_of_the_export_is_refused() {
    let api = api(None);
    for path in ["../outside.txt", "sub/../../outside.txt", "/etc/passwd"] {
        let (status, _) = api
            .get(&format!("/api/exports/test/file?path={}", urlencode(path)))
            .await;
        assert!(
            status == StatusCode::NOT_FOUND || status == StatusCode::FORBIDDEN,
            "{path} produced {status}, which is neither a refusal nor a miss"
        );
    }
}

fn urlencode(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' | '~' | '/' => c.to_string(),
            other => format!("%{:02X}", other as u32),
        })
        .collect()
}

// ------------------------------------------------------------------ writes

#[tokio::test]
async fn file_post_writes_and_mkdir_creates() {
    let api = api(None);

    let (status, _) = api
        .post("/api/exports/test/file?path=written.txt", "via the api")
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        std::fs::read_to_string(api.dir.path().join("written.txt")).unwrap(),
        "via the api",
        "the write must reach the real export"
    );

    let (status, _) = api.post("/api/exports/test/mkdir?path=fresh", "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(api.dir.path().join("fresh").is_dir());
}

#[tokio::test]
async fn delete_removes_and_then_misses() {
    let api = api(None);

    let (status, _) = api.post("/api/exports/test/delete?path=one.txt", "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!api.dir.path().join("one.txt").exists());

    let (status, _) = api.post("/api/exports/test/delete?path=one.txt", "").await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "deleting twice must not claim success"
    );
}

/// A read-only export must refuse writes through the API just as it does over
/// the wire — the flag is not a client-side courtesy.
#[tokio::test]
async fn a_read_only_export_refuses_writes() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(dir.path().join("one.txt"), b"immutable").unwrap();
    let mut cfg = AgentConfig::default();
    cfg.exports.insert(
        "ro".to_string(),
        ExportConfig {
            path: dir.path().to_path_buf(),
            read_only: true,
            exclude: Vec::new(),
            client: None,
        },
    );
    let registry = Arc::new(ExportRegistry::from_config(&cfg).unwrap());

    let app = alloyfs_http::router(registry, None);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/exports/ro/file?path=one.txt")
                .body(Body::from("overwrite"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(
        resp.status(),
        StatusCode::OK,
        "a read-only export accepted a write"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("one.txt")).unwrap(),
        "immutable"
    );
}

// -------------------------------------------------------------------- SSE

/// The event stream must open and stay open. Its contents are the watcher's
/// business (covered in the agent); what matters here is that the endpoint
/// negotiates as an event stream rather than 404ing or closing immediately.
#[tokio::test]
async fn the_event_stream_opens() {
    let api = api(None);
    let app = alloyfs_http::router(api.registry.clone(), None);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/exports/test/events")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("router");
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(ct.starts_with("text/event-stream"), "content-type was {ct:?}");
}
