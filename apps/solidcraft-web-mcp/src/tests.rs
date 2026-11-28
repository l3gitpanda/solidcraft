//! The relay end to end, over real sockets: an MCP client on one side, a stand-in for the browser
//! tab on the other. The stand-in speaks the tab's side of the link (`apps/solidcraft-web`'s
//! `mcp_link`): it answers file messages from a map in memory and control requests however the
//! test says.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::{Value, json};

use crate::backend::Waits;
use crate::sync::digest;
use crate::ws::testing::{read_server_frame, write_client_frame, write_client_text};
use crate::{Config, Relay};

const TOKEN: &str = "a-token-long-enough-for-the-relay-0123456789";

/// A relay serving a small site, with a data folder for the agent's files.
struct Fixture {
    addr: SocketAddr,
    data: PathBuf,
    _site: tempfile::TempDir,
    _data: tempfile::TempDir,
}

fn relay_with(change: impl FnOnce(&mut Config)) -> Fixture {
    let site = tempfile::tempdir().unwrap();
    std::fs::write(site.path().join("index.html"), "<html>app").unwrap();
    let data_dir = tempfile::tempdir().unwrap();
    let data = std::fs::canonicalize(data_dir.path()).unwrap();
    let mut config = Config {
        listen: "127.0.0.1:0".into(),
        site: site.path().to_path_buf(),
        token: Some(TOKEN.into()),
        open_link: false,
        origins: Vec::new(),
        url: None,
        scope: Some(solidcraft_mcp::agent_scope_with(None, [data.clone()])),
        folders: vec![data.clone()],
        options: solidcraft_mcp::Options::default(),
        waits: Waits {
            call: Duration::from_secs(10),
            settle: Duration::from_secs(10),
            file: Duration::from_secs(10),
            step: Duration::from_millis(10),
        },
    };
    change(&mut config);
    let relay = Relay::bind(config).unwrap();
    let addr = relay.local_addr().unwrap();
    relay.spawn().unwrap();
    Fixture { addr, data, _site: site, _data: data_dir }
}

fn relay() -> Fixture {
    relay_with(|_| {})
}

/// An HTTP response: its status, its head (lower case) and its body.
struct Reply {
    status: u16,
    head: String,
    body: Vec<u8>,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        let name = format!("{}:", name.to_ascii_lowercase());
        self.head.lines().find_map(|line| line.strip_prefix(name.as_str())).map(str::trim)
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|e| panic!("{e}: {}", self.text()))
    }

    /// The JSON-RPC messages of the reply: its one body, or each `data:` line of an event stream.
    fn messages(&self) -> Vec<Value> {
        if self.header("content-type").is_some_and(|t| t.starts_with("text/event-stream")) {
            self.text().lines().filter_map(|line| line.strip_prefix("data: ")).map(|data| serde_json::from_str(data).unwrap()).collect()
        } else {
            vec![self.json()]
        }
    }
}

/// Send one request (its head as given, a body if any) and read the reply to the connection's end.
fn send(addr: SocketAddr, head: &str, body: &[u8]) -> Reply {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let mut request = format!("{head}\r\nHost: {addr}\r\n").into_bytes();
    if !body.is_empty() {
        request.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
    }
    request.extend_from_slice(b"\r\n");
    request.extend_from_slice(body);
    stream.write_all(&request).unwrap();
    let mut all = Vec::new();
    stream.read_to_end(&mut all).unwrap();
    let split = all.windows(4).position(|w| w == b"\r\n\r\n").unwrap_or_else(|| panic!("no head in {:?}", String::from_utf8_lossy(&all)));
    let head = String::from_utf8_lossy(&all[..split]).to_ascii_lowercase();
    let status = head.split(' ').nth(1).and_then(|s| s.parse().ok()).unwrap();
    Reply { status, head, body: all[split + 4..].to_vec() }
}

fn get(addr: SocketAddr, path: &str) -> Reply {
    send(addr, &format!("GET {path} HTTP/1.1"), b"")
}

/// `POST /mcp` with the token; `extra`: more header lines.
fn post(addr: SocketAddr, extra: &[String], message: &Value) -> Reply {
    let mut head = format!("POST /mcp HTTP/1.1\r\nAuthorization: Bearer {TOKEN}\r\nContent-Type: application/json");
    for line in extra {
        head.push_str("\r\n");
        head.push_str(line);
    }
    send(addr, &head, message.to_string().as_bytes())
}

/// An MCP client with a session open.
struct Client {
    addr: SocketAddr,
    session: String,
    next: u64,
    /// Whether tool calls ask for an event stream.
    stream: bool,
}

impl Client {
    fn open(addr: SocketAddr) -> Client {
        let hello = json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}});
        let reply = post(addr, &[], &hello);
        assert_eq!(reply.status, 200, "{}", reply.text());
        let session = reply.header("mcp-session-id").unwrap_or_else(|| panic!("no session in {}", reply.head)).to_string();
        assert_eq!(session.len(), 64);
        assert!(reply.json()["result"]["serverInfo"].is_object(), "{}", reply.text());
        let client = Client { addr, session, next: 1, stream: false };
        assert_eq!(client.post(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).status, 202);
        client
    }

    fn post(&self, message: &Value) -> Reply {
        let mut extra = vec![format!("Mcp-Session-Id: {}", self.session)];
        if self.stream {
            extra.push("Accept: application/json, text/event-stream".into());
        }
        post(self.addr, &extra, message)
    }

    /// Call a tool; the reply's `result`.
    fn call(&mut self, tool: &str, arguments: Value) -> Value {
        let id = self.next;
        self.next += 1;
        let reply = self.post(&json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": tool, "arguments": arguments}}));
        assert_eq!(reply.status, 200, "{}", reply.text());
        assert_eq!(reply.header("content-type").is_some_and(|t| t.starts_with("text/event-stream")), self.stream, "{}", reply.head);
        let answer = reply.messages().into_iter().find(|m| m["id"] == id).unwrap_or_else(|| panic!("no answer in {}", reply.text()));
        answer["result"].clone()
    }
}

