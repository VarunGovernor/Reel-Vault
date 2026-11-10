//! Instagram official export ingestion.
//!
//! Contract: every recognized file parses, every unrecognized structure is
//! reported, nothing is invented. A report with issues is "partial" and is
//! shown to the user verbatim; counts are measured, never estimated.

use crate::db;
use rusqlite::Connection;
use serde::Serialize;
use std::collections::HashSet;
use std::io::Read;
use std::path::Path;

const MAX_FILE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_WALK_DEPTH: usize = 4;

#[derive(Serialize, Clone, Debug, Default)]
pub struct ImportCounts {
    pub reels_new: u32,
    pub reels_merged: u32,
    pub duplicates: u32,
    pub collections: u32,
    pub memberships: u32,
    pub invalid_links: u32,
    pub unknown_structures: u32,
    pub skipped: u32,
}

#[derive(Serialize, Clone, Debug)]
pub struct ImportIssue {
    pub kind: String,
    pub file: String,
    pub detail: String,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct ImportReport {
    pub source: String,
    pub status: String,
    pub files_read: Vec<String>,
    pub counts: ImportCounts,
    pub issues: Vec<ImportIssue>,
}

#[derive(Debug)]
pub enum ImportError {
    SourceMissing(String),
    Read(String),
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImportError::SourceMissing(p) => write!(f, "source not found: {p}"),
            ImportError::Read(m) => write!(f, "read failed: {m}"),
        }
    }
}

enum Entry {
    Link(String, String), // canonical uri, shortcode
    Invalid(String),
    Empty,
    Unknown,
}

pub fn run_import(conn: &Connection, source: &Path, now: i64) -> Result<ImportReport, ImportError> {
    let mut report = ImportReport {
        source: source.display().to_string(),
        status: "complete".to_string(),
        ..Default::default()
    };

    if !source.exists() {
        return Err(ImportError::SourceMissing(report.source.clone()));
    }

    let files: Vec<(String, Vec<u8>)> = if is_zip(source) {
        read_zip(source, &mut report)?
    } else {
        read_dir_files(source, &mut report)?
    };

    let mut saw_any_relevant = false;
    let mut handled: HashSet<String> = HashSet::new();
    for (name, bytes) in files {
        let base = normalize_base(&name);
        let kind = match base.as_str() {
            "saved_posts.json" => Some("posts"),
            "saved_collections.json" => Some("collections_map"),
            "saved_saved_collections.json" => Some("collections_items"),
            _ => None,
        };
        if kind.is_none() || !handled.insert(name.clone()) {
            continue;
        }
        saw_any_relevant = true;
        report.files_read.push(name.clone());
        parse_file(conn, kind.unwrap(), &bytes, &name, now, &mut report);
    }

    if !saw_any_relevant {
        report.issues.push(ImportIssue {
            kind: "no_recognized_files".into(),
            file: report.source.clone(),
            detail: "expected saved_posts.json or collection files; none matched".into(),
        });
        report.status = "empty".into();
    }

    if report.status != "empty" && !report.issues.is_empty() {
        report.status = "partial".into();
    }

    db::touch_library(conn, now).ok();
    Ok(report)
}

fn is_zip(p: &Path) -> bool {
    let mut magic = [0u8; 2];
    std::fs::File::open(p)
        .and_then(|mut f| f.read_exact(&mut magic))
        .map(|_| magic == [b'P', b'K'])
        .unwrap_or(false)
}

fn read_zip(path: &Path, report: &mut ImportReport) -> Result<Vec<(String, Vec<u8>)>, ImportError> {
    let file = std::fs::File::open(path).map_err(|e| ImportError::Read(e.to_string()))?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|e| ImportError::Read(format!("zip: {e}")))?;
    let mut out = Vec::new();
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| ImportError::Read(format!("zip entry {i}: {e}")))?;
        if entry.is_dir() {
            continue;
        }
        if entry.size() > MAX_FILE_BYTES {
            report.issues.push(ImportIssue {
                kind: "oversized".into(),
                file: entry.name().to_string(),
                detail: format!("entry larger than {MAX_FILE_BYTES} bytes; skipped"),
            });
            continue;
        }
        let mut buf = Vec::new();
        if let Err(e) = (&mut entry).take(MAX_FILE_BYTES).read_to_end(&mut buf) {
            report.issues.push(ImportIssue {
                kind: "read_failed".into(),
                file: entry.name().to_string(),
                detail: e.to_string(),
            });
            continue;
        }
        out.push((entry.name().to_string(), buf));
    }
    Ok(out)
}

