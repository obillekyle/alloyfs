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
            ..Default::default()
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

    /// POST with `content-type: application/json`, which axum's `Json`
    /// extractor requires — without it the request is rejected before the
    /// handler runs, and the test would be measuring the extractor.
    async fn post_json(&self, uri: &str, body: &str) -> (StatusCode, String) {
        self.request("POST", uri, &[("content-type", "application/json")], body)
            .await
    }

    /// One request with arbitrary headers, for the conditional-write cases.
    async fn request(
        &self,
        method: &str,
        uri: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> (StatusCode, String) {
        let app = alloyfs_http::router(self.registry.clone(), self.token.clone());
        let mut req = Request::builder().method(method).uri(uri);
        if let Some(t) = &self.token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let resp = app
            .oneshot(req.body(Body::from(body.to_string())).unwrap())
            .await
            .expect("router");
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .expect("body");
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    /// A response header, for reading back the ETag a GET issued.
    async fn header_of(&self, uri: &str, name: &str) -> Option<String> {
        let app = alloyfs_http::router(self.registry.clone(), self.token.clone());
        let mut req = Request::builder().method("GET").uri(uri);
        if let Some(t) = &self.token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        let resp = app.oneshot(req.body(Body::empty()).unwrap()).await.ok()?;
        resp.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
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
    // Every GET route the router declares. This list is maintained by hand,
    // which is a weakness — but an unauthenticated endpoint is the one
    // mistake on this surface that exposes every export, so a list that has
    // to be extended alongside a new route is still worth more than testing
    // whichever route came first.
    for uri in [
        "/api/status",
        "/api/exports",
        "/api/exports/test/browse?path=",
        "/api/exports/test/stat?path=one.txt",
        "/api/exports/test/statfs",
        "/api/exports/test/readlink?path=one.txt",
        "/api/exports/test/file?path=one.txt",
        "/api/exports/test/events",
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
        "/api/exports/test/rename",
        "/api/exports/test/copy",
        "/api/exports/test/setattr?path=x",
        "/api/exports/test/bulk",
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
            ..Default::default()
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

// ------------------------------------------------------- the gaps, closed

/// A directory answered as ONE unbounded array before this: a
/// hundred-thousand-entry directory was a hundred-thousand-entry response,
/// built in memory before a byte went out, with nothing in the API to ask
/// for less.
#[tokio::test]
async fn browse_pages_and_the_cursor_walks_the_whole_directory() {
    let api = api(None);
    for i in 0..25 {
        std::fs::write(api.dir.path().join(format!("f{i:03}.txt")), b"x").unwrap();
    }

    let (status, body) = api.get("/api/exports/test/browse?path=&limit=10").await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let total = v["total"].as_u64().unwrap();
    assert_eq!(
        v["entries"].as_array().unwrap().len(),
        10,
        "limit must bound the page"
    );
    assert_eq!(v["next_cursor"].as_u64(), Some(10));

    // Walk to the end and prove the union is the whole directory exactly
    // once — the property a positional cursor is easy to get wrong on.
    let mut seen: Vec<String> = Vec::new();
    let mut cursor = 0u64;
    loop {
        let (_, body) = api
            .get(&format!(
                "/api/exports/test/browse?path=&limit=10&cursor={cursor}"
            ))
            .await;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        for e in v["entries"].as_array().unwrap() {
            seen.push(e["name"].as_str().unwrap().to_string());
        }
        match v["next_cursor"].as_u64() {
            Some(n) => cursor = n,
            None => break,
        }
    }
    assert_eq!(seen.len() as u64, total, "every entry, exactly once");
    let mut dedup = seen.clone();
    dedup.sort();
    dedup.dedup();
    assert_eq!(dedup.len(), seen.len(), "no entry served twice");
    assert!(!seen.iter().any(|n| n == "secret.key"), "excludes still hold");
}

/// One path's attributes without listing its parent — which was O(directory)
/// to answer a question about a single entry.
#[tokio::test]
async fn stat_answers_one_path_and_honours_excludes() {
    let api = api(None);

    let (status, body) = api.get("/api/exports/test/stat?path=one.txt").await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["name"], "one.txt");
    assert_eq!(v["kind"], "file");
    assert_eq!(v["size"].as_u64(), Some(10));

    let (status, _) = api.get("/api/exports/test/stat?path=sub").await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = api.get("/api/exports/test/stat?path=secret.key").await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "an excluded path must not be stattable"
    );
}

#[tokio::test]
async fn statfs_reports_capacity() {
    let api = api(None);
    let (status, body) = api.get("/api/exports/test/statfs").await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(v["bytes_total"].as_u64().unwrap() > 0);
    assert!(v["blocks"].as_u64().unwrap() > 0);
    assert!(v["bytes_free"].as_u64().unwrap() <= v["bytes_total"].as_u64().unwrap());
}

/// The lost-update hole: `file_get` handed out ETags and nothing accepted
/// them back, so two clients that each read, edited and wrote lost one edit
/// silently.
#[tokio::test]
async fn a_stale_if_match_is_refused_and_a_current_one_is_accepted() {
    let api = api(None);
    let etag = api
        .header_of("/api/exports/test/file?path=one.txt", "etag")
        .await
        .expect("GET must issue an ETag");

    // Someone else writes first, so the tag we hold is now stale.
    let (status, _) = api
        .post("/api/exports/test/file?path=one.txt", "written by the other")
        .await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = api
        .request(
            "POST",
            "/api/exports/test/file?path=one.txt",
            &[("if-match", &etag)],
            "my edit, based on what I read",
        )
        .await;
    assert_eq!(
        status,
        StatusCode::PRECONDITION_FAILED,
        "a write against a stale ETag must be refused, not silently applied"
    );
    assert_eq!(
        std::fs::read_to_string(api.dir.path().join("one.txt")).unwrap(),
        "written by the other",
        "and it must not have written anything"
    );

    // The current tag is accepted.
    let fresh = api
        .header_of("/api/exports/test/file?path=one.txt", "etag")
        .await
        .unwrap();
    let (status, _) = api
        .request(
            "POST",
            "/api/exports/test/file?path=one.txt",
            &[("if-match", &fresh)],
            "mine now",
        )
        .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn if_none_match_star_claims_a_path_only_when_absent() {
    let api = api(None);

    let (status, _) = api
        .request(
            "POST",
            "/api/exports/test/file?path=claimed.txt",
            &[("if-none-match", "*")],
            "first",
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = api
        .request(
            "POST",
            "/api/exports/test/file?path=claimed.txt",
            &[("if-none-match", "*")],
            "second",
        )
        .await;
    assert_eq!(
        status,
        StatusCode::PRECONDITION_FAILED,
        "the second claim must lose"
    );
    assert_eq!(
        std::fs::read_to_string(api.dir.path().join("claimed.txt")).unwrap(),
        "first"
    );
}

/// `MAX_BODY` capped a request at 256 MiB, and with no partial write that was
/// a hard ceiling on the size of file this API could produce at all.
#[tokio::test]
async fn offset_writes_build_a_file_larger_than_one_request() {
    let api = api(None);

    let (status, _) = api.post("/api/exports/test/file?path=chunked.bin", "AAAA").await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = api
        .post("/api/exports/test/file?path=chunked.bin&offset=4", "BBBB")
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("\"partial\":true"), "body was {body}");
    let (status, _) = api
        .post("/api/exports/test/file?path=chunked.bin&offset=8", "CCCC")
        .await;
    assert_eq!(status, StatusCode::OK);

    assert_eq!(
        std::fs::read_to_string(api.dir.path().join("chunked.bin")).unwrap(),
        "AAAABBBBCCCC",
        "chunks must land end to end, not replace each other"
    );

    // And an offset past the end extends rather than failing.
    let (status, _) = api
        .post("/api/exports/test/file?path=chunked.bin&offset=16", "D")
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        std::fs::metadata(api.dir.path().join("chunked.bin"))
            .unwrap()
            .len(),
        17
    );
}

/// A move used to be GET + POST + DELETE: not atomic, twice the bytes, and a
/// window where the file existed twice or not at all.
#[tokio::test]
async fn rename_moves_atomically_and_copy_duplicates() {
    let api = api(None);

    let (status, _) = api
        .post_json(
            "/api/exports/test/rename",
            r#"{"from":"one.txt","to":"sub/moved.txt"}"#,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!api.dir.path().join("one.txt").exists());
    assert_eq!(
        std::fs::read_to_string(api.dir.path().join("sub/moved.txt")).unwrap(),
        "first file"
    );

    let (status, body) = api
        .post_json(
            "/api/exports/test/copy",
            r#"{"from":"sub/moved.txt","to":"copy.txt"}"#,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(api.dir.path().join("sub/moved.txt").exists(), "source stays");
    assert_eq!(
        std::fs::read_to_string(api.dir.path().join("copy.txt")).unwrap(),
        "first file"
    );

    // A rename that would escape the export is refused by the same check
    // every other write path uses.
    let (status, _) = api
        .post_json(
            "/api/exports/test/rename",
            r#"{"from":"copy.txt","to":"../escaped.txt"}"#,
        )
        .await;
    assert!(
        status.is_client_error(),
        "a traversal target must be refused, got {status}"
    );
}

#[tokio::test]
async fn setattr_changes_mtime_and_reports_it_back() {
    let api = api(None);
    let (status, body) = api
        .request(
            "POST",
            "/api/exports/test/setattr?path=one.txt",
            &[("content-type", "application/json")],
            r#"{"mtime_ms":1000000000000}"#,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (_, body) = api.get("/api/exports/test/stat?path=one.txt").await;
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        v["mtime_ms"].as_u64(),
        Some(1_000_000_000_000),
        "the value set must be the value read back"
    );
}

/// Per-path results, and one bad path does not take the others with it —
/// which is the entire reason to have a bulk endpoint rather than a loop.
#[tokio::test]
async fn bulk_reports_each_path_independently() {
    let api = api(None);

    let (status, body) = api
        .post_json(
            "/api/exports/test/bulk",
            r#"{"op":"stat","paths":["one.txt","nope.txt","sub","secret.key"]}"#,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "a mixed batch is still a 200: {body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let r = v.as_array().unwrap();
    assert_eq!(r.len(), 4, "one result per path, in order");
    assert_eq!(r[0]["ok"], true);
    assert_eq!(r[0]["entry"]["size"].as_u64(), Some(10));
    assert_eq!(r[1]["ok"], false);
    assert_eq!(r[1]["error"], "not_found");
    assert_eq!(r[2]["ok"], true);
    assert_eq!(r[2]["entry"]["kind"], "dir");
    assert_eq!(r[3]["ok"], false, "an excluded path is not visible in bulk");

    // Bulk delete, same contract.
    let (status, body) = api
        .post_json(
            "/api/exports/test/bulk",
            r#"{"op":"delete","paths":["one.txt","nope.txt"]}"#,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v[0]["ok"], true);
    assert_eq!(v[1]["ok"], false);
    assert!(!api.dir.path().join("one.txt").exists());
}

#[tokio::test]
async fn a_read_only_export_refuses_the_new_write_routes() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(dir.path().join("a.txt"), b"x").unwrap();
    let mut cfg = AgentConfig::default();
    cfg.exports.insert(
        "ro".to_string(),
        ExportConfig {
            path: dir.path().to_path_buf(),
            read_only: true,
            ..Default::default()
        },
    );
    let api = Api {
        dir,
        registry: Arc::new(ExportRegistry::from_config(&cfg).unwrap()),
        token: None,
    };

    for (uri, body) in [
        ("/api/exports/ro/rename", r#"{"from":"a.txt","to":"b.txt"}"#),
        ("/api/exports/ro/copy", r#"{"from":"a.txt","to":"b.txt"}"#),
        ("/api/exports/ro/bulk", r#"{"op":"delete","paths":["a.txt"]}"#),
    ] {
        let (status, _) = api.post_json(uri, body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri} wrote to a read-only export");
    }
    let (status, _) = api
        .request(
            "POST",
            "/api/exports/ro/setattr?path=a.txt",
            &[("content-type", "application/json")],
            r#"{"mode":511}"#,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        std::fs::read_to_string(api.dir.path().join("a.txt")).unwrap(),
        "x",
        "nothing may have changed"
    );
}

#[tokio::test]
async fn recursive_delete_is_opt_in() {
    let api = api(None);
    std::fs::create_dir_all(api.dir.path().join("tree/deep")).unwrap();
    std::fs::write(api.dir.path().join("tree/deep/f.txt"), b"x").unwrap();

    let (status, _) = api.post("/api/exports/test/delete?path=tree", "").await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "a non-empty directory must not vanish without asking"
    );
    assert!(api.dir.path().join("tree/deep/f.txt").exists());

    let (status, _) = api
        .post("/api/exports/test/delete?path=tree&recursive=true", "")
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!api.dir.path().join("tree").exists());
}

/// `browse` reported `kind: "symlink"` and gave no way to resolve one.
#[cfg(unix)]
#[tokio::test]
async fn readlink_returns_the_stored_target() {
    let api = api(None);
    std::os::unix::fs::symlink("one.txt", api.dir.path().join("link.txt")).unwrap();

    let (status, body) = api.get("/api/exports/test/readlink?path=link.txt").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["target"], "one.txt");

    let (status, _) = api.get("/api/exports/test/readlink?path=one.txt").await;
    assert!(
        status.is_client_error(),
        "a plain file is not a link, got {status}"
    );
}

// ------------------------------------------------------------------- CORS

/// Sending nothing is the default, and it is a security property rather than
/// an omission: an agent on loopback with no token is reachable by anything
/// on the machine, and the only reason a page you happen to visit cannot read
/// every export is that the browser discards a reply with no
/// `Access-Control-Allow-Origin`.
#[tokio::test]
async fn no_cors_headers_unless_origins_are_configured() {
    let api = api(None);
    let app = alloyfs_http::router(api.registry.clone(), None);
    let res = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/status")
                .header("origin", "https://dashboard.example.com")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert!(
        res.headers().get("access-control-allow-origin").is_none(),
        "a default agent must not tell a browser it may read this"
    );
}

async fn cors_app(api: &Api, origins: &[&str]) -> axum::Router {
    alloyfs_http::router_with_cors(
        api.registry.clone(),
        api.token.clone(),
        origins.iter().map(|s| s.to_string()).collect(),
    )
}

#[tokio::test]
async fn a_listed_origin_is_echoed_and_an_unlisted_one_is_not() {
    let api = api(None);
    let allowed = "https://dashboard.example.com";

    let res = cors_app(&api, &[allowed])
        .await
        .oneshot(
            Request::builder()
                .uri("/api/status")
                .header("origin", allowed)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        res.headers()
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some(allowed),
        "the listed origin must be echoed back verbatim"
    );
    // Without this the browser hands JS a response with almost no headers,
    // and `read()` would come back with no ETag — which looks exactly like
    // the server forgetting to send one, and silently breaks safe writes.
    let expose = res
        .headers()
        .get("access-control-expose-headers")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(expose.contains("etag"), "expose was {expose:?}");
    assert!(res.headers().get("vary").is_some(), "must vary on origin");

    // A different origin gets a normal response with no CORS headers, so the
    // browser discards it.
    let res = cors_app(&api, &[allowed])
        .await
        .oneshot(
            Request::builder()
                .uri("/api/status")
                .header("origin", "https://evil.example.com")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        res.headers().get("access-control-allow-origin").is_none(),
        "an unlisted origin must never be echoed"
    );
}

/// The subtle one. A preflight is an `OPTIONS` carrying no credentials — the
/// browser will not attach them — so if the CORS layer sat behind auth every
/// preflight would 401 and the real request would never be sent. The symptom
/// is a CORS error naming a header that was configured perfectly.
#[tokio::test]
async fn a_preflight_succeeds_without_a_token_even_on_a_protected_api() {
    let api = api(Some("s3cret"));
    let allowed = "https://dashboard.example.com";
    let res = cors_app(&api, &[allowed])
        .await
        .oneshot(
            Request::builder()
                .method("OPTIONS")
                .uri("/api/exports/test/rename")
                .header("origin", allowed)
                .header("access-control-request-method", "POST")
                .header("access-control-request-headers", "authorization,content-type")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        StatusCode::NO_CONTENT,
        "a preflight carries no token and must still be answered"
    );
    let h = res.headers();
    assert_eq!(
        h.get("access-control-allow-origin").and_then(|v| v.to_str().ok()),
        Some(allowed)
    );
    let allow_headers = h
        .get("access-control-allow-headers")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    for needed in ["authorization", "range", "if-match", "if-none-match"] {
        assert!(
            allow_headers.contains(needed),
            "{needed} must be allowed or the browser drops it: {allow_headers:?}"
        );
    }
}

#[tokio::test]
async fn a_preflight_from_an_unlisted_origin_is_refused() {
    let api = api(None);
    let res = cors_app(&api, &["https://dashboard.example.com"])
        .await
        .oneshot(
            Request::builder()
                .method("OPTIONS")
                .uri("/api/status")
                .header("origin", "https://evil.example.com")
                .header("access-control-request-method", "GET")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    assert!(res.headers().get("access-control-allow-origin").is_none());
}

/// Chrome refuses a public page's request to 127.0.0.1 unless the preflight
/// says this. Without it the headline case — a hosted dashboard talking to
/// the agent on your own machine — cannot work, and fails with a message
/// about the network rather than about CORS.
#[tokio::test]
async fn private_network_access_is_granted_only_when_asked_for() {
    let api = api(None);
    let allowed = "https://dashboard.example.com";

    let res = cors_app(&api, &[allowed])
        .await
        .oneshot(
            Request::builder()
                .method("OPTIONS")
                .uri("/api/status")
                .header("origin", allowed)
                .header("access-control-request-method", "GET")
                .header("access-control-request-private-network", "true")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        res.headers()
            .get("access-control-allow-private-network")
            .and_then(|v| v.to_str().ok()),
        Some("true")
    );

    // Not volunteered when the browser did not ask.
    let res = cors_app(&api, &[allowed])
        .await
        .oneshot(
            Request::builder()
                .method("OPTIONS")
                .uri("/api/status")
                .header("origin", allowed)
                .header("access-control-request-method", "GET")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(res
        .headers()
        .get("access-control-allow-private-network")
        .is_none());
}

/// CORS decides whether a browser lets JS READ a reply; it never grants
/// access. A listed origin with no token is still a 401.
#[tokio::test]
async fn an_allowed_origin_still_needs_the_token() {
    let api = api(Some("s3cret"));
    let allowed = "https://dashboard.example.com";
    let res = cors_app(&api, &[allowed])
        .await
        .oneshot(
            Request::builder()
                .uri("/api/status")
                .header("origin", allowed)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        StatusCode::UNAUTHORIZED,
        "being on the origin list is not authorisation"
    );
}

/// `*` turns the check off. Supported for local development, where a dev
/// server's port changes per run and cannot be listed usefully.
#[tokio::test]
async fn a_wildcard_allows_any_origin() {
    let api = api(None);
    for origin in [
        "http://localhost:5173",
        "http://localhost:61234",
        "https://anything.example.com",
    ] {
        let res = cors_app(&api, &["*"])
            .await
            .oneshot(
                Request::builder()
                    .uri("/api/status")
                    .header("origin", origin)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            res.headers()
                .get("access-control-allow-origin")
                .and_then(|v| v.to_str().ok()),
            Some(origin),
            "the REQUESTING origin is echoed, not a literal `*` — a literal is \
             rejected by the browser the moment a request is credentialed"
        );
        assert_eq!(
            res.headers().get("vary").and_then(|v| v.to_str().ok()),
            Some("origin"),
            "still varies on origin, so a shared cache cannot cross-serve"
        );
    }
}

#[tokio::test]
async fn a_wildcard_answers_any_preflight() {
    let api = api(Some("s3cret"));
    let res = cors_app(&api, &["*"])
        .await
        .oneshot(
            Request::builder()
                .method("OPTIONS")
                .uri("/api/exports/test/bulk")
                .header("origin", "http://localhost:5173")
                .header("access-control-request-method", "POST")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        res.headers()
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some("http://localhost:5173")
    );
}

/// The property that survives turning CORS off: it was never authorisation.
/// `*` says a browser may READ the reply; the token still decides whether
/// there is a reply worth reading.
#[tokio::test]
async fn a_wildcard_does_not_bypass_the_token() {
    let api = api(Some("s3cret"));
    let res = cors_app(&api, &["*"])
        .await
        .oneshot(
            Request::builder()
                .uri("/api/status")
                .header("origin", "https://evil.example.com")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        StatusCode::UNAUTHORIZED,
        "turning CORS off must not turn auth off"
    );
}

/// No origin header at all — every non-browser caller, curl included — is
/// unaffected either way.
#[tokio::test]
async fn a_request_without_an_origin_is_untouched() {
    let api = api(None);
    for origins in [vec![], vec!["*"]] {
        let res = cors_app(&api, &origins)
            .await
            .oneshot(Request::builder().uri("/api/status").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(res.headers().get("access-control-allow-origin").is_none());
    }
}
