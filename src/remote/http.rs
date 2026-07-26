// Minimal HTTP/1.1 request parsing + response building for the remote log
// console. Hand-rolled to keep the crate dependency-free and sync — the
// service already runs everything on OS threads (no async runtime).
//
// The parsing and building here are pure (no sockets); `mod.rs` owns the
// TcpListener accept loop and feeds a BufReader into `parse_request`.

use std::collections::HashMap;
use std::io::{self, BufRead, Write};

/// A parsed inbound request. Only the fields the router needs are kept.
#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub query: HashMap<String, String>,
    /// Header names are lowercased so lookups are case-insensitive.
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl Request {
    /// Value of the named cookie from the `Cookie:` header, if present.
    /// Parses `a=1; session=abc; b=2` into pairs and matches `name`.
    pub fn cookie(&self, name: &str) -> Option<&str> {
        let raw = self.headers.get("cookie")?;
        for pair in raw.split(';') {
            let pair = pair.trim();
            if let Some((k, v)) = pair.split_once('=') {
                if k.trim() == name {
                    return Some(v.trim());
                }
            }
        }
        None
    }
}

/// Parse a single HTTP/1.1 request: request line, headers, and the body
/// (when a `Content-Length` header is present). Returns `InvalidData` for a
/// malformed request line or an unparseable `Content-Length`.
pub fn parse_request<R: BufRead>(reader: &mut R) -> io::Result<Request> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "empty request"));
    }

    let mut parts = line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing method"))?
        .to_string();
    let target = parts
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing target"))?;

    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), parse_query(q)),
        None => (target.to_string(), HashMap::new()),
    };

    let mut headers = HashMap::new();
    loop {
        let mut header_line = String::new();
        if reader.read_line(&mut header_line)? == 0 {
            break;
        }
        let trimmed = header_line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }

    let body = match headers.get("content-length") {
        Some(len) => {
            let n: usize = len
                .parse()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad content-length"))?;
            let mut buf = vec![0u8; n];
            reader.read_exact(&mut buf)?;
            buf
        }
        None => Vec::new(),
    };

    Ok(Request {
        method,
        path,
        query,
        headers,
        body,
    })
}

/// Split a raw `k=v&k2=v2` query string into a map. Keys/values are taken
/// verbatim — the console only sends ASCII params (`service`, integers), so
/// no percent-decoding is needed.
fn parse_query(raw: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for pair in raw.split('&') {
        if pair.is_empty() {
            continue;
        }
        match pair.split_once('=') {
            Some((k, v)) => map.insert(k.to_string(), v.to_string()),
            None => map.insert(pair.to_string(), String::new()),
        };
    }
    map
}

