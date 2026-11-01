//! Just enough HTTP/1.1 for sls: one request per connection
//! (`Connection: close`), bodies always with `Content-Length`.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

const MAX_HEAD: usize = 16 * 1024;
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub method: String,
    /// Still percent-encoded.
    pub path: String,
    pub query: String,
    pub headers: Vec<(String, String)>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        header(&self.headers, name)
    }
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

/// The start line and header lines, up to the empty line.
fn read_head(r: &mut impl BufRead) -> io::Result<Vec<String>> {
    let mut lines = Vec::new();
    let mut total = 0;
    loop {
        let mut line = Vec::new();
        let n = r
            .by_ref()
            .take((MAX_HEAD + 1 - total) as u64)
            .read_until(b'\n', &mut line)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed",
            ));
        }
        total += n;
        if total > MAX_HEAD {
            return Err(invalid("header too large"));
        }
        let line = String::from_utf8(line).map_err(|_| invalid("header is not UTF-8"))?;
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            if lines.is_empty() {
                continue;
            }
            return Ok(lines);
        }
        lines.push(line.to_string());
    }
}

fn parse_headers(lines: &[String]) -> Vec<(String, String)> {
    lines
        .iter()
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect()
}

pub fn read_request(r: &mut impl BufRead) -> io::Result<Request> {
    let lines = read_head(r)?;
    let mut parts = lines[0].split_whitespace();
    let (Some(method), Some(target), Some(_version)) = (parts.next(), parts.next(), parts.next())
    else {
        return Err(invalid("bad request line"));
    };
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    Ok(Request {
        method: method.to_string(),
        path: path.to_string(),
        query: query.to_string(),
        headers: parse_headers(&lines[1..]),
    })
}

/// The request body (`Content-Length` bytes, at most `limit`).
pub fn read_body(r: &mut impl Read, req: &Request, limit: usize) -> io::Result<Vec<u8>> {
    let len: usize = match req.header("content-length") {
        Some(v) => v.parse().map_err(|_| invalid("bad Content-Length"))?,
        None => 0,
    };
    if len > limit {
        return Err(invalid("body too large"));
    }
    let mut body = vec![0; len];
    r.read_exact(&mut body)?;
    Ok(body)
}

/// Encodes one path segment (everything except unreserved characters).
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// `/prefix/<seg>/<seg>…` with every segment encoded.
pub fn build_path(prefix: &str, segments: &[&str]) -> String {
    let mut out = prefix.trim_end_matches('/').to_string();
    for s in segments {
        out.push('/');
        out.push_str(&percent_encode(s));
    }
    out
}

#[derive(Debug, PartialEq)]
pub enum Range {
    /// No (usable) Range header: send everything.
    Full,
    /// Inclusive byte range.
    Part(u64, u64),
    /// 416.
    Unsatisfiable,
}

/// A single `bytes=` range; multi-range requests get the whole file.
pub fn parse_range(value: Option<&str>, len: u64) -> Range {
    let Some(spec) = value.and_then(|v| v.trim().strip_prefix("bytes=")) else {
        return Range::Full;
    };
    if spec.contains(',') {
        return Range::Full;
    }
    let Some((a, b)) = spec.split_once('-') else {
        return Range::Full;
    };
    let (a, b) = (a.trim(), b.trim());
    let range = if a.is_empty() {
        // The last N bytes.
        match b.parse::<u64>() {
            Ok(0) => return Range::Unsatisfiable,
            Ok(n) => (len.saturating_sub(n), len.saturating_sub(1)),
            Err(_) => return Range::Full,
        }
    } else {
        let Ok(start) = a.parse::<u64>() else {
            return Range::Full;
        };
        let end = match b {
            "" => len.saturating_sub(1),
            b => match b.parse::<u64>() {
                Ok(e) => e.min(len.saturating_sub(1)),
                Err(_) => return Range::Full,
            },
        };
        if start > end {
            return if start >= len {
                Range::Unsatisfiable
            } else {
                Range::Full
            };
        }
        (start, end)
    };
    if len == 0 || range.0 >= len {
        Range::Unsatisfiable
    } else {
        Range::Part(range.0, range.1)
    }
}

pub fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        206 => "Partial Content",
        301 => "Moved Permanently",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Content Too Large",
        416 => "Range Not Satisfiable",
        429 => "Too Many Requests",
        503 => "Service Unavailable",
        _ => "Error",
    }
}

pub fn write_head(w: &mut impl Write, status: u16, headers: &[(&str, String)]) -> io::Result<()> {
    let mut head = format!("HTTP/1.1 {status} {}\r\n", reason(status));
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("Connection: close\r\n\r\n");
    w.write_all(head.as_bytes())
}

pub struct ClientResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    /// Exactly `len` bytes (or less if the connection breaks).
    pub body: io::Take<BufReader<TcpStream>>,
    pub len: u64,
}

impl ClientResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        header(&self.headers, name)
    }

    pub fn read_all(mut self, limit: u64) -> io::Result<Vec<u8>> {
        if self.len > limit {
            return Err(invalid("response too large"));
        }
        let mut out = Vec::with_capacity(self.len as usize);
        self.body.read_to_end(&mut out)?;
        if (out.len() as u64) < self.len {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection lost",
            ));
        }
        Ok(out)
    }
}

