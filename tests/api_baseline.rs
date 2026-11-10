//! Baseline API contract tests exercised with a request-only harness
//! (no real listener). This is the same layer the operator UI talks to.

use axum::body::{to_bytes, Body};
use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::Router;
use reelvault::api::{AppState, Shared};
use reelvault::db;
use reelvault::ollama::OllamaClient;
use std::sync::Arc;
use tempfile::TempDir;
use tower::ServiceExt;

fn data_state(dir: &TempDir) -> Shared {
    let conn = db::open(&dir.path().join("library.db")).expect("open");
    db::migrate(&conn).expect("migrate");
    Arc::new(AppState {
        db: tokio::sync::Mutex::new(conn),
        ollama: OllamaClient {
            base_url: "http://127.0.0.1:1".to_string(), // unreachable: graceful path testable
            probe_timeout_secs: 2,
            call_timeout_secs: 3,
        },
        version: "2.0.0".into(),
        data_dir: dir.path().display().to_string(),
    })
}

fn router_of(state: Shared) -> Router {
    reelvault::api::build_router(state)
}

async fn call(app: Router, req: Request) -> (StatusCode, serde_json::Value) {
    let resp = app.oneshot(req).await.expect("oneshot");
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.expect("body");
    let value: serde_json::Value = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or(serde_json::json!({"raw": String::from_utf8_lossy(&bytes)}))
    };
    (status, value)
}

fn json_req(method: Method, uri: &str, body: Option<serde_json::Value>, host: &str) -> Request {
    let builder = axum::http::Request::builder()
        .method(method)
        .uri(uri)
        .header("host", host)
        .header("content-type", "application/json");
    let req = match body {
        Some(v) => builder.body(Body::from(v.to_string())).expect("body"),
        None => builder.body(Body::empty()).expect("body"),
    };
    req
}

fn build_export(dir: &TempDir, posts: &str, collections: Option<&str>) -> std::path::PathBuf {
    let root = dir.path().join("export");
    std::fs::create_dir_all(root.join("saved")).expect("mkdir");
    std::fs::write(root.join("saved/saved_posts.json"), posts).expect("write posts");
    if let Some(c) = collections {
        std::fs::write(root.join("saved/saved_collections.json"), c).expect("write collections");
    }
    root
}

const POSTS_A: &str = r#"[
    {"title":"ffmpeg filter chain","uri":"https://www.instagram.com/p/AAA111/"},
    {"title":"rust lifetimes explainer","uri":"https://www.instagram.com/reel/BBB222/"},
    {"title":"sql window functions","uri":"https://www.instagram.com/p/CCC333/"}
]"#;

const COLLS_A: &str =
    r#"[{"name":"Coding","media_map":{"k":[{"uri":"https://www.instagram.com/p/AAA111/"}]}}]"#;

