//! HTTP API for the browser UI. All state lives in one SQLite connection
//! behind an async mutex; local-only semantics enforced in security.rs.

use crate::db;
use crate::import::{run_import, ImportError};
use crate::ollama::OllamaClient;
use crate::security;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rusqlite::types::Value as SqlValue;
use rusqlite::Connection as DbConn;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

pub const DEFAULT_AI_MODEL: &str = "qwen2.5:3b";
pub const MAX_NOTE_BODY: usize = 10_000;
pub const MAX_PROMPT: usize = 4_000;

pub struct AppState {
    pub db: Mutex<DbConn>,
    pub ollama: OllamaClient,
    pub version: String,
    pub data_dir: String,
}

pub type Shared = Arc<AppState>;

fn err_json(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({"error": msg}))).into_response()
}

fn bad(msg: &str) -> Response {
    err_json(StatusCode::BAD_REQUEST, msg)
}

fn internal(msg: &str) -> Response {
    err_json(StatusCode::INTERNAL_SERVER_ERROR, msg)
}

// ---------- status ----------

#[derive(Serialize)]
pub struct StatusResp {
    pub version: String,
    pub data_dir: String,
    pub reels: i64,
    pub collections: i64,
    pub notes: i64,
    pub ai_opt_in: bool,
    pub ai_model: String,
    pub ollama_available: bool,
    pub ollama_models: Vec<String>,
    pub ollama_error: Option<String>,
}

async fn status(State(state): State<Shared>) -> Response {
    let (ollama_available, ollama_models, ollama_error) = match state.ollama.available().await {
        Ok(models) => (true, models, None),
        Err(e) => (false, Vec::new(), Some(e)),
    };
    let conn = state.db.lock().await;
    match status_of(&conn, &state, ollama_available, ollama_models, ollama_error) {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(e) => internal(&e.to_string()),
    }
}

fn status_of(
    conn: &DbConn,
    state: &AppState,
    ollama_available: bool,
    ollama_models: Vec<String>,
    ollama_error: Option<String>,
) -> rusqlite::Result<StatusResp> {
    let count = |sql: &str| -> rusqlite::Result<i64> { conn.query_row(sql, [], |r| r.get(0)) };
    Ok(StatusResp {
        version: state.version.clone(),
        data_dir: state.data_dir.clone(),
        reels: count("SELECT COUNT(*) FROM reels")?,
        collections: count("SELECT COUNT(*) FROM collections")?,
        notes: count("SELECT COUNT(*) FROM notes")?,
        ai_opt_in: db::get_setting(conn, "ai_opt_in")
            .map(|v| v == "true")
            .unwrap_or(false),
        ai_model: ai_model(conn),
        ollama_available,
        ollama_models,
        ollama_error,
    })
}

fn ai_model(conn: &DbConn) -> String {
    db::get_setting(conn, "ai_model")
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| {
            std::env::var("REELVAULT_AI_MODEL")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_AI_MODEL.to_string())
        })
}

// ---------- collections ----------

#[derive(Serialize)]
pub struct CollectionRow {
    pub id: i64,
    pub name: String,
    pub reels: i64,
}

async fn collections(State(state): State<Shared>) -> Response {
    let conn = state.db.lock().await;
    let mut stmt = match conn.prepare(
        "SELECT c.id, c.name,
                (SELECT COUNT(*) FROM collection_items ci WHERE ci.collection_id = c.id)
         FROM collections c ORDER BY c.name",
    ) {
        Ok(s) => s,
        Err(e) => return internal(&e.to_string()),
    };
    let rows = match stmt.query_map([], |r| {
        Ok(CollectionRow {
            id: r.get(0)?,
            name: r.get(1)?,
            reels: r.get(2)?,
        })
    }) {
        Ok(m) => m.collect::<rusqlite::Result<Vec<_>>>(),
        Err(e) => return internal(&e.to_string()),
    };
    match rows {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(e) => internal(&e.to_string()),
    }
}

