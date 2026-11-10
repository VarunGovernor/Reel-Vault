//! ReelVault server: localhost-only Rust backend serving the browser UI.

use reelvault::api::{self, AppState};
use reelvault::datadir;
use reelvault::db;
use reelvault::ollama::OllamaClient;
use std::sync::Arc;
use tokio::sync::Mutex;
use tower_http::services::ServeDir;

fn web_dir() -> std::path::PathBuf {
    std::env::var("REELVAULT_WEB_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("web"))
}

#[tokio::main]
async fn main() {
    let port: u16 = std::env::var("REELVAULT_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8423);

    let data_dir = datadir::resolve();
    if let Err(e) = std::fs::create_dir_all(&data_dir) {
        eprintln!(
            "ReelVault: cannot create data dir {}: {e}",
            data_dir.display()
        );
        std::process::exit(1);
    }
    let db_path = data_dir.join("library.db");
    let conn = match db::open(&db_path).and_then(|c| {
        db::migrate(&c)?;
        Ok(c)
    }) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "ReelVault: cannot open library at {}: {e}",
                db_path.display()
            );
            std::process::exit(1);
        }
    };

    let state: api::Shared = Arc::new(AppState {
        db: Mutex::new(conn),
        ollama: OllamaClient::from_env(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        data_dir: data_dir.display().to_string(),
    });

    let app: axum::Router = api::build_router(state)
        .fallback_service(ServeDir::new(web_dir()).append_index_html_on_directories(true));

    // Localhost only, by contract.
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("ReelVault: cannot bind 127.0.0.1:{port}: {e}");
            std::process::exit(1);
        }
    };
    eprintln!(
        "ReelVault v{} at http://127.0.0.1:{port} (data: {})",
        env!("CARGO_PKG_VERSION"),
        db_path.display()
    );
    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("ReelVault: serve error: {e}");
        std::process::exit(1);
    }
}