fn read_dir_files(
    dir: &Path,
    report: &mut ImportReport,
) -> Result<Vec<(String, Vec<u8>)>, ImportError> {
    let mut out = Vec::new();
    walk(dir, dir, 0, report, &mut out)?;
    Ok(out)
}

fn walk(
    root: &Path,
    dir: &Path,
    depth: usize,
    report: &mut ImportReport,
    out: &mut Vec<(String, Vec<u8>)>,
) -> Result<(), ImportError> {
    if depth > MAX_WALK_DEPTH {
        report.issues.push(ImportIssue {
            kind: "depth_truncated".into(),
            file: dir.display().to_string(),
            detail: format!("stopped walk deeper than {MAX_WALK_DEPTH} levels"),
        });
        return Ok(());
    }
    let entries = std::fs::read_dir(dir).map_err(|e| ImportError::Read(e.to_string()))?;
    let mut sorted: Vec<std::fs::DirEntry> = entries.flatten().collect();
    sorted.sort_by_key(|e| e.file_name());
    for entry in sorted {
        let path = entry.path();
        if path.is_dir() {
            walk(root, &path, depth + 1, report, out)?;
        } else {
            let md = std::fs::metadata(&path).map_err(|e| ImportError::Read(e.to_string()))?;
            let rel = path
                .strip_prefix(root)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| path.display().to_string());
            if md.len() > MAX_FILE_BYTES {
                report.issues.push(ImportIssue {
                    kind: "oversized".into(),
                    file: rel.clone(),
                    detail: format!("file larger than {MAX_FILE_BYTES} bytes; skipped"),
                });
                continue;
            }
            let bytes = std::fs::read(&path).map_err(|e| ImportError::Read(e.to_string()))?;
            out.push((rel, bytes));
        }
    }
    Ok(())
}

fn normalize_base(name: &str) -> String {
    name.replace('\\', "/")
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_lowercase()
}

fn parse_file(
    conn: &Connection,
    kind: &str,
    bytes: &[u8],
    file_name: &str,
    now: i64,
    report: &mut ImportReport,
) {
    let parsed: serde_json::Value = match serde_json::from_slice(bytes) {
        Ok(v) => v,
        Err(e) => {
            report.issues.push(ImportIssue {
                kind: "invalid_json".into(),
                file: file_name.into(),
                detail: e.to_string(),
            });
            return;
        }
    };
    match kind {
        "posts" => parse_post_list(conn, &parsed, file_name, now, report),
        "collections_map" => parse_collections_map(conn, &parsed, file_name, now, report),
        "collections_items" => parse_collections_items(conn, &parsed, file_name, now, report),
        _ => {
            report.issues.push(ImportIssue {
                kind: "unknown_structure".into(),
                file: file_name.into(),
                detail: "unclassified file kind".into(),
            });
        }
    }
}

fn as_array(v: &serde_json::Value) -> Vec<&serde_json::Value> {
    v.as_array().map(|a| a.iter().collect()).unwrap_or_default()
}

/// Global saved list: array of entries, links via `uri`,
/// `media_map_data[].href`, or `string_map_data.*.href`.
fn parse_post_list(
    conn: &Connection,
    json: &serde_json::Value,
    file_name: &str,
    now: i64,
    report: &mut ImportReport,
) {
    if !json.is_array() {
        record_unknown(json, file_name, "expected array", report);
        return;
    }
    for entry in as_array(json) {
        match classify(entry) {
            Entry::Link(uri, code) => {
                apply_reel(conn, &uri, &code, &entry_title(entry), "", now, report)
            }
            Entry::Invalid(msg) => record_invalid(file_name, &msg, report),
            Entry::Empty => record_skipped(file_name, "empty saved entry", report),
            Entry::Unknown => record_unknown(
                entry,
                file_name,
                "saved entry lacks recognizable link",
                report,
            ),
        }
    }
}

