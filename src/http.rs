// A minimal hand-written HTTP/1.1 client: it only serves the advisor's
// one-shot POST to the local Ollama (plain HTTP, fixed endpoint). Rust's std
// has no HTTP client, and to avoid a heavy dependency (the Go version relied
// on net/http) this goes straight to TcpStream, preserving the same "single
// binary, nearly zero dependencies" property.
//
// Go's context.WithTimeout is a deadline over the whole request, while
// set_read_timeout bounds only a single syscall — a slow-drip response could
// overshoot the total. So an Instant deadline recalculates the remaining time
// before every read/write. The response body is capped at 1MB, matching the
// Go version's io.LimitReader.

use std::io::{ErrorKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

const MAX_BODY_BYTES: usize = 1 << 20; // aligned with the Go version's io.LimitReader(resp.Body, 1<<20)
const MAX_CHUNK: usize = 1 << 22; // per-chunk cap (defensive; Ollama is far below this)

#[derive(Debug)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>, // at most MAX_BODY_BYTES (truncated beyond that; the upper layer's JSON parse then reports it)
}

// post_json: POST application/json. Any failure returns Err(String); the
// caller (ask_advisor) uniformly wraps it as "advisor API unreachable".
pub fn post_json(url: &str, body: &str, timeout: Duration) -> Result<HttpResponse, String> {
    let deadline = Instant::now() + timeout;

    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("unsupported url (only plain http): {url}"))?;
    let (host_port, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    // host:port is all we need; IPv6 bracket forms are not handled (the endpoint is hard-coded localhost)
    let (host, port) = match host_port.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => {
            (h, p.parse::<u16>().map_err(|e| format!("invalid port: {e}"))?)
        }
        _ => (host_port, 80),
    };

    let addrs: Vec<_> = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("resolve {host}:{port}: {e}"))?
        .collect();
    let mut stream = None;
    let mut last_err = String::from("no address resolved");
    for addr in addrs {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(timeout_msg(timeout));
        }
        match TcpStream::connect_timeout(&addr, remaining) {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(e) => last_err = e.to_string(),
        }
    }
    let mut stream = stream.ok_or_else(|| format!("dial {host}:{port}: {last_err}"))?;

    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: {host_port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    write_all_deadline(&mut stream, req.as_bytes(), deadline).map_err(|e| translate_io(&e, timeout))?;

    // ---- read the response ----
    let mut raw: Vec<u8> = Vec::new();
    let header_end = loop {
        if let Some(pos) = find(&raw, b"\r\n\r\n") {
            break pos;
        }
        if !fill(&mut stream, &mut raw, deadline).map_err(|e| translate_io(&e, timeout))? {
            return Err("connection closed before response headers were complete".into());
        }
        if raw.len() > MAX_BODY_BYTES {
            return Err("response headers too large".into());
        }
    };

    let head = String::from_utf8_lossy(&raw[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let status = status_line
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| format!("malformed status line: {status_line}"))?;
    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim().to_ascii_lowercase();
            let v = v.trim();
            if k == "content-length" {
                content_length = v.parse().ok();
            } else if k == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked") {
                chunked = true;
            }
        }
    }

    let mut body: Vec<u8> = raw[header_end + 4..].to_vec();
    if body.len() < MAX_BODY_BYTES {
        if chunked {
            body = read_chunked(&mut stream, body, deadline).map_err(|e| translate_io(&e, timeout))?;
        } else if let Some(n) = content_length {
            while body.len() < n.min(MAX_BODY_BYTES) {
                if !fill(&mut stream, &mut body, deadline).map_err(|e| translate_io(&e, timeout))? {
                    break; // early EOF: the upper layer's JSON parse reports it (same path as the Go version)
                }
            }
        } else {
            // Connection: close with no Content-Length: read to EOF or the cap
            while body.len() < MAX_BODY_BYTES {
                if !fill(&mut stream, &mut body, deadline).map_err(|e| translate_io(&e, timeout))? {
                    break;
                }
            }
        }
    }
    body.truncate(MAX_BODY_BYTES);
    Ok(HttpResponse { status, body })
}