#[tokio::test]
async fn status_shows_counts_and_graceful_ai() {
    let dir = TempDir::new().unwrap();
    let app = router_of(data_state(&dir));
    let (status, body) = call(
        app,
        json_req(Method::GET, "/api/status", None, "127.0.0.1:8423"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["reels"], 0);
    assert_eq!(body["collections"], 0);
    assert_eq!(body["notes"], 0);
    assert_eq!(body["ai_opt_in"], false);
    assert_eq!(body["ai_model"], "qwen2.5:3b");
    assert_eq!(body["ollama_available"], false);
    assert!(body["ollama_error"].as_str().unwrap().contains("ollama"));
}

#[tokio::test]
async fn import_then_search_notes_collections() {
    let dir = TempDir::new().unwrap();
    let export = build_export(&dir, POSTS_A, Some(COLLS_A));
    let app = router_of(data_state(&dir));

    let (status, report) = call(
        app.clone(),
        json_req(
            Method::POST,
            "/api/import",
            Some(serde_json::json!({"path": export.display().to_string()})),
            "127.0.0.1:8423",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(report["counts"]["reels_new"], 3);
    assert_eq!(report["counts"]["memberships"], 1);

    // Search by note text after adding a note to reel 1.
    let (status, _) = call(
        app.clone(),
        json_req(
            Method::POST,
            "/api/notes/1",
            Some(serde_json::json!({"body": "trim filter syntax worth memorizing"})),
            "127.0.0.1:8423",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (_, body) = call(
        app.clone(),
        json_req(
            Method::GET,
            "/api/entries?q=memorizing&page_size=10",
            None,
            "127.0.0.1:8423",
        ),
    )
    .await;
    assert_eq!(body["total"], 1);
    assert_eq!(body["items"][0]["code"], "AAA111");
    assert_eq!(body["items"][0]["note_count"], 1);

    // Title search also goes through FTS.
    let (_, body) = call(
        app.clone(),
        json_req(
            Method::GET,
            "/api/entries?q=lifetimes",
            None,
            "127.0.0.1:8423",
        ),
    )
    .await;
    assert_eq!(body["total"], 1);
    assert_eq!(body["items"][0]["code"], "BBB222");

    // Collection listing and filter.
    let (_, colls) = call(
        app.clone(),
        json_req(Method::GET, "/api/collections", None, "127.0.0.1:8423"),
    )
    .await;
    assert_eq!(colls[0]["name"], "Coding");
    assert_eq!(colls[0]["reels"], 1);
    let (_, body) = call(
        app.clone(),
        json_req(
            Method::GET,
            "/api/entries?collection=1",
            None,
            "127.0.0.1:8423",
        ),
    )
    .await;
    assert_eq!(body["total"], 1);
    assert_eq!(body["items"][0]["code"], "AAA111");

    // Pagination contract.
    let (_, body) = call(
        app.clone(),
        json_req(
            Method::GET,
            "/api/entries?page=2&page_size=2",
            None,
            "127.0.0.1:8423",
        ),
    )
    .await;
    assert_eq!(body["total"], 3);
    assert_eq!(body["items"].as_array().unwrap().len(), 1);

    // Note delete removes from index.
    let note_id = call(
        app.clone(),
        json_req(Method::GET, "/api/reel/1", None, "127.0.0.1:8423"),
    )
    .await
    .1["notes"][0]["id"]
        .as_i64()
        .unwrap();
    let (status, _) = call(
        app.clone(),
        json_req(Method::DELETE, "/api/notes/1", None, "127.0.0.1:8423"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, body) = call(
        app.clone(),
        json_req(
            Method::GET,
            "/api/entries?q=memorizing",
            None,
            "127.0.0.1:8423",
        ),
    )
    .await;
    assert_eq!(body["total"], 0);
    let _ = note_id;
}

#[tokio::test]
async fn reimport_keeps_notes_and_counts() {
    let dir = TempDir::new().unwrap();
    let export = build_export(&dir, POSTS_A, None);
    let app = router_of(data_state(&dir));

    let _ = call(
        app.clone(),
        json_req(
            Method::POST,
            "/api/import",
            Some(serde_json::json!({"path": export.display().to_string()})),
            "127.0.0.1:8423",
        ),
    )
    .await;
    let (status, note) = call(
        app.clone(),
        json_req(
            Method::POST,
            "/api/notes/2",
            Some(serde_json::json!({"body": "lifetime pattern"})),
            "127.0.0.1:8423",
        ),
    )
    .await;
    // Reel id 2 matters here: a buggy last_insert_rowid read would only
    // work for the first note on reel 1 by rowid collision.
    assert_eq!(status, StatusCode::OK);
    assert_eq!(note["body"], "lifetime pattern");
    assert_eq!(note["reel_id"], 2);

    let (status, report) = call(
        app.clone(),
        json_req(
            Method::POST,
            "/api/import",
            Some(serde_json::json!({"path": export.display().to_string()})),
            "127.0.0.1:8423",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(report["counts"]["reels_new"], 0);
    assert_eq!(report["counts"]["reels_merged"], 3);

    let (_, body) = call(
        app.clone(),
        json_req(Method::GET, "/api/reel/2", None, "127.0.0.1:8423"),
    )
    .await;
    assert_eq!(body["notes"][0]["body"], "lifetime pattern");
}

#[tokio::test]
async fn backup_restore_roundtrip_on_fresh_folders() {
    let live = TempDir::new().unwrap();
    let scratch = TempDir::new().unwrap();
    let export = build_export(&scratch, POSTS_A, None);
    let app = router_of(data_state(&live));

    let _ = call(
        app.clone(),
        json_req(
            Method::POST,
            "/api/import",
            Some(serde_json::json!({"path": export.display().to_string()})),
            "127.0.0.1:8423",
        ),
    )
    .await;
    let _ = call(
        app.clone(),
        json_req(
            Method::POST,
            "/api/notes/1",
            Some(serde_json::json!({"body": "note must survive restore"})),
            "127.0.0.1:8423",
        ),
    )
    .await;

    let backup_dir = scratch.path().join("backup");
    std::fs::create_dir_all(&backup_dir).unwrap();
    let (status, body) = call(
        app.clone(),
        json_req(
            Method::POST,
            "/api/backup",
            Some(serde_json::json!({"target_dir": backup_dir.display().to_string()})),
            "127.0.0.1:8423",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let snap = body["file"].as_str().unwrap().to_string();
    assert!(std::path::Path::new(&snap).is_file());

    // Refuse a second snapshot into the same (now non-empty) folder.
    let (status, _) = call(
        app.clone(),
        json_req(
            Method::POST,
            "/api/backup",
            Some(serde_json::json!({"target_dir": backup_dir.display().to_string()})),
            "127.0.0.1:8423",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Restore into a fresh directory.
    let restore_dir = scratch.path().join("restored");
    let (status, body) = call(
        app.clone(),
        json_req(
            Method::POST,
            "/api/restore",
            Some(serde_json::json!({
                "snapshot_path": snap,
                "data_dir": restore_dir.display().to_string()
            })),
            "127.0.0.1:8423",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["restored_to"].as_str().unwrap().contains("library.db"));

    // The restored library is a real, working library.
    let restored_conn = db::open(&restore_dir.join("library.db")).expect("open restored");
    let reels: i64 = restored_conn
        .query_row("SELECT COUNT(*) FROM reels", [], |r| r.get(0))
        .unwrap();
    assert_eq!(reels, 3);
    let notes: i64 = restored_conn
        .query_row("SELECT COUNT(*) FROM notes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(notes, 1);

    // Never overwrite the live library.
    let (status, _) = call(
        app.clone(),
        json_req(
            Method::POST,
            "/api/restore",
            Some(serde_json::json!({
                "snapshot_path": snap,
                "data_dir": live.path().display().to_string()
            })),
            "127.0.0.1:8423",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Non-sqlite file rejected.
    let junk = scratch.path().join("junk.db");
    std::fs::write(&junk, b"not a database").unwrap();
    let (status, _) = call(
        app.clone(),
        json_req(
            Method::POST,
            "/api/restore",
            Some(serde_json::json!({
                "snapshot_path": junk.display().to_string(),
                "data_dir": scratch.path().join("fresh").display().to_string()
            })),
            "127.0.0.1:8423",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn ai_gate_and_unreachable_engine() {
    let dir = TempDir::new().unwrap();
    let app = router_of(data_state(&dir));

    // Off by default: 403.
    let (status, body) = call(
        app.clone(),
        json_req(
            Method::POST,
            "/api/ai/generate",
            Some(serde_json::json!({"prompt": "hello"})),
            "127.0.0.1:8423",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body["error"].as_str().unwrap().contains("off"));

    // Opt in, then call with an unreachable Ollama: clean 502, no hang.
    let (status, _) = call(
        app.clone(),
        json_req(
            Method::POST,
            "/api/settings",
            Some(serde_json::json!({"ai_opt_in": true})),
            "127.0.0.1:8423",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = call(
        app.clone(),
        json_req(
            Method::POST,
            "/api/ai/generate",
            Some(serde_json::json!({"prompt": "hello"})),
            "127.0.0.1:8423",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(body["error"].as_str().unwrap().contains("failed"));

    // Cross-site origin is rejected before reaching the handler.
    let bad_req = json_req(
        Method::POST,
        "/api/import",
        Some(serde_json::json!({"path": "/tmp/x"})),
        "evil.example",
    );
    let resp = app.oneshot(bad_req).await.expect("oneshot");
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}