/// Newer shape: collections with `media_map` (object of item arrays) or
/// `media_list_data` arrays per collection.
fn parse_collections_map(
    conn: &Connection,
    json: &serde_json::Value,
    file_name: &str,
    now: i64,
    report: &mut ImportReport,
) {
    if !json.is_array() {
        record_unknown(json, file_name, "expected array", report);
        return;
    }
    for coll in as_array(json) {
        let name = coll["name"].as_str().unwrap_or("").trim().to_string();
        if name.is_empty() {
            record_skipped(file_name, "collection with empty name", report);
            continue;
        }
        db::ensure_collection(conn, &name, now);
        report.counts.collections += 1;
        let mut items: Vec<&serde_json::Value> = Vec::new();
        if let Some(map) = coll["media_map"].as_object() {
            for v in map.values() {
                if let Some(arr) = v.as_array() {
                    items.extend(arr.iter());
                }
            }
        }
        if let Some(arr) = coll["media_list_data"].as_array() {
            items.extend(arr.iter());
        }
        if items.is_empty() && looks_structural(coll) {
            record_unknown(
                coll,
                file_name,
                &format!("collection '{name}' has media_map/media_list_data absent"),
                report,
            );
        }
        for item in items {
            match classify(item) {
                Entry::Link(uri, code) => {
                    apply_reel(conn, &uri, &code, &entry_title(item), "", now, report);
                    attach_membership(conn, &name, &code, report);
                }
                Entry::Invalid(msg) => record_invalid(file_name, &msg, report),
                Entry::Empty => record_skipped(file_name, "empty collection item", report),
                Entry::Unknown => record_unknown(
                    item,
                    file_name,
                    &format!("collection '{name}' item lacks link"),
                    report,
                ),
            }
        }
    }
}

/// Older shape: collections carry items directly in a `saved_items` array.
fn parse_collections_items(
    conn: &Connection,
    json: &serde_json::Value,
    file_name: &str,
    now: i64,
    report: &mut ImportReport,
) {
    if !json.is_array() {
        record_unknown(json, file_name, "expected array", report);
        return;
    }
    for coll in as_array(json) {
        let name = coll["name"].as_str().unwrap_or("").trim().to_string();
        if name.is_empty() {
            record_skipped(file_name, "collection with empty name", report);
            continue;
        }
        match coll["saved_items"].as_array() {
            None => record_unknown(
                coll,
                file_name,
                &format!("collection '{name}' without saved_items array"),
                report,
            ),
            Some(items) => {
                db::ensure_collection(conn, &name, now);
                report.counts.collections += 1;
                for item in items {
                    match classify(item) {
                        Entry::Link(uri, code) => {
                            apply_reel(conn, &uri, &code, &entry_title(item), "", now, report);
                            attach_membership(conn, &name, &code, report);
                        }
                        Entry::Invalid(msg) => record_invalid(file_name, &msg, report),
                        Entry::Empty => record_skipped(file_name, "empty collection item", report),
                        Entry::Unknown => record_unknown(
                            item,
                            file_name,
                            &format!("collection '{name}' item lacks link"),
                            report,
                        ),
                    }
                }
            }
        }
    }
}

fn looks_structural(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Array(a) => !a.is_empty(),
        serde_json::Value::Object(m) => !m.is_empty(),
        serde_json::Value::String(s) => !s.trim().is_empty(),
        serde_json::Value::Number(_) | serde_json::Value::Bool(_) => true,
        serde_json::Value::Null => false,
    }
}

fn classify(entry: &serde_json::Value) -> Entry {
    if let Some((u, c)) = extract_link(entry) {
        return Entry::Link(u, c);
    }
    let obj = match entry.as_object() {
        Some(o) => o,
        // Bare scalars/arrays inside the entries list carry nothing
        // actionable; skipping them invents nothing.
        None => return Entry::Empty,
    };

    let uri_nonempty = entry["uri"]
        .as_str()
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    let map_ok = entry["media_map_data"]
        .as_array()
        .map(|a| !a.is_empty())
        .unwrap_or(false);
    let smap_ok = entry["string_map_data"]
        .as_object()
        .map(|m| !m.is_empty())
        .unwrap_or(false);

    // A recognized link carrier is present but its link did not parse.
    if uri_nonempty || map_ok || smap_ok {
        return Entry::Invalid(
            "link fields present but no parseable instagram reel link".to_string(),
        );
    }

    // No link attempt at all. Known keys and empty values are benign
    // export padding; anything unrecognized is reported so nothing is
    // silently accepted.
    let unrecognized = obj.iter().any(|(k, v)| {
        !matches!(
            k.as_str(),
            "title" | "name" | "uri" | "media_list_data" | "media_map_data" | "string_map_data"
        ) && looks_structural(v)
    });
    if unrecognized {
        Entry::Unknown
    } else {
        Entry::Empty
    }
}