// ---------- entries (paginated search) ----------

#[derive(Deserialize)]
pub struct EntriesQuery {
    pub q: Option<String>,
    pub collection: Option<i64>,
    pub page: Option<u32>,
    pub page_size: Option<u32>,
}

#[derive(Serialize)]
pub struct EntryRow {
    pub id: i64,
    pub uri: String,
    pub code: String,
    pub title: String,
    pub note_count: i64,
    pub collections: Vec<String>,
}

#[derive(Serialize)]
pub struct EntriesResp {
    pub items: Vec<EntryRow>,
    pub total: i64,
    pub page: u32,
    pub page_size: u32,
}

impl EntriesQuery {
    fn clamped(&self) -> (u32, u32, u32) {
        let page = self.page.unwrap_or(1).max(1);
        let size = self.page_size.unwrap_or(50).clamp(1, 200);
        let offset = (page - 1) * size;
        (page, size, offset)
    }

    fn fts_match(&self) -> Option<String> {
        let q = self.q.as_ref()?;
        let tokens: Vec<String> = q
            .split_whitespace()
            .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
            .collect::<Vec<_>>();
        if tokens.is_empty() {
            None
        } else {
            Some(tokens.join(" "))
        }
    }
}

async fn entries(State(state): State<Shared>, Query(q): Query<EntriesQuery>) -> Response {
    let conn = state.db.lock().await;
    let (page, size, offset) = q.clamped();
    let match_q = q.fts_match();
    let has_q = match_q.is_some();
    let has_coll = q.collection.is_some();

    let where_sql = match (has_q, has_coll) {
        (true, true) => "WHERE reels_fts MATCH ?1 AND ci.collection_id = ?2",
        (true, false) => "WHERE reels_fts MATCH ?1",
        (false, true) => "WHERE ci.collection_id = ?1",
        (false, false) => "",
    };
    let joins = if has_coll {
        "JOIN collection_items ci ON ci.reel_id = r.id"
    } else {
        ""
    };
    let from_sql = if has_q {
        format!("FROM reels_fts f JOIN reels r ON r.id = f.rowid {joins} {where_sql}")
    } else {
        format!("FROM reels r {joins} {where_sql}")
    };

    let mut params: Vec<SqlValue> = Vec::new();
    if let Some(m) = &match_q {
        params.push(SqlValue::Text(m.clone()));
    }
    if let Some(c) = q.collection {
        params.push(SqlValue::Integer(c));
    }

    let total_sql = format!("SELECT COUNT(*) {from_sql}");
    let total: i64 =
        match conn.query_row(&total_sql, rusqlite::params_from_iter(params.iter()), |r| {
            r.get(0)
        }) {
            Ok(t) => t,
            Err(e) => return internal(&e.to_string()),
        };

    let order_sql = if has_q {
        "ORDER BY f.rank, r.id DESC"
    } else {
        "ORDER BY r.id DESC"
    };
    let items_sql = format!(
        "SELECT r.id, r.uri, r.code, r.title,
                (SELECT COUNT(*) FROM notes n WHERE n.reel_id = r.id),
                (SELECT GROUP_CONCAT(c.name, ' | ')
                 FROM collection_items ci2
                 JOIN collections c ON c.id = ci2.collection_id
                 WHERE ci2.reel_id = r.id)
         {from_sql} {order_sql} LIMIT {size} OFFSET {offset}",
    );

    let mut stmt = match conn.prepare(&items_sql) {
        Ok(s) => s,
        Err(e) => return internal(&e.to_string()),
    };
    let rows = match stmt.query_map(rusqlite::params_from_iter(params.iter()), |r| {
        Ok(EntryRow {
            id: r.get(0)?,
            uri: r.get(1)?,
            code: r.get(2)?,
            title: r.get(3)?,
            note_count: r.get(4)?,
            collections: r
                .get::<_, Option<String>>(5)?
                .map(|s| s.split(" | ").map(str::to_string).collect())
                .unwrap_or_default(),
        })
    }) {
        Ok(m) => m.collect::<rusqlite::Result<Vec<_>>>(),
        Err(e) => return internal(&e.to_string()),
    };

    match rows {
        Ok(items) => (
            StatusCode::OK,
            Json(EntriesResp {
                items,
                total,
                page,
                page_size: size,
            }),
        )
            .into_response(),
        Err(e) => internal(&e.to_string()),
    }
}