/// The text blocks of a tool result, joined.
fn text(result: &Value) -> String {
    result["content"].as_array().into_iter().flatten().filter_map(|c| c["text"].as_str()).collect::<Vec<_>>().join("\n")
}

/// A tool result's reply: its first text block, read as JSON.
fn reply(result: &Value) -> Value {
    result["content"][0]["text"].as_str().and_then(|t| serde_json::from_str(t).ok()).unwrap_or(Value::Null)
}

fn failed(result: &Value) -> bool {
    result["isError"] == true
}

/// How the stand-in tab answers a control request.
enum Answer {
    /// At once.
    Now(Value),
    /// After a while.
    Later(Duration, Value),
    /// Never.
    Silent,
}

fn ok(result: Value) -> Answer {
    Answer::Now(json!({"ok": true, "result": result}))
}

type Files = Arc<Mutex<BTreeMap<String, Vec<u8>>>>;
type Handler = Box<dyn FnMut(&str, &Value, &Files) -> Answer + Send>;

/// The stand-in tab.
struct FakeTab {
    socket: TcpStream,
    /// The files in the tab's memory.
    files: Files,
    /// The control requests it was sent: method and params.
    seen: Arc<Mutex<Vec<(String, Value)>>>,
    /// The paths it was sent files for, one entry per first chunk.
    written: Arc<Mutex<Vec<String>>>,
    /// Paths a hostile tab adds to every listing, whatever was asked for.
    also_lists: Arc<Mutex<Vec<String>>>,
    /// Paths it lists but then fails to hand over.
    unreadable: Arc<Mutex<Vec<String>>>,
}

/// Open the link's WebSocket as a page served by the relay would, and say hello.
fn link(addr: SocketAddr, token: Option<&str>, origin: Option<String>) -> Result<(TcpStream, BufReader<TcpStream>), String> {
    let stream = TcpStream::connect(addr).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let mut output = stream.try_clone().unwrap();
    let origin = origin.map(|o| format!("Origin: {o}\r\n")).unwrap_or_default();
    write!(output, "GET /mcp-link HTTP/1.1\r\nHost: {addr}\r\n{origin}Connection: keep-alive, Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n")
        .unwrap();
    let mut input = BufReader::new(stream);
    let mut status = String::new();
    input.read_line(&mut status).unwrap();
    if !status.starts_with("HTTP/1.1 101 ") {
        return Err(status.trim().to_string());
    }
    let mut accepted = false;
    loop {
        let mut line = String::new();
        input.read_line(&mut line).unwrap();
        accepted |= line.to_ascii_lowercase().starts_with("sec-websocket-accept: s3ppLMBiTxaQ9kYGzzhZRbK+xOo=".to_ascii_lowercase().as_str());
        if line == "\r\n" {
            break;
        }
    }
    assert!(accepted, "the handshake's accept key");
    write_client_text(&mut output, &json!({"hello": {"token": token, "version": "9.9.9", "hidden": false}}).to_string()).unwrap();
    Ok((output, input))
}

