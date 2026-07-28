//! v2 filesystem discovery handlers — `POST /v2/fs/list` and `POST /v2/fs/exists`.
//!
//! Agents load server-side files by path via `POST /v2/sessions/:sid/open`;
//! these two read-only endpoints let them discover what's on disk first
//! (browse a data directory, confirm a path resolves) before committing to an
//! open. They are deliberately **session-independent** — discovery precedes
//! session creation — so they mount at `/v2/fs/*`, not under `:sid`.
//!
//! There is no path confinement: `open` already reads arbitrary paths, and the
//! server is loopback-only + SSH-tunnel + trusted-agent by design, so these add
//! no capability that isn't already reachable. Both run their blocking `std::fs`
//! calls on the blocking pool.

use std::path::Path;

use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::{AppError, Result};

#[derive(Deserialize)]
pub struct ListParams {
    /// Directory to list (an absolute path is recommended — relative paths
    /// resolve against the server process's working directory).
    pub path: String,
    /// Optional shell-style filter matched against each entry's *name* (not the
    /// full path): `*` matches any run, `?` one character. E.g. `*.fits`,
    /// `m51_?.fit`. Case-sensitive. Omitted returns everything.
    #[serde(default)]
    pub glob: Option<String>,
    /// Include dotfiles / hidden entries (names beginning with `.`). Default false.
    #[serde(default)]
    pub include_hidden: bool,
}

#[derive(Deserialize)]
pub struct ExistsParams {
    /// Path to stat.
    pub path: String,
}

/// POST /v2/fs/list — non-recursive directory listing.
pub async fn list(Json(params): Json<ListParams>) -> Result<Json<Value>> {
    let ListParams { path, glob, include_hidden } = params;
    let body =
        tokio::task::spawn_blocking(move || list_dir(&path, glob.as_deref(), include_hidden))
            .await
            .map_err(|e| AppError::Internal(anyhow::anyhow!("task panic: {e}")))??;
    Ok(Json(body))
}

/// POST /v2/fs/exists — stat a single path. A missing path is `exists: false`,
/// not an error.
pub async fn exists(Json(params): Json<ExistsParams>) -> Result<Json<Value>> {
    let path = params.path;
    let body = tokio::task::spawn_blocking(move || stat_path(&path))
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("task panic: {e}")))?;
    Ok(Json(body))
}

fn list_dir(path: &str, glob: Option<&str>, include_hidden: bool) -> Result<Value> {
    let dir = Path::new(path);
    let meta = std::fs::metadata(dir).map_err(|e| AppError::BadRequestWithHint {
        code: "fs_not_found",
        message: format!("cannot stat {path}: {e}"),
        hint: Some("provide a path to an existing directory".into()),
    })?;
    if !meta.is_dir() {
        return Err(AppError::BadRequestWithHint {
            code: "not_a_directory",
            message: format!("{path} is not a directory"),
            hint: Some("fs/list lists directories; use fs/exists to stat a file".into()),
        });
    }

    let rd = std::fs::read_dir(dir)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("read_dir {path}: {e}")))?;

    let mut entries = Vec::new();
    for ent in rd {
        let Ok(ent) = ent else { continue };
        let name = ent.file_name().to_string_lossy().into_owned();
        if !include_hidden && name.starts_with('.') {
            continue;
        }
        if let Some(g) = glob {
            if !glob_match(g, &name) {
                continue;
            }
        }
        // file_type() does not follow symlinks, so a link is reported as such
        // rather than as its target; metadata() (follows) supplies size/mtime.
        let kind = ent
            .file_type()
            .map(|ft| type_str(ft.is_symlink(), ft.is_dir(), ft.is_file()))
            .unwrap_or("other");
        let followed = ent.metadata().ok();
        let size = followed
            .as_ref()
            .filter(|_| kind == "file")
            .map(|m| m.len());
        let modified = followed.as_ref().and_then(mtime_unix);
        entries.push(json!({
            "name": name,
            "type": kind,
            "size": size,
            "modified_unix": modified,
        }));
    }

    // Deterministic ordering: directories first, then everything else, each
    // group sorted by name.
    entries.sort_by(|a, b| {
        let rank = |v: &Value| if v["type"] == "dir" { 0 } else { 1 };
        rank(a)
            .cmp(&rank(b))
            .then_with(|| a["name"].as_str().cmp(&b["name"].as_str()))
    });

    Ok(json!({
        "path": path,
        "count": entries.len(),
        "entries": entries,
    }))
}

