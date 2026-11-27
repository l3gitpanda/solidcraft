//! MCP over HTTP (the Streamable HTTP transport) in front of `solidcraft-mcp`'s server.
//!
//! A client `POST`s one JSON-RPC message to `/mcp`. `initialize` opens a session, named in the
//! reply's `Mcp-Session-Id` header and carried by every later request. Each session has its own
//! `solidcraft_mcp::Server` on a thread of its own (that server is not `Send`, and its calls run
//! one at a time, as over stdio), with the browser tab as its backend. A `tools/call` is answered
//! as an event stream when the client takes one: progress goes out as it comes and a comment line
//! every few seconds keeps a proxy from closing a quiet connection; anything else is answered as
//! plain JSON.

use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use solidcraft_engine::io::vfs::PathScope;
use solidcraft_mcp::Options;

use crate::backend::{TabBackend, Waits};
use crate::http::{Request, Response, head, write_response};
use crate::hub::Hub;

/// The most sessions kept; the one unused longest gives way to a new one.
const MAX_SESSIONS: usize = 32;
/// The most messages one session may have waiting for its server.
const MAX_QUEUED: usize = 64;
/// How often a quiet event stream is sent a comment line.
const KEEP_ALIVE: Duration = Duration::from_secs(15);

/// What the session's server sends back for one message.
enum Event {
    /// A notification made while the message is handled (`notifications/progress`).
    Notification(Value),
    /// The response; `None` for a message that gets none (a notification).
    Done(Option<Value>),
}

/// A message for a session's server, and where its events go.
struct Job {
    message: Value,
    events: Sender<Event>,
}

struct Session {
    jobs: Sender<Job>,
    queued: Arc<AtomicUsize>,
    /// Set when the client cancels the request in progress; the backend's waits look at it.
    cancel: Arc<AtomicBool>,
    /// The JSON-RPC ids of the message the server is handling now.
    current: Arc<Mutex<Vec<Value>>>,
    used: Instant,
}

/// What every session's server is made with.
pub struct Settings {
    pub hub: Arc<Hub>,
    /// Where a person opens the app (for errors that ask them to).
    pub url: String,
    /// The folders the agent's files are confined to.
    pub scope: Option<PathScope>,
    /// The agent's folders as they were spelled when given.
    pub folders: Vec<std::path::PathBuf>,
    pub options: Options,
    pub waits: Waits,
}

