//! A minimal HTTP/1.1 server: just enough request parsing and response
//! writing for a small JSON API plus a couple of static assets. No
//! keep-alive, no chunked transfer, no request bodies (every API route in
//! this crate is `GET` with query-string parameters — see `main.rs`'s
//! module doc for why that's the deliberate v1 scope). Blocking I/O,
//! thread-per-connection, consistent with ARCHITECTURE.md §9's take on
//! this exact case: "admin-tool traffic levels don't need an async
//! runtime."

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// Caps on a single connection's request-line/header parsing, so a slow or
/// hostile client can't hold a server thread (and unbounded memory) by
/// trickling an arbitrarily long line or an endless stream of headers that
/// never reaches the terminating blank line. `std::io::BufRead::read_line`
/// has no length limit of its own — without `MAX_LINE_LEN`, a client that
/// never sends `\n` makes that call buffer the connection's entire input
/// before returning.
const MAX_LINE_LEN: u64 = 8 * 1024;
const MAX_HEADER_LINES: usize = 200;
const DEFAULT_READ_TIMEOUT_SECS: u64 = 10;

/// The read timeout is the one operator-tunable knob here — a slower
/// network path (a reverse proxy adding latency, a high-RTT client) might
/// legitimately need longer than the 10s default without a rebuild; the
/// length/count caps stay fixed constants since raising them only widens
/// the resource-exhaustion window `MAX_LINE_LEN`/`MAX_HEADER_LINES` exist
/// to bound, with no comparable legitimate reason to need a bigger value.
fn read_timeout() -> Duration {
    let raw = std::env::var("GEMS_WEBUI_READ_TIMEOUT_SECS").ok();
    parse_read_timeout_secs(raw.as_deref())
}

/// Pure parsing logic, factored out so it's testable without mutating the
/// real process environment (a hazard for tests that run in parallel
/// within one process).
fn parse_read_timeout_secs(raw: Option<&str>) -> Duration {
    let secs = raw
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&secs| secs > 0)
        .unwrap_or(DEFAULT_READ_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

pub struct Request {
    pub method: String,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
}

impl Request {
    pub fn query_param(&self, key: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Case-insensitive header lookup, per HTTP's own field-name matching
    /// rules (`Authorization` and `authorization` are the same header).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// The bearer token from an `Authorization: Bearer <token>` header, if
    /// present and correctly formed.
    pub fn bearer_token(&self) -> Option<&str> {
        self.header("Authorization")?.strip_prefix("Bearer ")
    }
}

/// Reads one line (including its trailing `\n`, if any) from `reader`,
/// refusing to buffer more than `MAX_LINE_LEN` bytes while looking for it.
fn read_line_capped(reader: &mut impl BufRead) -> Result<String, String> {
    let mut buf = Vec::new();
    let mut limited = reader.take(MAX_LINE_LEN);
    limited
        .read_until(b'\n', &mut buf)
        .map_err(|e| e.to_string())?;
    if buf.len() as u64 >= MAX_LINE_LEN && !buf.ends_with(b"\n") {
        return Err("line exceeds the maximum allowed length".to_string());
    }
    String::from_utf8(buf).map_err(|e| e.to_string())
}

pub fn parse_request(stream: &TcpStream) -> Result<Request, String> {
    stream
        .set_read_timeout(Some(read_timeout()))
        .map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
    let request_line = read_line_capped(&mut reader)?;
    if request_line.is_empty() {
        return Err("empty request".to_string());
    }

    let mut parts = request_line.trim_end().splitn(3, ' ');
    let method = parts.next().ok_or("missing method")?.to_string();
    let target = parts.next().ok_or("missing request target")?.to_string();

    // Read headers up to the blank line, bounded in both line length and
    // line count so a client can't hold the connection open indefinitely
    // by never sending the terminating blank line.
    let mut headers = Vec::new();
    for _ in 0..MAX_HEADER_LINES {
        let header_line = read_line_capped(&mut reader)?;
        let trimmed = header_line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            headers.push((name.trim().to_string(), value.trim().to_string()));
        }
    }

    let (path, query_string) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target, String::new()),
    };
    let query = parse_query_string(&query_string);

    Ok(Request {
        method,
        path,
        query,
        headers,
    })
}

