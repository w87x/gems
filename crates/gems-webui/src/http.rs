//! A minimal HTTP/1.1 server: just enough request parsing and response
//! writing for a small JSON API plus a couple of static assets. No
//! keep-alive, no chunked transfer, no request bodies (every API route in
//! this crate is `GET` with query-string parameters — see `main.rs`'s
//! module doc for why that's the deliberate v1 scope). Blocking I/O,
//! thread-per-connection, consistent with ARCHITECTURE.md §9's take on
//! this exact case: "admin-tool traffic levels don't need an async
//! runtime."

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;

pub struct Request {
    pub method: String,
    pub path: String,
    pub query: Vec<(String, String)>,
}

impl Request {
    pub fn query_param(&self, key: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

pub fn parse_request(stream: &TcpStream) -> Result<Request, String> {
    let mut reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .map_err(|e| e.to_string())?;
    if request_line.is_empty() {
        return Err("empty request".to_string());
    }

    let mut parts = request_line.trim_end().splitn(3, ' ');
    let method = parts.next().ok_or("missing method")?.to_string();
    let target = parts.next().ok_or("missing request target")?.to_string();

    // Drain (and ignore) headers up to the blank line — no header this
    // crate's routes need to read.
    loop {
        let mut header_line = String::new();
        let n = reader
            .read_line(&mut header_line)
            .map_err(|e| e.to_string())?;
        if n == 0 || header_line.trim().is_empty() {
            break;
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
}
