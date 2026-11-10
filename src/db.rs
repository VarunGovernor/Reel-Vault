//! SQLite schema, reversible migrations, and FTS index sync.
//! The bundled SQLite build is shared read/write with nothing else;
//! snapshots are manual (see api.rs backup), never concurrent WAL sharing.

use rusqlite::Connection;

pub const SCHEMA_VERSION: i64 = 1;

pub struct Db {
    pub conn: Connection,
}

pub fn now_i64() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Open (creating the file if missing) with WAL and foreign keys on.
pub fn open(path: &std::path::Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(conn)
}

/// One migration step: up SQL, and the matching down SQL.
pub struct Migration {
    pub version: i64,
    pub up: &'static str,
    pub down: &'static str,
}

pub fn migrations() -> Vec<Migration> {
    vec![Migration {
        version: 1,
        up: "
            CREATE TABLE IF NOT EXISTS collections (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL UNIQUE,
                source TEXT NOT NULL DEFAULT 'instagram-export',
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS reels (
                id INTEGER PRIMARY KEY,
                uri TEXT NOT NULL UNIQUE,
                code TEXT NOT NULL UNIQUE,
                title TEXT NOT NULL DEFAULT '',
                caption TEXT NOT NULL DEFAULT '',
                taken_at INTEGER,
                imported_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS collection_items (
                collection_id INTEGER NOT NULL REFERENCES collections(id) ON DELETE CASCADE,
                reel_id INTEGER NOT NULL REFERENCES reels(id) ON DELETE CASCADE,
                PRIMARY KEY (collection_id, reel_id)
            );
            CREATE TABLE IF NOT EXISTS notes (
                id INTEGER PRIMARY KEY,
                reel_id INTEGER NOT NULL REFERENCES reels(id) ON DELETE CASCADE,
                body TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS settings (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS import_runs (
                id INTEGER PRIMARY KEY,
                imported_at INTEGER NOT NULL,
                report TEXT NOT NULL
            );
            CREATE VIRTUAL TABLE IF NOT EXISTS reels_fts USING fts5(
                uri UNINDEXED, title, caption, notes
            );
        ",
        down: "
            DROP TABLE IF EXISTS reels_fts;
            DROP TABLE IF EXISTS import_runs;
            DROP TABLE IF EXISTS settings;
            DROP TABLE IF EXISTS notes;
            DROP TABLE IF EXISTS collection_items;
            DROP TABLE IF EXISTS reels;
            DROP TABLE IF EXISTS collections;
        ",
    }]
}

/// Apply pending migrations forward, in one transaction per step.
pub fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    for m in migrations() {
        if m.version > current {
            conn.execute_batch("BEGIN;")?;
            let r = conn
                .execute_batch(m.up)
                .and_then(|_| conn.pragma_update(None, "user_version", m.version));
            match r {
                Ok(()) => {
                    conn.execute_batch("COMMIT;")?;
                }
                Err(e) => {
                    let _ = conn.execute_batch("ROLLBACK;");
                    return Err(e);
                }
            }
        }
    }
    Ok(())
}

/// Roll schema back to `version` (0 = empty). Ships the matching down SQL.
pub fn revert_to(conn: &Connection, version: i64) -> rusqlite::Result<()> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let migs = migrations();
    let mut steps: Vec<&Migration> = migs
        .iter()
        .filter(|m| m.version > version && m.version <= current)
        .collect();
    steps.reverse();
    for m in steps {
        conn.execute_batch("BEGIN;")?;
        let r = conn
            .execute_batch(m.down)
            .and_then(|_| conn.pragma_update(None, "user_version", m.version - 1));
        match r {
            Ok(()) => {
                conn.execute_batch("COMMIT;")?;
            }
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK;");
                return Err(e);
            }
        }
    }
    Ok(())
}

/// Replace this reel's FTS row from reel fields and its notes.
pub fn reindex_reel(conn: &Connection, reel_id: i64) -> rusqlite::Result<()> {
    let reel = conn
        .query_row(
            "SELECT uri, title, caption FROM reels WHERE id = ?1",
            [reel_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )
        .map_err(|_| rusqlite::Error::QueryReturnedNoRows)?;
    conn.execute("DELETE FROM reels_fts WHERE rowid = ?1", [reel_id])?;
    let notes = concat_notes(conn, reel_id)?;
    conn.execute(
        "INSERT INTO reels_fts(rowid, uri, title, caption, notes) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![reel_id, reel.0, reel.1, reel.2, notes],
    )?;
    Ok(())
}

pub fn concat_notes(conn: &Connection, reel_id: i64) -> rusqlite::Result<String> {
    let mut stmt =
        conn.prepare("SELECT body FROM notes WHERE reel_id = ?1 ORDER BY created_at, id")?;
    let bodies: Vec<String> = stmt
        .query_map([reel_id], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(bodies.join("\n"))
}

/// Settings are a flat string key/value store; callers own conversions.
pub fn get_setting(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| {
        r.get(0)
    })
    .ok()
}

pub fn set_setting(conn: &Connection, key: &str, value: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO settings(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = ?2",
        rusqlite::params![key, value],
    )?;
    Ok(())
}

pub fn touch_library(conn: &Connection, now: i64) -> rusqlite::Result<()> {
    set_setting(conn, "library_modified_at", &now.to_string())
}

/// Create the collection row if missing; refresh its update timestamp.
pub fn ensure_collection(conn: &Connection, name: &str, now: i64) {
    let _ = conn.execute(
        "INSERT INTO collections(name, source, created_at, updated_at)
         VALUES (?1, 'instagram-export', ?2, ?2)
         ON CONFLICT(name) DO UPDATE SET updated_at = ?2",
        rusqlite::params![name, now],
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn temp_conn() -> (TempDir, Connection) {
        let dir = TempDir::new().expect("tempdir");
        let conn = open(&dir.path().join("library.db")).expect("open");
        (dir, conn)
    }

    #[test]
    fn fts5_index_is_available_in_bundled_sqlite() {
        let (_d, conn) = temp_conn();
        migrate(&conn).expect("migrate");
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM reels_fts", [], |r| r.get(0))
            .expect("fts5 table must exist");
        assert_eq!(count, 0);
        conn.execute(
            "INSERT INTO reels_fts(rowid, uri, title, caption, notes) VALUES (1, 'u', 'ffmpeg trim', 'caption', 'notes')",
            [],
        )
        .expect("fts5 insert");
        let hits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM reels_fts WHERE reels_fts MATCH 'ffmpeg'",
                [],
                |r| r.get(0),
            )
            .expect("fts match");
        assert_eq!(hits, 1);
    }

    #[test]
    fn migrations_apply_and_revert() {
        let (_d, conn) = temp_conn();
        migrate(&conn).expect("migrate up");
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        revert_to(&conn, 0).expect("revert down");
        let v0: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v0, 0);
        let tables: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN ('reels','notes','collections')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tables, 0);
        migrate(&conn).expect("migrate up again");
    }

    #[test]
    fn wal_and_foreign_keys_are_on() {
        let (_d, conn) = temp_conn();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode.to_uppercase(), "WAL");
        migrate(&conn).unwrap();
        let err = conn
            .execute(
                "INSERT INTO notes(reel_id, body, created_at) VALUES (999, 'x', 0)",
                [],
            )
            .expect_err("missing reel must violate FK");
        assert!(
            matches!(err, rusqlite::Error::SqliteFailure(_, Some(ref m)) if m.contains("FOREIGN KEY")),
            "expected FK violation, got {err:?}"
        );
    }
}
