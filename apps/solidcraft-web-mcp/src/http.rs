//! HTTP/1.1 as far as the relay needs it: one request read from a connection (its head parsed by
//! `httparse`), one response written. Everything a client sends is hostile: the head and the body
//! are capped, and a connection that stalls runs into its socket's timeouts.

use std::io::{BufRead, Read, Write};

/// The most a request's head (request line and headers) may take.
const MAX_HEAD: usize = 32 * 1024;
/// The most headers a request may carry.
const MAX_HEADERS: usize = 64;

/// A request as read from the connection.
#[derive(Debug)]
pub struct Request {
    pub method: String,
    /// The path, percent-encoding and all, without its query.
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    /// The first header named `name` (any case), trimmed.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.trim())
    }

    /// Whether the comma-separated header `name` lists `token` (any case).
    pub fn header_lists(&self, name: &str, token: &str) -> bool {
        self.headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case(name))
            .any(|(_, v)| v.split(',').any(|part| part.split(';').next().is_some_and(|t| t.trim().eq_ignore_ascii_case(token))))
    }

    /// The bearer token of the `Authorization` header.
    pub fn bearer(&self) -> Option<&str> {
        let value = self.header("authorization")?;
        let (scheme, token) = value.split_once(' ')?;
        scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
    }
}

/// Why no request was read.
#[derive(Debug)]
pub enum ReadError {
    /// The client closed the connection, or went quiet, before sending a request.
    Closed,
    /// The request cannot be served: the status and what to say.
    Refused(u16, &'static str),
}

/// Read one request, its body at most `max_body` bytes.
pub fn read_request(input: &mut impl BufRead, max_body: usize) -> Result<Request, ReadError> {
    let mut head = Vec::new();
    loop {
        let before = head.len();
        // One line at a time, never more than the cap plus one byte.
        let read = input.by_ref().take((MAX_HEAD + 1 - before.min(MAX_HEAD)) as u64).read_until(b'\n', &mut head).map_err(|_| ReadError::Closed)?;
        if read == 0 {
            return Err(if head.is_empty() { ReadError::Closed } else { ReadError::Refused(400, "incomplete request") });
        }
        if head.len() > MAX_HEAD {
            return Err(ReadError::Refused(431, "request head too large"));
        }
        let line = head.get(before..).unwrap_or_default();
        // A blank line before the request line is skipped, as HTTP allows.
        if before == 0 && (line == b"\r\n" || line == b"\n") {
            head.clear();
            continue;
        }
        if line == b"\r\n" || line == b"\n" {
            break;
        }
    }
    let mut slots = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut parsed = httparse::Request::new(&mut slots);
    match parsed.parse(&head) {
        Ok(httparse::Status::Complete(_)) => {}
        Ok(httparse::Status::Partial) => return Err(ReadError::Refused(400, "incomplete request")),
        Err(httparse::Error::TooManyHeaders) => return Err(ReadError::Refused(431, "too many headers")),
        Err(_) => return Err(ReadError::Refused(400, "malformed request")),
    }
    let method = parsed.method.unwrap_or_default().to_string();
    let target = parsed.path.unwrap_or_default();
    let path = target.split(['?', '#']).next().unwrap_or_default().to_string();
    let headers: Vec<(String, String)> = parsed.headers.iter().map(|h| (h.name.to_string(), String::from_utf8_lossy(h.value).into_owned())).collect();
    let mut request = Request { method, path, headers, body: Vec::new() };
    if request.header_lists("transfer-encoding", "chunked") {
        request.body = read_chunked(input, max_body)?;
    } else if let Some(length) = request.header("content-length") {
        let length: usize = length.parse().map_err(|_| ReadError::Refused(400, "bad Content-Length"))?;
        if length > max_body {
            return Err(ReadError::Refused(413, "request body too large"));
        }
        let mut body = vec![0; length];
        input.read_exact(&mut body).map_err(|_| ReadError::Refused(400, "incomplete request body"))?;
        request.body = body;
    }
    Ok(request)
}

/// A chunked request body (a reverse proxy may send one), at most `max` bytes.
fn read_chunked(input: &mut impl BufRead, max: usize) -> Result<Vec<u8>, ReadError> {
    let bad = ReadError::Refused(400, "malformed chunked body");
    let mut body = Vec::new();
    loop {
        let mut line = Vec::new();
        input.by_ref().take(64).read_until(b'\n', &mut line).map_err(|_| ReadError::Refused(400, "incomplete request body"))?;
        let text = std::str::from_utf8(&line).map_err(|_| ReadError::Refused(400, "malformed chunked body"))?;
        if !text.ends_with('\n') {
            return Err(bad);
        }
        let size = usize::from_str_radix(text.trim().split(';').next().unwrap_or_default().trim(), 16)
            .map_err(|_| ReadError::Refused(400, "malformed chunked body"))?;
        if size > max.saturating_sub(body.len()) {
            return Err(ReadError::Refused(413, "request body too large"));
        }
        if size == 0 {
            // Trailers, if any, up to the blank line.
            for _ in 0..MAX_HEADERS {
                let mut trailer = Vec::new();
                input.by_ref().take(1024).read_until(b'\n', &mut trailer).map_err(|_| ReadError::Refused(400, "incomplete request body"))?;
                if trailer == b"\r\n" || trailer == b"\n" || trailer.is_empty() {
                    return Ok(body);
                }
            }
            return Err(bad);
        }
        let start = body.len();
        body.resize(start + size, 0);
        input.read_exact(body.get_mut(start..).unwrap_or_default()).map_err(|_| ReadError::Refused(400, "incomplete request body"))?;
        let mut end = [0; 2];
        input.read_exact(&mut end).map_err(|_| ReadError::Refused(400, "incomplete request body"))?;
        if &end != b"\r\n" {
            return Err(bad);
        }
    }
}

/// A response with its whole body in memory.
#[derive(Debug, Default)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(&'static str, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn new(status: u16) -> Self {
        Response { status, headers: Vec::new(), body: Vec::new() }
    }

    /// A plain-text response; the text ends with a newline.
    pub fn text(status: u16, text: &str) -> Self {
        Response::new(status).with("Content-Type", "text/plain; charset=utf-8").body(format!("{text}\n").into_bytes())
    }

    pub fn json(status: u16, value: &serde_json::Value) -> Self {
        Response::new(status).with("Content-Type", "application/json").body(value.to_string().into_bytes())
    }

    pub fn with(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }

    pub fn body(mut self, body: Vec<u8>) -> Self {
        self.body = body;
        self
    }
}

