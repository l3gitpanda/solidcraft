//! The tab's link to the server that serves it, for MCP (`apps/solidcraft-web-mcp`,
//! docs/web-mcp.md): a WebSocket to `mcp-link` beside the page. Each control request the server
//! sends goes into the queue `window.solidcraft` feeds (`api.rs`) and its reply goes back, so an
//! agent's MCP tools drive the design open in this tab while a person watches. The server also
//! copies files in and out of the engine's in-memory files (`vfs`) around a request: a design to
//! open comes in, a screenshot or an export goes back.
//!
//! Built only with the `mcp-link` feature. A page served by anything else has no `mcp-link`
//! beside it: the socket fails, and the tab tries again now and then without a word.
//!
//! The socket is reached through `js_sys::Reflect`, not web-sys's typed `WebSocket`: the feature
//! then adds nothing to the app's web-sys feature list.

use serde_json::{Value, json};

/// The local-storage name the link's token is kept under (per release channel).
#[cfg(target_arch = "wasm32")]
const TOKEN_KEY: &str = "solidcraft.mcp_link.token";

/// The most bytes of a file read into one reply, whatever the server asks for.
const MAX_CHUNK: usize = 8 << 20;

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64, padded.
pub fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let symbol = |n: u32| char::from(ALPHABET.get((n & 63) as usize).copied().unwrap_or(b'A'));
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
        out.push(symbol(n >> 18));
        out.push(symbol(n >> 12));
        out.push(if chunk.len() > 1 { symbol(n >> 6) } else { '=' });
        out.push(if chunk.len() > 2 { symbol(n) } else { '=' });
    }
    out
}

/// Standard base64, padded or not; `None` for anything else.
pub fn decode(text: &str) -> Option<Vec<u8>> {
    let symbols = text.trim_end_matches('=').as_bytes();
    if symbols.len() % 4 == 1 || text.len() - symbols.len() > 2 {
        return None;
    }
    let mut out = Vec::with_capacity(symbols.len() / 4 * 3 + 2);
    for chunk in symbols.chunks(4) {
        let mut n = 0u32;
        for (i, symbol) in chunk.iter().enumerate() {
            n |= (ALPHABET.iter().position(|a| a == symbol)? as u32) << (18 - 6 * i);
        }
        out.extend(n.to_be_bytes().iter().skip(1).take(chunk.len() - 1));
    }
    Some(out)
}

/// FNV-1a, 64 bits, as 16 hex digits: how the server tells a changed file from an unchanged one.
pub fn digest(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// The link's address: `mcp-link` in the folder the page is served from, over the page's own
/// kind of connection.
pub fn link_url(protocol: &str, host: &str, pathname: &str) -> String {
    let scheme = if protocol == "https:" { "wss" } else { "ws" };
    let folder = pathname.rsplit_once('/').map_or("", |(folder, _)| folder);
    format!("{scheme}://{host}{folder}/mcp-link")
}

/// The token in the page address's fragment (`#mcp-token=…`, alone or among `&`-joined parts).
pub fn token_in_fragment(fragment: &str) -> Option<String> {
    fragment.trim_start_matches('#').split('&').find_map(|part| part.strip_prefix("mcp-token=")).filter(|t| !t.is_empty()).map(str::to_string)
}

/// The engine's in-memory files, as the file messages use them.
pub trait Store {
    fn read(&self, path: &str) -> Option<Vec<u8>>;
    fn insert(&mut self, path: &str, bytes: Vec<u8>);
    fn remove(&mut self, path: &str);
    /// The files whose path starts with `prefix`, with their sizes.
    fn list(&self, prefix: &str) -> Vec<(String, u64)>;
}

/// Answer a file message, `{"fs": "write" | "read" | "list" | "remove", …}`: the control-reply
/// shape, `{"ok": true, "result": …}` or `{"ok": false, "error": …}`.
pub fn file_reply(store: &mut dyn Store, message: &Value) -> Value {
    let text = |key: &str| message.get(key).and_then(Value::as_str);
    let result = match text("fs") {
        Some("write") => (|| {
            let path = text("path").ok_or("missing `path`")?;
            let mut bytes = decode(text("data").unwrap_or_default()).ok_or("`data` is not base64")?;
            if message.get("append").and_then(Value::as_bool) == Some(true) {
                let mut whole = store.read(path).ok_or("nothing to append to")?;
                whole.append(&mut bytes);
                bytes = whole;
            }
            let size = bytes.len();
            store.insert(path, bytes);
            Ok(json!({"size": size}))
        })(),
        Some("read") => (|| {
            let path = text("path").ok_or("missing `path`")?;
            let bytes = store.read(path).ok_or("no such file in this tab")?;
            let offset = message.get("offset").and_then(Value::as_u64).unwrap_or(0).min(bytes.len() as u64) as usize;
            let wanted = message.get("len").and_then(Value::as_u64).unwrap_or(MAX_CHUNK as u64).min(MAX_CHUNK as u64) as usize;
            let chunk = bytes.get(offset..bytes.len().min(offset.saturating_add(wanted))).unwrap_or_default();
            Ok(json!({"data": encode(chunk), "size": bytes.len()}))
        })(),
        Some("list") => {
            let mut files = Vec::new();
            for root in message.get("roots").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
                // The server's paths are its own computer's: `/data/x` or `C:\data\x`. The files
                // are kept under the path as it was given, so either separator ends the root.
                let root = root.trim_end_matches(['/', '\\']);
                for (path, size) in store.list(root) {
                    if path.get(root.len()..).is_some_and(|rest| rest.starts_with(['/', '\\']))
                        && files.iter().all(|f: &Value| f["path"] != path.as_str())
                        && let Some(bytes) = store.read(&path)
                    {
                        files.push(json!({"path": path, "size": size, "digest": digest(&bytes)}));
                    }
                }
            }
            Ok(json!({"files": files}))
        }
        Some("remove") => {
            for path in message.get("paths").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
                store.remove(path);
            }
            Ok(json!({}))
        }
        _ => Err("unknown file message"),
    };
    match result {
        Ok(result) => json!({"ok": true, "result": result}),
        Err(error) => json!({"ok": false, "error": error}),
    }
}