fn record_invalid(file: &str, msg: &str, report: &mut ImportReport) {
    report.counts.invalid_links += 1;
    report.issues.push(ImportIssue {
        kind: "invalid_link".into(),
        file: file.into(),
        detail: msg.into(),
    });
}

fn record_skipped(file: &str, msg: &str, report: &mut ImportReport) {
    report.counts.skipped += 1;
    report.issues.push(ImportIssue {
        kind: "skipped".into(),
        file: file.into(),
        detail: msg.into(),
    });
}

fn record_unknown(value: &serde_json::Value, file: &str, msg: &str, report: &mut ImportReport) {
    report.counts.unknown_structures += 1;
    report.issues.push(ImportIssue {
        kind: "unknown_structure".into(),
        file: file.into(),
        detail: format!("{msg} | got {} | {}", type_name(value), truncated(value)),
    });
}

fn type_name(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::Bool(_) => "bool",
        _ => "null",
    }
}

fn truncated(v: &serde_json::Value) -> String {
    let s = serde_json::to_string(v).unwrap_or_default();
    s.chars().take(160).collect()
}

fn entry_title(v: &serde_json::Value) -> String {
    v["title"].as_str().unwrap_or("").trim().to_string()
}

/// Pull the first recognized reel link out of an export entry, in the
/// shapes official exports use: `uri`, `media_map_data[].href`,
/// `string_map_data.*.href`.
fn extract_link(entry: &serde_json::Value) -> Option<(String, String)> {
    if let Some(uri) = entry["uri"].as_str() {
        if let Some(found) = canonical_reel(uri) {
            return Some(found);
        }
    }
    if let Some(list) = entry["media_map_data"].as_array() {
        for m in list {
            if let Some(href) = m["href"].as_str() {
                if let Some(found) = canonical_reel(href) {
                    return Some(found);
                }
            }
        }
    }
    if let Some(map) = entry["string_map_data"].as_object() {
        for (_k, v) in map {
            if let Some(href) = v["href"].as_str() {
                if let Some(found) = canonical_reel(href) {
                    return Some(found);
                }
            }
        }
    }
    None
}

/// Canonicalize an instagram link to https://www.instagram.com/<kind>/<code>/
/// and return (canonical_uri, shortcode). Shortcode is the identity key.
/// Accepted: full URLs on the instagram.com host (http/https, optional www,
/// no port), or link text starting with a slash for known kinds. Anything
/// else is None so the caller reports it instead of inventing a slot.
pub fn canonical_reel(input: &str) -> Option<(String, String)> {
    let raw = input.trim();
    if raw.is_empty() {
        return None;
    }

    let tail = match raw.split_once("://") {
        Some((_scheme, rest)) => rest,
        None => raw,
    };
    let host_seg = tail.split('/').next().unwrap_or("");
    let path = if host_seg.is_empty() {
        // Relative link text such as "/p/AAA111/".
        tail.to_string()
    } else {
        let clean = host_seg
            .to_lowercase()
            .trim_start_matches("www.")
            .to_string();
        if clean == "instagram.com" {
            tail[host_seg.len()..].to_string()
        } else {
            return None; // wrong host (or host without scheme): not ours to fix
        }
    };

    let path = path.split(['?', '#']).next().unwrap_or("");
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let kinds = ["p", "reel", "reels", "tv"];
    let idx = segs
        .iter()
        .position(|s| kinds.contains(&s.to_lowercase().as_str()))?;
    let kind = segs[idx].to_lowercase();
    let kind = if kind == "reels" {
        "reel".to_string()
    } else {
        kind
    };
    let code = segs.get(idx + 1)?;
    finish_link(&kind, code)
}

fn finish_link(kind: &str, code: &str) -> Option<(String, String)> {
    if code.is_empty()
        || !code
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
    {
        return None;
    }
    Some((
        format!("https://www.instagram.com/{kind}/{code}/"),
        code.to_string(),
    ))
}