// ---------- reel detail + notes ----------

#[derive(Serialize)]
pub struct ReelDetail {
    pub id: i64,
    pub uri: String,
    pub code: String,
    pub title: String,
    pub collections: Vec<String>,
    pub imported_at: i64,
    pub updated_at: i64,
    pub notes: Vec<NoteRow>,
}

#[derive(Serialize, Clone)]
pub struct NoteRow {
    pub id: i64,
    pub reel_id: i64,
    pub body: String,
    pub created_at: i64,
}

async fn reel_detail(State(state): State<Shared>, Path(id): Path<i64>) -> Response {
    let conn = state.db.lock().await;
    let reel = match conn.query_row(
        "SELECT id, uri, code, title, imported_at, updated_at FROM reels WHERE id = ?1",
        [id],
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
            ))
        },
    ) {
        Ok(v) => v,
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            return err_json(StatusCode::NOT_FOUND, "reel not found")
        }
        Err(e) => return internal(&e.to_string()),
    };

    let mut stmt = match conn.prepare(
        "SELECT c.name FROM collections c
         JOIN collection_items ci ON ci.collection_id = c.id
         WHERE ci.reel_id = ?1 ORDER BY c.name",
    ) {
        Ok(s) => s,
        Err(e) => return internal(&e.to_string()),
    };
    let colls = match stmt.query_map([id], |r| r.get::<_, String>(0)) {
        Ok(m) => match m.collect::<rusqlite::Result<Vec<_>>>() {
            Ok(v) => v,
            Err(e) => return internal(&e.to_string()),
        },
        Err(e) => return internal(&e.to_string()),
    };

    let notes = match notes_of(&conn, id) {
        Ok(v) => v,
        Err(e) => return internal(&e.to_string()),
    };

    (
        StatusCode::OK,
        Json(ReelDetail {
            id: reel.0,
            uri: reel.1,
            code: reel.2,
            title: reel.3,
            collections: colls,
            imported_at: reel.4,
            updated_at: reel.5,
            notes,
        }),
    )
        .into_response()
}