/// A response ready to serialize. Bodies are buffered (small JSON / the
/// embedded page / a zip); SSE streams use `write_sse_headers` instead.
#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub reason: &'static str,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    fn new(status: u16, reason: &'static str, content_type: &str, body: Vec<u8>) -> Self {
        Response {
            status,
            reason,
            headers: vec![("Content-Type".to_string(), content_type.to_string())],
            body,
        }
    }

    pub fn html(body: impl Into<Vec<u8>>) -> Self {
        Response::new(200, "OK", "text/html; charset=utf-8", body.into())
    }

    pub fn json(status: u16, reason: &'static str, body: impl Into<Vec<u8>>) -> Self {
        Response::new(status, reason, "application/json", body.into())
    }

    pub fn ok_json(body: impl Into<Vec<u8>>) -> Self {
        Response::json(200, "OK", body)
    }

    pub fn not_found() -> Self {
        Response::json(404, "Not Found", r#"{"error":"not_found"}"#)
    }

    pub fn unauthorized() -> Self {
        Response::json(401, "Unauthorized", r#"{"error":"unauthorized"}"#)
    }

    pub fn zip(bytes: Vec<u8>) -> Self {
        let mut r = Response::new(200, "OK", "application/zip", bytes);
        r.headers.push((
            "Content-Disposition".to_string(),
            "attachment; filename=\"purrpause-logs.zip\"".to_string(),
        ));
        r
    }

    /// Attach a `Set-Cookie` header. The token cookie is `HttpOnly`,
    /// `SameSite=Strict`, `Path=/` — no `Secure` because the LAN console is
    /// plain HTTP by design ("friction, not fort-knox").
    pub fn with_cookie(mut self, name: &str, value: &str) -> Self {
        self.headers.push((
            "Set-Cookie".to_string(),
            format!("{name}={value}; HttpOnly; SameSite=Strict; Path=/"),
        ));
        self
    }

    /// Write the full response (status line, headers, `Content-Length`, body).
    pub fn write_to<W: Write>(&self, w: &mut W) -> io::Result<()> {
        write!(w, "HTTP/1.1 {} {}\r\n", self.status, self.reason)?;
        for (k, v) in &self.headers {
            write!(w, "{k}: {v}\r\n")?;
        }
        write!(w, "Content-Length: {}\r\n", self.body.len())?;
        write!(w, "Connection: close\r\n\r\n")?;
        w.write_all(&self.body)?;
        w.flush()
    }
}

/// SSE keep-alive comment — emitted periodically so proxies/clients keep the
/// connection open during quiet periods.
pub const SSE_PING: &str = ": ping\n\n";

/// One SSE `data:` frame for a single log line.
pub fn sse_data(line: &str) -> String {
    format!("data: {line}\n\n")
}

/// Write the SSE response preamble: `200 OK` + event-stream headers, no
/// `Content-Length` (the body streams until the socket closes).
pub fn write_sse_headers<W: Write>(w: &mut W) -> io::Result<()> {
    write!(w, "HTTP/1.1 200 OK\r\n")?;
    write!(w, "Content-Type: text/event-stream\r\n")?;
    write!(w, "Cache-Control: no-cache\r\n")?;
    write!(w, "Connection: keep-alive\r\n\r\n")?;
    w.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;

    fn parse(raw: &str) -> Request {
        let mut r = BufReader::new(raw.as_bytes());
        parse_request(&mut r).unwrap()
    }

    #[test]
    fn parses_get_with_query_and_headers() {
        let req = parse("GET /api/logs?file=service&tail=50 HTTP/1.1\r\nHost: x.local\r\nCookie: session=abc123\r\n\r\n");
        assert_eq!(req.method, "GET");
        assert_eq!(req.path, "/api/logs");
        assert_eq!(req.query.get("file").unwrap(), "service");
        assert_eq!(req.query.get("tail").unwrap(), "50");
        assert_eq!(req.headers.get("host").unwrap(), "x.local");
        assert!(req.body.is_empty());
    }

    #[test]
    fn header_lookup_is_case_insensitive() {
        let req = parse("GET / HTTP/1.1\r\nCONTENT-TYPE: text/plain\r\n\r\n");
        assert_eq!(req.headers.get("content-type").unwrap(), "text/plain");
    }

    #[test]
    fn parses_post_body_by_content_length() {
        let req = parse("POST /api/auth HTTP/1.1\r\nContent-Length: 13\r\n\r\n{\"pin\":\"42\"}!");
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, "/api/auth");
        assert_eq!(req.body, b"{\"pin\":\"42\"}!");
    }

    #[test]
    fn cookie_extracts_named_value() {
        let req = parse("GET / HTTP/1.1\r\nCookie: a=1; session=tok99; b=2\r\n\r\n");
        assert_eq!(req.cookie("session"), Some("tok99"));
        assert_eq!(req.cookie("missing"), None);
    }

    #[test]
    fn cookie_absent_header_returns_none() {
        let req = parse("GET / HTTP/1.1\r\n\r\n");
        assert_eq!(req.cookie("session"), None);
    }

    #[test]
    fn empty_request_is_eof_error() {
        let mut r = BufReader::new(&b""[..]);
        let err = parse_request(&mut r).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn response_write_shape() {
        let resp = Response::ok_json(r#"{"ok":true}"#);
        let mut out = Vec::new();
        resp.write_to(&mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("Content-Type: application/json\r\n"));
        assert!(text.contains("Content-Length: 11\r\n"));
        assert!(text.ends_with("\r\n\r\n{\"ok\":true}"));
    }

    #[test]
    fn with_cookie_sets_hardened_attributes() {
        let resp = Response::ok_json("{}").with_cookie("session", "tok");
        let cookie = resp
            .headers
            .iter()
            .find(|(k, _)| k == "Set-Cookie")
            .map(|(_, v)| v.as_str())
            .unwrap();
        assert_eq!(cookie, "session=tok; HttpOnly; SameSite=Strict; Path=/");
    }

    #[test]
    fn unauthorized_is_401() {
        let resp = Response::unauthorized();
        assert_eq!(resp.status, 401);
    }

    #[test]
    fn sse_frame_shapes() {
        assert_eq!(sse_data("hello world"), "data: hello world\n\n");
        assert_eq!(SSE_PING, ": ping\n\n");
    }

    #[test]
    fn sse_headers_have_no_content_length() {
        let mut out = Vec::new();
        write_sse_headers(&mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("Content-Type: text/event-stream\r\n"));
        assert!(!text.to_ascii_lowercase().contains("content-length"));
        assert!(text.ends_with("\r\n\r\n"));
    }
}