pub struct Sessions {
    settings: Settings,
    open: Mutex<HashMap<String, Session>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The ids of the JSON-RPC requests in `message` (one object, or a batch array).
fn request_ids(message: &Value) -> Vec<Value> {
    let one = |v: &Value| v.get("method").and_then(Value::as_str).and_then(|_| v.get("id")).filter(|id| !id.is_null()).cloned();
    match message {
        Value::Array(items) => items.iter().filter_map(one).collect(),
        other => one(other).into_iter().collect(),
    }
}

/// Whether `message` is, or holds, a request or notification of `method`.
fn has_method(message: &Value, method: &str) -> bool {
    match message {
        Value::Array(items) => items.iter().any(|v| v.get("method").and_then(Value::as_str) == Some(method)),
        other => other.get("method").and_then(Value::as_str) == Some(method),
    }
}

/// A new session id: 32 random bytes as hex. `None` when the system gives no random bytes.
fn new_id() -> Option<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).ok()?;
    Some(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn rpc_error(status: u16, code: i64, message: &str) -> Response {
    Response::json(status, &json!({"jsonrpc": "2.0", "id": null, "error": {"code": code, "message": message}}))
}

impl Sessions {
    pub fn new(settings: Settings) -> Self {
        Sessions { settings, open: Mutex::new(HashMap::new()) }
    }

    /// How many sessions are open.
    pub fn count(&self) -> usize {
        lock(&self.open).len()
    }

    /// Open a session: its server starts on a thread of its own and ends when the session is
    /// closed or gives way. `None` when no thread or random id could be had.
    fn open(&self) -> Option<String> {
        let id = new_id()?;
        let (jobs, inbox) = channel::<Job>();
        let queued = Arc::new(AtomicUsize::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        let current = Arc::new(Mutex::new(Vec::new()));
        let worker = Worker {
            inbox,
            queued: queued.clone(),
            cancel: cancel.clone(),
            current: current.clone(),
            hub: self.settings.hub.clone(),
            url: self.settings.url.clone(),
            scope: self.settings.scope.clone(),
            folders: self.settings.folders.clone(),
            options: self.settings.options,
            waits: self.settings.waits,
        };
        std::thread::Builder::new().name("mcp-session".into()).spawn(move || worker.run()).ok()?;
        let mut open = lock(&self.open);
        if open.len() >= MAX_SESSIONS
            && let Some(oldest) = open.iter().min_by_key(|(_, s)| s.used).map(|(id, _)| id.clone())
        {
            open.remove(&oldest);
        }
        open.insert(id.clone(), Session { jobs, queued, cancel, current, used: Instant::now() });
        Some(id)
    }

    /// Queue `message` for session `id`; `Err`: the response to send instead.
    fn enqueue(&self, id: &str, message: Value) -> Result<Receiver<Event>, Response> {
        let mut open = lock(&self.open);
        let Some(session) = open.get_mut(id) else {
            // 404 tells the client to start a new session.
            return Err(rpc_error(404, -32001, "unknown or expired session: initialize again"));
        };
        session.used = Instant::now();
        // A cancellation is acted on now, not when the server gets to it behind the call it cancels.
        if let Some(target) = message.get("params").filter(|_| has_method(&message, "notifications/cancelled")).and_then(|p| p.get("requestId"))
            && lock(&session.current).contains(target)
        {
            session.cancel.store(true, Ordering::Relaxed);
        }
        if session.queued.load(Ordering::Relaxed) >= MAX_QUEUED {
            return Err(rpc_error(429, -32000, "too many requests are waiting in this session"));
        }
        let (events, inbox) = channel();
        session.queued.fetch_add(1, Ordering::Relaxed);
        if session.jobs.send(Job { message, events }).is_err() {
            open.remove(id);
            return Err(rpc_error(404, -32001, "the session ended: initialize again"));
        }
        Ok(inbox)
    }

    /// Give up on the request session `id` is handling: its client went away.
    fn abandon(&self, id: &str) {
        if let Some(session) = lock(&self.open).get(id) {
            session.cancel.store(true, Ordering::Relaxed);
        }
    }

    /// Answer a request to `/mcp`. The caller has checked the token and the origin.
    pub fn serve(&self, request: &Request, output: &mut impl Write) -> std::io::Result<()> {
        let session_header = request.header("mcp-session-id").map(str::to_string);
        match request.method.as_str() {
            "POST" => {}
            "DELETE" => {
                let known = session_header.is_some_and(|id| lock(&self.open).remove(&id).is_some());
                return write_response(output, &Response::new(if known { 204 } else { 404 }), false);
            }
            // No stream of the server's own: it sends nothing a client did not ask for.
            _ => return write_response(output, &Response::text(405, "use POST").with("Allow", "POST, DELETE"), request.method == "HEAD"),
        }
        if request.header("content-type").is_some_and(|t| !t.to_ascii_lowercase().starts_with("application/json")) {
            return write_response(output, &rpc_error(415, -32600, "the body must be application/json"), false);
        }
        let Ok(message) = serde_json::from_slice::<Value>(&request.body) else {
            return write_response(output, &rpc_error(400, -32700, "the body is not JSON"), false);
        };
        let initialize = has_method(&message, "initialize");
        let (id, opened) = match (session_header, initialize) {
            (Some(id), _) => (id, false),
            (None, true) => match self.open() {
                Some(id) => (id, true),
                None => return write_response(output, &rpc_error(503, -32000, "no session could be started"), false),
            },
            (None, false) => {
                return write_response(output, &rpc_error(400, -32600, "missing Mcp-Session-Id: send initialize first"), false);
            }
        };
        let expects_reply = !request_ids(&message).is_empty();
        let stream = expects_reply && has_method(&message, "tools/call") && request.header_lists("accept", "text/event-stream");
        let inbox = match self.enqueue(&id, message) {
            Ok(inbox) => inbox,
            Err(response) => return write_response(output, &response, false),
        };
        if !expects_reply {
            // Notifications and responses: accepted, nothing to wait for.
            return write_response(output, &Response::new(202), false);
        }
        let session_headers = |mut headers: Vec<(&'static str, String)>| {
            if opened {
                headers.push(("Mcp-Session-Id", id.clone()));
            }
            headers
        };
        if !stream {
            let reply = loop {
                match inbox.recv() {
                    Ok(Event::Notification(_)) => {}
                    Ok(Event::Done(reply)) => break reply,
                    Err(_) => break None,
                }
            };
            let Some(reply) = reply else {
                return write_response(output, &rpc_error(503, -32000, "the session ended before answering"), false);
            };
            let mut response = Response::json(200, &reply);
            response.headers = session_headers(response.headers);
            return write_response(output, &response, false);
        }
        // An event stream ends with its connection: no length is known ahead.
        let headers = session_headers(vec![
            ("Content-Type", "text/event-stream".to_string()),
            ("Cache-Control", "no-cache".to_string()),
            ("X-Accel-Buffering", "no".to_string()),
        ]);
        let sent = (|| {
            output.write_all(&head(200, &headers, None))?;
            output.flush()?;
            loop {
                match inbox.recv_timeout(KEEP_ALIVE) {
                    Ok(Event::Notification(note)) => write!(output, "event: message\ndata: {note}\n\n")?,
                    Ok(Event::Done(reply)) => {
                        if let Some(reply) = reply {
                            write!(output, "event: message\ndata: {reply}\n\n")?;
                        }
                        return output.flush();
                    }
                    Err(RecvTimeoutError::Timeout) => output.write_all(b": keep-alive\n\n")?,
                    Err(RecvTimeoutError::Disconnected) => return output.flush(),
                }
                output.flush()?;
            }
        })();
        if sent.is_err() {
            // The client went away: stop waiting for the tab on its behalf.
            self.abandon(&id);
        }
        sent
    }
}

/// A session's server and what it shares with the HTTP side.
struct Worker {
    inbox: Receiver<Job>,
    queued: Arc<AtomicUsize>,
    cancel: Arc<AtomicBool>,
    current: Arc<Mutex<Vec<Value>>>,
    hub: Arc<Hub>,
    url: String,
    scope: Option<PathScope>,
    folders: Vec<std::path::PathBuf>,
    options: Options,
    waits: Waits,
}

impl Worker {
    fn run(self) {
        // Where the notifications of the message being handled go.
        let events: Arc<Mutex<Option<Sender<Event>>>> = Arc::default();
        let notifier = events.clone();
        let backend = TabBackend::new(self.hub, self.url, self.cancel.clone()).with_waits(self.waits).with_folders(self.folders);
        let mut server = solidcraft_mcp::Server::new(Box::new(backend)).with_path_scope(self.scope).with_options(self.options).with_notifier(
            Arc::new(move |note| {
                if let Some(events) = lock(&notifier).as_ref() {
                    let _ = events.send(Event::Notification(note));
                }
            }),
        );
        // Until the session's sender is dropped: closed, or given way to a newer one.
        while let Ok(job) = self.inbox.recv() {
            self.queued.fetch_sub(1, Ordering::Relaxed);
            *lock(&self.current) = request_ids(&job.message);
            self.cancel.store(false, Ordering::Relaxed);
            *lock(&events) = Some(job.events.clone());
            let reply = server.handle(job.message);
            *lock(&events) = None;
            lock(&self.current).clear();
            let _ = job.events.send(Event::Done(reply));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_ids_are_those_of_requests_only() {
        assert_eq!(request_ids(&json!({"jsonrpc": "2.0", "id": 7, "method": "tools/list"})), vec![json!(7)]);
        assert!(request_ids(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).is_empty());
        assert!(request_ids(&json!({"jsonrpc": "2.0", "id": 7, "result": {}})).is_empty(), "a response is not a request");
        assert_eq!(
            request_ids(&json!([{"id": "a", "method": "ping"}, {"method": "notifications/cancelled"}, {"id": 2, "method": "ping"}])),
            vec![json!("a"), json!(2)]
        );
        assert!(has_method(&json!([{"method": "x"}, {"method": "initialize"}]), "initialize"));
        assert!(!has_method(&json!({"result": {"method": "initialize"}}), "initialize"));
    }
}