fn apply_reel(
    conn: &Connection,
    uri: &str,
    code: &str,
    title: &str,
    caption: &str,
    now: i64,
    report: &mut ImportReport,
) {
    let existing: Option<i64> = conn
        .query_row("SELECT id FROM reels WHERE code = ?1", [code], |r| r.get(0))
        .ok();
    match existing {
        Some(id) => {
            // Redundant reference to a reel already in the library. Both
            // counters apply: a refresh of stored metadata is applied and
            // the source entry itself is redundant. The entry still adds
            // at most one duplicate fact, never a second reel row.
            report.counts.reels_merged += 1;
            report.counts.duplicates += 1;
            let _ = conn.execute(
                "UPDATE reels SET
                    title = CASE WHEN ?2 <> '' THEN ?2 ELSE title END,
                    caption = CASE WHEN ?3 <> '' THEN ?3 ELSE caption END,
                    uri = ?4,
                    updated_at = ?5
                 WHERE id = ?1",
                rusqlite::params![id, title, caption, uri, now],
            );
            db::reindex_reel(conn, id).ok();
        }
        None => {
            let insert = conn.execute(
                "INSERT INTO reels(code, uri, title, caption, imported_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
                rusqlite::params![code, uri, title, caption, now],
            );
            match insert {
                Ok(_) => {
                    report.counts.reels_new += 1;
                    if let Ok(id) =
                        conn.query_row("SELECT id FROM reels WHERE code = ?1", [code], |r| r.get(0))
                    {
                        db::reindex_reel(conn, id).ok();
                    }
                }
                Err(_) => {
                    report.counts.duplicates += 1;
                    report.issues.push(ImportIssue {
                        kind: "duplicate".into(),
                        file: uri.into(),
                        detail: format!("conflict on code {code}"),
                    });
                }
            }
        }
    }
}

