//! The HTTP pass's server and the check that it serves the bytes the local
//! run read.
//!
//! Every manifest sample is served at `/<kind>/<path>` for the manifest path
//! `<kind>:<path>` (FATE samples and generated files alike, resolved as the
//! local run resolves them), with HTTP/1.1 Range support, one thread per
//! connection.

use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Maps a manifest path to the file on disk.
pub type Resolver = fn(&str) -> Option<PathBuf>;

/// Starts the server on a background thread and returns its base URL. The
/// listener is never closed (process exit reaps it), so every entry reuses
/// the same server.
pub fn start(resolve: Resolver) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind http server");
    let port = listener.local_addr().expect("http server address").port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            // A response the client stopped reading (a seek dropped it)
            // must not hold up the next request.
            std::thread::spawn(move || serve(stream, resolve));
        }
    });
    format!("http://127.0.0.1:{port}")
}

/// The URL path a manifest path is served at.
pub fn url_path(manifest_path: &str) -> String {
    let (kind, rel) = manifest_path.split_once(':').unwrap_or(("", manifest_path));
    let rel: Vec<String> = rel.split('/').map(urlencode).collect();
    format!("{kind}/{}", rel.join("/"))
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The byte range (inclusive) a `Range: bytes=…` value selects from a
/// `total`-byte file: `a-b`, `a-` or the suffix `-n`. `None` when it selects
/// nothing (416).
fn byte_range(spec: &str, total: u64) -> Option<(u64, u64)> {
    let (first, last) = spec.trim().split_once('-')?;
    let (first, last) = (first.trim(), last.trim());
    let range = if first.is_empty() {
        let n: u64 = last.parse().ok()?;
        (total.checked_sub(n.min(total))?, total.checked_sub(1)?)
    } else {
        let a: u64 = first.parse().ok()?;
        let b = if last.is_empty() { total.checked_sub(1)? } else { last.parse::<u64>().ok()?.min(total.checked_sub(1)?) };
        (a, b)
    };
    (range.0 <= range.1 && range.1 < total).then_some(range)
}

/// Answers one request: the sample at the URL path, or a byte range of it.
fn serve(mut stream: TcpStream, resolve: Resolver) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    // Read until the end of the request head.
    loop {
        match stream.read(&mut byte) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                buf.push(byte[0]);
                if buf.ends_with(b"\r\n\r\n") || buf.ends_with(b"\n\n") {
                    break;
                }
            }
        }
    }
    let req = String::from_utf8_lossy(&buf);
    let mut lines = req.lines();
    let mut first = lines.next().unwrap_or("").split_whitespace();
    let method = first.next().unwrap_or("");
    let target = first.next().unwrap_or("");
    let path = percent_decode(target.split('?').next().unwrap_or(""));
    let file = path
        .trim_start_matches('/')
        .split_once('/')
        .and_then(|(kind, rel)| resolve(&format!("{kind}:{rel}")));
    let range = lines.find_map(|l| {
        let (name, value) = l.split_once(':')?;
        name.trim().eq_ignore_ascii_case("range").then(|| value.trim().to_string())
    });
    let Some((file, total)) = file.and_then(|f| std::fs::metadata(&f).ok().map(|m| (f, m.len()))) else {
        let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return;
    };
    let (status, start, end) = match range.as_deref().map(|r| r.strip_prefix("bytes=").and_then(|s| byte_range(s, total))) {
        None => ("200 OK", 0, total.saturating_sub(1)),
        Some(Some((a, b))) => ("206 Partial Content", a, b),
        Some(None) => {
            let head = format!(
                "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{total}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            let _ = stream.write_all(head.as_bytes());
            return;
        }
    };
    let len = if total == 0 { 0 } else { end - start + 1 };
    let content_range =
        if range.is_some() { format!("Content-Range: bytes {start}-{end}/{total}\r\n") } else { String::new() };
    let head = format!(
        "HTTP/1.1 {status}\r\nAccept-Ranges: bytes\r\n{content_range}Content-Type: application/octet-stream\r\n\
         Content-Length: {len}\r\nConnection: close\r\n\r\n"
    );
    if stream.write_all(head.as_bytes()).is_err() || method == "HEAD" {
        return;
    }
    let Ok(mut f) = std::fs::File::open(&file) else { return };
    if f.seek(SeekFrom::Start(start)).is_err() {
        return;
    }
    let mut remaining = len;
    let mut chunk = vec![0u8; 256 * 1024];
    while remaining > 0 {
        let n = (remaining as usize).min(chunk.len());
        match f.read(&mut chunk[..n]) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if stream.write_all(&chunk[..n]).is_err() {
                    break;
                }
                remaining -= n as u64;
            }
        }
    }
}