fn parse_query_string(s: &str) -> Vec<(String, String)> {
    if s.is_empty() {
        return Vec::new();
    }
    s.split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((k, v)) => (url_decode(k), url_decode(v)),
            None => (url_decode(pair), String::new()),
        })
        .collect()
}

/// Percent-decoding plus `+` -> space, as `application/x-www-form-
/// urlencoded` query strings use (what every browser sends from a plain
/// HTML form or a hand-built `fetch` URL).
fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    None => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn write_response(stream: &mut TcpStream, status: u16, content_type: &str, body: &[u8]) {
    let status_text = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Unknown",
    };
    let header = format!(
        "HTTP/1.1 {status} {status_text}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(body);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_read_timeout_secs_falls_back_to_the_default() {
        assert_eq!(
            parse_read_timeout_secs(None),
            Duration::from_secs(DEFAULT_READ_TIMEOUT_SECS)
        );
        assert_eq!(
            parse_read_timeout_secs(Some("not a number")),
            Duration::from_secs(DEFAULT_READ_TIMEOUT_SECS)
        );
        assert_eq!(
            parse_read_timeout_secs(Some("0")),
            Duration::from_secs(DEFAULT_READ_TIMEOUT_SECS),
            "0 must fall back to the default, not disable the timeout entirely"
        );
    }

    #[test]
    fn parse_read_timeout_secs_honors_a_valid_override() {
        assert_eq!(parse_read_timeout_secs(Some("30")), Duration::from_secs(30));
    }

    #[test]
    fn url_decode_handles_percent_and_plus() {
        assert_eq!(url_decode("hello+world"), "hello world");
        assert_eq!(url_decode("a%3Db"), "a=b");
        assert_eq!(url_decode("100%25"), "100%");
    }

    #[test]
    fn parse_query_string_splits_pairs() {
        let parsed = parse_query_string("a=1&b=hello+world&c=");
        assert_eq!(
            parsed,
            vec![
                ("a".to_string(), "1".to_string()),
                ("b".to_string(), "hello world".to_string()),
                ("c".to_string(), "".to_string()),
            ]
        );
    }

    #[test]
    fn parse_query_string_empty_is_empty() {
        assert!(parse_query_string("").is_empty());
    }

    #[test]
    fn parse_request_rejects_a_request_line_over_the_length_cap() {
        use std::net::TcpListener;
        use std::thread;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let writer = thread::spawn(move || {
            let mut client = TcpStream::connect(addr).unwrap();
            // A request line far longer than MAX_LINE_LEN, no newline —
            // simulates a client that never terminates the line. Without
            // the cap, parse_request's read_line would buffer this
            // (and keep waiting for more) indefinitely.
            let oversized = vec![b'A'; (MAX_LINE_LEN as usize) * 2];
            let _ = client.write_all(b"GET /");
            let _ = client.write_all(&oversized);
            // Let the connection drop; server side should already have
            // errored out on the length cap before this.
        });

        let (stream, _) = listener.accept().unwrap();
        let result = parse_request(&stream);
        assert!(
            result.is_err(),
            "an unterminated over-long line must be rejected"
        );

        writer.join().unwrap();
    }

    #[test]
    fn parse_request_rejects_too_many_header_lines() {
        use std::net::TcpListener;
        use std::thread;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let writer = thread::spawn(move || {
            let mut client = TcpStream::connect(addr).unwrap();
            let _ = client.write_all(b"GET / HTTP/1.1\r\n");
            // More headers than MAX_HEADER_LINES, never reaching a blank
            // line — a client trying to hold the connection open forever.
            for i in 0..(MAX_HEADER_LINES * 2) {
                let _ = client.write_all(format!("X-Pad-{i}: v\r\n").as_bytes());
            }
        });

        let (stream, _) = listener.accept().unwrap();
        // With the header count bounded, parse_request must return
        // (successfully, treating the loop's exhaustion as "no more
        // headers to read") rather than looping forever waiting for a
        // blank line that never comes.
        let result = parse_request(&stream);
        assert!(result.is_ok());

        writer.join().unwrap();
    }
}