impl FakeTab {
    /// Link a tab that answers control requests with `handler`.
    fn link(fixture: &Fixture, mut handler: Handler) -> FakeTab {
        let (socket, mut input) = link(fixture.addr, Some(TOKEN), Some(format!("http://{}", fixture.addr))).unwrap();
        let (opcode, welcome) = read_server_frame(&mut input).unwrap();
        assert_eq!(opcode, 1);
        assert!(serde_json::from_slice::<Value>(&welcome).unwrap()["welcome"]["tab"].is_u64());
        let tab = FakeTab {
            socket,
            files: Files::default(),
            seen: Arc::default(),
            written: Arc::default(),
            also_lists: Arc::default(),
            unreadable: Arc::default(),
        };
        let (files, seen, written, also_lists, unreadable) =
            (tab.files.clone(), tab.seen.clone(), tab.written.clone(), tab.also_lists.clone(), tab.unreadable.clone());
        let output = Arc::new(Mutex::new(tab.socket.try_clone().unwrap()));
        std::thread::spawn(move || {
            let reply = |output: &Arc<Mutex<TcpStream>>, mut reply: Value, id: &Value| {
                reply["id"] = id.clone();
                let _ = write_client_text(&mut *output.lock().unwrap(), &reply.to_string());
            };
            while let Ok((opcode, payload)) = read_server_frame(&mut input) {
                match opcode {
                    // A ping: the pong carries its payload.
                    9 => {
                        let _ = write_client_frame(&mut *output.lock().unwrap(), true, 10, &payload);
                    }
                    8 => break,
                    1 => {
                        let message: Value = serde_json::from_slice(&payload).unwrap();
                        let id = message["id"].clone();
                        if message.get("fs").is_some() {
                            let answer = file_reply(&files, &written, &also_lists, &unreadable, &message);
                            reply(&output, answer, &id);
                        } else if let Some(method) = message["method"].as_str() {
                            seen.lock().unwrap().push((method.to_string(), message["params"].clone()));
                            match handler(method, &message["params"], &files) {
                                Answer::Now(answer) => reply(&output, answer, &id),
                                Answer::Later(wait, answer) => {
                                    let output = output.clone();
                                    std::thread::spawn(move || {
                                        std::thread::sleep(wait);
                                        reply(&output, answer, &id);
                                    });
                                }
                                Answer::Silent => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
        });
        tab
    }

    /// A tab that answers every request with an empty result.
    fn plain(fixture: &Fixture) -> FakeTab {
        FakeTab::link(fixture, Box::new(|_, _, _| ok(json!({}))))
    }

    /// The commands it was asked to run (`engine.execute`), by id.
    fn commands(&self) -> Vec<String> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(method, _)| method == "engine.execute")
            .filter_map(|(_, p)| p["command"].as_str().map(str::to_string))
            .collect()
    }

    /// Close the tab, as a person closing or reloading it does.
    fn close(&self) {
        let _ = self.socket.shutdown(Shutdown::Both);
    }
}

impl Drop for FakeTab {
    fn drop(&mut self) {
        self.close();
    }
}

/// The tab's file messages, as `mcp_link::file_reply` answers them.
fn file_reply(
    files: &Files,
    written: &Mutex<Vec<String>>,
    also_lists: &Mutex<Vec<String>>,
    unreadable: &Mutex<Vec<String>>,
    message: &Value,
) -> Value {
    let b64 = base64::engine::general_purpose::STANDARD;
    let mut files = files.lock().unwrap();
    let result = match message["fs"].as_str() {
        Some("write") => {
            let path = message["path"].as_str().unwrap().to_string();
            let bytes = b64.decode(message["data"].as_str().unwrap()).unwrap();
            if message["append"] == true {
                files.get_mut(&path).unwrap().extend_from_slice(&bytes);
            } else {
                written.lock().unwrap().push(path.clone());
                files.insert(path.clone(), bytes);
            }
            json!({"size": files[&path].len()})
        }
        Some("read") => {
            let path = message["path"].as_str().unwrap();
            let Some(bytes) = files.get(path).filter(|_| !unreadable.lock().unwrap().iter().any(|u| u == path)) else {
                return json!({"ok": false, "error": "no such file in this tab"});
            };
            let offset = (message["offset"].as_u64().unwrap_or(0) as usize).min(bytes.len());
            let end = bytes.len().min(offset + message["len"].as_u64().unwrap_or(u64::MAX).min(1 << 30) as usize);
            json!({"data": b64.encode(&bytes[offset..end]), "size": bytes.len()})
        }
        Some("list") => {
            // As the real tab does: a root's files lie beyond either separator (Windows spells paths with `\`).
            let roots: Vec<&str> = message["roots"].as_array().unwrap().iter().map(|r| r.as_str().unwrap().trim_end_matches(['/', '\\'])).collect();
            let under = |path: &str, root: &str| path.strip_prefix(root).is_some_and(|rest| rest.starts_with(['/', '\\']));
            let extra = also_lists.lock().unwrap();
            let listed: Vec<Value> = files
                .iter()
                .filter(|(path, _)| roots.iter().any(|root| under(path, root)) || extra.contains(path))
                .map(|(path, bytes)| json!({"path": path, "size": bytes.len(), "digest": digest(bytes)}))
                .collect();
            json!({"files": listed})
        }
        Some("remove") => {
            for path in message["paths"].as_array().unwrap() {
                files.remove(path.as_str().unwrap());
            }
            json!({})
        }
        other => panic!("unknown file message {other:?}"),
    };
    json!({"ok": true, "result": result})
}

/// Wait until `done` says so; fail after ten seconds.
fn until(what: &str, mut done: impl FnMut() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < Duration::from_secs(10), "waited ten seconds for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn status(addr: SocketAddr) -> Value {
    get(addr, "/mcp-link/status").json()
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[test]
fn the_app_its_health_and_the_link_status_are_served() {
    let fixture = relay();
    let index = get(fixture.addr, "/");
    assert_eq!((index.status, index.text().as_str()), (200, "<html>app"));
    assert_eq!(index.header("cache-control"), Some("no-cache"));
    assert_eq!(get(fixture.addr, "/missing.js").status, 404);
    assert_eq!(get(fixture.addr, "/healthz").text(), "ok\n");
    assert_eq!(send(fixture.addr, "POST / HTTP/1.1", b"x").status, 405);
    assert_eq!(status(fixture.addr), json!({"tabs": 0, "active": null, "running": 0, "sessions": 0}));
    let tab = FakeTab::plain(&fixture);
    let linked = status(fixture.addr);
    assert_eq!((&linked["tabs"], &linked["active"]["version"], &linked["active"]["hidden"]), (&json!(1), &json!("9.9.9"), &json!(false)));
    // The tab says when its page is hidden, and when it goes.
    write_client_text(&mut tab.socket.try_clone().unwrap(), r#"{"hidden":true}"#).unwrap();
    until("the tab to be marked hidden", || status(fixture.addr)["active"]["hidden"] == true);
    tab.close();
    until("the tab to be dropped", || status(fixture.addr)["tabs"] == 0);
}

#[test]
fn mcp_wants_the_token_and_no_other_site() {
    let fixture = relay();
    let ping = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}).to_string();
    let none = send(fixture.addr, "POST /mcp HTTP/1.1\r\nContent-Type: application/json", ping.as_bytes());
    assert_eq!((none.status, none.header("www-authenticate")), (401, Some("bearer")));
    let wrong = send(fixture.addr, "POST /mcp HTTP/1.1\r\nAuthorization: Bearer not-the-token-not-the-token-not-the-token", ping.as_bytes());
    assert_eq!(wrong.status, 401);
    // A page on another site is refused even with the token; the relay's own origin is not.
    let foreign =
        post(fixture.addr, &["Origin: https://elsewhere.example".into()], &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}));
    assert_eq!(foreign.status, 403);
    let own =
        post(fixture.addr, &[format!("Origin: http://{}", fixture.addr)], &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}));
    assert_eq!(own.status, 200);
    assert_eq!(send(fixture.addr, "GET /mcp HTTP/1.1", b"").status, 401, "the token comes before anything else");
    let got = send(fixture.addr, &format!("GET /mcp HTTP/1.1\r\nAuthorization: Bearer {TOKEN}"), b"");
    assert_eq!((got.status, got.header("allow")), (405, Some("post, delete")));
}