/// What the server sent.
#[derive(Debug, PartialEq)]
pub enum Incoming {
    /// A control request for the app's queue.
    Control {
        id: u64,
        method: String,
        params: Value,
    },
    /// A file message, answered at once.
    File {
        id: u64,
        message: Value,
    },
    /// The server took the hello: the tab is linked.
    Welcome,
    /// The server refused the hello's token.
    Refused,
    Other,
}

pub fn incoming(text: &str) -> Incoming {
    let Ok(message) = serde_json::from_str::<Value>(text) else { return Incoming::Other };
    let id = message.get("id").and_then(Value::as_u64);
    match (id, message.get("method").and_then(Value::as_str)) {
        (Some(id), Some(method)) => {
            Incoming::Control { id, method: method.to_string(), params: message.get("params").cloned().unwrap_or_else(|| json!({})) }
        }
        (Some(id), None) if message.get("fs").is_some() => Incoming::File { id, message },
        _ if message.get("welcome").is_some() => Incoming::Welcome,
        _ if message.get("refused").is_some() => Incoming::Refused,
        _ => Incoming::Other,
    }
}

/// A reply with the id of the request it answers.
pub fn with_id(mut reply: Value, id: u64) -> String {
    if let Some(o) = reply.as_object_mut() {
        o.insert("id".into(), json!(id));
    }
    reply.to_string()
}

#[cfg(target_arch = "wasm32")]
pub use web::start;

#[cfg(target_arch = "wasm32")]
mod web {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    use std::sync::mpsc::{Receiver, Sender, TryRecvError};

    use js_sys::{Array, Function, Reflect};
    use serde_json::{Value, json};
    use solidcraft_control::ControlRequest;
    use solidcraft_engine::io::vfs;
    use wasm_bindgen::{JsCast as _, JsValue, closure::Closure};

    use super::{Incoming, Store, TOKEN_KEY, file_reply, incoming, link_url, token_in_fragment, with_id};

    /// The engine's in-memory files.
    struct Files;

    impl Store for Files {
        fn read(&self, path: &str) -> Option<Vec<u8>> {
            vfs::read(path, usize::MAX).ok()
        }
        fn insert(&mut self, path: &str, bytes: Vec<u8>) {
            // As a file the host loaded: not a write of a command's, so nothing is stored or
            // downloaded for it.
            vfs::insert(path, bytes);
        }
        fn remove(&mut self, path: &str) {
            let _ = vfs::remove(path);
        }
        fn list(&self, prefix: &str) -> Vec<(String, u64)> {
            vfs::list(prefix)
        }
    }