/// What a status code is called.
pub fn reason(status: u16) -> &'static str {
    match status {
        101 => "Switching Protocols",
        200 => "OK",
        202 => "Accepted",
        204 => "No Content",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        413 => "Content Too Large",
        415 => "Unsupported Media Type",
        426 => "Upgrade Required",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        503 => "Service Unavailable",
        _ => "Error",
    }
}

/// A header value with nothing that could start another header.
fn clean(value: &str) -> String {
    value.chars().filter(|c| !c.is_control()).collect()
}

/// The status line and headers of a response, up to and including the blank line. `length`:
/// the body's length, when it is known before it is sent. Every connection carries one request:
/// the response says so, and the relay closes it after.
pub fn head(status: u16, headers: &[(&'static str, String)], length: Option<u64>) -> Vec<u8> {
    let mut out = format!("HTTP/1.1 {status} {}\r\n", reason(status));
    for (name, value) in headers {
        out.push_str(&format!("{name}: {}\r\n", clean(value)));
    }
    if let Some(length) = length {
        out.push_str(&format!("Content-Length: {length}\r\n"));
    }
    out.push_str("Connection: close\r\n\r\n");
    out.into_bytes()
}

/// Write `response`; `head_only` leaves the body out (a `HEAD` request) but keeps its length.
pub fn write_response(output: &mut impl Write, response: &Response, head_only: bool) -> std::io::Result<()> {
    output.write_all(&head(response.status, &response.headers, Some(response.body.len() as u64)))?;
    if !head_only {
        output.write_all(&response.body)?;
    }
    output.flush()
}