#[test]
fn only_the_app_served_here_links_and_only_with_the_token() {
    let fixture = relay();
    assert_eq!(get(fixture.addr, "/mcp-link").status, 426, "not a WebSocket");
    assert!(link(fixture.addr, Some(TOKEN), None).unwrap_err().contains(" 403 "), "no Origin: not a page");
    assert!(link(fixture.addr, Some(TOKEN), Some("https://elsewhere.example".into())).unwrap_err().contains(" 403 "));
    for token in [None, Some("not-the-token-not-the-token-not-the-token")] {
        let (_socket, mut input) = link(fixture.addr, token, Some(format!("http://{}", fixture.addr))).unwrap();
        let (opcode, refused) = read_server_frame(&mut input).unwrap();
        assert_eq!((opcode, serde_json::from_slice::<Value>(&refused).unwrap()), (1, json!({"refused": "token"})));
        let (opcode, close) = read_server_frame(&mut input).unwrap();
        assert_eq!((opcode, close), (8, 1008u16.to_be_bytes().to_vec()));
    }
    assert_eq!(status(fixture.addr)["tabs"], 0);

    // An origin the relay is told of (a reverse proxy's name) links; so does any tab when the
    // proxy signs people in and the link is left open.
    let proxied = relay_with(|config| {
        config.origins = vec!["https://cad.example".into()];
        config.open_link = true;
    });
    let (_socket, mut input) = link(proxied.addr, None, Some("https://cad.example".into())).unwrap();
    assert!(serde_json::from_slice::<Value>(&read_server_frame(&mut input).unwrap().1).unwrap()["welcome"].is_object());
}

#[test]
fn a_relay_open_to_a_network_must_have_a_token() {
    let site = tempfile::tempdir().unwrap();
    std::fs::write(site.path().join("index.html"), "").unwrap();
    let config = |listen: &str, token: Option<&str>| Config {
        listen: listen.into(),
        site: site.path().to_path_buf(),
        token: token.map(str::to_string),
        open_link: false,
        origins: Vec::new(),
        url: None,
        scope: None,
        folders: Vec::new(),
        options: solidcraft_mcp::Options::default(),
        waits: Waits::default(),
    };
    assert!(config("0.0.0.0:8080", None).check().unwrap_err().contains("SOLIDCRAFT_WEB_MCP_TOKEN"));
    assert!(config("0.0.0.0:8080", Some("short")).check().unwrap_err().contains("at least"));
    assert!(config("0.0.0.0:8080", Some(TOKEN)).check().is_ok());
    assert!(config("127.0.0.1:8080", None).check().is_ok());
    assert!(config("[::1]:8080", None).check().is_ok());
    assert!(Relay::bind(config("127.0.0.1:0", None)).is_ok());
    let empty = tempfile::tempdir().unwrap();
    let mut unbuilt = config("127.0.0.1:0", None);
    unbuilt.site = empty.path().to_path_buf();
    assert!(Relay::bind(unbuilt).err().unwrap().contains("index.html"));

    // Without a token the relay answers only under this computer's own names (DNS rebinding).
    let open = relay_with(|config| config.token = None);
    let hello = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}).to_string();
    let rebound = {
        let mut stream = TcpStream::connect(open.addr).unwrap();
        write!(stream, "POST /mcp HTTP/1.1\r\nHost: evil.example\r\nContent-Length: {}\r\n\r\n{hello}", hello.len()).unwrap();
        let mut all = String::new();
        stream.read_to_string(&mut all).unwrap();
        all
    };
    assert!(rebound.starts_with("HTTP/1.1 403 "), "{rebound}");
    assert_eq!(send(open.addr, "POST /mcp HTTP/1.1", hello.as_bytes()).status, 200);
}

#[test]
fn a_session_opens_lists_the_tools_and_closes() {
    let fixture = relay();
    let client = Client::open(fixture.addr);
    assert_eq!(status(fixture.addr)["sessions"], 1);
    let tools = client.post(&json!({"jsonrpc": "2.0", "id": 5, "method": "tools/list"})).json();
    let names: Vec<&str> = tools["result"]["tools"].as_array().unwrap().iter().filter_map(|t| t["name"].as_str()).collect();
    for tool in ["execute", "screenshot", "inspect_design", "pending"] {
        assert!(names.contains(&tool), "{tool} in {names:?}");
    }
    // A request must say which session it belongs to, and the session must be open.
    let list = json!({"jsonrpc": "2.0", "id": 6, "method": "tools/list"});
    assert_eq!(post(fixture.addr, &[], &list).status, 400);
    assert_eq!(post(fixture.addr, &["Mcp-Session-Id: 0000".into()], &list).status, 404);
    assert_eq!(
        send(fixture.addr, &format!("POST /mcp HTTP/1.1\r\nAuthorization: Bearer {TOKEN}\r\nMcp-Session-Id: {}", client.session), b"not json").status,
        400
    );
    assert_eq!(
        send(
            fixture.addr,
            &format!("POST /mcp HTTP/1.1\r\nAuthorization: Bearer {TOKEN}\r\nContent-Type: text/plain\r\nMcp-Session-Id: {}", client.session),
            b"{}"
        )
        .status,
        415
    );
    let delete = format!("DELETE /mcp HTTP/1.1\r\nAuthorization: Bearer {TOKEN}\r\nMcp-Session-Id: {}", client.session);
    assert_eq!(send(fixture.addr, &delete, b"").status, 204);
    assert_eq!(send(fixture.addr, &delete, b"").status, 404);
    assert_eq!(client.post(&list).status, 404, "a closed session is gone");
    assert_eq!(status(fixture.addr)["sessions"], 0);
}

