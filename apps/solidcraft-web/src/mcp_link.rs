//! The tab's link to the server that serves it, for MCP (`apps/solidcraft-web-mcp`,
//! docs/web-mcp.md): a WebSocket to `mcp-link` beside the page. Each control request the server
//! sends goes into the queue `window.solidcraft` feeds (`api.rs`) and its reply goes back, so an
//! agent's MCP tools drive the design open in this tab while a person watches. The server also
//! copies files in and out of the engine's in-memory files (`vfs`) around a request: a design to
//! open comes in, a screenshot or an export goes back.
//!
//! The server's welcome also lists the designs in its folders, which the Home page shows under
//! Recent designs ([`merge`]): opening one asks the server for its file, and saving a design that
//! lives there writes it back to the server's disk (docs/web-mcp.md, "Recent designs").
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

/// The local-storage name the server's folders are kept under, so its designs stay on the Home
/// page between visits, before the link is back (per release channel).
#[cfg(target_arch = "wasm32")]
const FOLDERS_KEY: &str = "solidcraft.mcp_link.folders";

/// The most bytes of a file read into one reply, whatever the server asks for.
const MAX_CHUNK: usize = 8 << 20;
/// The bytes of a design sent to the server in one message.
#[cfg(target_arch = "wasm32")]
const UPLOAD_CHUNK: usize = 4 << 20;
/// The most folders and designs taken from a welcome.
const MAX_FOLDERS: usize = 64;
const MAX_DESIGNS: usize = 200;

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

/// Whether `path` lies inside folder `root`, a path of the server's own (`/data`, `C:\\data`):
/// either separator ends the folder's name, as the file messages take it.
pub fn under(root: &str, path: &str) -> bool {
    let root = root.trim_end_matches(['/', '\\']);
    !root.is_empty() && path.starts_with(root) && path.get(root.len()..).is_some_and(|rest| rest.starts_with(['/', '\\']))
}

/// What a welcome says of the server's designs: the agent's folders, and the designs in them
/// (path, last changed in seconds since 1970), newest first.
#[derive(Debug, Default, PartialEq)]
pub struct Library {
    pub folders: Vec<String>,
    pub designs: Vec<(String, u64)>,
}

impl Library {
    pub fn from_welcome(welcome: &Value) -> Library {
        let list = |key: &str| welcome.get(key).and_then(Value::as_array).cloned().unwrap_or_default();
        let folders = list("folders").iter().filter_map(Value::as_str).filter(|f| !f.is_empty()).take(MAX_FOLDERS).map(str::to_string).collect();
        let designs = list("designs")
            .iter()
            .filter_map(|d| Some((d.get("path")?.as_str()?.to_string(), d.get("modified").and_then(Value::as_u64).unwrap_or(0))))
            .take(MAX_DESIGNS)
            .collect();
        Library { folders, designs }
    }

    /// Whether `path` is in one of the server's folders.
    pub fn holds(&self, path: &str) -> bool {
        self.folders.iter().any(|f| under(f, path))
    }
}