/// One response.
pub struct Response {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

/// A minimal HTTP/1.1 client for the local server: `method` on `url` with an
/// optional `Range` value.
pub fn fetch(method: &str, url: &str, range: Option<&str>) -> Result<Response, String> {
    let rest = url.strip_prefix("http://").ok_or_else(|| format!("{url}: not http://"))?;
    let (host, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    let mut stream = TcpStream::connect(host).map_err(|e| format!("connect {host}: {e}"))?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let range = range.map(|r| format!("Range: {r}\r\n")).unwrap_or_default();
    let request = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\n{range}Connection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).map_err(|e| e.to_string())?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).map_err(|e| e.to_string())?;
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or("response without a header end")?;
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or("response without a status")?;
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    Ok(Response { status, headers, body: raw[split + 4..].to_vec() })
}

/// Checks that `url` serves exactly `local`'s bytes: HEAD reports its length,
/// a plain GET returns all of it, and ranged GETs (`a-b`, the `a-` the player's
/// HTTP source sends, and a suffix) return the matching slices with 206.
pub fn verify_bytes(url: &str, local: &Path) -> Result<(), String> {
    let bytes = std::fs::read(local).map_err(|e| format!("read {}: {e}", local.display()))?;
    let total = bytes.len() as u64;
    let head = fetch("HEAD", url, None)?;
    let length = head.headers.get("content-length").and_then(|v| v.parse::<u64>().ok());
    if head.status != 200 || length != Some(total) || !head.body.is_empty() {
        return Err(format!(
            "HEAD {url}: status {}, length {length:?} (file {total}), {} body bytes",
            head.status,
            head.body.len()
        ));
    }
    let get = fetch("GET", url, None)?;
    if get.status != 200 || get.body != bytes {
        return Err(format!(
            "GET {url}: status {}, md5 {} vs file {}",
            get.status,
            refcheck::md5_hex(&get.body),
            refcheck::md5_hex(&bytes)
        ));
    }
    let mid = total / 2;
    let checks = [
        (format!("bytes={}-{}", total / 3, (total / 3 + 4095).min(total.saturating_sub(1))), total / 3, (total / 3 + 4096).min(total)),
        (format!("bytes={mid}-"), mid, total),
        (format!("bytes=-{}", total.min(1000)), total - total.min(1000), total),
    ];
    for (spec, from, to) in checks {
        if from >= to {
            continue;
        }
        let r = fetch("GET", url, Some(&spec))?;
        let want = &bytes[from as usize..to as usize];
        let content_range = format!("bytes {from}-{}/{total}", to - 1);
        if r.status != 206 || r.body != want || r.headers.get("content-range") != Some(&content_range) {
            return Err(format!(
                "GET {url} Range {spec}: status {}, {} bytes, content-range {:?}; want 206, {} bytes, {content_range}",
                r.status,
                r.body.len(),
                r.headers.get("content-range"),
                want.len()
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fate(manifest_path: &str) -> Option<PathBuf> {
        manifest_path.strip_prefix("fate:").map(refcheck::fate)
    }

    #[test]
    fn byte_ranges_follow_rfc_9110() {
        assert_eq!(byte_range("0-99", 1000), Some((0, 99)));
        assert_eq!(byte_range("900-", 1000), Some((900, 999)));
        assert_eq!(byte_range("-100", 1000), Some((900, 999)), "suffix: the last 100 bytes");
        assert_eq!(byte_range("-5000", 1000), Some((0, 999)));
        assert_eq!(byte_range("990-2000", 1000), Some((990, 999)));
        assert_eq!(byte_range("1000-", 1000), None, "past the end");
        assert_eq!(byte_range("5-4", 1000), None);
    }

    #[test]
    fn the_server_serves_a_fate_sample_byte_for_byte() {
        // Before, the server was rooted at the generated-corpus directory and
        // looked FATE samples up by file name: every fate: entry got a 404.
        let base = start(fate);
        let manifest_path = "fate:sub/SubRip_capability_tester.srt";
        let url = format!("{base}/{}", url_path(manifest_path));
        verify_bytes(&url, &refcheck::fate("sub/SubRip_capability_tester.srt")).unwrap();
        let old_url = format!("{base}/SubRip_capability_tester.srt");
        assert_eq!(fetch("HEAD", &old_url, None).unwrap().status, 404);
    }

    #[test]
    fn verify_bytes_catches_a_server_serving_other_bytes() {
        let base = start(fate);
        let url = format!("{base}/{}", url_path("fate:sub/SubRip_capability_tester.srt"));
        let other = refcheck::fate("sub/MicroDVD_capability_tester.sub");
        assert!(verify_bytes(&url, &other).is_err());
    }
}