#[test]
fn a_tool_call_runs_in_the_tab_and_its_result_comes_back() {
    let fixture = relay();
    let tab = FakeTab::link(
        &fixture,
        Box::new(|method, params, _| match (method, params["command"].as_str()) {
            ("engine.execute", Some("solid.box")) => ok(json!({"created": "Body 1", "size": params["params"]["size"]})),
            ("engine.execute", Some(other)) => Answer::Now(json!({"ok": false, "error": format!("unknown command `{other}`")})),
            _ => ok(json!({})),
        }),
    );
    let mut client = Client::open(fixture.addr);
    for stream in [false, true] {
        client.stream = stream;
        let made = client.call("execute", json!({"command": "solid.box", "params": {"size": [30, 20, 10]}}));
        assert!(!failed(&made), "{made}");
        assert!(reply(&made) == json!({"created": "Body 1", "size": [30, 20, 10]}), "{made}");
    }
    assert_eq!(tab.commands(), ["solid.box", "solid.box"]);
    // The first call to reach a tab takes it off the Home page, once; then the call itself.
    let seen = tab.seen.lock().unwrap().clone();
    assert_eq!(seen[0], ("ui.home".to_string(), json!({"open": false})), "{seen:?}");
    assert_eq!(seen[1].1, json!({"command": "solid.box", "params": {"size": [30, 20, 10]}}));
    assert_eq!(seen.iter().filter(|(method, _)| method == "ui.home").count(), 1, "{seen:?}");
    // The tab's own refusal is the tool's error.
    let refused = client.call("execute", json!({"command": "no.such"}));
    assert!(failed(&refused) && text(&refused).contains("unknown command `no.such`"), "{refused}");
    // What the relay answers itself never reaches the tab.
    let before = tab.seen.lock().unwrap().len();
    let running = client.call("running", json!({}));
    assert!(!failed(&running), "{running}");
    assert_eq!(tab.seen.lock().unwrap().len(), before, "{:?}", tab.seen.lock().unwrap());
}

#[test]
fn without_a_tab_a_call_says_where_to_open_the_app() {
    let fixture = relay_with(|config| config.url = Some("https://cad.example/".into()));
    let mut client = Client::open(fixture.addr);
    let result = client.call("execute", json!({"command": "solid.box"}));
    assert!(failed(&result), "{result}");
    let said = text(&result);
    assert!(said.contains("no browser tab is linked") && said.contains("Open https://cad.example/"), "{said}");
    assert!(said.starts_with("no SolidCraft app is listening at"), "what `batch` reads as never sent: {said}");
    // Once a tab links, the same session's calls reach it.
    let tab = FakeTab::plain(&fixture);
    assert!(!failed(&client.call("execute", json!({"command": "solid.box"}))));
    assert_eq!(tab.commands(), ["solid.box"]);
}

#[test]
fn the_tab_linked_last_is_the_one_driven() {
    let fixture = relay();
    let first = FakeTab::plain(&fixture);
    let second = FakeTab::plain(&fixture);
    let mut client = Client::open(fixture.addr);
    client.call("execute", json!({"command": "solid.box"}));
    assert_eq!((first.commands().len(), second.commands().len()), (0, 1));
    second.close();
    until("the second tab to be dropped", || status(fixture.addr)["tabs"] == 1);
    client.call("execute", json!({"command": "solid.box"}));
    assert_eq!((first.commands().len(), second.commands().len()), (1, 1));
}

#[test]
fn a_call_keeps_to_the_tab_it_began_in_when_another_links() {
    let fixture = relay();
    let first = FakeTab::link(
        &fixture,
        Box::new(|_, params, files| match params["command"].as_str() {
            Some("file.export") => {
                files.lock().unwrap().insert(params["params"]["path"].as_str().unwrap().to_string(), b"from the first tab".to_vec());
                Answer::Later(Duration::from_millis(400), json!({"ok": true, "result": {"exported": true}}))
            }
            _ => ok(json!({})),
        }),
    );
    let mut client = Client::open(fixture.addr);
    let out = path_text(&fixture.data.join("part.step"));
    let (result, second) = std::thread::scope(|scope| {
        let call = scope.spawn(|| client.call("execute", json!({"command": "file.export", "params": {"path": out}})));
        until("the first tab to get the request", || !first.commands().is_empty());
        // A second window opens while the export runs: it is the one driven from now on.
        let second = FakeTab::plain(&fixture);
        second.files.lock().unwrap().insert(out.clone(), b"an older file in the second tab".to_vec());
        (call.join().unwrap(), second)
    });
    assert!(!failed(&result), "{result}");
    assert_eq!(std::fs::read(&out).unwrap(), b"from the first tab", "the export comes back from the tab that made it");
    // The next call goes to the second tab.
    client.call("execute", json!({"command": "solid.box"}));
    assert_eq!(second.commands(), ["solid.box"]);
}

/// A tab whose `file.save` keeps `params.text` at `params.path`, as a save of the design would.
fn saving_tab(fixture: &Fixture) -> FakeTab {
    FakeTab::link(
        fixture,
        Box::new(|_, params, files| {
            if params["command"] == "file.save" {
                files
                    .lock()
                    .unwrap()
                    .insert(params["params"]["path"].as_str().unwrap().to_string(), params["params"]["text"].as_str().unwrap().as_bytes().to_vec());
            }
            ok(json!({}))
        }),
    )
}