// read_chunked: decode Transfer-Encoding: chunked (Ollama's non-streaming
// responses usually give Content-Length, but that's not guaranteed). Stops at
// the body cap, mimicking the Go LimitReader's truncation.
fn read_chunked(stream: &mut TcpStream, mut buf: Vec<u8>, deadline: Instant) -> Result<Vec<u8>, std::io::Error> {
    let mut out = Vec::new();
    loop {
        while find(&buf, b"\r\n").is_none() {
            // cap the size-line scan (Go's chunked reader bails early on
            // overlong lines) to prevent unbounded buffering
            if buf.len() > MAX_BODY_BYTES {
                return Err(invalid_data("chunk size line too long"));
            }
            if !fill(stream, &mut buf, deadline)? {
                return Err(invalid_data("eof in chunk size"));
            }
        }
        let pos = find(&buf, b"\r\n").unwrap();
        let line = String::from_utf8_lossy(&buf[..pos]).to_string();
        let size = usize::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| invalid_data("malformed chunk size"))?;
        buf.drain(..pos + 2);
        if size == 0 {
            return Ok(out); // terminal chunk; trailers are rare, just discard them (Connection: close)
        }
        if size > MAX_CHUNK {
            return Err(invalid_data("chunk too large"));
        }
        while buf.len() < size + 2 {
            if !fill(stream, &mut buf, deadline)? {
                return Err(invalid_data("eof in chunk data"));
            }
        }
        out.extend_from_slice(&buf[..size]);
        if &buf[size..size + 2] != b"\r\n" {
            return Err(invalid_data("malformed chunk terminator"));
        }
        buf.drain(..size + 2);
        if out.len() >= MAX_BODY_BYTES {
            return Ok(out);
        }
    }
}

// fill: read one round into buf; false means EOF. The remaining deadline is
// set as the read timeout before every read — the stand-in for Go's
// ctx-timeout semantics.
fn fill(stream: &mut TcpStream, buf: &mut Vec<u8>, deadline: Instant) -> Result<bool, std::io::Error> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(std::io::Error::new(ErrorKind::TimedOut, "deadline exceeded"));
    }
    stream.set_read_timeout(Some(remaining))?;
    let mut tmp = [0u8; 16384];
    match stream.read(&mut tmp) {
        Ok(0) => Ok(false),
        Ok(n) => {
            buf.extend_from_slice(&tmp[..n]);
            Ok(true)
        }
        Err(e) if e.kind() == ErrorKind::Interrupted => Ok(true), // retry
        Err(e) => Err(e),
    }
}

