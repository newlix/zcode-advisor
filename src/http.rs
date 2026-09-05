// 手寫的極簡 HTTP/1.1 client：只服務 advisor 對本機 Ollama（純 HTTP、固定端點）的
// 單發 POST。Rust std 沒有 HTTP client，為了不引入重依賴（Go 版靠 net/http），
// 這裡直接走 TcpStream，換取與 Go 版相同的「單 binary、近乎零依賴」性質。
//
// Go 版的 context.WithTimeout 是「整個請求」的 deadline，而 set_read_timeout 只
// 約束單次 syscall——慢速滴漏回應會讓總時長超標，因此全程以 Instant deadline
// 在每次讀寫前重算剩餘時間。回應 body 上限 1MB，對應 Go 版的 io.LimitReader。

use std::io::{ErrorKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

const MAX_BODY_BYTES: usize = 1 << 20; // 與 Go 版 io.LimitReader(resp.Body, 1<<20) 對齊
const MAX_CHUNK: usize = 1 << 22; // 單一 chunk 上限（防禦值；Ollama 遠低於此）

#[derive(Debug)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>, // 至多 MAX_BODY_BYTES（超過就截斷，交由上層 JSON 解析報錯）
}

// post_json：POST application/json。任何失敗以 Err(String) 回傳，
// 呼叫端（ask_advisor）統一包成「advisor API unreachable」。
pub fn post_json(url: &str, body: &str, timeout: Duration) -> Result<HttpResponse, String> {
    let deadline = Instant::now() + timeout;

    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("unsupported url (only plain http): {url}"))?;
    let (host_port, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    // 只需涵蓋 host:port；不處理 IPv6 bracket 形式（端點寫死是 localhost）
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

    // ---- 讀回應 ----
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
                    break; // 提前 EOF：交給上層 JSON 解析報錯（與 Go 版同路徑）
                }
            }
        } else {
            // Connection: close 且無 Content-Length：讀到 EOF 或上限
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

// read_chunked：解 Transfer-Encoding: chunked（Ollama 非串流回應通常給
// Content-Length，但不保證）。到達 body 上限即停，模擬 Go 版 LimitReader 的截斷。
fn read_chunked(stream: &mut TcpStream, mut buf: Vec<u8>, deadline: Instant) -> Result<Vec<u8>, std::io::Error> {
    let mut out = Vec::new();
    loop {
        while find(&buf, b"\r\n").is_none() {
            // size 行掃描設上限（Go 的 chunked reader 對超長行會早停），防無限緩衝
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
            return Ok(out); // terminal chunk；trailer 罕見，直接丟棄（Connection: close）
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

// fill：讀一輪進 buf；回傳 false 代表 EOF。每次讀前把剩餘 deadline 設成
// read timeout——這是 Go ctx-timeout 語義的代替品。
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
        Err(e) if e.kind() == ErrorKind::Interrupted => Ok(true), // 重試
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
        let _ = s.read(&mut buf); // 請求整包通常一次讀完，測試用途足夠
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
        // drop(s) 關連線 → close-delimited body
    }

    fn serve_silent(_s: TcpStream) {
        std::thread::sleep(Duration::from_secs(10)); // 永不回應
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
        // localhost 可能解析出 ::1 與 127.0.0.1（順序依系統而異）；server 只綁
        // 127.0.0.1 時，client 必須逐一嘗試所有位址（同 Go dialer）而非只試第一個
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
        // size 行無止盡（無 CRLF）→ 不得無限緩衝，超過 1MB 即報錯（防 OOM）
        fn serve(mut s: TcpStream) {
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            let _ = write!(s, "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n");
            let mut sent = 0usize;
            let chunk = [b'z'; 65536]; // 非十六進位、無 CRLF
            while sent < 4 << 20 {
                let n = chunk.len().min((4 << 20) - sent);
                if s.write_all(&chunk[..n]).is_err() {
                    break; // client 已斷開（上限觸發）
                }
                sent += n;
            }
        }
        let port = spawn_server(serve);
        let err = post_json(&format!("http://127.0.0.1:{port}/"), "{}", Duration::from_secs(10)).unwrap_err();
        assert!(err.contains("chunk size line too long") || err.contains("malformed"), "unexpected: {err}");
    }
}