#[test]
fn a_tab_returned_to_does_not_bring_back_its_old_files() {
    let fixture = relay();
    let design = path_text(&fixture.data.join("part.sc"));
    let save = |text: &str| json!({"command": "file.save", "params": {"path": design, "text": text}});
    let mut client = Client::open(fixture.addr);
    let first = saving_tab(&fixture);
    client.call("execute", save("saved in the first tab"));
    assert_eq!(std::fs::read_to_string(&design).unwrap(), "saved in the first tab");
    // A second window saves a newer design to the same path, then closes.
    let second = saving_tab(&fixture);
    client.call("execute", save("saved later in the second tab"));
    assert_eq!(std::fs::read_to_string(&design).unwrap(), "saved later in the second tab");
    second.close();
    until("the second tab to be dropped", || status(fixture.addr)["tabs"] == 1);
    // Back in the first tab, which still holds its older copy: a call that saves nothing must
    // not put that copy back over the newer file.
    client.call("execute", json!({"command": "solid.box"}));
    assert_eq!(first.commands(), ["file.save", "solid.box"]);
    assert_eq!(std::fs::read_to_string(&design).unwrap(), "saved later in the second tab");
    // And when the first tab does save again, that is new and lands.
    client.call("execute", save("saved again in the first tab"));
    assert_eq!(std::fs::read_to_string(&design).unwrap(), "saved again in the first tab");
}

#[test]
fn a_late_request_whose_files_cannot_be_copied_is_reported_as_failed() {
    let fixture = relay_with(|config| config.waits.call = Duration::from_millis(150));
    let out = path_text(&fixture.data.join("part.step"));
    let tab = FakeTab::link(
        &fixture,
        Box::new(|_, params, files| match params["command"].as_str() {
            Some("file.export") => {
                files.lock().unwrap().insert(params["params"]["path"].as_str().unwrap().to_string(), b"exported".to_vec());
                Answer::Later(Duration::from_millis(700), json!({"ok": true, "result": {"exported": true}}))
            }
            _ => ok(json!({})),
        }),
    );
    // The tab lists the export but then fails to hand it over.
    tab.unreadable.lock().unwrap().push(out.clone());
    let mut client = Client::open(fixture.addr);
    let slow = client.call("execute", json!({"command": "file.export", "params": {"path": out}}));
    assert!(text(&slow).starts_with("still running:"), "{slow}");
    // Another call waits the export out; its files are collected then, and that fails.
    assert!(!failed(&client.call("execute", json!({"command": "solid.box"}))));
    assert_eq!(tab.commands(), ["file.export", "solid.box"]);
    let pending = client.call("pending", json!({}));
    assert!(failed(&pending) && text(&pending).contains("could not be copied from the browser tab"), "{pending}");
    assert!(!Path::new(&out).exists());
    // The failure was the export's, told once: later calls are not failed for it. Asked for
    // again, the export is tried again, and lands once the tab hands it over.
    assert!(!failed(&client.call("execute", json!({"command": "solid.box"}))));
    tab.unreadable.lock().unwrap().clear();
    client.call("execute", json!({"command": "file.export", "params": {"path": out}}));
    let retried = client.call("pending", json!({}));
    assert!(!failed(&retried), "{retried}");
    assert_eq!(std::fs::read(&out).unwrap(), b"exported");
}

#[test]
fn a_tab_that_goes_mid_call_takes_the_call_with_it() {
    let fixture = relay();
    let tab = FakeTab::link(&fixture, Box::new(|_, _, _| Answer::Silent));
    let mut client = Client::open(fixture.addr);
    let result = std::thread::scope(|scope| {
        let call = scope.spawn(|| client.call("execute", json!({"command": "solid.box"})));
        until("the tab to get the request", || !tab.commands().is_empty());
        tab.close();
        call.join().unwrap()
    });
    assert!(failed(&result) && text(&result).contains("closed or reloaded while"), "{result}");
    assert_eq!(status(fixture.addr)["running"], 0);
}

#[test]
fn a_call_that_outlasts_its_wait_is_still_running_and_pending_gets_its_result() {
    let fixture = relay_with(|config| config.waits.call = Duration::from_millis(150));
    let tab = FakeTab::link(
        &fixture,
        Box::new(|_, params, _| match params["command"].as_str() {
            Some("solid.slow") => Answer::Later(Duration::from_millis(900), json!({"ok": true, "result": {"slow": "done"}})),
            _ => ok(json!({})),
        }),
    );
    let mut client = Client::open(fixture.addr);
    let slow = client.call("execute", json!({"command": "solid.slow"}));
    assert!(failed(&slow) && text(&slow).starts_with("still running:"), "{slow}");
    assert!(text(&slow).contains("`pending`"), "{slow}");
    let running = client.call("running", json!({}));
    assert!(text(&running).contains("solid.slow"), "{running}");
    let pending = client.call("pending", json!({}));
    assert!(!failed(&pending) && reply(&pending)["result"] == json!({"slow": "done"}), "{pending}");
    assert!(text(&client.call("pending", json!({}))).contains("nothing is running"));

    // A new call waits for the earlier one first, and `pending` still has its result after.
    client.call("execute", json!({"command": "solid.slow"}));
    let next = client.call("execute", json!({"command": "solid.box"}));
    assert!(!failed(&next), "{next}");
    assert_eq!(tab.commands(), ["solid.slow", "solid.slow", "solid.box"]);
    let earlier = client.call("pending", json!({}));
    assert!(reply(&earlier)["result"] == json!({"slow": "done"}), "{earlier}");
}