fn write_all_deadline(stream: &mut TcpStream, mut data: &[u8], deadline: Instant) -> Result<(), std::io::Error> {
    while !data.is_empty() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(std::io::Error::new(ErrorKind::TimedOut, "deadline exceeded"));
        }
        stream.set_write_timeout(Some(remaining))?;
        match stream.write(data) {
            Ok(0) => return Err(invalid_data("write returned 0")),
            Ok(n) => data = &data[n..],
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn translate_io(e: &std::io::Error, timeout: Duration) -> String {
    match e.kind() {
        ErrorKind::WouldBlock | ErrorKind::TimedOut => timeout_msg(timeout),
        _ => e.to_string(),
    }
}

fn timeout_msg(timeout: Duration) -> String {
    format!("request timed out after {timeout:?}")
}

fn invalid_data(msg: &str) -> std::io::Error {
    std::io::Error::new(ErrorKind::InvalidData, msg)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn spawn_server(
        respond: fn(TcpStream),
    ) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((s, _)) = listener.accept() {
                respond(s);
            }
        });
        port
    }

    fn serve_content_length(mut s: TcpStream) {
        let mut buf = [0u8; 4096];
        let _ = s.read(&mut buf); // the whole request usually arrives in one read; good enough for tests
        let body = r#"{"choices":[{"message":{"content":"hello"},"finish_reason":"stop"}]}"#;
        let _ = write!(
            s,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
    }

    fn serve_chunked(mut s: TcpStream) {
        let mut buf = [0u8; 4096];
        let _ = s.read(&mut buf);
        let payload = concat!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n",
            "5\r\n{\"a\":\r\n",
            "a\r\n\"choices\"}\r\n", // \"choices\"} = 10 bytes = hex a
            "0\r\n\r\n"
        );
        let _ = write!(s, "{payload}");
    }

    fn serve_no_length(mut s: TcpStream) {
        let mut buf = [0u8; 4096];
        let _ = s.read(&mut buf);
        let _ = write!(s, "HTTP/1.1 500 Internal Server Error\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\nplain text error");
        // drop(s) closes the connection → close-delimited body
    }

    fn serve_silent(_s: TcpStream) {
        std::thread::sleep(Duration::from_secs(10)); // never responds
    }

    #[test]
    fn parses_content_length_response() {
        let port = spawn_server(serve_content_length);
        let resp = post_json(&format!("http://127.0.0.1:{port}/v1/x"), "{}", Duration::from_secs(5)).unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(String::from_utf8_lossy(&resp.body), r#"{"choices":[{"message":{"content":"hello"},"finish_reason":"stop"}]}"#);
    }

    #[test]
    fn parses_chunked_response() {
        let port = spawn_server(serve_chunked);
        let resp = post_json(&format!("http://127.0.0.1:{port}/"), "{}", Duration::from_secs(5)).unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(String::from_utf8_lossy(&resp.body), r#"{"a":"choices"}"#);
    }

    #[test]
    fn parses_close_delimited_response() {
        let port = spawn_server(serve_no_length);
        let resp = post_json(&format!("http://127.0.0.1:{port}/"), "{}", Duration::from_secs(5)).unwrap();
        assert_eq!(resp.status, 500);
        assert_eq!(String::from_utf8_lossy(&resp.body), "plain text error");
    }

    #[test]
    fn total_deadline_is_enforced() {
        let port = spawn_server(serve_silent);
        let start = Instant::now();
        let err = post_json(&format!("http://127.0.0.1:{port}/"), "{}", Duration::from_millis(800)).unwrap_err();
        assert!(err.contains("timed out"), "unexpected error: {err}");
        let elapsed = start.elapsed();
        assert!(elapsed >= Duration::from_millis(700), "returned too early: {elapsed:?}");
        assert!(elapsed < Duration::from_secs(5), "deadline not enforced: {elapsed:?}");
    }

    #[test]
    fn connects_via_localhost_name_when_ipv6_unavailable() {
        // localhost may resolve to both ::1 and 127.0.0.1 (order varies by
        // system); when the server binds only 127.0.0.1, the client must try
        // every address in turn (like the Go dialer), not just the first
        let port = spawn_server(serve_content_length);
        let resp = post_json(&format!("http://localhost:{port}/v1/x"), "{}", Duration::from_secs(5)).unwrap();
        assert_eq!(resp.status, 200);
    }

    #[test]
    fn rejects_non_http_url() {
        assert!(post_json("https://x/y", "{}", Duration::from_secs(1)).is_err());
    }

    #[test]
    fn chunked_size_line_capped() {
        // an endless size line (no CRLF) → must not buffer forever; error past
        // 1MB (OOM guard)
        fn serve(mut s: TcpStream) {
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            let _ = write!(s, "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n");
            let mut sent = 0usize;
            let chunk = [b'z'; 65536]; // non-hex, no CRLF
            while sent < 4 << 20 {
                let n = chunk.len().min((4 << 20) - sent);
                if s.write_all(&chunk[..n]).is_err() {
                    break; // client hung up (cap tripped)
                }
                sent += n;
            }
        }
        let port = spawn_server(serve);
        let err = post_json(&format!("http://127.0.0.1:{port}/"), "{}", Duration::from_secs(10)).unwrap_err();
        assert!(err.contains("chunk size line too long") || err.contains("malformed"), "unexpected: {err}");
    }
}
