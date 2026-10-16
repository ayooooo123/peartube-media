//! The native route's HTTP/1.1 server on 127.0.0.1.

use std::collections::HashMap;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use parking_lot::Mutex;

pub(crate) enum Body {
    Ready(std::sync::Arc<[u8]>),
    Pending(std::sync::mpsc::Receiver<Result<std::sync::Arc<[u8]>, String>>),
}

pub(crate) trait Resources: Send + Sync + 'static {
    /// `path`: the request path after `/<token>/`, without query, e.g. "master.m3u8", "seg12.m4s". None -> 404. Called on the connection's thread; may block briefly.
    fn get(&self, path: &str) -> Option<(Body, &'static str /* Content-Type */)>;
}

struct ServerInner {
    stopped: AtomicBool,
    connections: Mutex<HashMap<u64, TcpStream>>,
    next_conn_id: AtomicU64,
}

pub(crate) struct Server {
    port: u16,
    token: String,
    inner: Arc<ServerInner>,
    accept_handle: Mutex<Option<JoinHandle<()>>>,
}

fn generate_token() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);

    let rs = RandomState::new();
    let mut h1 = rs.build_hasher();
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    h1.write_u128(now.as_nanos());
    h1.write_u64(count);
    let val1 = h1.finish();

    let mut h2 = rs.build_hasher();
    h2.write_u64(val1);
    h2.write_u128(now.as_micros());
    h2.write_u64(!count);
    let val2 = h2.finish();

    format!("{:016x}{:016x}", val1, val2)
}

#[derive(Debug, PartialEq, Eq)]
enum RangeSpec {
    Satisfiable { start: usize, end: usize },
    Unsatisfiable,
    None,
}

fn parse_range(header: &str, total_len: usize) -> RangeSpec {
    let header = header.trim();
    if !header.starts_with("bytes=") {
        return RangeSpec::None;
    }
    let spec = header["bytes=".len()..].trim();
    if spec.contains(',') {
        return RangeSpec::None;
    }
    if total_len == 0 {
        return RangeSpec::Unsatisfiable;
    }

    if let Some(suffix_str) = spec.strip_prefix('-') {
        match suffix_str.parse::<usize>() {
            Ok(n) => {
                if n == 0 {
                    return RangeSpec::Unsatisfiable;
                }
                let start = total_len.saturating_sub(n);
                let end = total_len - 1;
                RangeSpec::Satisfiable { start, end }
            }
            Err(_) => RangeSpec::None,
        }
    } else if let Some((start_str, end_str)) = spec.split_once('-') {
        let start = match start_str.parse::<usize>() {
            Ok(s) => s,
            Err(_) => return RangeSpec::None,
        };
        if start >= total_len {
            return RangeSpec::Unsatisfiable;
        }
        if end_str.is_empty() {
            let end = total_len - 1;
            RangeSpec::Satisfiable { start, end }
        } else {
            let end = match end_str.parse::<usize>() {
                Ok(e) => e,
                Err(_) => return RangeSpec::None,
            };
            if start > end {
                return RangeSpec::Unsatisfiable;
            }
            let end = end.min(total_len - 1);
            RangeSpec::Satisfiable { start, end }
        }
    } else {
        RangeSpec::None
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

fn send_200(
    stream: &mut TcpStream,
    content_type: &str,
    body: &[u8],
    is_head: bool,
    keep_alive: bool,
) -> std::io::Result<()> {
    let conn_hdr = if keep_alive { "Connection: keep-alive\r\n" } else { "Connection: close\r\n" };
    let headers = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: {}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-cache\r\n\
         Accept-Ranges: bytes\r\n\
         {}\r\n",
        content_type,
        body.len(),
        conn_hdr
    );
    stream.write_all(headers.as_bytes())?;
    if !is_head {
        stream.write_all(body)?;
    }
    stream.flush()
}

fn send_206(
    stream: &mut TcpStream,
    content_type: &str,
    content_range: &str,
    slice: &[u8],
    is_head: bool,
    keep_alive: bool,
) -> std::io::Result<()> {
    let conn_hdr = if keep_alive { "Connection: keep-alive\r\n" } else { "Connection: close\r\n" };
    let headers = format!(
        "HTTP/1.1 206 Partial Content\r\n\
         Content-Type: {}\r\n\
         Content-Range: {}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-cache\r\n\
         Accept-Ranges: bytes\r\n\
         {}\r\n",
        content_type,
        content_range,
        slice.len(),
        conn_hdr
    );
    stream.write_all(headers.as_bytes())?;
    if !is_head {
        stream.write_all(slice)?;
    }
    stream.flush()
}

fn send_416(
    stream: &mut TcpStream,
    content_range: &str,
    keep_alive: bool,
) -> std::io::Result<()> {
    let conn_hdr = if keep_alive { "Connection: keep-alive\r\n" } else { "Connection: close\r\n" };
    let headers = format!(
        "HTTP/1.1 416 Range Not Satisfiable\r\n\
         Content-Range: {}\r\n\
         Content-Length: 0\r\n\
         Cache-Control: no-cache\r\n\
         {}\r\n",
        content_range,
        conn_hdr
    );
    stream.write_all(headers.as_bytes())?;
    stream.flush()
}

fn send_404(
    stream: &mut TcpStream,
    keep_alive: bool,
) -> std::io::Result<()> {
    let conn_hdr = if keep_alive { "Connection: keep-alive\r\n" } else { "Connection: close\r\n" };
    let headers = format!(
        "HTTP/1.1 404 Not Found\r\n\
         Content-Length: 0\r\n\
         Cache-Control: no-cache\r\n\
         {}\r\n",
        conn_hdr
    );
    stream.write_all(headers.as_bytes())?;
    stream.flush()
}

fn send_400(stream: &mut TcpStream) -> std::io::Result<()> {
    let headers = "HTTP/1.1 400 Bad Request\r\n\
                   Content-Length: 0\r\n\
                   Connection: close\r\n\
                   \r\n";
    stream.write_all(headers.as_bytes())?;
    stream.flush()?;
    let _ = stream.shutdown(Shutdown::Write);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(50)));
    let mut discard = [0u8; 4096];
    while let Ok(n) = stream.read(&mut discard) {
        if n == 0 {
            break;
        }
    }
    Ok(())
}