    struct Link {
        sender: Sender<ControlRequest>,
        ctx: egui::Context,
        url: String,
        token: Option<String>,
        socket: RefCell<Option<JsValue>>,
        /// Requests in the app's queue: the server's id, and where the reply comes.
        pending: RefCell<Vec<(u64, Receiver<Value>)>>,
        /// Whether a timer is set to look at `pending` again.
        polling: Cell<bool>,
        /// The server refused this tab's token: asking again would get the same answer.
        refused: Cell<bool>,
        /// How long to wait before the next try, growing while the server stays away.
        retry_ms: Cell<i32>,
        /// The handlers, made once and given to every socket.
        on_open: RefCell<Option<Function>>,
        on_message: RefCell<Option<Function>>,
        on_close: RefCell<Option<Function>>,
        on_retry: RefCell<Option<Function>>,
        on_poll: RefCell<Option<Function>>,
    }

    fn hidden() -> bool {
        web_sys::window().and_then(|w| w.document()).is_some_and(|d| d.hidden())
    }

    fn later(callback: &RefCell<Option<Function>>, ms: i32) {
        if let (Some(window), Some(callback)) = (web_sys::window(), callback.borrow().as_ref()) {
            let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(callback, ms);
        }
    }

    impl Link {
        fn send(&self, text: &str) {
            if let Some(socket) = self.socket.borrow().as_ref()
                && let Ok(send) = Reflect::get(socket, &"send".into()).and_then(|f| f.dyn_into::<Function>())
            {
                let _ = send.call1(socket, &text.into());
            }
        }

        fn connect(&self) {
            let Some(constructor) =
                web_sys::window().and_then(|w| Reflect::get(&w, &"WebSocket".into()).ok()).and_then(|c| c.dyn_into::<Function>().ok())
            else {
                return;
            };
            let Ok(socket) = Reflect::construct(&constructor, &Array::of1(&self.url.as_str().into())) else {
                later(&self.on_retry, self.retry_ms.get());
                return;
            };
            for (name, handler) in [("onopen", &self.on_open), ("onmessage", &self.on_message), ("onclose", &self.on_close)] {
                if let Some(handler) = handler.borrow().as_ref() {
                    let _ = Reflect::set(&socket, &name.into(), handler);
                }
            }
            *self.socket.borrow_mut() = Some(socket.into());
        }

        fn opened(&self) {
            self.send(&json!({"hello": {"token": self.token, "version": env!("CARGO_PKG_VERSION"), "hidden": hidden()}}).to_string());
        }

        fn closed(&self) {
            *self.socket.borrow_mut() = None;
            // The requests the server was waiting for are lost to it; their replies go nowhere.
            self.pending.borrow_mut().clear();
            if self.refused.get() {
                return;
            }
            let wait = self.retry_ms.get();
            self.retry_ms.set((wait * 2).min(10_000));
            later(&self.on_retry, wait);
        }

        fn message(&self, text: &str) {
            match incoming(text) {
                Incoming::Control { id, method, params } => {
                    let (request, reply) = ControlRequest::new(method, params);
                    if self.sender.send(request).is_err() {
                        self.send(&with_id(json!({"ok": false, "error": "SolidCraft is not running"}), id));
                        return;
                    }
                    self.pending.borrow_mut().push((id, reply));
                    self.ctx.request_repaint();
                    if !self.polling.replace(true) {
                        later(&self.on_poll, 4);
                    }
                }
                Incoming::File { id, message } => self.send(&with_id(file_reply(&mut Files, &message), id)),
                Incoming::Welcome => {
                    self.retry_ms.set(1000);
                    log::info!("MCP link: this tab is linked to {}", self.url);
                }
                Incoming::Refused => {
                    self.refused.set(true);
                    log::warn!(
                        "MCP link: the server refused this tab's token. Open the app once at <its address>#mcp-token=<the server's token> to link it."
                    );
                }
                Incoming::Other => {}
            }
        }

        /// Send the replies that have come, and look again shortly while any is awaited. The app
        /// answers between frames, so it is kept painting meanwhile.
        fn poll(&self) {
            let mut done = Vec::new();
            self.pending.borrow_mut().retain(|(id, reply)| match reply.try_recv() {
                Ok(reply) => {
                    done.push(with_id(reply, *id));
                    false
                }
                Err(TryRecvError::Empty) => true,
                Err(TryRecvError::Disconnected) => {
                    done.push(with_id(json!({"ok": false, "error": "SolidCraft request dropped"}), *id));
                    false
                }
            });
            for reply in done {
                self.send(&reply);
            }
            if self.pending.borrow().is_empty() {
                self.polling.set(false);
            } else {
                self.ctx.request_repaint();
                later(&self.on_poll, 8);
            }
        }
    }

    fn handler(run: impl Fn(JsValue) + 'static) -> Option<Function> {
        Closure::<dyn Fn(JsValue)>::new(run).into_js_value().dyn_into().ok()
    }

