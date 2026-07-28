//! v2 filesystem handlers — discovery (`POST /v2/fs/list`, `POST /v2/fs/exists`)
//! and a raw byte-serving endpoint (`GET`/`HEAD /v2/fs/raw`).
//!
//! Agents load server-side files by path via `POST /v2/sessions/:sid/open`;
//! the read-only `list`/`exists` endpoints let them discover what's on disk
//! first (browse a data directory, confirm a path resolves) before committing
//! to an open, and `raw` streams the exact file bytes back for a client that
//! wants a verbatim copy. They are deliberately **session-independent** —
//! discovery/transfer precede or stand apart from session creation — so they
//! mount at `/v2/fs/*`, not under `:sid`. `raw` in particular takes no session
//! and no shared state, so it is outside the `SESSION_MAX`/`JOBS_MAX` caps:
//! arbitrarily many concurrent pulls are allowed by design.
//!
//! There is no path confinement: `open` already reads arbitrary paths, and the
//! server is loopback-only + SSH-tunnel + trusted-agent by design, so these add
//! no capability that isn't already reachable. `list`/`exists` run their
//! blocking `std::fs` calls on the blocking pool; `raw` streams from disk via
//! `tokio::fs` so it never buffers a whole file in server RAM.

use std::path::Path;

use astroburst_lib::infra::fits::mef_writer::{
    write_compressed_mef, CompressMode, CompressOptions, MefReport,
};
use axum::{
    body::Body,
    extract::Query,
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::Response,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

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

#[derive(Deserialize)]
pub struct RawParams {
    /// Absolute path of the file to serve.
    pub path: String,
    /// Optional transform. Absent / `none` → verbatim bytes (Range-native).
    /// `lossless` → a per-HDU GZIP_2 lossless-compressed FITS (bit-exact,
    /// ~1.8× smaller), generated on the fly.
    #[serde(default)]
    pub compress: Option<String>,
    /// Optional comma-separated EXTNAMEs to omit from the output (e.g.
    /// `PSF,WCS-WAVE`). Requires `compress=lossless` (it rewrites structure).
    #[serde(default)]
    pub drop: Option<String>,
    /// Optional comma-separated EXTNAMEs to pass through **uncompressed** (a
    /// blocklist): everything else is compressed, these stay verbatim but
    /// present. Requires `compress=lossless`.
    #[serde(default)]
    pub raw: Option<String>,
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

/// GET /v2/fs/raw?path=… — stream a file's exact bytes as `application/fits`.
///
/// Stateless: no session, no shared state, so it is not bounded by
/// `SESSION_MAX`/`JOBS_MAX`. Advertises `Accept-Ranges: bytes` and honors a
/// single `Range:` request (`bytes=S-`, `bytes=S-E`, or suffix `bytes=-N`) with
/// a `206 Partial Content` + `Content-Range` response, so transfers over a
/// flaky link are resumable/retryable. Missing path → 404, directory → 400,
/// unsatisfiable range → 416.
pub async fn raw_get(headers: HeaderMap, Query(params): Query<RawParams>) -> Result<Response> {
    let compress = parse_compress_request(&params)?; // 400 before touching the fs
    let meta = stat_regular_file(&params.path).await?; // 404 missing / 400 dir
    let range = headers.get(header::RANGE).and_then(|v| v.to_str().ok()).map(str::to_owned);

    match compress {
        // Verbatim: stream the file itself.
        None => {
            let file = tokio::fs::File::open(&params.path).await.map_err(|e| {
                // Raced with a delete/rename between stat and open.
                AppError::NotFound(format!("cannot open {}: {e}", params.path))
            })?;
            build_stream_response(file, meta.len(), range.as_deref()).await
        }
        // Compressed: materialize to a temp file, then stream from its (now
        // unlinked) fd — same Range logic, bounded RAM.
        Some(req) => {
            let (file, total, report) = compress_to_temp(&params.path, req).await?;
            let mut resp = build_stream_response(file, total, range.as_deref()).await?;
            set_hdu_headers(&mut resp, &report);
            Ok(resp)
        }
    }
}

/// HEAD /v2/fs/raw?path=… — size/headers only, no body. For a verbatim request
/// this only stats the file. With `compress=lossless` it performs the
/// compression to report an accurate `Content-Length` (same cost as a GET —
/// bulk clients should just GET). Registered as its own handler so axum's
/// automatic-HEAD doesn't run the streaming GET handler just to discard the
/// body (which would also leave `Content-Length` unset for a stream).
pub async fn raw_head(Query(params): Query<RawParams>) -> Result<Response> {
    let compress = parse_compress_request(&params)?;
    let meta = stat_regular_file(&params.path).await?;

    let (total, report) = match compress {
        None => (meta.len(), None),
        Some(req) => {
            let (file, total, report) = compress_to_temp(&params.path, req).await?;
            drop(file); // HEAD has no body; release the unlinked temp fd now
            (total, Some(report))
        }
    };

    let mut resp = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/fits")
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, total)
        .body(Body::empty())
        .map_err(|e| AppError::Internal(anyhow::anyhow!("build response: {e}")))?;
    if let Some(report) = report {
        set_hdu_headers(&mut resp, &report);
    }
    Ok(resp)
}

/// Build a `200`/`206` streaming response from an already-open file, honoring an
/// optional `Range:` header (shared by the verbatim and compressed paths).
async fn build_stream_response(
    mut file: tokio::fs::File,
    total: u64,
    range: Option<&str>,
) -> Result<Response> {
    let (status, start, len) = match range {
        Some(spec) => match parse_range(spec, total) {
            RangeOutcome::Satisfiable(s, l) => (StatusCode::PARTIAL_CONTENT, s, l),
            RangeOutcome::Unsatisfiable => return unsatisfiable(total),
            // Malformed/unsupported Range is ignored per RFC 7233 → full body.
            RangeOutcome::Ignore => (StatusCode::OK, 0u64, total),
        },
        None => (StatusCode::OK, 0u64, total),
    };

    if start > 0 {
        file.seek(std::io::SeekFrom::Start(start))
            .await
            .map_err(|e| AppError::Internal(anyhow::anyhow!("seek: {e}")))?;
    }
    let body = Body::from_stream(ReaderStream::new(file.take(len)));

    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/fits")
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, len);
    if status == StatusCode::PARTIAL_CONTENT {
        // len >= 1 here: a satisfiable range always covers at least one byte.
        let end = start + len - 1;
        builder = builder.header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{total}"));
    }
    builder
        .body(body)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("build response: {e}")))
}