fn send_405(stream: &mut TcpStream) -> std::io::Result<()> {
    let headers = "HTTP/1.1 405 Method Not Allowed\r\n\
                   Allow: GET, HEAD\r\n\
                   Content-Length: 0\r\n\
                   Connection: close\r\n\
                   \r\n";
    stream.write_all(headers.as_bytes())?;
    stream.flush()
}

fn send_503(stream: &mut TcpStream) -> std::io::Result<()> {
    let headers = "HTTP/1.1 503 Service Unavailable\r\n\
                   Content-Length: 0\r\n\
                   Connection: close\r\n\
                   Cache-Control: no-cache\r\n\
                   \r\n";
    stream.write_all(headers.as_bytes())?;
    stream.flush()
}

fn handle_connection(
    mut stream: TcpStream,
    token: &str,
    resources: &dyn Resources,
    inner: &ServerInner,
) -> std::io::Result<()> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));

    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 2048];

    'conn_loop: loop {
        if inner.stopped.load(Ordering::Relaxed) {
            return Ok(());
        }

        // Find end of headers in buf: \r\n\r\n or \n\n
        let (header_end, delim_len) = loop {
            if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
                break (pos, 4);
            }
            if let Some(pos) = find_subslice(&buf, b"\n\n") {
                break (pos, 2);
            }

            if buf.len() >= 8192 {
                let _ = send_400(&mut stream);
                return Ok(());
            }

            match stream.read(&mut chunk) {
                Ok(0) => {
                    return Ok(());
                }
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    return Ok(());
                }
                Err(e) => return Err(e),
            }
        };

        let remaining = buf.split_off(header_end + delim_len);
        let header_data = std::mem::replace(&mut buf, remaining);
        let header_bytes = &header_data[..header_end];
        let header_str = match std::str::from_utf8(header_bytes) {
            Ok(s) => s,
            Err(_) => {
                let _ = send_400(&mut stream);
                return Ok(());
            }
        };

        let mut lines = header_str.lines();
        let req_line = match lines.next() {
            Some(l) => l.trim_end_matches('\r'),
            None => {
                let _ = send_400(&mut stream);
                return Ok(());
            }
        };

        let req_parts: Vec<&str> = req_line.split_whitespace().collect();
        if req_parts.len() != 3 {
            let _ = send_400(&mut stream);
            return Ok(());
        }
        let method = req_parts[0];
        let target = req_parts[1];
        let version = req_parts[2];

        if version != "HTTP/1.1" && version != "HTTP/1.0" {
            let _ = send_400(&mut stream);
            return Ok(());
        }

        let mut keep_alive = version == "HTTP/1.1";
        let mut range_header: Option<String> = None;

        for line in lines {
            let line = line.trim_end_matches('\r');
            if line.is_empty() {
                break;
            }
            if let Some((k, v)) = line.split_once(':') {
                let k = k.trim().to_ascii_lowercase();
                let v = v.trim();
                if k == "connection" {
                    if v.eq_ignore_ascii_case("close") {
                        keep_alive = false;
                    } else if v.eq_ignore_ascii_case("keep-alive") {
                        keep_alive = version == "HTTP/1.1";
                    }
                } else if k == "range" {
                    range_header = Some(v.to_string());
                }
            }
        }

        let is_head = method == "HEAD";
        if method != "GET" && !is_head {
            let _ = send_405(&mut stream);
            return Ok(());
        }

        let prefix = format!("/{}/", token);
        if !target.starts_with(&prefix) {
            let _ = send_404(&mut stream, keep_alive);
            if !keep_alive {
                return Ok(());
            }
            continue 'conn_loop;
        }

        let after_prefix = &target[prefix.len()..];
        let path = after_prefix.split('?').next().unwrap_or("");

        let resource = match resources.get(path) {
            Some(r) => r,
            None => {
                let _ = send_404(&mut stream, keep_alive);
                if !keep_alive {
                    return Ok(());
                }
                continue 'conn_loop;
            }
        };

        let (body, content_type) = resource;
        match body {
            Body::Ready(bytes) => {
                let range_spec = range_header.as_deref().map(|r| parse_range(r, bytes.len()));
                match range_spec {
                    Some(RangeSpec::Satisfiable { start, end }) => {
                        let slice = &bytes[start..=end];
                        let content_range = format!("bytes {}-{}/{}", start, end, bytes.len());
                        if send_206(&mut stream, content_type, &content_range, slice, is_head, keep_alive).is_err() {
                            return Ok(());
                        }
                    }
                    Some(RangeSpec::Unsatisfiable) => {
                        let content_range = format!("bytes */{}", bytes.len());
                        if send_416(&mut stream, &content_range, keep_alive).is_err() {
                            return Ok(());
                        }
                    }
                    Some(RangeSpec::None) | None => {
                        if send_200(&mut stream, content_type, &bytes, is_head, keep_alive).is_err() {
                            return Ok(());
                        }
                    }
                }
                if !keep_alive {
                    return Ok(());
                }
            }
            Body::Pending(rx) => {
                if is_head {
                    // HEAD on Pending waits for the body (no chunked early header) and sends only headers.
                    let bytes = loop {
                        if inner.stopped.load(Ordering::Relaxed) {
                            return Ok(());
                        }
                        match rx.recv_timeout(Duration::from_millis(50)) {
                            Ok(Ok(b)) => break b,
                            Ok(Err(_)) | Err(RecvTimeoutError::Disconnected) => {
                                let _ = send_503(&mut stream);
                                return Ok(());
                            }
                            Err(RecvTimeoutError::Timeout) => {}
                        }
                    };

                    let range_spec = range_header.as_deref().map(|r| parse_range(r, bytes.len()));
                    match range_spec {
                        Some(RangeSpec::Satisfiable { start, end }) => {
                            let slice = &bytes[start..=end];
                            let content_range = format!("bytes {}-{}/{}", start, end, bytes.len());
                            if send_206(&mut stream, content_type, &content_range, slice, true, keep_alive).is_err() {
                                return Ok(());
                            }
                        }
                        Some(RangeSpec::Unsatisfiable) => {
                            let content_range = format!("bytes */{}", bytes.len());
                            if send_416(&mut stream, &content_range, keep_alive).is_err() {
                                return Ok(());
                            }
                        }
                        Some(RangeSpec::None) | None => {
                            if send_200(&mut stream, content_type, &bytes, true, keep_alive).is_err() {
                                return Ok(());
                            }
                        }
                    }
                    if !keep_alive {
                        return Ok(());
                    }
                } else {
                    // GET on Pending
                    let start_time = Instant::now();
                    let timeout_1_5s = Duration::from_millis(1500);
                    let mut early_body: Option<Result<Arc<[u8]>, String>> = None;

                    while Instant::now().duration_since(start_time) < timeout_1_5s {
                        if inner.stopped.load(Ordering::Relaxed) {
                            return Ok(());
                        }
                        let elapsed = Instant::now().duration_since(start_time);
                        let remaining = timeout_1_5s.saturating_sub(elapsed);
                        let step = remaining.min(Duration::from_millis(50));
                        match rx.recv_timeout(step) {
                            Ok(res) => {
                                early_body = Some(res);
                                break;
                            }
                            Err(RecvTimeoutError::Timeout) => {}
                            Err(RecvTimeoutError::Disconnected) => {
                                let _ = send_503(&mut stream);
                                return Ok(());
                            }
                        }
                    }

                    if early_body.is_none() {
                        match rx.try_recv() {
                            Ok(res) => early_body = Some(res),
                            Err(mpsc::TryRecvError::Disconnected) => {
                                let _ = send_503(&mut stream);
                                return Ok(());
                            }
                            Err(mpsc::TryRecvError::Empty) => {}
                        }
                    }

                    match early_body {
                        Some(Ok(bytes)) => {
                            let range_spec = range_header.as_deref().map(|r| parse_range(r, bytes.len()));
                            match range_spec {
                                Some(RangeSpec::Satisfiable { start, end }) => {
                                    let slice = &bytes[start..=end];
                                    let content_range = format!("bytes {}-{}/{}", start, end, bytes.len());
                                    if send_206(&mut stream, content_type, &content_range, slice, false, keep_alive).is_err() {
                                        return Ok(());
                                    }
                                }
                                Some(RangeSpec::Unsatisfiable) => {
                                    let content_range = format!("bytes */{}", bytes.len());
                                    if send_416(&mut stream, &content_range, keep_alive).is_err() {
                                        return Ok(());
                                    }
                                }
                                Some(RangeSpec::None) | None => {
                                    if send_200(&mut stream, content_type, &bytes, false, keep_alive).is_err() {
                                        return Ok(());
                                    }
                                }
                            }
                            if !keep_alive {
                                return Ok(());
                            }
                        }
                        Some(Err(_)) => {
                            let _ = send_503(&mut stream);
                            return Ok(());
                        }
                        None => {
                            // 1.5s elapsed without body: send chunked headers immediately!
                            let conn_hdr = if keep_alive { "Connection: keep-alive\r\n" } else { "Connection: close\r\n" };
                            let headers = format!(
                                "HTTP/1.1 200 OK\r\n\
                                 Content-Type: {}\r\n\
                                 Transfer-Encoding: chunked\r\n\
                                 Cache-Control: no-cache\r\n\
                                 {}\r\n",
                                content_type,
                                conn_hdr
                            );
                            if stream.write_all(headers.as_bytes()).is_err() || stream.flush().is_err() {
                                return Ok(());
                            }

                            loop {
                                if inner.stopped.load(Ordering::Relaxed) {
                                    return Ok(());
                                }
                                match rx.recv_timeout(Duration::from_millis(50)) {
                                    Ok(Ok(bytes)) => {
                                        let chunk_header = format!("{:x}\r\n", bytes.len());
                                        if stream.write_all(chunk_header.as_bytes()).is_err() {
                                            return Ok(());
                                        }
                                        if stream.write_all(&bytes).is_err() {
                                            return Ok(());
                                        }
                                        if stream.write_all(b"\r\n0\r\n\r\n").is_err() {
                                            return Ok(());
                                        }
                                        if stream.flush().is_err() {
                                            return Ok(());
                                        }
                                        break;
                                    }
                                    Ok(Err(_)) | Err(RecvTimeoutError::Disconnected) => {
                                        // Close connection WITHOUT final chunk!
                                        return Ok(());
                                    }
                                    Err(RecvTimeoutError::Timeout) => {}
                                }
                            }
                            if !keep_alive {
                                return Ok(());
                            }
                        }
                    }
                }
            }
        }
    }
}