fn stat_path(path: &str) -> Value {
    // Follow symlinks, matching `open` (which opens through links): a dangling
    // link then reads as exists:false, honestly reflecting "can't open it".
    match std::fs::metadata(Path::new(path)) {
        Ok(m) => {
            let kind = type_str(false, m.is_dir(), m.is_file());
            let size = if m.is_file() { Some(m.len()) } else { None };
            json!({
                "path": path,
                "exists": true,
                "type": kind,
                "size": size,
                "modified_unix": mtime_unix(&m),
            })
        }
        Err(_) => json!({
            "path": path,
            "exists": false,
            "type": Value::Null,
            "size": Value::Null,
            "modified_unix": Value::Null,
        }),
    }
}

fn type_str(is_symlink: bool, is_dir: bool, is_file: bool) -> &'static str {
    if is_symlink {
        "symlink"
    } else if is_dir {
        "dir"
    } else if is_file {
        "file"
    } else {
        "other"
    }
}

fn mtime_unix(m: &std::fs::Metadata) -> Option<i64> {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
}

/// Case-sensitive shell-style glob supporting `*` (any run, incl. empty) and
/// `?` (exactly one char). Classic two-pointer match with `*` backtracking.
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut mark = 0usize;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_matches_extensions_and_wildcards() {
        assert!(glob_match("*.fits", "m51_ha.fits"));
        assert!(glob_match("m51_?.fit", "m51_a.fit"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("abc", "abc"));
        assert!(!glob_match("*.fits", "m51_ha.fit"));
        assert!(!glob_match("m51_?.fit", "m51_ab.fit"));
        assert!(!glob_match("abc", "abd"));
        // case-sensitive
        assert!(!glob_match("*.fits", "M51.FITS"));
    }

    #[test]
    fn list_dir_reports_entries_filtered_and_sorted() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("b.fits"), b"x").unwrap();
        std::fs::write(root.join("a.fits"), b"yy").unwrap();
        std::fs::write(root.join("note.txt"), b"z").unwrap();
        std::fs::write(root.join(".hidden"), b"z").unwrap();
        std::fs::create_dir(root.join("sub")).unwrap();

        let path = root.to_string_lossy().to_string();

        // Full listing (no glob): hidden excluded, dir first, names sorted.
        let v = list_dir(&path, None, false).unwrap();
        assert_eq!(v["count"], 4);
        let names: Vec<&str> = v["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["sub", "a.fits", "b.fits", "note.txt"]);
        assert_eq!(v["entries"][0]["type"], "dir");
        assert_eq!(v["entries"][0]["size"], Value::Null);
        assert_eq!(v["entries"][1]["type"], "file");
        assert_eq!(v["entries"][1]["size"], 2); // a.fits has 2 bytes

        // Hidden included.
        let v = list_dir(&path, None, true).unwrap();
        assert_eq!(v["count"], 5);

        // Glob filter.
        let v = list_dir(&path, Some("*.fits"), false).unwrap();
        assert_eq!(v["count"], 2);

        // Not a directory → error.
        let file = root.join("a.fits").to_string_lossy().to_string();
        assert!(list_dir(&file, None, false).is_err());
    }

    #[test]
    fn stat_path_reports_existence_and_kind() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("img.fits");
        std::fs::write(&f, b"hello").unwrap();

        let v = stat_path(&f.to_string_lossy());
        assert_eq!(v["exists"], true);
        assert_eq!(v["type"], "file");
        assert_eq!(v["size"], 5);

        let d = stat_path(&tmp.path().to_string_lossy());
        assert_eq!(d["exists"], true);
        assert_eq!(d["type"], "dir");
        assert_eq!(d["size"], Value::Null);

        let missing = stat_path(&tmp.path().join("nope.fits").to_string_lossy());
        assert_eq!(missing["exists"], false);
        assert_eq!(missing["type"], Value::Null);
    }
}