/// Compress `source_path` to a temp file (lossless GZIP_2, with any dropped
/// HDUs), reopen it, then unlink it so the returned fd is the only reference —
/// on Linux the inode's blocks persist until the stream drops, giving bounded
/// RAM and automatic cleanup. Returns `(open temp fd, byte length, dropped
/// EXTNAMEs)`.
async fn compress_to_temp(
    source_path: &str,
    req: CompressRequest,
) -> Result<(tokio::fs::File, u64, MefReport)> {
    let source = source_path.to_owned();
    let (tmp, report) = tokio::task::spawn_blocking(
        move || -> anyhow::Result<(tempfile::NamedTempFile, MefReport)> {
            let tmp = tempfile::Builder::new().suffix(".fits").tempfile()?;
            let tmp_path = tmp.path().to_string_lossy().to_string();
            let opts = CompressOptions {
                mode: CompressMode::Lossless,
                drop_extnames: req.drop_extnames,
                raw_extnames: req.raw_extnames,
            };
            let report = write_compressed_mef(&source, &tmp_path, &opts)?;
            Ok((tmp, report))
        },
    )
    .await
    .map_err(|e| AppError::Internal(anyhow::anyhow!("task panic: {e}")))?
    .map_err(|e| AppError::Internal(anyhow::anyhow!("compress {source_path}: {e:#}")))?;

    let file = tokio::fs::File::open(tmp.path())
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("reopen temp: {e}")))?;
    let total = file
        .metadata()
        .await
        .map(|m| m.len())
        .map_err(|e| AppError::Internal(anyhow::anyhow!("stat temp: {e}")))?;
    drop(tmp); // unlink; the open `file` fd keeps the inode alive
    Ok((file, total, report))
}

/// Echo the actually-dropped / kept-raw EXTNAMEs in `x-dropped-hdus` /
/// `x-raw-hdus` (comma-separated), when non-empty.
fn set_hdu_headers(resp: &mut Response, report: &MefReport) {
    for (name, list) in [("x-dropped-hdus", &report.dropped), ("x-raw-hdus", &report.kept_raw)] {
        if list.is_empty() {
            continue;
        }
        if let Ok(v) = HeaderValue::from_str(&list.join(",")) {
            resp.headers_mut().insert(name, v);
        }
    }
}

/// A validated `compress=lossless` request.
struct CompressRequest {
    /// EXTNAMEs to omit entirely.
    drop_extnames: Vec<String>,
    /// EXTNAMEs to pass through uncompressed.
    raw_extnames: Vec<String>,
}