    /// Link this tab to the server it came from, and keep it linked. `sender` feeds the app's
    /// control queue, as `window.solidcraft` does.
    pub fn start(sender: Sender<ControlRequest>, ctx: egui::Context) {
        let Some(window) = web_sys::window() else { return };
        let location = window.location();
        let (Ok(protocol), Ok(host), Ok(pathname)) = (location.protocol(), location.host(), location.pathname()) else { return };
        // A token in the address is kept for later visits and taken out of the address.
        let storage = window.local_storage().ok().flatten();
        let key = crate::channel::key(TOKEN_KEY);
        let fresh = location.hash().ok().and_then(|fragment| token_in_fragment(&fragment));
        if let Some(token) = &fresh {
            if let Some(storage) = &storage {
                let _ = storage.set_item(&key, token);
            }
            let _ = location.set_hash("");
        }
        let token = fresh.or_else(|| storage.and_then(|s| s.get_item(&key).ok().flatten()));
        let link = Rc::new(Link {
            sender,
            ctx,
            url: link_url(&protocol, &host, &pathname),
            token,
            socket: RefCell::default(),
            pending: RefCell::default(),
            polling: Cell::new(false),
            refused: Cell::new(false),
            retry_ms: Cell::new(1000),
            on_open: RefCell::default(),
            on_message: RefCell::default(),
            on_close: RefCell::default(),
            on_retry: RefCell::default(),
            on_poll: RefCell::default(),
        });
        // The handlers keep the link for the life of the page.
        let l = link.clone();
        *link.on_open.borrow_mut() = handler(move |_| l.opened());
        let l = link.clone();
        *link.on_message.borrow_mut() = handler(move |event| {
            if let Some(text) = Reflect::get(&event, &"data".into()).ok().and_then(|d| d.as_string()) {
                l.message(&text);
            }
        });
        let l = link.clone();
        *link.on_close.borrow_mut() = handler(move |_| l.closed());
        let l = link.clone();
        *link.on_retry.borrow_mut() = handler(move |_| l.connect());
        let l = link.clone();
        *link.on_poll.borrow_mut() = handler(move |_| l.poll());
        // A hidden tab is paused by the browser: the server says so to whoever waits on it.
        let l = link.clone();
        if let (Some(document), Some(changed)) = (window.document(), handler(move |_| l.send(&json!({"hidden": hidden()}).to_string()))) {
            let _ = document.add_event_listener_with_callback("visibilitychange", &changed);
        }
        link.connect();
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[derive(Default)]
    struct Memory(BTreeMap<String, Vec<u8>>);

    impl Store for Memory {
        fn read(&self, path: &str) -> Option<Vec<u8>> {
            self.0.get(path).cloned()
        }
        fn insert(&mut self, path: &str, bytes: Vec<u8>) {
            self.0.insert(path.to_string(), bytes);
        }
        fn remove(&mut self, path: &str) {
            self.0.remove(path);
        }
        fn list(&self, prefix: &str) -> Vec<(String, u64)> {
            self.0.iter().filter(|(p, _)| p.starts_with(prefix)).map(|(p, b)| (p.clone(), b.len() as u64)).collect()
        }
    }

    #[test]
    fn base64_round_trips_and_matches_the_standard() {
        // RFC 4648, section 10.
        for (plain, coded) in
            [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("fooba", "Zm9vYmE="), ("foobar", "Zm9vYmFy")]
        {
            assert_eq!(encode(plain.as_bytes()), coded);
            assert_eq!(decode(coded).as_deref(), Some(plain.as_bytes()));
        }
        let bytes: Vec<u8> = (0..=255u8).cycle().take(1000).collect();
        assert_eq!(decode(&encode(&bytes)), Some(bytes));
        assert_eq!(decode("Zm8").as_deref(), Some(b"fo".as_slice()), "padding may be left out");
        for bad in ["Z", "Zm9v!", "Zg===", "Zm 9v"] {
            assert_eq!(decode(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_digest_is_fnv_1a() {
        assert_eq!(digest(b""), "cbf29ce484222325");
        assert_eq!(digest(b"foobar"), "85944171f73967e8");
    }

    #[test]
    fn the_link_sits_beside_the_page() {
        assert_eq!(link_url("https:", "cad.example", "/"), "wss://cad.example/mcp-link");
        assert_eq!(link_url("http:", "localhost:8080", "/index.html"), "ws://localhost:8080/mcp-link");
        assert_eq!(link_url("https:", "cad.example", "/app-rc/"), "wss://cad.example/app-rc/mcp-link");
        assert_eq!(link_url("http:", "h", ""), "ws://h/mcp-link");
    }

    #[test]
    fn the_token_is_read_from_the_fragment() {
        assert_eq!(token_in_fragment("#mcp-token=abc").as_deref(), Some("abc"));
        assert_eq!(token_in_fragment("#x=1&mcp-token=abc&y").as_deref(), Some("abc"));
        assert_eq!(token_in_fragment("#mcp-token="), None);
        assert_eq!(token_in_fragment("#other"), None);
        assert_eq!(token_in_fragment(""), None);
    }

    #[test]
    fn messages_are_told_apart() {
        assert_eq!(
            incoming(r#"{"id":3,"method":"engine.execute","params":{"command":"solid.box"}}"#),
            Incoming::Control { id: 3, method: "engine.execute".into(), params: json!({"command": "solid.box"}) }
        );
        assert_eq!(incoming(r#"{"id":4,"method":"ui.inspect"}"#), Incoming::Control { id: 4, method: "ui.inspect".into(), params: json!({}) });
        assert!(matches!(incoming(r#"{"id":5,"fs":"list","roots":[]}"#), Incoming::File { id: 5, .. }));
        assert_eq!(incoming(r#"{"welcome":{"tab":1}}"#), Incoming::Welcome);
        assert_eq!(incoming(r#"{"refused":"token"}"#), Incoming::Refused);
        for other in ["", "not json", "[]", r#"{"method":"no.id"}"#, r#"{"id":"text","method":"x"}"#, r#"{"id":1}"#] {
            assert_eq!(incoming(other), Incoming::Other, "{other}");
        }
        assert_eq!(with_id(json!({"ok": true, "result": 1}), 9), r#"{"id":9,"ok":true,"result":1}"#);
    }

    #[test]
    fn files_are_written_in_chunks_listed_read_back_and_removed() {
        let mut store = Memory::default();
        let call = |store: &mut Memory, message: Value| file_reply(store, &message);
        assert_eq!(call(&mut store, json!({"fs": "write", "path": "/data/a.bin", "data": encode(b"hello "), "append": false}))["result"]["size"], 6);
        assert_eq!(call(&mut store, json!({"fs": "write", "path": "/data/a.bin", "data": encode(b"world"), "append": true}))["result"]["size"], 11);
        store.insert("/data/sub/b.bin", vec![7; 10]);
        store.insert("/database/c.bin", vec![1]);
        store.insert("/tmp/shot.png", vec![2]);
        store.insert(r"C:\data\win\d.bin", vec![3]);
        store.insert(r"C:\database\e.bin", vec![4]);
        let listed = call(&mut store, json!({"fs": "list", "roots": ["/data", "/data/", "/tmp"]}));
        let paths: Vec<&str> = listed["result"]["files"].as_array().unwrap().iter().map(|f| f["path"].as_str().unwrap()).collect();
        assert_eq!(paths, ["/data/a.bin", "/data/sub/b.bin", "/tmp/shot.png"], "a root names a folder, never a longer name; no file twice");
        assert_eq!(listed["result"]["files"][0], json!({"path": "/data/a.bin", "size": 11, "digest": digest(b"hello world")}));
        let part = call(&mut store, json!({"fs": "read", "path": "/data/a.bin", "offset": 6, "len": 3}));
        assert_eq!(part["result"], json!({"data": encode(b"wor"), "size": 11}));
        assert_eq!(call(&mut store, json!({"fs": "read", "path": "/data/a.bin", "offset": 99}))["result"]["data"], "");
        // A server on Windows names its folders its own way.
        let windows = call(&mut store, json!({"fs": "list", "roots": [r"C:\data", r"C:\data\"]}));
        assert_eq!(windows["result"]["files"], json!([{"path": r"C:\data\win\d.bin", "size": 1, "digest": digest(&[3])}]));
        assert_eq!(call(&mut store, json!({"fs": "remove", "paths": ["/tmp/shot.png", "/nowhere"]}))["ok"], true);
        assert!(store.read("/tmp/shot.png").is_none());
        for (bad, why) in [
            (json!({"fs": "read", "path": "/nowhere"}), "no such file"),
            (json!({"fs": "write", "path": "/x", "data": "!"}), "base64"),
            (json!({"fs": "write", "path": "/new", "data": "", "append": true}), "append"),
            (json!({"fs": "write"}), "path"),
            (json!({"fs": "chmod"}), "unknown"),
        ] {
            let reply = call(&mut store, bad.clone());
            assert!(reply["ok"] == false && reply["error"].as_str().unwrap().contains(why), "{bad}: {reply}");
        }
    }
}