impl Server {
    /// Binds 127.0.0.1:0 and serves on background threads.
    pub(crate) fn start(resources: Arc<dyn Resources>) -> std::io::Result<Server> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        let token = generate_token();
        let inner = Arc::new(ServerInner {
            stopped: AtomicBool::new(false),
            connections: Mutex::new(HashMap::new()),
            next_conn_id: AtomicU64::new(1),
        });

        let inner_clone = Arc::clone(&inner);
        let token_clone = token.clone();
        let resources_clone = resources;

        let accept_handle = thread::spawn(move || {
            for stream_res in listener.incoming() {
                if inner_clone.stopped.load(Ordering::SeqCst) {
                    break;
                }
                match stream_res {
                    Ok(stream) => {
                        if inner_clone.stopped.load(Ordering::SeqCst) {
                            break;
                        }
                        let conn_id = inner_clone.next_conn_id.fetch_add(1, Ordering::Relaxed);
                        if let Ok(clone) = stream.try_clone() {
                            let mut map = inner_clone.connections.lock();
                            if inner_clone.stopped.load(Ordering::SeqCst) {
                                let _ = clone.shutdown(Shutdown::Both);
                                break;
                            }
                            map.insert(conn_id, clone);
                        }

                        let inner_conn = Arc::clone(&inner_clone);
                        let token_thread = token_clone.clone();
                        let resources_thread = Arc::clone(&resources_clone);

                        thread::spawn(move || {
                            struct ConnGuard {
                                inner: Arc<ServerInner>,
                                id: u64,
                            }
                            impl Drop for ConnGuard {
                                fn drop(&mut self) {
                                    self.inner.connections.lock().remove(&self.id);
                                }
                            }
                            let _guard = ConnGuard {
                                inner: Arc::clone(&inner_conn),
                                id: conn_id,
                            };

                            let _ = handle_connection(
                                stream,
                                &token_thread,
                                &*resources_thread,
                                &inner_conn,
                            );
                        });
                    }
                    Err(_) => {
                        if inner_clone.stopped.load(Ordering::SeqCst) {
                            break;
                        }
                    }
                }
            }
        });