fn notes_of(conn: &DbConn, reel_id: i64) -> rusqlite::Result<Vec<NoteRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, reel_id, body, created_at FROM notes
         WHERE reel_id = ?1 ORDER BY created_at DESC, id DESC",
    )?;
    let rows = stmt
        .query_map([reel_id], |r| {
            Ok(NoteRow {
                id: r.get(0)?,
                reel_id: r.get(1)?,
                body: r.get(2)?,
                created_at: r.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

#[derive(Deserialize)]
pub struct NoteIn {
    pub body: String,
}

async fn add_note(
    State(state): State<Shared>,
    Path(id): Path<i64>,
    Json(body): Json<NoteIn>,
) -> Response {
    let trimmed = body.body.trim().to_string();
    if trimmed.is_empty() {
        return bad("note body is empty");
    }
    if trimmed.len() > MAX_NOTE_BODY {
        return bad(&format!("note body exceeds {MAX_NOTE_BODY} bytes"));
    }
    let conn = state.db.lock().await;
    let now = db::now_i64();
    let exists: Option<i64> = conn
        .query_row("SELECT id FROM reels WHERE id = ?1", [id], |r| r.get(0))
        .ok();
    if exists.is_none() {
        return err_json(StatusCode::NOT_FOUND, "reel not found");
    }
    let insert = conn.execute(
        "INSERT INTO notes(reel_id, body, created_at) VALUES (?1, ?2, ?3)",
        rusqlite::params![id, trimmed, now],
    );
    let note_id = match insert {
        // Read the new rowid now: reindexing below runs its own writes and
        // would repoint last_insert_rowid elsewhere.
        Ok(_) => conn.last_insert_rowid(),
        Err(e) => return internal(&e.to_string()),
    };
    // Keep FTS and the modified marker in step with user data changes.
    let _ = db::reindex_reel(&conn, id);
    let _ = db::touch_library(&conn, now);
    match conn.query_row(
        "SELECT id, reel_id, body, created_at FROM notes WHERE id = ?1",
        [note_id],
        |r| {
            Ok(NoteRow {
                id: r.get(0)?,
                reel_id: r.get(1)?,
                body: r.get(2)?,
                created_at: r.get(3)?,
            })
        },
    ) {
        Ok(note) => (StatusCode::OK, Json(note)).into_response(),
        Err(e) => internal(&e.to_string()),
    }
}

async fn delete_note(State(state): State<Shared>, Path(id): Path<i64>) -> Response {
    let conn = state.db.lock().await;
    let reel_id: Option<i64> =
        match conn.query_row("SELECT reel_id FROM notes WHERE id = ?1", [id], |r| {
            r.get(0)
        }) {
            Ok(v) => Some(v),
            Err(rusqlite::Error::QueryReturnedNoRows) => None,
            Err(e) => return internal(&e.to_string()),
        };
    match conn.execute("DELETE FROM notes WHERE id = ?1", [id]) {
        Ok(0) => err_json(StatusCode::NOT_FOUND, "note not found"),
        Ok(_) => {
            if let Some(reel) = reel_id {
                let _ = db::reindex_reel(&conn, reel);
                let _ = db::touch_library(&conn, db::now_i64());
            }
            (StatusCode::OK, Json(json!({"deleted": id}))).into_response()
        }
        Err(e) => internal(&e.to_string()),
    }
}

// ---------- import ----------

#[derive(Deserialize)]
pub struct ImportIn {
    pub path: String,
}

async fn import(State(state): State<Shared>, Json(body): Json<ImportIn>) -> Response {
    let path_text = expand_tilde(body.path.trim());
    if path_text.is_empty() {
        return bad("path is empty");
    }
    if body.path.contains("..") {
        return bad("relative path segments are not accepted");
    }
    let path = PathBuf::from(path_text);
    let conn = state.db.lock().await;
    let report = run_import(&conn, &path, db::now_i64());
    match report {
        Ok(report) => {
            let stored = serde_json::to_string(&report).unwrap_or_default();
            let _ = conn.execute(
                "INSERT INTO import_runs(imported_at, report) VALUES (?1, ?2)",
                rusqlite::params![db::now_i64(), stored],
            );
            (StatusCode::OK, Json(report)).into_response()
        }
        Err(ImportError::SourceMissing(p)) => {
            err_json(StatusCode::NOT_FOUND, &format!("source not found: {p}"))
        }
        Err(ImportError::Read(msg)) => bad(&format!("read failed: {msg}")),
    }
}

// ---------- backup / restore (fresh-folder only) ----------

#[derive(Deserialize)]
pub struct BackupIn {
    pub target_dir: String,
}

async fn backup(State(state): State<Shared>, Json(body): Json<BackupIn>) -> Response {
    let target = PathBuf::from(expand_tilde(body.target_dir.trim()));
    if target.as_os_str().is_empty() {
        return bad("target_dir is empty");
    }
    if target.exists() {
        if !target.is_dir() {
            return bad("target_dir is not a directory");
        }
        if std::fs::read_dir(&target)
            .map(|mut d| d.next().is_some())
            .unwrap_or(true)
        {
            return err_json(
                StatusCode::CONFLICT,
                "target_dir must be a fresh empty directory",
            );
        }
    } else if let Err(e) = std::fs::create_dir_all(&target) {
        return internal(&format!("create target_dir: {e}"));
    }

    let conn = state.db.lock().await;
    let now = db::now_i64();
    let _ = db::touch_library(&conn, now);

    // VACUUM INTO writes a standalone consistent snapshot file and refuses
    // to overwrite: an existing file would fail the whole statement.
    let mut file = target.join(format!("reelvault-{now}.db"));
    let mut attempt = 0;
    loop {
        if !file.exists() {
            break;
        }
        attempt += 1;
        file = target.join(format!("reelvault-{now}-{attempt}.db"));
        if attempt > 100 {
            return internal("could not pick a fresh snapshot filename");
        }
    }
    let escaped = file.display().to_string().replace('\'', "''");
    if let Err(e) = conn.execute_batch(&format!("VACUUM INTO '{escaped}';")) {
        return internal(&format!("snapshot failed: {e}"));
    }
    (
        StatusCode::OK,
        Json(json!({"status": "ok", "file": file.display().to_string()})),
    )
        .into_response()
}

#[derive(Deserialize)]
pub struct RestoreIn {
    pub snapshot_path: String,
    pub data_dir: String,
}

async fn restore(State(state): State<Shared>, Json(body): Json<RestoreIn>) -> Response {
    let snap = PathBuf::from(expand_tilde(body.snapshot_path.trim()));
    let data_dir = PathBuf::from(expand_tilde(body.data_dir.trim()));
    if snap.as_os_str().is_empty() || data_dir.as_os_str().is_empty() {
        return bad("snapshot_path and data_dir are required");
    }
    if !snap.is_file() {
        return bad("snapshot_path is not a file");
    }

    // Live library protection: refuse restoring into the active data dir.
    let live = state.data_dir.clone();
    if snap.display().to_string() != live {
        if let Ok(canon_live) = std::fs::canonicalize(&live) {
            if snap == canon_live || data_dir == canon_live {
                return err_json(
                    StatusCode::CONFLICT,
                    "refusing to overwrite the live library; restore into a fresh directory",
                );
            }
        }
    }
    if data_dir.display().to_string() == live
        || data_dir.display().to_string() == body.snapshot_path.trim()
    {
        return err_json(
            StatusCode::CONFLICT,
            "refusing to overwrite the live library; restore into a fresh directory",
        );
    }

    if data_dir.exists() {
        if !data_dir.is_dir() {
            return bad("data_dir is not a directory");
        }
        if std::fs::read_dir(&data_dir)
            .map(|mut d| d.next().is_some())
            .unwrap_or(true)
        {
            return err_json(
                StatusCode::CONFLICT,
                "data_dir must be a fresh empty directory; a snapshot is never merged",
            );
        }
    }

    // Header sanity before any copying.
    let header = match std::fs::File::open(&snap) {
        Ok(mut f) => {
            let mut b = [0u8; 16];
            match std::io::Read::read_exact(&mut f, &mut b) {
                Ok(()) => b,
                Err(e) => return bad(&format!("snapshot unreadable: {e}")),
            }
        }
        Err(e) => return bad(&format!("snapshot unreadable: {e}")),
    };
    if &header != b"SQLite format 3\x00" {
        return bad("snapshot_path is not a SQLite database file");
    }
    match DbConn::open_with_flags(&snap, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(conn) => {
            let version: i64 = conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap_or(0);
            if version < db::SCHEMA_VERSION {
                return bad(&format!(
                    "snapshot schema {version} is older than supported {}",
                    db::SCHEMA_VERSION
                ));
            }
            if version > db::SCHEMA_VERSION {
                return bad(&format!(
                    "snapshot schema {version} is newer than this app version"
                ));
            }
        }
        Err(e) => return bad(&format!("snapshot not open as db: {e}")),
    }

    if let Err(e) = std::fs::create_dir_all(&data_dir) {
        return internal(&format!("create data_dir: {e}"));
    }
    let dest = data_dir.join("library.db");
    if dest.exists() {
        return err_json(
            StatusCode::CONFLICT,
            "data_dir became non-empty during restore",
        );
    }
    match std::fs::copy(&snap, &dest) {
        Ok(_) => (
            StatusCode::OK,
            Json(json!({
                "status": "ok",
                "restored_to": dest.display().to_string(),
                "note": "relaunch with REELVAULT_DATA_DIR set to this directory"
            })),
        )
            .into_response(),
        Err(e) => internal(&format!("copy failed: {e}")),
    }
}

// ---------- settings / ai ----------

#[derive(Deserialize)]
pub struct SettingsIn {
    pub ai_opt_in: Option<bool>,
    pub ai_model: Option<String>,
}

#[derive(Serialize)]
pub struct SettingsResp {
    pub ai_opt_in: bool,
    pub ai_model: String,
}

async fn settings_get(State(state): State<Shared>) -> Response {
    let conn = state.db.lock().await;
    (
        StatusCode::OK,
        Json(SettingsResp {
            ai_opt_in: db::get_setting(&conn, "ai_opt_in")
                .map(|v| v == "true")
                .unwrap_or(false),
            ai_model: ai_model(&conn),
        }),
    )
        .into_response()
}

async fn settings_set(State(state): State<Shared>, Json(body): Json<SettingsIn>) -> Response {
    let conn = state.db.lock().await;
    if let Some(v) = body.ai_opt_in {
        if let Err(e) = db::set_setting(&conn, "ai_opt_in", if v { "true" } else { "false" }) {
            return internal(&e.to_string());
        }
    }
    if let Some(m) = body.ai_model.clone() {
        let trimmed = m.trim().to_string();
        if trimmed.len() > 128 {
            return bad("ai_model too long");
        }
        if !trimmed.is_empty() {
            if let Err(e) = db::set_setting(&conn, "ai_model", &trimmed) {
                return internal(&e.to_string());
            }
        }
    }
    (
        StatusCode::OK,
        Json(SettingsResp {
            ai_opt_in: db::get_setting(&conn, "ai_opt_in")
                .map(|v| v == "true")
                .unwrap_or(false),
            ai_model: ai_model(&conn),
        }),
    )
        .into_response()
}

#[derive(Deserialize)]
pub struct AiIn {
    pub prompt: String,
}

async fn ai_generate(State(state): State<Shared>, Json(body): Json<AiIn>) -> Response {
    let prompt = body.prompt.trim().to_string();
    let conn = state.db.lock().await;
    let opt_in = db::get_setting(&conn, "ai_opt_in")
        .map(|v| v == "true")
        .unwrap_or(false);
    if !opt_in {
        return err_json(
            StatusCode::FORBIDDEN,
            "local AI is off; enable it in settings first",
        );
    }
    if prompt.is_empty() {
        return bad("prompt is empty");
    }
    if prompt.len() > MAX_PROMPT {
        return bad(&format!("prompt exceeds {MAX_PROMPT} bytes"));
    }
    let model = ai_model(&conn);
    drop(conn);
    match state.ollama.generate(&model, &prompt).await {
        Ok(text) => (StatusCode::OK, Json(json!({"model": model, "text": text}))).into_response(),
        Err(e) => err_json(
            StatusCode::BAD_GATEWAY,
            &format!("local model call failed: {e}"),
        ),
    }
}

// ---------- router ----------

/// Expand a leading "~/" to HOME so the UI can paste home-relative paths.
fn expand_tilde(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{}", std::path::PathBuf::from(home).join(rest).display());
        }
    }
    p.to_string()
}

pub fn build_router(state: Shared) -> Router {
    Router::new()
        .route("/api/status", get(status))
        .route("/api/collections", get(collections))
        .route("/api/entries", get(entries))
        .route("/api/reel/{id}", get(reel_detail))
        .route("/api/notes/{id}", post(add_note).delete(delete_note))
        .route("/api/import", post(import))
        .route("/api/backup", post(backup))
        .route("/api/restore", post(restore))
        .route("/api/settings", get(settings_get).post(settings_set))
        .route("/api/ai/generate", post(ai_generate))
        .layer(middleware::from_fn(security::local_guard))
        .with_state(state)
}