/// `GET path` (already encoded); `read_timeout` applies to every read.
pub fn get(
    addr: SocketAddr,
    path: &str,
    extra_headers: &[(&str, String)],
    read_timeout: Duration,
) -> io::Result<ClientResponse> {
    request(addr, "GET", path, extra_headers, None, read_timeout)
}

/// `POST path` with a JSON body.
pub fn post_json(
    addr: SocketAddr,
    path: &str,
    body: &[u8],
    read_timeout: Duration,
) -> io::Result<ClientResponse> {
    let headers = [("Content-Type", "application/json".to_string())];
    request(addr, "POST", path, &headers, Some(body), read_timeout)
}

fn request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    extra_headers: &[(&str, String)],
    body: Option<&[u8]>,
    read_timeout: Duration,
) -> io::Result<ClientResponse> {
    let mut stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)?;
    stream.set_read_timeout(Some(read_timeout))?;
    stream.set_write_timeout(Some(read_timeout))?;
    stream.set_nodelay(true)?;
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nUser-Agent: sls\r\nConnection: close\r\n"
    );
    for (k, v) in extra_headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    if let Some(body) = body {
        req.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    req.push_str("\r\n");
    let mut out = req.into_bytes();
    out.extend_from_slice(body.unwrap_or_default());
    stream.write_all(&out)?;
    let mut r = BufReader::with_capacity(64 * 1024, stream);
    let lines = read_head(&mut r)?;
    let status = lines[0]
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| invalid("bad status line"))?;
    let headers = parse_headers(&lines[1..]);
    let len = match header(&headers, "content-length") {
        Some(v) => v.parse().map_err(|_| invalid("bad Content-Length"))?,
        None => u64::MAX,
    };
    Ok(ClientResponse {
        status,
        headers,
        body: r.take(len),
        len,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_parsing() {
        let raw =
            b"\r\nGET /f/My%20Music/a.mp3?x=1 HTTP/1.1\r\nHost: h\r\nRange:  bytes=5-\r\n\r\nBODY";
        let mut r = &raw[..];
        let req = read_request(&mut r).unwrap();
        assert_eq!(req.method, "GET");
        assert_eq!(req.path, "/f/My%20Music/a.mp3");
        assert_eq!(req.query, "x=1");
        assert_eq!(req.header("range"), Some("bytes=5-"));
        assert_eq!(r, b"BODY");
    }

    #[test]
    fn bodies() {
        let raw = b"POST /api/offer HTTP/1.1\r\nContent-Length: 4\r\n\r\nBODYmore";
        let mut r = &raw[..];
        let req = read_request(&mut r).unwrap();
        assert_eq!(read_body(&mut r, &req, 100).unwrap(), b"BODY");
        let mut r = &raw[..];
        let req = read_request(&mut r).unwrap();
        assert!(read_body(&mut r, &req, 3).is_err());
        let mut r = &b"GET / HTTP/1.1\r\n\r\n"[..];
        let req = read_request(&mut r).unwrap();
        assert!(read_body(&mut r, &req, 3).unwrap().is_empty());
    }

    #[test]
    fn bad_requests() {
        assert!(read_request(&mut &b"GET\r\n\r\n"[..]).is_err());
        assert!(read_request(&mut &b"GET / HTTP/1.1\r\nHost: x\r\n"[..]).is_err());
        let huge = format!("GET / HTTP/1.1\r\nX: {}\r\n\r\n", "a".repeat(MAX_HEAD));
        assert!(read_request(&mut huge.as_bytes()).is_err());
    }

    #[test]
    fn percent_round_trip() {
        for s in ["plain", "a b/c?d#e%f", "Música — ñ.mp3", "100%"] {
            assert_eq!(percent_decode(&percent_encode(s)).as_deref(), Some(s));
        }
        assert_eq!(percent_encode("a b/ü"), "a%20b%2F%C3%BC");
        assert_eq!(percent_decode("%2"), None);
        assert_eq!(percent_decode("%zz"), None);
        assert_eq!(percent_decode("%FF"), None); // not UTF-8
        assert_eq!(
            build_path("/f/", &["My Music", "a/b"]),
            "/f/My%20Music/a%2Fb"
        );
        assert_eq!(build_path("/api/list", &[]), "/api/list");
    }

    #[test]
    fn ranges() {
        assert_eq!(parse_range(None, 10), Range::Full);
        assert_eq!(parse_range(Some("bytes=0-"), 10), Range::Part(0, 9));
        assert_eq!(parse_range(Some("bytes=2-4"), 10), Range::Part(2, 4));
        assert_eq!(parse_range(Some("bytes=2-400"), 10), Range::Part(2, 9));
        assert_eq!(parse_range(Some("bytes=-3"), 10), Range::Part(7, 9));
        assert_eq!(parse_range(Some("bytes=-30"), 10), Range::Part(0, 9));
        assert_eq!(parse_range(Some("bytes=10-"), 10), Range::Unsatisfiable);
        assert_eq!(parse_range(Some("bytes=0-"), 0), Range::Unsatisfiable);
        assert_eq!(parse_range(Some("bytes=5-2"), 10), Range::Full);
        assert_eq!(parse_range(Some("bytes=0-1,4-5"), 10), Range::Full);
        assert_eq!(parse_range(Some("items=0-1"), 10), Range::Full);
    }
}