fn attach_membership(
    conn: &Connection,
    collection_name: &str,
    code: &str,
    report: &mut ImportReport,
) {
    // An ignored membership is the effect of an already-counted redundant
    // reference in apply_reel; do not count it a second time here.
    let inserted = conn
        .execute(
            "INSERT OR IGNORE INTO collection_items(collection_id, reel_id)
             SELECT c.id, r.id FROM collections c, reels r
             WHERE c.name = ?1 AND r.code = ?2",
            rusqlite::params![collection_name, code],
        )
        .unwrap_or(0);
    if inserted == 1 {
        report.counts.memberships += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

    fn temp_db() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let conn = crate::db::open(&dir.path().join("library.db")).expect("open");
        crate::db::migrate(&conn).expect("migrate");
        (dir, conn)
    }

    fn export_dir(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::TempDir::new().expect("tempdir");
        for (rel, body) in files {
            let path = dir.path().join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("mkdir");
            }
            std::fs::write(path, body).expect("write fixture");
        }
        let root = dir.path().to_path_buf();
        (dir, root)
    }

    fn report_of(r: &ImportReport) -> (u32, u32, u32, u32, String) {
        (
            r.counts.reels_new,
            r.counts.duplicates,
            r.counts.invalid_links,
            r.counts.unknown_structures,
            r.status.clone(),
        )
    }

    #[test]
    fn canonicalizes_instagram_links() {
        let cases = [
            ("https://www.instagram.com/p/ABC123/?igsh=x", "ABC123"),
            ("https://instagram.com/reel/DEF456/", "DEF456"),
            ("http://www.instagram.com/reels/GHI789/?hl=en", "GHI789"),
        ];
        for (input, code) in cases {
            let c = canonical_reel(input).expect(input);
            assert_eq!(c.1, code);
            assert!(c.0.starts_with("https://www.instagram.com/"));
        }
        assert!(canonical_reel("https://www.youtube.com/watch?v=x").is_none());
        assert!(canonical_reel("").is_none());
        assert!(canonical_reel("https://www.instagram.com/profile/foo/").is_none());
        assert!(canonical_reel("https://www.instagram.com/p/has space/").is_none());
    }

    #[test]
    fn imports_posts_and_collections_with_counts() {
        let (_d, conn) = temp_db();
        let (_root, dir) = export_dir(&[
            (
                "saved/saved_posts.json",
                r#"[
                    {"title":"ffmpeg trim basics","uri":"https://www.instagram.com/p/AAA111/"},
                    {"title":"dup","uri":"https://instagram.com/p/AAA111/"},
                    {"title":"bad link","uri":"https://www.youtube.com/watch?v=z"},
                    {"title":"empty","uri":"","media_list_data":[]},
                    {"title":"no link shape","foo":1}
                ]"#,
            ),
            (
                "saved/saved_collections.json",
                r#"[{"name":"Coding","media_map":{"k1":[{"uri":"https://www.instagram.com/reel/BBB222/"}]}}]"#,
            ),
            (
                "saved/saved_saved_collections.json",
                r#"[{"name":"Coding","saved_items":[{"title":"same reel","uri":"https://www.instagram.com/reel/BBB222/"},{"title":"extra","uri":"https://www.instagram.com/reel/CCC333/"}]}]"#,
            ),
        ]);
        let report = run_import(&conn, &dir, 1_000).expect("import");
        let (new_n, dup_n, invalid_n, unknown_n, status) = report_of(&report);
        assert_eq!(new_n, 3); // AAA111, BBB222, CCC333
        assert_eq!(dup_n, 2); // "dup" merge + collection duplicate instance
        assert_eq!(invalid_n, 1);
        assert_eq!(unknown_n, 1); // {"foo":1}
        assert_eq!(report.counts.skipped, 1);
        assert_eq!(status, "partial");

        let reels: i64 = conn
            .query_row("SELECT COUNT(*) FROM reels", [], |r| r.get(0))
            .unwrap();
        assert_eq!(reels, 3);

        // Collection membership from both shapes consolidates.
        let members: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM collection_items ci JOIN collections c ON c.id=ci.collection_id WHERE c.name='Coding'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(members, 2); // BBB222 twice dedupes to one row, plus CCC333
    }

    #[test]
    fn reimport_preserves_notes_and_does_not_duplicate() {
        let (_d, conn) = temp_db();
        let (_root, dir) = export_dir(&[(
            "saved/saved_posts.json",
            r#"[{"title":"t1","uri":"https://www.instagram.com/p/AAA111/"}]"#,
        )]);
        run_import(&conn, &dir, 1_000).expect("first import");
        conn.execute(
            "INSERT INTO notes(reel_id, body, created_at) SELECT id, 'my note about trimming', 5 FROM reels WHERE code='AAA111'",
            [],
        )
        .expect("note insert");
        crate::db::reindex_reel(&conn, 1).unwrap();

        let report = run_import(&conn, &dir, 2_000).expect("second import");
        assert_eq!(report.counts.reels_new, 0);
        assert_eq!(report.counts.reels_merged, 1);

        let notes: i64 = conn
            .query_row("SELECT COUNT(*) FROM notes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(notes, 1);
        let reels: i64 = conn
            .query_row("SELECT COUNT(*) FROM reels", [], |r| r.get(0))
            .unwrap();
        assert_eq!(reels, 1);
    }

    #[test]
    fn unexpected_shapes_are_reported_not_invented() {
        let (_d, conn) = temp_db();
        let (_root, dir) = export_dir(&[(
            "saved/saved_posts.json",
            r#"[{"weird_field":"https://www.instagram.com/p/XXX111/"}]"#,
        )]);
        let report = run_import(&conn, &dir, 1_000).expect("import");
        assert_eq!(report.counts.unknown_structures, 1);
        assert_eq!(report.counts.reels_new, 0);
        assert_eq!(report.status, "partial");
        let reels: i64 = conn
            .query_row("SELECT COUNT(*) FROM reels", [], |r| r.get(0))
            .unwrap();
        assert_eq!(reels, 0);
    }

    #[test]
    fn missing_source_errors() {
        let (_d, conn) = temp_db();
        let report = run_import(&conn, Path::new("/nonexistent/path"), 0);
        assert!(matches!(report, Err(ImportError::SourceMissing(_))));
    }

    #[test]
    fn zip_is_read_without_extraction() {
        let (_d, conn) = temp_db();
        // Build a zip in memory via Rust zip writer to exercise the path.
        let paths = tempfile::TempDir::new().unwrap();
        let src = paths.path().join("export");
        std::fs::create_dir_all(src.join("saved")).unwrap();
        std::fs::write(
            src.join("saved/saved_posts.json"),
            br#"[{"title":"z1","uri":"https://www.instagram.com/p/ZZZ111/"}]"#,
        )
        .unwrap();

        let zip_path = paths.path().join("export.zip");
        let f = std::fs::File::create(&zip_path).unwrap();
        let mut writer = zip::ZipWriter::new(f);
        writer
            .start_file(
                "export/saved/saved_posts.json",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
        writer
            .write_all(br#"[{"title":"z1","uri":"https://www.instagram.com/p/ZZZ111/"}]"#)
            .unwrap();
        writer.finish().unwrap();

        let report = run_import(&conn, &zip_path, 1_000).expect("zip import");
        assert_eq!(report.counts.reels_new, 1);
        assert_eq!(report.files_read.len(), 1);
    }

    #[test]
    fn empty_export_is_reported_as_empty() {
        let (_d, conn) = temp_db();
        let (_root, dir) = export_dir(&[("misc/notes.txt", "nothing relevant")]);
        let report = run_import(&conn, &dir, 1_000).expect("import");
        assert_eq!(report.status, "empty");
    }
}
