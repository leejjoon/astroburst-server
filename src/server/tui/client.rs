//! Minimal blocking HTTP client for the TUI's polling loop.
//!
//! Deliberately dependency-free (std `TcpStream` only), like
//! `connect::probe_health`: requests go out as HTTP/1.0 with
//! `Connection: close`, so the server never chunk-encodes and the body is
//! simply everything up to EOF. All traffic is small JSON on loopback (or a
//! loopback tunnel) — a fresh connection per request at ~1 Hz is fine.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::Value;

const CONNECT_TIMEOUT: Duration = Duration::from_millis(1500);
const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// `http://host:port` → `host:port` (the only scheme the dashboard talks).
fn host_port(base_url: &str) -> Result<String> {
    let rest = base_url
        .strip_prefix("http://")
        .with_context(|| format!("unsupported URL '{base_url}' (http:// only)"))?;
    let hp = rest.trim_end_matches('/');
    if hp.is_empty() {
        bail!("empty host in '{base_url}'");
    }
    Ok(hp.to_string())
}

/// Issue one request and return `(status_code, parsed_body, elapsed)`.
/// Empty bodies (204, HEAD-ish responses) come back as `Value::Null`.
pub fn request(
    base_url: &str,
    method: &str,
    path: &str,
) -> Result<(u16, Value, Duration)> {
    let hp = host_port(base_url)?;
    let started = Instant::now();

    let addr = hp
        .parse()
        .or_else(|_| -> Result<_, std::io::Error> {
            // Non-literal host: resolve via ToSocketAddrs.
            use std::net::ToSocketAddrs;
            hp.to_socket_addrs()?
                .next()
                .ok_or_else(|| std::io::Error::other("no address"))
        })
        .with_context(|| format!("cannot resolve '{hp}'"))?;
    let mut stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)
        .with_context(|| format!("connect to {hp} failed"))?;
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));

    let req = format!("{method} {path} HTTP/1.0\r\nHost: {hp}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).context("request write failed")?;

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).context("response read failed")?;
    let elapsed = started.elapsed();

    let text = String::from_utf8_lossy(&raw);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .with_context(|| format!("malformed HTTP response from {hp}"))?;
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .with_context(|| format!("unparseable status line from {hp}"))?;

    let body = body.trim();
    let json = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(body).unwrap_or(Value::Null)
    };
    Ok((status, json, elapsed))
}

/// GET returning the body only when the status is 2xx.
pub fn get_json(base_url: &str, path: &str) -> Result<(Value, Duration)> {
    let (status, json, elapsed) = request(base_url, "GET", path)?;
    if !(200..300).contains(&status) {
        bail!("GET {path} → {status}");
    }
    Ok((json, elapsed))
}

pub fn delete(base_url: &str, path: &str) -> Result<()> {
    let (status, _, _) = request(base_url, "DELETE", path)?;
    if !(200..300).contains(&status) {
        bail!("DELETE {path} → {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// One-shot mock server answering a canned response.
    fn mock(response: &'static str) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 2048];
            let _ = s.read(&mut buf);
            let _ = s.write_all(response.as_bytes());
        });
        port
    }

    #[test]
    fn get_json_parses_body_and_status() {
        let port = mock(
            "HTTP/1.0 200 OK\r\ncontent-type: application/json\r\n\r\n{\"count\":2}",
        );
        let (json, _) = get_json(&format!("http://127.0.0.1:{port}"), "/v2/sessions").unwrap();
        assert_eq!(json["count"], 2);
    }

    #[test]
    fn non_2xx_is_an_error_with_status() {
        let port = mock("HTTP/1.0 404 Not Found\r\n\r\n{\"error\":\"nope\"}");
        let err = get_json(&format!("http://127.0.0.1:{port}"), "/v2/sessions/x/history")
            .unwrap_err();
        assert!(err.to_string().contains("404"), "{err}");
    }

    #[test]
    fn delete_accepts_204_empty_body() {
        let port = mock("HTTP/1.1 204 No Content\r\n\r\n");
        delete(&format!("http://127.0.0.1:{port}"), "/v2/sessions/x").unwrap();
    }

    #[test]
    fn refused_connection_is_a_clean_error() {
        let free = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = free.local_addr().unwrap().port();
        drop(free);
        assert!(get_json(&format!("http://127.0.0.1:{port}"), "/health").is_err());
    }

    #[test]
    fn rejects_non_http_urls() {
        assert!(get_json("https://example.com", "/health").is_err());
        assert!(get_json("http://", "/health").is_err());
    }
}