#[test]
fn a_cancelled_call_stops_waiting_for_the_tab() {
    let fixture = relay();
    let tab = FakeTab::link(&fixture, Box::new(|_, _, _| Answer::Silent));
    let mut client = Client::open(fixture.addr);
    client.stream = true;
    let id = client.next;
    let cancel = json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": id, "reason": "test"}});
    let (addr, session) = (client.addr, client.session.clone());
    let result = std::thread::scope(|scope| {
        let call = scope.spawn(|| client.call("execute", json!({"command": "solid.box"})));
        until("the tab to get the request", || !tab.commands().is_empty());
        assert_eq!(post(addr, &[format!("Mcp-Session-Id: {session}")], &cancel).status, 202);
        call.join().unwrap()
    });
    assert!(failed(&result) && text(&result).starts_with("cancelled:"), "{result}");
}

#[test]
fn a_screenshot_comes_back_from_the_tab_and_the_tabs_copy_is_dropped() {
    let fixture = relay();
    let png = b"\x89PNG\r\n\x1a\n a picture".to_vec();
    let picture = png.clone();
    let tab = FakeTab::link(
        &fixture,
        Box::new(move |method, params, files| match method {
            "ui.screenshot" => {
                files.lock().unwrap().insert(params["path"].as_str().unwrap().to_string(), picture.clone());
                ok(json!({"width": 640, "height": 400}))
            }
            _ => ok(json!({})),
        }),
    );
    let mut client = Client::open(fixture.addr);
    let shot = client.call("screenshot", json!({}));
    assert!(!failed(&shot), "{shot}");
    assert_eq!(shot["content"][0]["type"], "image");
    assert_eq!(shot["content"][0]["data"], base64::engine::general_purpose::STANDARD.encode(&png));
    assert!(text(&shot).contains(r#""width":640"#), "{shot}");
    assert!(tab.files.lock().unwrap().is_empty(), "the scratch picture stays in neither place: {:?}", tab.files.lock().unwrap().keys());

    // A picture kept under a name lands in the agent's folder, and stays in the tab too.
    let kept = path_text(&fixture.data.join("shots").join("front.png"));
    let shot = client.call("screenshot", json!({"path": kept}));
    assert!(!failed(&shot), "{shot}");
    assert_eq!(std::fs::read(&kept).unwrap(), png);
    assert!(tab.files.lock().unwrap().contains_key(&kept));
}

#[test]
fn files_a_call_names_go_to_the_tab_and_what_the_tab_writes_comes_back() {
    let fixture = relay();
    let outside = tempfile::tempdir().unwrap();
    let stray = path_text(&outside.path().join("stray.txt"));
    let escape = stray.clone();
    let tab = FakeTab::link(
        &fixture,
        Box::new(move |_, params, files| {
            let path = params["params"]["path"].as_str().unwrap_or_default().to_string();
            let mut files = files.lock().unwrap();
            match params["command"].as_str() {
                // The design the call names is in the tab's memory by the time the call runs.
                Some("file.open") => match files.get(&path) {
                    Some(bytes) => ok(json!({"opened": String::from_utf8_lossy(bytes)})),
                    None => Answer::Now(json!({"ok": false, "error": format!("{path}: no such file")})),
                },
                Some("file.export") => {
                    files.insert(path, vec![7; 100_000]);
                    files.insert(escape.clone(), b"not the agent's".to_vec());
                    ok(json!({"exported": true}))
                }
                _ => ok(json!({})),
            }
        }),
    );
    let mut client = Client::open(fixture.addr);
    let design = path_text(&fixture.data.join("part.sc"));
    std::fs::write(&design, "a design").unwrap();
    let opened = client.call("execute", json!({"command": "file.open", "params": {"path": design}}));
    assert!(!failed(&opened) && text(&opened).contains("a design"), "{opened}");
    // Unchanged, it is not sent again; changed, it is.
    client.call("execute", json!({"command": "file.open", "params": {"path": design}}));
    assert_eq!(tab.written.lock().unwrap().as_slice(), [design.as_str()]);
    std::fs::write(&design, "a design, changed since").unwrap();
    let reopened = client.call("execute", json!({"command": "file.open", "params": {"path": design}}));
    assert!(text(&reopened).contains("changed since"), "{reopened}");
    assert_eq!(tab.written.lock().unwrap().len(), 2);

    // What the tab writes under the agent's folder lands on disk, in a new folder if need be.
    // A tab that lists a file elsewhere gets nothing written there.
    tab.also_lists.lock().unwrap().push(stray.clone());
    let out = path_text(&fixture.data.join("out").join("part.step"));
    let exported = client.call("execute", json!({"command": "file.export", "params": {"path": out}}));
    assert!(!failed(&exported), "{exported}");
    assert_eq!(std::fs::read(&out).unwrap(), vec![7; 100_000]);
    // Written over an existing file, the new bytes replace it whole and nothing else is left beside it.
    std::fs::write(&out, "an earlier export").unwrap();
    client.call("execute", json!({"command": "file.export", "params": {"path": out}}));
    assert_eq!(std::fs::read(&out).unwrap(), vec![7; 100_000]);
    let beside: Vec<String> =
        std::fs::read_dir(fixture.data.join("out")).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(beside, ["part.step"]);
    assert!(!Path::new(&stray).exists(), "outside the agent's folders");
    // A path outside them is not sent to the tab either.
    std::fs::write(&stray, "private").unwrap();
    client.call("execute", json!({"command": "file.open", "params": {"path": stray}}));
    assert!(!tab.written.lock().unwrap().contains(&stray));
    assert_eq!(std::fs::read_to_string(&stray).unwrap(), "private");
}

/// The next text message the relay sends a link, as JSON.
fn next_message(input: &mut BufReader<TcpStream>) -> Value {
    loop {
        let (opcode, payload) = read_server_frame(input).unwrap();
        if opcode == 1 {
            return serde_json::from_slice(&payload).unwrap();
        }
    }
}

#[test]
fn home_lists_the_agents_designs_opens_them_and_saves_them_back() {
    let fixture = relay();
    let b64 = base64::engine::general_purpose::STANDARD;
    let part = path_text(&fixture.data.join("part.sc"));
    std::fs::write(&part, "a design").unwrap();
    std::fs::create_dir_all(fixture.data.join("sub")).unwrap();
    let older = path_text(&fixture.data.join("sub").join("older.sc"));
    std::fs::write(&older, "older").unwrap();
    std::fs::write(fixture.data.join("part.step"), "an export").unwrap();
    let (mut output, mut input) = link(fixture.addr, Some(TOKEN), Some(format!("http://{}", fixture.addr))).unwrap();

    // The welcome names the agent's folder and its designs, and nothing else there.
    let welcome = next_message(&mut input)["welcome"].clone();
    assert_eq!(welcome["folders"], json!([path_text(&fixture.data)]));
    let mut listed: Vec<&str> = welcome["designs"].as_array().unwrap().iter().map(|d| d["path"].as_str().unwrap()).collect();
    listed.sort_unstable();
    assert_eq!(listed, [part.as_str(), older.as_str()]);

    // Opened from Home, a design comes whole, then the word that it has.
    write_client_text(&mut output, &json!({"fetch": {"path": part}}).to_string()).unwrap();
    let piece = next_message(&mut input);
    assert_eq!(piece["file"]["path"], part.as_str());
    assert_eq!(piece["file"]["append"], false);
    assert_eq!(b64.decode(piece["file"]["data"].as_str().unwrap()).unwrap(), b"a design");
    assert_eq!(next_message(&mut input), json!({"fetched": {"path": part}}));

    // Nothing outside the agent's folder, and nothing that is not a design, is sent.
    let outside = tempfile::tempdir().unwrap();
    let private = path_text(&outside.path().join("private.sc"));
    std::fs::write(&private, "private").unwrap();
    let export = path_text(&fixture.data.join("part.step"));
    for (path, why) in [(&private, ""), (&export, "not a design")] {
        write_client_text(&mut output, &json!({"fetch": {"path": path}}).to_string()).unwrap();
        let answer = next_message(&mut input);
        assert_eq!(answer["fetched"]["path"], path.as_str(), "{answer}");
        assert!(answer["fetched"]["error"].as_str().is_some_and(|e| e.contains(why)), "{answer}");
    }

    // A design saved in the tab is written to disk, in a new folder if need be, once whole.
    let saved = path_text(&fixture.data.join("new").join("saved.sc"));
    let put = |number: u64, path: &str, data: &[u8], append: bool, last: bool| {
        json!({"put": {"ref": number, "path": path, "data": b64.encode(data), "append": append, "last": last}}).to_string()
    };
    write_client_text(&mut output, &put(1, &saved, b"hel", false, false)).unwrap();
    assert!(!Path::new(&saved).exists(), "not before the last piece");
    write_client_text(&mut output, &put(1, &saved, b"lo", true, true)).unwrap();
    assert_eq!(next_message(&mut input), json!({"saved": {"ref": 1}}));
    assert_eq!(std::fs::read_to_string(&saved).unwrap(), "hello");

    // Outside the folder, or not a design, it is refused and nothing is written.
    let stray = path_text(&outside.path().join("stray.sc"));
    let text = path_text(&fixture.data.join("notes.txt"));
    for (number, path) in [(2, &stray), (3, &text)] {
        write_client_text(&mut output, &put(number, path, b"x", false, true)).unwrap();
        let answer = next_message(&mut input);
        assert_eq!(answer["saved"]["ref"], number);
        assert!(answer["saved"]["error"].is_string(), "{answer}");
        assert!(!Path::new(path).exists());
    }
    // A piece without its start is refused, not appended to anything.
    write_client_text(&mut output, &put(4, &saved, b"tail", true, true)).unwrap();
    assert!(next_message(&mut input)["saved"]["error"].is_string());
    assert_eq!(std::fs::read_to_string(&saved).unwrap(), "hello");
}

#[test]
fn a_design_saved_from_home_is_not_written_again_by_the_next_call() {
    let fixture = relay();
    let tab = FakeTab::plain(&fixture);
    let design = path_text(&fixture.data.join("part.sc"));
    let mut output = tab.socket.try_clone().unwrap();
    let data = base64::engine::general_purpose::STANDARD.encode(b"saved in the tab");
    write_client_text(&mut output, &json!({"put": {"ref": 1, "path": design, "data": data, "append": false, "last": true}}).to_string()).unwrap();
    until("the save to land", || std::fs::read_to_string(&design).is_ok_and(|t| t == "saved in the tab"));
    // The tab keeps its copy; the file then changes on disk.
    tab.files.lock().unwrap().insert(design.clone(), b"saved in the tab".to_vec());
    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(&design, "changed on disk since").unwrap();
    // A call that saves nothing must not put the tab's copy back over the newer file.
    let mut client = Client::open(fixture.addr);
    client.call("execute", json!({"command": "solid.box"}));
    assert_eq!(tab.commands(), ["solid.box"]);
    assert_eq!(std::fs::read_to_string(&design).unwrap(), "changed on disk since");
}