/// Put the server's designs on the Home page's recent list. A design of the server's folders the
/// server no longer has goes; one it has and the list lacks joins at the end, unless the person
/// removed it from the list (`forgotten`) or the list is full; one already listed keeps its place.
/// A design that joins shows when it last changed until it is opened here.
pub fn merge(
    recent: &mut Vec<String>,
    forgotten: &mut std::collections::BTreeSet<String>,
    opened_at: &mut std::collections::BTreeMap<String, u64>,
    library: &Library,
    max: usize,
) {
    let listed = |p: &String| library.designs.iter().any(|(d, _)| d == p);
    recent.retain(|p| !library.holds(p) || listed(p));
    for (path, modified) in &library.designs {
        if recent.len() < max && library.holds(path) && !recent.contains(path) && !forgotten.contains(path) {
            recent.push(path.clone());
            opened_at.entry(path.clone()).or_insert(*modified);
        }
    }
    // A removal is kept while the server has the design, and no longer.
    forgotten.retain(|p| !library.holds(p) || listed(p));
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
    /// The server took the hello: the tab is linked. It lists the server's designs.
    Welcome(Value),
    /// A piece of a design the person opened from Home: `{path, data, append}`.
    Piece(Value),
    /// The whole design asked for has come, or why it cannot.
    Fetched {
        path: String,
        error: Option<String>,
    },
    /// The server wrote a design this tab saved, or says why not.
    Saved {
        number: u64,
        error: Option<String>,
    },
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
        (None, None) if message.get("welcome").is_some_and(Value::is_object) => Incoming::Welcome(message["welcome"].clone()),
        (None, None) if message.get("file").is_some_and(Value::is_object) => Incoming::Piece(message["file"].clone()),
        (None, None) if message.get("fetched").is_some() => {
            let fetched = &message["fetched"];
            match fetched.get("path").and_then(Value::as_str) {
                Some(path) => Incoming::Fetched { path: path.to_string(), error: fetched.get("error").and_then(Value::as_str).map(str::to_string) },
                None => Incoming::Other,
            }
        }
        (None, None) if message.get("saved").is_some() => {
            let saved = &message["saved"];
            match saved.get("ref").and_then(Value::as_u64) {
                Some(number) => Incoming::Saved { number, error: saved.get("error").and_then(Value::as_str).map(str::to_string) },
                None => Incoming::Other,
            }
        }
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
pub use web::{remembered, restore, save, start, tend};

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

    use super::{
        FOLDERS_KEY, Incoming, Library, Store, TOKEN_KEY, UPLOAD_CHUNK, encode, file_reply, incoming, link_url, merge, token_in_fragment, under,
        with_id,
    };

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
        /// The server took this tab's hello and the socket is still open.
        linked: Cell<bool>,
        /// The server's folders, as its last welcome named them (kept between visits).
        folders: RefCell<Vec<String>>,
        /// The server's designs from its welcome, for the Home page to take ([`tend`]).
        library: RefCell<Option<Library>>,
        /// Designs fetched from the server, to open.
        opened: RefCell<Vec<String>>,
        /// Messages for the status line: text, whether it is an error.
        messages: RefCell<Vec<(String, bool)>>,
        /// Saves sent to the server and not answered yet: their number, and the promise's resolve.
        saves: RefCell<std::collections::HashMap<u64, Function>>,
        last_save: Cell<u64>,
    }

    /// How long a save waits for the server to say it wrote the design.
    const SAVE_WAIT_MS: i32 = 5 * 60 * 1000;

    thread_local! {
        /// The page's one link, for the app's hooks below.
        static LINK: RefCell<Option<Rc<Link>>> = const { RefCell::new(None) };
    }

    fn the_link() -> Option<Rc<Link>> {
        LINK.with(|l| l.borrow().clone())
    }

    /// Whether `path` is a design in the server's folders: kept on the Home page's recent list
    /// while the server is away, opened by asking the server ([`restore`]).
    pub fn remembered(path: &str) -> bool {
        the_link().is_some_and(|link| link.owns(path))
    }

    /// Open a design of the server's from the Home page: ask the server for its file; it opens
    /// once it has come. False: not one of the server's.
    pub fn restore(path: &str) -> bool {
        let Some(link) = the_link().filter(|link| link.owns(path)) else { return false };
        if link.linked.get() {
            link.send(&json!({"fetch": {"path": path}}).to_string());
        } else {
            link.say(format!("{path} is on SolidCraft's server, which this tab is not linked to now: try again in a moment"), true);
        }
        true
    }

    /// Write a design saved in this tab to the server's disk, when it lives in the server's
    /// folders; anything else is not the server's and is left alone. Done when the server has
    /// written it, so a save that did not reach the disk says so.
    pub async fn save(path: &str, bytes: &[u8]) -> Result<(), String> {
        let Some(link) = the_link().filter(|link| link.owns(path)) else { return Ok(()) };
        if !link.linked.get() {
            return Err("not saved on SolidCraft's server: this tab is not linked to it now, so the design is only in this tab".into());
        }
        let number = link.last_save.get() + 1;
        link.last_save.set(number);
        let mut resolve = None;
        let answered = js_sys::Promise::new(&mut |done, _| resolve = Some(done));
        if let Some(done) = resolve {
            link.saves.borrow_mut().insert(number, done);
        }
        let pieces: Vec<&[u8]> = if bytes.is_empty() { vec![&[]] } else { bytes.chunks(UPLOAD_CHUNK).collect() };
        let count = pieces.len();
        for (i, piece) in pieces.into_iter().enumerate() {
            let put = json!({"put": {"ref": number, "path": path, "data": encode(piece), "append": i > 0, "last": i + 1 == count}});
            link.send(&put.to_string());
        }
        // Saves are written one after another: one the server never answers must not hold up
        // the rest for ever.
        let late = JsValue::from_str("may not be saved on SolidCraft's server: it did not answer within five minutes");
        let timer = js_sys::Promise::new(&mut |done, _| {
            if let Some(window) = web_sys::window() {
                let _ = window.set_timeout_with_callback_and_timeout_and_arguments_1(&done, SAVE_WAIT_MS, &late);
            }
        });
        let answer = wasm_bindgen_futures::JsFuture::from(js_sys::Promise::race(&Array::of2(&answered, &timer))).await;
        link.saves.borrow_mut().remove(&number);
        match answer {
            Ok(answer) => answer.as_string().map_or(Ok(()), Err),
            Err(_) => Err("not saved on SolidCraft's server: its answer was lost".into()),
        }
    }

    /// Each frame: open the designs fetched from the server, show the link's messages, and once
    /// the Home page's recent list is built from the browser's own designs (`recents_ready`),
    /// add the server's to it.
    pub fn tend(app: &mut solidcraft_ui_egui::SolveApp, recents_ready: bool) {
        let Some(link) = the_link() else { return };
        let opened: Vec<String> = link.opened.borrow_mut().drain(..).collect();
        for path in opened {
            app.open_path_async(&path);
        }
        let messages: Vec<(String, bool)> = link.messages.borrow_mut().drain(..).collect();
        for (text, error) in messages {
            app.set_status(text, error);
        }
        if recents_ready && let Some(library) = link.library.borrow_mut().take() {
            let home = &mut app.home;
            merge(&mut home.recent, &mut home.forgotten, &mut home.opened_at, &library, solidcraft_ui_egui::home::MAX_RECENT);
        }
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
        fn owns(&self, path: &str) -> bool {
            solidcraft_engine::io::is_design_path(path) && self.folders.borrow().iter().any(|f| under(f, path))
        }

        fn say(&self, text: String, error: bool) {
            self.messages.borrow_mut().push((text, error));
            self.ctx.request_repaint();
        }

        /// Answer save `number` (`None`: written) and forget it.
        fn answer_save(&self, number: u64, error: Option<String>) {
            let done = self.saves.borrow_mut().remove(&number);
            if let Some(done) = done {
                let answer = error.map_or(JsValue::NULL, |e| JsValue::from_str(&e));
                let _ = done.call1(&JsValue::NULL, &answer);
            }
        }

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
            self.linked.set(false);
            // Saves the server had not answered may not have reached its disk.
            let unanswered: Vec<u64> = self.saves.borrow().keys().copied().collect();
            for number in unanswered {
                self.answer_save(number, Some("may not be saved on SolidCraft's server: the link to it broke before it answered".into()));
            }
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
                Incoming::Welcome(welcome) => {
                    self.retry_ms.set(1000);
                    self.linked.set(true);
                    log::info!("MCP link: this tab is linked to {}", self.url);
                    let library = Library::from_welcome(&welcome);
                    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
                        let _ = storage.set_item(&crate::channel::key(FOLDERS_KEY), &json!(library.folders).to_string());
                    }
                    *self.folders.borrow_mut() = library.folders.clone();
                    *self.library.borrow_mut() = Some(library);
                    self.ctx.request_repaint();
                }
                Incoming::Piece(piece) => {
                    let write = json!({"fs": "write", "path": piece.get("path"), "data": piece.get("data"), "append": piece.get("append")});
                    let reply = file_reply(&mut Files, &write);
                    if reply["ok"] != true {
                        log::warn!("MCP link: a design from the server could not be kept: {}", reply["error"]);
                    }
                }
                Incoming::Fetched { path, error: None } if vfs::exists(&path) => {
                    self.opened.borrow_mut().push(path);
                    self.ctx.request_repaint();
                }
                Incoming::Fetched { path, error } => {
                    let why = error.unwrap_or_else(|| "it did not arrive whole".into());
                    self.say(format!("{path} could not be opened from SolidCraft's server: {why}"), true);
                }
                Incoming::Saved { number, error } => self.answer_save(number, error),
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
        // The server's folders from an earlier visit: its designs stay on Home until it says more.
        let folders = storage
            .as_ref()
            .and_then(|s| s.get_item(&crate::channel::key(FOLDERS_KEY)).ok().flatten())
            .and_then(|text| serde_json::from_str::<Vec<String>>(&text).ok())
            .unwrap_or_default();
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
            linked: Cell::new(false),
            folders: RefCell::new(folders),
            library: RefCell::default(),
            opened: RefCell::default(),
            messages: RefCell::default(),
            saves: RefCell::default(),
            last_save: Cell::new(0),
        });
        LINK.with(|l| *l.borrow_mut() = Some(link.clone()));
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
        assert_eq!(incoming(r#"{"welcome":{"tab":1}}"#), Incoming::Welcome(json!({"tab": 1})));
        assert_eq!(
            incoming(r#"{"file":{"path":"/data/a.sc","data":"","append":false}}"#),
            Incoming::Piece(json!({"path": "/data/a.sc", "data": "", "append": false}))
        );
        assert_eq!(incoming(r#"{"fetched":{"path":"/data/a.sc"}}"#), Incoming::Fetched { path: "/data/a.sc".into(), error: None });
        assert_eq!(
            incoming(r#"{"fetched":{"path":"/data/a.sc","error":"gone"}}"#),
            Incoming::Fetched { path: "/data/a.sc".into(), error: Some("gone".into()) }
        );
        assert_eq!(incoming(r#"{"saved":{"ref":2}}"#), Incoming::Saved { number: 2, error: None });
        assert_eq!(incoming(r#"{"saved":{"ref":3,"error":"full"}}"#), Incoming::Saved { number: 3, error: Some("full".into()) });
        assert_eq!(incoming(r#"{"refused":"token"}"#), Incoming::Refused);
        for other in [
            "",
            "not json",
            "[]",
            r#"{"method":"no.id"}"#,
            r#"{"id":"text","method":"x"}"#,
            r#"{"id":1}"#,
            r#"{"welcome":true}"#,
            r#"{"fetched":{}}"#,
            r#"{"saved":{"ref":"x"}}"#,
        ] {
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

    #[test]
    fn a_folder_holds_only_what_lies_beyond_its_name() {
        assert!(under("/data", "/data/a.sc"));
        assert!(under("/data/", "/data/sub/a.sc"));
        assert!(under(r"C:\data", r"C:\data\a.sc"));
        assert!(!under("/data", "/database/a.sc"));
        assert!(!under("/data", "/data"));
        assert!(!under("", "/a.sc"));
        assert!(!under("/", "/a.sc"), "an empty folder name holds nothing");
    }

    #[test]
    fn the_welcome_lists_the_servers_designs_for_home() {
        let library = Library::from_welcome(&json!({
            "tab": 1,
            "folders": ["/data", 7, ""],
            "designs": [{"path": "/data/new.sc", "modified": 30}, {"path": "/data/old.sc", "modified": 10}, {"modified": 5}, "x"],
        }));
        assert_eq!(library.folders, ["/data"]);
        assert_eq!(library.designs, [("/data/new.sc".to_string(), 30), ("/data/old.sc".to_string(), 10)]);
        assert_eq!(Library::from_welcome(&json!({"tab": 1})), Library::default(), "an older server lists nothing");
    }

    #[test]
    fn the_servers_designs_join_the_recent_list_and_leave_it_when_gone() {
        use std::collections::{BTreeMap, BTreeSet};
        let library = Library {
            folders: vec!["/data".into()],
            designs: vec![("/data/new.sc".into(), 30), ("/data/kept.sc".into(), 20), ("/data/removed.sc".into(), 10), ("/elsewhere.sc".into(), 5)],
        };
        // The browser's own designs and a remembered one of the server's keep their places; one
        // the server no longer has goes; the person's removal stands.
        let mut recent: Vec<String> = ["/opfs/mine.sc", "/data/kept.sc", "/data/deleted.sc"].map(String::from).to_vec();
        let mut forgotten: BTreeSet<String> = ["/data/removed.sc", "/data/deleted.sc", "/opfs/old.sc"].map(String::from).into();
        let mut opened_at = BTreeMap::from([("/data/kept.sc".to_string(), 99)]);
        merge(&mut recent, &mut forgotten, &mut opened_at, &library, 12);
        assert_eq!(recent, ["/opfs/mine.sc", "/data/kept.sc", "/data/new.sc"]);
        assert_eq!(forgotten, ["/data/removed.sc", "/opfs/old.sc"].map(String::from).into(), "a removal lasts while the server has the design");
        assert_eq!(opened_at, BTreeMap::from([("/data/kept.sc".to_string(), 99), ("/data/new.sc".to_string(), 30)]));
        // A full list takes no more.
        let mut full: Vec<String> = (0..12).map(|i| format!("/opfs/{i}.sc")).collect();
        merge(&mut full, &mut BTreeSet::new(), &mut BTreeMap::new(), &library, 12);
        assert!(full.iter().all(|p| p.starts_with("/opfs/")));
    }
}