/// Split a comma-separated EXTNAME list, trimming and dropping empties.
fn split_extnames(v: Option<&str>) -> Vec<String> {
    v.unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Parse/validate the `compress`, `drop`, and `raw` query params. `Ok(None)` =
/// serve verbatim; `Ok(Some(..))` = lossless compression. Unknown `compress`, or
/// `drop`/`raw` without `compress`, → `400`.
fn parse_compress_request(params: &RawParams) -> Result<Option<CompressRequest>> {
    let drop_extnames = split_extnames(params.drop.as_deref());
    let raw_extnames = split_extnames(params.raw.as_deref());

    match params.compress.as_deref() {
        None | Some("") | Some("none") => {
            if !drop_extnames.is_empty() || !raw_extnames.is_empty() {
                return Err(AppError::BadRequestWithHint {
                    code: "bad_request",
                    message: "`drop`/`raw` require `compress=lossless`".into(),
                    hint: Some(
                        "selecting HDUs to drop or keep-raw rewrites the file, which only the \
                         compressed path does"
                            .into(),
                    ),
                });
            }
            Ok(None)
        }
        Some("lossless") => Ok(Some(CompressRequest { drop_extnames, raw_extnames })),
        Some(other) => Err(AppError::BadRequestWithHint {
            code: "bad_request",
            message: format!("unknown compress mode {other:?}"),
            hint: Some("supported: compress=lossless (or omit for verbatim bytes)".into()),
        }),
    }
}

/// `416 Range Not Satisfiable` with the required `Content-Range: bytes */TOTAL`.
fn unsatisfiable(total: u64) -> Result<Response> {
    Response::builder()
        .status(StatusCode::RANGE_NOT_SATISFIABLE)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_RANGE, format!("bytes */{total}"))
        .body(Body::empty())
        .map_err(|e| AppError::Internal(anyhow::anyhow!("build response: {e}")))
}

/// Stat a path that must be a readable regular file (follows symlinks, matching
/// `open`/`stat_path`): missing → 404, directory → 400 `not_a_directory`, other
/// stat error (e.g. permission) → 400.
async fn stat_regular_file(path: &str) -> Result<std::fs::Metadata> {
    let meta = tokio::fs::metadata(path).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            AppError::NotFound(format!("no such file: {path}"))
        } else {
            AppError::BadRequest(format!("cannot stat {path}: {e}"))
        }
    })?;
    if meta.is_dir() {
        return Err(AppError::BadRequestWithHint {
            code: "not_a_directory",
            message: format!("{path} is a directory"),
            hint: Some("fs/raw serves file bytes; use fs/list to browse a directory".into()),
        });
    }
    Ok(meta)
}

/// The three ways a `Range:` header resolves against a file (RFC 7233).
enum RangeOutcome {
    /// A satisfiable single range: serve `(start, len)` as `206`.
    Satisfiable(u64, u64),
    /// Well-formed but no bytes satisfy it (start ≥ EOF, empty file, `-0`) → `416`.
    Unsatisfiable,
    /// Not a range we parse (wrong unit, multi-range, garbage). Per RFC 7233 an
    /// unparseable Range is ignored and the full representation is served → `200`.
    Ignore,
}