        Ok(Server {
            port,
            token,
            inner,
            accept_handle: Mutex::new(Some(accept_handle)),
        })
    }

    /// `http://127.0.0.1:<port>/<token>/<path>`.
    pub(crate) fn url(&self, path: &str) -> String {
        let clean = path.strip_prefix('/').unwrap_or(path);
        format!("http://127.0.0.1:{}/{}/{}", self.port, self.token, clean)
    }

    /// Stops accepting, shuts down every open connection (unblocking any request waiting on a Pending body), joins the accept thread. Idempotent; Drop calls it.
    pub(crate) fn stop(&self) {
        if self.inner.stopped.swap(true, Ordering::SeqCst) {
            return;
        }

        // Shut down every open connection tracked so far
        let conns: Vec<TcpStream> = {
            let mut map = self.inner.connections.lock();
            std::mem::take(&mut *map).into_values().collect()
        };
        for stream in conns {
            let _ = stream.shutdown(Shutdown::Both);
        }

        // Wake the accept thread
        let _ = TcpStream::connect(format!("127.0.0.1:{}", self.port));

        // Join the accept thread
        if let Some(handle) = self.accept_handle.lock().take() {
            let _ = handle.join();
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_response(stream: &mut TcpStream) -> (u16, HashMap<String, String>, Vec<u8>) {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        let (header_end, delim_len) = loop {
            if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
                break (pos, 4);
            }
            let n = stream.read(&mut chunk).expect("read failed");
            assert!(n > 0, "unexpected EOF while reading headers");
            buf.extend_from_slice(&chunk[..n]);
        };

        let header_str = std::str::from_utf8(&buf[..header_end]).unwrap();
        let mut lines = header_str.lines();
        let status_line = lines.next().unwrap();
        let status_code: u16 = status_line.split_whitespace().nth(1).unwrap().parse().unwrap();

        let mut headers = HashMap::new();
        for line in lines {
            if let Some((k, v)) = line.split_once(':') {
                headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
            }
        }

        let mut body = buf.split_off(header_end + delim_len);
        if let Some(cl_str) = headers.get("content-length") {
            let content_length: usize = cl_str.parse().unwrap();
            while body.len() < content_length {
                let n = stream.read(&mut chunk).expect("read body failed");
                assert!(n > 0, "unexpected EOF while reading body");
                body.extend_from_slice(&chunk[..n]);
            }
        }

        (status_code, headers, body)
    }

    #[test]
    fn test_keep_alive_two_gets_on_one_socket() {
        struct KaRes;
        impl Resources for KaRes {
            fn get(&self, path: &str) -> Option<(Body, &'static str)> {
                match path {
                    "one" => Some((Body::Ready(Arc::from(&b"first"[..])), "text/plain")),
                    "two" => Some((Body::Ready(Arc::from(&b"second"[..])), "text/plain")),
                    _ => None,
                }
            }
        }

        let server = Server::start(Arc::new(KaRes)).unwrap();
        let mut client = TcpStream::connect(format!("127.0.0.1:{}", server.port)).unwrap();

        // 1st request
        let req1 = format!("GET /{}/one HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n", server.token);
        client.write_all(req1.as_bytes()).unwrap();
        let (status1, _, body1) = read_response(&mut client);
        assert_eq!(status1, 200);
        assert_eq!(body1, b"first");

        // 2nd request on the SAME socket
        let req2 = format!("GET /{}/two HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n", server.token);
        client.write_all(req2.as_bytes()).unwrap();
        let (status2, _, body2) = read_response(&mut client);
        assert_eq!(status2, 200);
        assert_eq!(body2, b"second");
    }

    #[test]
    fn test_wrong_token_404() {
        struct DummyRes;
        impl Resources for DummyRes {
            fn get(&self, _path: &str) -> Option<(Body, &'static str)> {
                Some((Body::Ready(Arc::from(&b"data"[..])), "text/plain"))
            }
        }
        let server = Server::start(Arc::new(DummyRes)).unwrap();
        let mut client = TcpStream::connect(format!("127.0.0.1:{}", server.port)).unwrap();
        let bad_req = "GET /00000000000000000000000000000000/res HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";
        client.write_all(bad_req.as_bytes()).unwrap();

        let mut resp = Vec::new();
        client.read_to_end(&mut resp).unwrap();
        let resp_str = String::from_utf8_lossy(&resp);
        assert!(resp_str.starts_with("HTTP/1.1 404 Not Found\r\n"));
    }

    #[test]
    fn test_head() {
        struct HeadRes;
        impl Resources for HeadRes {
            fn get(&self, path: &str) -> Option<(Body, &'static str)> {
                if path == "doc" {
                    Some((Body::Ready(Arc::from(&b"document body"[..])), "text/plain"))
                } else {
                    None
                }
            }
        }
        let server = Server::start(Arc::new(HeadRes)).unwrap();
        let mut client = TcpStream::connect(format!("127.0.0.1:{}", server.port)).unwrap();
        let req = format!("HEAD /{}/doc HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n", server.token);
        client.write_all(req.as_bytes()).unwrap();

        let mut resp = Vec::new();
        client.read_to_end(&mut resp).unwrap();
        let resp_str = String::from_utf8_lossy(&resp);
        assert!(resp_str.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(resp_str.contains("Content-Length: 13\r\n"));

        let header_end = resp_str.find("\r\n\r\n").unwrap();
        let body_part = &resp[header_end + 4..];
        assert!(body_part.is_empty(), "HEAD must not send body");
    }

    #[test]
    fn test_head_pending() {
        let (tx, rx) = mpsc::channel();
        struct HeadPendRes {
            rx: Mutex<Option<Receiver<Result<Arc<[u8]>, String>>>>,
        }
        impl Resources for HeadPendRes {
            fn get(&self, path: &str) -> Option<(Body, &'static str)> {
                if path == "head_pend" {
                    let rx = self.rx.lock().take().unwrap();
                    Some((Body::Pending(rx), "text/plain"))
                } else {
                    None
                }
            }
        }
        let server = Server::start(Arc::new(HeadPendRes { rx: Mutex::new(Some(rx)) })).unwrap();

        thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            let _ = tx.send(Ok(Arc::from(&b"pending doc content"[..])));
        });

        let mut client = TcpStream::connect(format!("127.0.0.1:{}", server.port)).unwrap();
        let req = format!("HEAD /{}/head_pend HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n", server.token);
        client.write_all(req.as_bytes()).unwrap();

        let mut resp = Vec::new();
        client.read_to_end(&mut resp).unwrap();
        let resp_str = String::from_utf8_lossy(&resp);
        assert!(resp_str.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(resp_str.contains("Content-Length: 19\r\n"));
        assert!(!resp_str.contains("Transfer-Encoding: chunked"));

        let header_end = resp_str.find("\r\n\r\n").unwrap();
        let body_part = &resp[header_end + 4..];
        assert!(body_part.is_empty(), "HEAD on Pending must not send body");
    }

    #[test]
    fn test_range_206_and_416() {
        struct RangeRes;
        impl Resources for RangeRes {
            fn get(&self, path: &str) -> Option<(Body, &'static str)> {
                if path == "data" {
                    Some((Body::Ready(Arc::from(&b"0123456789"[..])), "application/octet-stream"))
                } else {
                    None
                }
            }
        }
        let server = Server::start(Arc::new(RangeRes)).unwrap();

        // 1. bytes=0-3 -> 206 "0123"
        {
            let mut client = TcpStream::connect(format!("127.0.0.1:{}", server.port)).unwrap();
            let req = format!("GET /{}/data HTTP/1.1\r\nHost: 127.0.0.1\r\nRange: bytes=0-3\r\nConnection: close\r\n\r\n", server.token);
            client.write_all(req.as_bytes()).unwrap();
            let (status, headers, body) = read_response(&mut client);
            assert_eq!(status, 206);
            assert_eq!(headers.get("content-range").unwrap(), "bytes 0-3/10");
            assert_eq!(headers.get("content-length").unwrap(), "4");
            assert_eq!(body, b"0123");
        }

        // 2. bytes=5- -> 206 "56789"
        {
            let mut client = TcpStream::connect(format!("127.0.0.1:{}", server.port)).unwrap();
            let req = format!("GET /{}/data HTTP/1.1\r\nHost: 127.0.0.1\r\nRange: bytes=5-\r\nConnection: close\r\n\r\n", server.token);
            client.write_all(req.as_bytes()).unwrap();
            let (status, headers, body) = read_response(&mut client);
            assert_eq!(status, 206);
            assert_eq!(headers.get("content-range").unwrap(), "bytes 5-9/10");
            assert_eq!(headers.get("content-length").unwrap(), "5");
            assert_eq!(body, b"56789");
        }

        // 3. bytes=-4 -> 206 "6789"
        {
            let mut client = TcpStream::connect(format!("127.0.0.1:{}", server.port)).unwrap();
            let req = format!("GET /{}/data HTTP/1.1\r\nHost: 127.0.0.1\r\nRange: bytes=-4\r\nConnection: close\r\n\r\n", server.token);
            client.write_all(req.as_bytes()).unwrap();
            let (status, headers, body) = read_response(&mut client);
            assert_eq!(status, 206);
            assert_eq!(headers.get("content-range").unwrap(), "bytes 6-9/10");
            assert_eq!(headers.get("content-length").unwrap(), "4");
            assert_eq!(body, b"6789");
        }

        // 4. Unsatisfiable: bytes=10-20 -> 416
        {
            let mut client = TcpStream::connect(format!("127.0.0.1:{}", server.port)).unwrap();
            let req = format!("GET /{}/data HTTP/1.1\r\nHost: 127.0.0.1\r\nRange: bytes=10-20\r\nConnection: close\r\n\r\n", server.token);
            client.write_all(req.as_bytes()).unwrap();
            let mut resp = Vec::new();
            client.read_to_end(&mut resp).unwrap();
            let resp_str = String::from_utf8_lossy(&resp);
            assert!(resp_str.starts_with("HTTP/1.1 416 Range Not Satisfiable\r\n"));
            assert!(resp_str.contains("Content-Range: bytes */10\r\n"));
        }

        // 5. Unsatisfiable: bytes=5-3 -> 416
        {
            let mut client = TcpStream::connect(format!("127.0.0.1:{}", server.port)).unwrap();
            let req = format!("GET /{}/data HTTP/1.1\r\nHost: 127.0.0.1\r\nRange: bytes=5-3\r\nConnection: close\r\n\r\n", server.token);
            client.write_all(req.as_bytes()).unwrap();
            let mut resp = Vec::new();
            client.read_to_end(&mut resp).unwrap();
            let resp_str = String::from_utf8_lossy(&resp);
            assert!(resp_str.starts_with("HTTP/1.1 416 Range Not Satisfiable\r\n"));
            assert!(resp_str.contains("Content-Range: bytes */10\r\n"));
        }
    }

    #[test]
    fn test_pending_ready_within_100ms_content_length() {
        let (tx, rx) = mpsc::channel();
        struct FastRes {
            rx: Mutex<Option<Receiver<Result<Arc<[u8]>, String>>>>,
        }
        impl Resources for FastRes {
            fn get(&self, path: &str) -> Option<(Body, &'static str)> {
                if path == "fast" {
                    let rx = self.rx.lock().take().unwrap();
                    Some((Body::Pending(rx), "text/plain"))
                } else {
                    None
                }
            }
        }

        let resources = Arc::new(FastRes { rx: Mutex::new(Some(rx)) });
        let server = Server::start(resources).unwrap();
        let port = server.port;

        thread::spawn(move || {
            thread::sleep(Duration::from_millis(30));
            let _ = tx.send(Ok(Arc::from(&b"fast payload"[..])));
        });

        let mut client = TcpStream::connect(format!("127.0.0.1:{}", port)).unwrap();
        let req = format!("GET /{}/fast HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n", server.token);
        client.write_all(req.as_bytes()).unwrap();

        let mut resp = Vec::new();
        client.read_to_end(&mut resp).unwrap();
        let resp_str = String::from_utf8_lossy(&resp);

        assert!(resp_str.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(resp_str.contains("Content-Length: 12\r\n"));
        assert!(!resp_str.contains("Transfer-Encoding: chunked"));
        assert!(resp_str.ends_with("fast payload"));
    }

    #[test]
    fn test_pending_ready_after_2s_chunked() {
        let (tx, rx) = mpsc::channel();
        struct SlowRes {
            rx: Mutex<Option<Receiver<Result<Arc<[u8]>, String>>>>,
        }
        impl Resources for SlowRes {
            fn get(&self, path: &str) -> Option<(Body, &'static str)> {
                if path == "slow" {
                    let rx = self.rx.lock().take().unwrap();
                    Some((Body::Pending(rx), "video/mp4"))
                } else {
                    None
                }
            }
        }

        let resources = Arc::new(SlowRes { rx: Mutex::new(Some(rx)) });
        let server = Server::start(resources).unwrap();
        let port = server.port;

        thread::spawn(move || {
            thread::sleep(Duration::from_millis(2000));
            let _ = tx.send(Ok(Arc::from(&b"slow chunked payload"[..])));
        });

        let mut client = TcpStream::connect(format!("127.0.0.1:{}", port)).unwrap();
        let req = format!("GET /{}/slow HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n", server.token);
        client.write_all(req.as_bytes()).unwrap();

        let mut resp = Vec::new();
        client.read_to_end(&mut resp).unwrap();
        let resp_str = String::from_utf8_lossy(&resp);

        assert!(resp_str.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(resp_str.contains("Transfer-Encoding: chunked\r\n"));

        let header_end = resp_str.find("\r\n\r\n").unwrap();
        let body_part = &resp[header_end + 4..];

        let hex_len = format!("{:x}\r\n", b"slow chunked payload".len());
        let mut expected_body = Vec::new();
        expected_body.extend_from_slice(hex_len.as_bytes());
        expected_body.extend_from_slice(b"slow chunked payload\r\n0\r\n\r\n");

        assert_eq!(body_part, expected_body.as_slice());
    }

    #[test]
    fn test_pending_dropped_no_terminating_chunk() {
        let (tx, rx) = mpsc::channel::<Result<Arc<[u8]>, String>>();
        struct DropRes {
            rx: Mutex<Option<Receiver<Result<Arc<[u8]>, String>>>>,
        }
        impl Resources for DropRes {
            fn get(&self, path: &str) -> Option<(Body, &'static str)> {
                if path == "drop" {
                    let rx = self.rx.lock().take().unwrap();
                    Some((Body::Pending(rx), "video/mp4"))
                } else {
                    None
                }
            }
        }

        let resources = Arc::new(DropRes { rx: Mutex::new(Some(rx)) });
        let server = Server::start(resources).unwrap();
        let port = server.port;

        thread::spawn(move || {
            thread::sleep(Duration::from_millis(1600));
            drop(tx);
        });

        let mut client = TcpStream::connect(format!("127.0.0.1:{}", port)).unwrap();
        let req = format!("GET /{}/drop HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n", server.token);
        client.write_all(req.as_bytes()).unwrap();

        let mut resp = Vec::new();
        client.read_to_end(&mut resp).unwrap();
        let resp_str = String::from_utf8_lossy(&resp);

        assert!(resp_str.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(resp_str.contains("Transfer-Encoding: chunked\r\n"));
        assert!(!resp_str.contains("0\r\n\r\n"), "must close connection without terminating chunk");
    }

    #[test]
    fn test_stop_unblocks_waiting_client_and_refuses_new_connections() {
        let (tx, rx) = mpsc::channel();
        let _never_send = tx;

        struct NeverRes {
            rx: Mutex<Option<Receiver<Result<Arc<[u8]>, String>>>>,
        }
        impl Resources for NeverRes {
            fn get(&self, path: &str) -> Option<(Body, &'static str)> {
                if path == "never" {
                    let rx = self.rx.lock().take().unwrap();
                    Some((Body::Pending(rx), "video/mp4"))
                } else {
                    None
                }
            }
        }

        let resources = Arc::new(NeverRes { rx: Mutex::new(Some(rx)) });
        let server = Server::start(resources).unwrap();
        let port = server.port;

        let mut client = TcpStream::connect(format!("127.0.0.1:{}", port)).unwrap();
        let req = format!("GET /{}/never HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n", server.token);
        client.write_all(req.as_bytes()).unwrap();

        let mut client_clone = client.try_clone().unwrap();
        let (done_tx, done_rx) = mpsc::channel();

        let reader_thread = thread::spawn(move || {
            let mut buf = [0u8; 1024];
            let res = client_clone.read(&mut buf);
            let _ = done_tx.send(res);
        });

        thread::sleep(Duration::from_millis(50));
        server.stop();

        let read_res = done_rx
            .recv_timeout(Duration::from_millis(1500))
            .expect("client read did not unblock!");
        match read_res {
            Ok(0) => {}
            Err(_) => {}
            Ok(n) => panic!("unexpected bytes read: {}", n),
        }

        reader_thread.join().unwrap();

        // Must refuse new connections
        let conn_attempt = TcpStream::connect(format!("127.0.0.1:{}", port));
        assert!(conn_attempt.is_err(), "must refuse new connections after stop()");
    }

    #[test]
    fn test_path_query_stripped() {
        struct QueryRes;
        impl Resources for QueryRes {
            fn get(&self, path: &str) -> Option<(Body, &'static str)> {
                if path == "target.m3u8" {
                    Some((Body::Ready(Arc::from(&b"content"[..])), "application/vnd.apple.mpegurl"))
                } else {
                    None
                }
            }
        }
        let server = Server::start(Arc::new(QueryRes)).unwrap();
        let mut client = TcpStream::connect(format!("127.0.0.1:{}", server.port)).unwrap();
        let req = format!("GET /{}/target.m3u8?query=param&another=1 HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n", server.token);
        client.write_all(req.as_bytes()).unwrap();

        let (status, _, body) = read_response(&mut client);
        assert_eq!(status, 200);
        assert_eq!(body, b"content");
    }

    #[test]
    fn test_header_too_large_400() {
        struct LargeRes;
        impl Resources for LargeRes {
            fn get(&self, _path: &str) -> Option<(Body, &'static str)> {
                None
            }
        }
        let server = Server::start(Arc::new(LargeRes)).unwrap();
        let mut client = TcpStream::connect(format!("127.0.0.1:{}", server.port)).unwrap();
        let big_header = "A".repeat(9000);
        let req = format!("GET /{}/res HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Big: {}\r\n\r\n", server.token, big_header);
        client.write_all(req.as_bytes()).unwrap();
        let _ = client.shutdown(Shutdown::Write);
        let mut resp = Vec::new();
        client.read_to_end(&mut resp).unwrap();
        let resp_str = String::from_utf8_lossy(&resp);
        assert!(resp_str.starts_with("HTTP/1.1 400 Bad Request\r\n"));
    }

    #[test]
    fn test_method_not_allowed_405() {
        struct MethodRes;
        impl Resources for MethodRes {
            fn get(&self, _path: &str) -> Option<(Body, &'static str)> {
                Some((Body::Ready(Arc::from(&b"data"[..])), "text/plain"))
            }
        }
        let server = Server::start(Arc::new(MethodRes)).unwrap();
        let mut client = TcpStream::connect(format!("127.0.0.1:{}", server.port)).unwrap();
        let req = format!("POST /{}/res HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", server.token);
        client.write_all(req.as_bytes()).unwrap();

        let mut resp = Vec::new();
        client.read_to_end(&mut resp).unwrap();
        let resp_str = String::from_utf8_lossy(&resp);
        assert!(resp_str.starts_with("HTTP/1.1 405 Method Not Allowed\r\n"));
        assert!(resp_str.contains("Allow: GET, HEAD\r\n"));
    }
}
