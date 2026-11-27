//! The relay's one listener: the web app's files, MCP at `/mcp`, and the tabs' link at
//! `/mcp-link`. Each connection carries one request on a thread of its own; a tab's link keeps
//! its thread for as long as the tab stays.

use std::io::{BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde_json::{Value, json};
use solidcraft_control::auth::{MIN_TOKEN_LEN, tokens_match};
use solidcraft_engine::io::vfs::PathScope;
use solidcraft_mcp::Options;

use crate::backend::Waits;
use crate::http::{ReadError, Request, Response, read_request, reason, write_response};
use crate::hub::Hub;
use crate::mcp::{Sessions, Settings};
use crate::site::Site;
use crate::ws::{self, Message};

/// The largest MCP request body: a tool call's arguments, as the control channel caps a line.
const MAX_BODY: usize = 16 << 20;
/// The largest message a tab may send: a control reply, or a file chunk in base64.
const MAX_TAB_MESSAGE: usize = 64 << 20;
/// The largest first message of a link: the hello, read before the tab has shown its token.
const MAX_HELLO: usize = 4096;
/// The most connections served at once; the next one is refused.
const MAX_CONNECTIONS: usize = 256;
/// How long a client has to send its request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// How long one write to a client may stall.
const WRITE_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a tab has to say hello after its link opens.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// How often every linked tab is pinged.
const PING_EVERY: Duration = Duration::from_secs(20);
/// A tab silent for this long (three pings unanswered) is gone.
const TAB_SILENCE: Duration = Duration::from_secs(75);

/// How the relay runs.
#[derive(Clone, Debug)]
pub struct Config {
    /// The address to listen on, such as `127.0.0.1:8080`.
    pub listen: String,
    /// The built web app's folder.
    pub site: PathBuf,
    /// The token MCP clients send as `Authorization: Bearer`, and tabs in their hello. `None`
    /// only when listening on this computer alone.
    pub token: Option<String>,
    /// Tabs link without the token: for a relay behind a proxy that signs people in itself.
    pub open_link: bool,
    /// Origins allowed besides the relay's own (`https://cad.example`).
    pub origins: Vec<String>,
    /// Where people open the app, for messages; `None`: `http://` and the listening address.
    pub url: Option<String>,
    /// The folders the agent's files are confined to; `None`: anywhere.
    pub scope: Option<PathScope>,
    /// The agent's folders as they were spelled when given (a link to a folder keeps its own
    /// name here; the scope has where it leads).
    pub folders: Vec<PathBuf>,
    pub options: Options,
    pub waits: Waits,
}

/// Whether `host` (a `Host` header, port and all) names this computer itself.
fn loopback_host(host: &str) -> bool {
    let name =
        host.strip_prefix('[').and_then(|h| h.split_once(']')).map_or_else(|| host.rsplit_once(':').map_or(host, |(name, _)| name), |(name, _)| name);
    name.eq_ignore_ascii_case("localhost") || name.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// The host and port of an origin (`https://cad.example:8443` → `cad.example:8443`).
fn origin_host(origin: &str) -> Option<&str> {
    let (scheme, host) = origin.split_once("://")?;
    (matches!(scheme, "http" | "https") && !host.is_empty() && !host.contains('/')).then_some(host)
}

impl Config {
    /// Refuse a configuration that would leave MCP open to a network.
    pub fn check(&self) -> Result<(), String> {
        let listens_locally = self.listen.rsplit_once(':').is_some_and(|(host, _)| loopback_host(host));
        match &self.token {
            Some(token) if token.len() < MIN_TOKEN_LEN => {
                Err(format!("the token must be at least {MIN_TOKEN_LEN} characters (for example: openssl rand -hex 32)"))
            }
            None if !listens_locally => Err(format!(
                "listening on {} reaches beyond this computer, so MCP clients must show a token: set SOLIDCRAFT_WEB_MCP_TOKEN (for example: openssl rand -hex 32)",
                self.listen
            )),
            _ => Ok(()),
        }
    }

    /// Whether a request may come from where its headers say. With a token, a page on another
    /// site is refused (a browser names its site in `Origin`; other clients send none). Without
    /// one the relay listens on this computer only, and a page that reached it under another
    /// name (DNS rebinding) is refused by that name.
    fn allows(&self, request: &Request) -> bool {
        let host = request.header("host").unwrap_or_default();
        if self.token.is_none() && !loopback_host(host) {
            return false;
        }
        match request.header("origin") {
            None => true,
            Some(origin) => {
                origin_host(origin).is_some_and(|o| o.eq_ignore_ascii_case(host))
                    || self.origins.iter().any(|allowed| allowed.eq_ignore_ascii_case(origin))
            }
        }
    }

    /// Whether `shown` is the token (always, when none is set).
    fn token_matches(&self, shown: Option<&str>) -> bool {
        match &self.token {
            None => true,
            Some(token) => shown.is_some_and(|shown| tokens_match(shown, token)),
        }
    }
}

struct Shared {
    config: Config,
    site: Site,
    hub: Arc<Hub>,
    sessions: Sessions,
    connections: AtomicUsize,
}

/// A relay bound to its address and ready to serve.
pub struct Relay {
    listener: TcpListener,
    shared: Arc<Shared>,
}

impl Relay {
    pub fn bind(config: Config) -> Result<Self, String> {
        config.check()?;
        let site = Site::open(&config.site)?;
        let listener = TcpListener::bind(&config.listen).map_err(|e| format!("{}: {e}", config.listen))?;
        let address = listener.local_addr().map_err(|e| e.to_string())?;
        let hub = Arc::new(Hub::default());
        let url = config.url.clone().unwrap_or_else(|| format!("http://{address}/"));
        let sessions = Sessions::new(Settings {
            hub: hub.clone(),
            url,
            scope: config.scope.clone(),
            folders: config.folders.clone(),
            options: config.options,
            waits: config.waits,
        });
        Ok(Relay { listener, shared: Arc::new(Shared { config, site, hub, sessions, connections: AtomicUsize::new(0) }) })
    }

    pub fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.listener.local_addr()
    }

    /// The tabs linked to this relay.
    pub fn hub(&self) -> Arc<Hub> {
        self.shared.hub.clone()
    }

    /// Serve until the listener fails.
    pub fn run(self) -> std::io::Result<()> {
        let hub = self.shared.hub.clone();
        std::thread::Builder::new().name("tab-pings".into()).spawn(move || ping_tabs(&hub))?;
        for stream in self.listener.incoming() {
            // A connection that failed to arrive is the client's loss; the next one is served.
            let Ok(stream) = stream else { continue };
            let shared = self.shared.clone();
            if shared.connections.fetch_add(1, Ordering::Relaxed) >= MAX_CONNECTIONS {
                shared.connections.fetch_sub(1, Ordering::Relaxed);
                let mut stream = stream;
                let _ = stream.set_write_timeout(Some(HELLO_TIMEOUT));
                let _ = write_response(&mut stream, &Response::text(503, "too many connections"), false);
                continue;
            }
            let served = shared.clone();
            let spawned = std::thread::Builder::new().name("connection".into()).spawn(move || {
                served.connection(stream);
                served.connections.fetch_sub(1, Ordering::Relaxed);
            });
            if spawned.is_err() {
                shared.connections.fetch_sub(1, Ordering::Relaxed);
            }
        }
        Ok(())
    }

    /// Serve on a thread of its own (tests, and a caller with other work).
    pub fn spawn(self) -> std::io::Result<std::thread::JoinHandle<std::io::Result<()>>> {
        std::thread::Builder::new().name("relay".into()).spawn(move || self.run())
    }
}

/// Ping every linked tab now and then: a tab whose socket takes no ping is gone, and one that
/// answers none runs into its reader's silence limit.
fn ping_tabs(hub: &Hub) {
    loop {
        std::thread::sleep(PING_EVERY);
        for (tab, writer) in hub.writers() {
            let mut socket = writer.lock().unwrap_or_else(PoisonError::into_inner);
            if ws::write_ping(&mut *socket).is_err() {
                let _ = socket.shutdown(std::net::Shutdown::Both);
                drop(socket);
                hub.detach(tab);
            }
        }
    }
}

impl Shared {
    fn connection(&self, stream: TcpStream) {
        let _ = stream.set_read_timeout(Some(REQUEST_TIMEOUT));
        let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
        let _ = stream.set_nodelay(true);
        let Ok(mut output) = stream.try_clone() else { return };
        let mut input = BufReader::new(stream);
        let request = match read_request(&mut input, MAX_BODY) {
            Ok(request) => request,
            Err(ReadError::Closed) => return,
            Err(ReadError::Refused(status, why)) => {
                let _ = write_response(&mut output, &Response::text(status, why), false);
                return;
            }
        };
        // A failed write is a client that left; there is nobody to tell.
        let _ = self.route(&request, input, &mut output);
        let _ = output.flush();
    }

    fn route(&self, request: &Request, input: BufReader<TcpStream>, output: &mut TcpStream) -> std::io::Result<()> {
        let head_only = request.method == "HEAD";
        match request.path.as_str() {
            "/mcp" => {
                if !self.config.allows(request) {
                    return write_response(output, &Response::text(403, "this origin may not use MCP here"), head_only);
                }
                if !self.config.token_matches(request.bearer()) {
                    let refusal = Response::text(401, "MCP here needs the server's token: send Authorization: Bearer <token>")
                        .with("WWW-Authenticate", "Bearer");
                    return write_response(output, &refusal, head_only);
                }
                // A long call sends nothing while it runs; its stream's keep-alive lines do.
                let _ = output.set_write_timeout(Some(WRITE_TIMEOUT));
                self.sessions.serve(request, output)
            }
            "/mcp-link" => self.link(request, input, output),
            "/mcp-link/status" => {
                let mut status = self.hub.status();
                status["sessions"] = json!(self.sessions.count());
                write_response(output, &Response::json(200, &status).with("Cache-Control", "no-store"), head_only)
            }
            "/healthz" => write_response(output, &Response::text(200, "ok").with("Cache-Control", "no-store"), head_only),
            _ if matches!(request.method.as_str(), "GET" | "HEAD") => self.site.serve(request, output),
            _ => write_response(output, &Response::text(405, "use GET").with("Allow", "GET, HEAD"), false),
        }
    }

    /// A tab's link: the WebSocket handshake, its hello, then its answers until it goes.
    fn link(&self, request: &Request, mut input: BufReader<TcpStream>, output: &mut TcpStream) -> std::io::Result<()> {
        let upgrade = request.method == "GET"
            && request.header_lists("connection", "upgrade")
            && request.header("upgrade").is_some_and(|u| u.eq_ignore_ascii_case("websocket"));
        let Some(key) = request.header("sec-websocket-key").filter(|_| upgrade && request.header("sec-websocket-version") == Some("13")) else {
            return write_response(output, &Response::text(426, "the tab link is a WebSocket").with("Upgrade", "websocket"), false);
        };
        // Only the app this relay serves links here: a page from elsewhere is refused.
        if request.header("origin").is_none() || !self.config.allows(request) {
            return write_response(output, &Response::text(403, "only the app served here may link"), false);
        }
        write!(
            output,
            "HTTP/1.1 101 {}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
            reason(101),
            ws::accept_key(key)
        )?;
        output.flush()?;

        let mut partial = None;
        input.get_ref().set_read_timeout(Some(HELLO_TIMEOUT))?;
        let hello = match ws::read_message(&mut input, &mut partial, MAX_HELLO)? {
            Message::Text(text) => serde_json::from_str::<Value>(&text).ok().and_then(|v| v.get("hello").cloned()),
            _ => None,
        };
        let Some(hello) = hello else {
            return ws::write_close(output, 1002);
        };
        if !self.config.open_link && !self.config.token_matches(hello.get("token").and_then(Value::as_str)) {
            ws::write_text(output, &json!({"refused": "token"}).to_string())?;
            return ws::write_close(output, 1008);
        }
        let version = hello.get("version").and_then(Value::as_str).unwrap_or_default().chars().take(64).collect();
        let hidden = hello.get("hidden").and_then(Value::as_bool).unwrap_or(false);
        let writer = Arc::new(Mutex::new(output.try_clone()?));
        let tab = self.hub.attach(writer.clone(), version, hidden);
        let send = |frame: &dyn Fn(&mut TcpStream) -> std::io::Result<()>| frame(&mut writer.lock().unwrap_or_else(PoisonError::into_inner));
        let served = (|| {
            send(&|socket| ws::write_text(socket, &json!({"welcome": {"tab": tab}}).to_string()))?;
            input.get_ref().set_read_timeout(Some(TAB_SILENCE))?;
            loop {
                match ws::read_message(&mut input, &mut partial, MAX_TAB_MESSAGE)? {
                    Message::Text(text) => {
                        let Ok(message) = serde_json::from_str::<Value>(&text) else { continue };
                        if let Some(id) = message.get("id").and_then(Value::as_u64) {
                            self.hub.deliver(tab, id, message);
                        } else if let Some(hidden) = message.get("hidden").and_then(Value::as_bool) {
                            self.hub.set_hidden(tab, hidden);
                        }
                    }
                    Message::Ping(payload) => send(&|socket| ws::write_pong(socket, &payload))?,
                    Message::Pong | Message::Binary => {}
                    Message::Close => return send(&|socket| ws::write_close(socket, 1000)),
                }
            }
        })();
        self.hub.detach(tab);
        served
    }
}