/// Parse a single HTTP byte-range against a known `total` length.
///
/// Supports `bytes=S-`, `bytes=S-E` (E clamped to EOF), and suffix `bytes=-N`
/// (last N bytes). Multi-range (`,`) and any malformed spec are `Ignore`d
/// (→ serve full body); a well-formed but unsatisfiable single range is
/// `Unsatisfiable` (→ 416). An inverted `bytes=E-S` (last < first) is an invalid
/// spec, so it is ignored rather than treated as a 416.
fn parse_range(spec: &str, total: u64) -> RangeOutcome {
    let Some(body) = spec.trim().strip_prefix("bytes=") else {
        return RangeOutcome::Ignore; // e.g. `Range: rows=…`
    };
    let body = body.trim();
    if body.contains(',') {
        return RangeOutcome::Ignore; // multi-range unsupported
    }
    let Some((s, e)) = body.split_once('-') else {
        return RangeOutcome::Ignore; // no `-` separator
    };
    let (s, e) = (s.trim(), e.trim());

    if s.is_empty() {
        // Suffix range: last N bytes.
        let Ok(n) = e.parse::<u64>() else {
            return RangeOutcome::Ignore; // `bytes=-` or `bytes=-abc`
        };
        let n = n.min(total);
        if n == 0 {
            return RangeOutcome::Unsatisfiable; // `-0`, or a suffix of an empty file
        }
        return RangeOutcome::Satisfiable(total - n, n);
    }

    let Ok(start) = s.parse::<u64>() else {
        return RangeOutcome::Ignore;
    };
    let end = if e.is_empty() {
        // Open-ended `bytes=S-`.
        if start >= total {
            return RangeOutcome::Unsatisfiable; // start at/after EOF (incl. empty file)
        }
        total - 1
    } else {
        let Ok(end) = e.parse::<u64>() else {
            return RangeOutcome::Ignore;
        };
        if end < start {
            return RangeOutcome::Ignore; // inverted range is invalid → ignore
        }
        if start >= total {
            return RangeOutcome::Unsatisfiable;
        }
        end.min(total - 1)
    };
    RangeOutcome::Satisfiable(start, end - start + 1)
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
    fn parse_range_handles_all_forms() {
        use RangeOutcome::*;
        let total = 1000u64;
        // Helper: assert Satisfiable(start, len).
        let sat = |spec: &str, s: u64, l: u64| match parse_range(spec, total) {
            Satisfiable(gs, gl) => assert_eq!((gs, gl), (s, l), "spec {spec}"),
            other => panic!("spec {spec}: expected Satisfiable, got {}", tag(&other)),
        };

        // Open-ended and closed ranges.
        sat("bytes=0-99", 0, 100);
        sat("bytes=100-", 100, 900);
        sat("bytes=0-", 0, 1000);
        // Suffix range: last N bytes.
        sat("bytes=-50", 950, 50);
        // Suffix larger than the file clamps to the whole file.
        sat("bytes=-5000", 0, 1000);
        // End past EOF clamps to the last byte.
        sat("bytes=990-100000", 990, 10);
        // Whitespace tolerated.
        sat("  bytes= 10 - 19 ", 10, 10);

        // Well-formed but unsatisfiable → 416.
        for spec in ["bytes=1000-", "bytes=99999-", "bytes=-0"] {
            assert!(matches!(parse_range(spec, total), Unsatisfiable), "spec {spec}");
        }
        // Empty file: every satisfiable-looking byte range is unsatisfiable.
        assert!(matches!(parse_range("bytes=0-", 0), Unsatisfiable));
        assert!(matches!(parse_range("bytes=-10", 0), Unsatisfiable));

        // Malformed / unsupported / invalid → Ignore (caller serves full body).
        for spec in [
            "bytes=abc",
            "bytes=0-99,200-299", // multi-range
            "items=0-99",         // wrong unit
            "bytes=-",
            "bytes=50-40", // inverted (last < first) is invalid → ignore
        ] {
            assert!(matches!(parse_range(spec, total), Ignore), "spec {spec}");
        }
    }

    fn tag(o: &RangeOutcome) -> &'static str {
        match o {
            RangeOutcome::Satisfiable(..) => "Satisfiable",
            RangeOutcome::Unsatisfiable => "Unsatisfiable",
            RangeOutcome::Ignore => "Ignore",
        }
    }

    fn params(compress: Option<&str>, drop: Option<&str>, raw: Option<&str>) -> RawParams {
        RawParams {
            path: "/x.fits".into(),
            compress: compress.map(str::to_owned),
            drop: drop.map(str::to_owned),
            raw: raw.map(str::to_owned),
        }
    }

    #[test]
    fn parse_compress_request_cases() {
        // Verbatim: absent / empty / explicit none, no drop/raw.
        assert!(parse_compress_request(&params(None, None, None)).unwrap().is_none());
        assert!(parse_compress_request(&params(Some(""), None, None)).unwrap().is_none());
        assert!(parse_compress_request(&params(Some("none"), None, None)).unwrap().is_none());

        // Lossless, no filters.
        let r = parse_compress_request(&params(Some("lossless"), None, None)).unwrap().unwrap();
        assert!(r.drop_extnames.is_empty() && r.raw_extnames.is_empty());

        // Lossless + drop list: split, trimmed, empties removed.
        let r = parse_compress_request(&params(Some("lossless"), Some("PSF, WCS-WAVE ,,"), None))
            .unwrap()
            .unwrap();
        assert_eq!(r.drop_extnames, vec!["PSF".to_string(), "WCS-WAVE".to_string()]);

        // Lossless + raw (blocklist) list.
        let r = parse_compress_request(&params(Some("lossless"), None, Some(" PSF ,, ZODI")))
            .unwrap()
            .unwrap();
        assert_eq!(r.raw_extnames, vec!["PSF".to_string(), "ZODI".to_string()]);

        // drop/raw without compress → error.
        assert!(parse_compress_request(&params(None, Some("PSF"), None)).is_err());
        assert!(parse_compress_request(&params(Some("none"), None, Some("PSF"))).is_err());

        // Unknown compress mode → error.
        assert!(parse_compress_request(&params(Some("gzip"), None, None)).is_err());
        assert!(parse_compress_request(&params(Some("lossy"), None, None)).is_err());
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
