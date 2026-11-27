//! The whole chain with the real web app: this server, headless Chrome with the app in a tab, and
//! an MCP client. Needs a built web app and Chrome, so it runs only when asked for:
//!
//! ```sh
//! (cd apps/solidcraft-web && trunk build --release --features mcp-link)
//! cargo test -p solidcraft-web-mcp --test browser -- --ignored --nocapture
//! ```
//!
//! `SOLIDCRAFT_WEB_MCP_E2E_SITE` names the built site (default `dist/web`); `CHROME` names the
//! browser (default: the usual install locations).

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::{Value, json};
use solidcraft_web_mcp::backend::Waits;
use solidcraft_web_mcp::{Config, Relay};

const TOKEN: &str = "an-end-to-end-token-0123456789abcdef0123";
/// How long the app may take to start in the tab (a real build compiles some 30 MB of wasm).
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(300);

fn find_chrome() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("CHROME") {
        return Some(PathBuf::from(p));
    }
    let installed = [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        r"C:\Program Files\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
    ];
    if let Some(p) = installed.iter().map(PathBuf::from).find(|p| p.is_file()) {
        return Some(p);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .flat_map(|d| ["google-chrome", "google-chrome-stable", "chromium", "chromium-browser"].map(|n| d.join(n)))
        .find(|p| p.is_file())
}

/// Chrome, closed when the test ends however it ends.
struct Browser(Child);

impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// One request; the reply's status, head (lower case) and body.
fn send(addr: SocketAddr, head: &str, body: &[u8]) -> (u16, String, Vec<u8>) {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(600))).unwrap();
    let mut request = format!("{head}\r\nHost: {addr}\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
    request.extend_from_slice(body);
    stream.write_all(&request).unwrap();
    let mut all = Vec::new();
    stream.read_to_end(&mut all).unwrap();
    let split = all.windows(4).position(|w| w == b"\r\n\r\n").expect("a response head");
    let head = String::from_utf8_lossy(&all[..split]).to_ascii_lowercase();
    let status = head.split(' ').nth(1).and_then(|s| s.parse().ok()).expect("a status");
    (status, head, all[split + 4..].to_vec())
}

struct Client {
    addr: SocketAddr,
    session: String,
    next: u64,
}

impl Client {
    fn open(addr: SocketAddr) -> Client {
        let hello = json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "e2e", "version": "0"}}});
        let (status, head, body) = send(
            addr,
            &format!("POST /mcp HTTP/1.1\r\nAuthorization: Bearer {TOKEN}\r\nContent-Type: application/json"),
            hello.to_string().as_bytes(),
        );
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
        let session = head.lines().find_map(|l| l.strip_prefix("mcp-session-id:")).expect("a session id").trim().to_string();
        Client { addr, session, next: 1 }
    }

    /// Call a tool over an event stream, as Claude Code does; the result, which must not be an error.
    fn call(&mut self, tool: &str, arguments: Value) -> Value {
        let id = self.next;
        self.next += 1;
        let head = format!(
            "POST /mcp HTTP/1.1\r\nAuthorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nMcp-Session-Id: {}",
            self.session
        );
        let message = json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": tool, "arguments": arguments}});
        let (status, _, body) = send(self.addr, &head, message.to_string().as_bytes());
        let body = String::from_utf8_lossy(&body);
        assert_eq!(status, 200, "{body}");
        let answer = body
            .lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .filter_map(|d| serde_json::from_str::<Value>(d).ok())
            .find(|m| m["id"] == id)
            .unwrap_or_else(|| panic!("no answer to {tool} in {body}"));
        let result = answer["result"].clone();
        assert!(result["isError"] != true, "{tool} failed: {result}");
        result
    }
}

fn text(result: &Value) -> String {
    result["content"].as_array().into_iter().flatten().filter_map(|c| c["text"].as_str()).collect::<Vec<_>>().join("\n")
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[test]
#[ignore = "needs a built web app (trunk build --release --features mcp-link) and Chrome"]
fn an_agent_models_in_the_tab_and_its_files_land_on_disk() {
    let site =
        std::env::var_os("SOLIDCRAFT_WEB_MCP_E2E_SITE").map_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../dist/web"), PathBuf::from);
    let chrome = find_chrome().expect("Chrome not found: set CHROME to its executable");
    let work = tempfile::tempdir().unwrap();
    let data = work.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let data = std::fs::canonicalize(&data).unwrap();
    // Windows spells a canonical path `\\?\C:\…`; the scope takes the plain spelling.
    let data = PathBuf::from(path_text(&data).trim_start_matches(r"\\?\"));
    let relay = Relay::bind(Config {
        listen: "127.0.0.1:0".into(),
        site,
        token: Some(TOKEN.into()),
        open_link: false,
        origins: Vec::new(),
        url: None,
        scope: Some(solidcraft_mcp::agent_scope_with(None, [data.clone()])),
        folders: vec![data.clone()],
        options: solidcraft_mcp::Options::default(),
        waits: Waits::default(),
    })
    .unwrap();
    let addr = relay.local_addr().unwrap();
    relay.spawn().unwrap();

    let status = || serde_json::from_slice::<Value>(&send(addr, "GET /mcp-link/status HTTP/1.1", b"").2).unwrap();
    let mut client = Client::open(addr);
    // No tab yet: the call says where to open the app.
    let unlinked = {
        let message = json!({"jsonrpc": "2.0", "id": 99, "method": "tools/call", "params": {"name": "measure", "arguments": {}}});
        let head =
            format!("POST /mcp HTTP/1.1\r\nAuthorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\nMcp-Session-Id: {}", client.session);
        String::from_utf8_lossy(&send(addr, &head, message.to_string().as_bytes()).2).into_owned()
    };
    assert!(unlinked.contains("no browser tab is linked"), "{unlinked}");

    let mut command = Command::new(&chrome);
    command
        .arg("--headless=new")
        .arg(format!("--user-data-dir={}", work.path().join("profile").display()))
        .args(["--no-first-run", "--no-default-browser-check", "--window-size=1280,800", "--enable-unsafe-swiftshader"])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Linux CI runners and containers don't allow Chrome's sandbox; the browser only opens this test's page.
    if cfg!(target_os = "linux") {
        command.arg("--no-sandbox");
    }
    command.arg(format!("http://{addr}/#mcp-token={TOKEN}"));
    let _browser = Browser(command.spawn().unwrap_or_else(|e| panic!("{}: {e}", chrome.display())));
    let started = Instant::now();
    while status()["tabs"] != 1 {
        assert!(started.elapsed() < LAUNCH_TIMEOUT, "the app did not link within {} s: {}", LAUNCH_TIMEOUT.as_secs(), status());
        std::thread::sleep(Duration::from_millis(250));
    }
    println!("the tab linked after {:.1} s: {}", started.elapsed().as_secs_f32(), status());

    // A box, checked by the server against what it should measure.
    client.call("new_design", json!({}));
    let made = client.call(
        "execute",
        json!({"command": "solid.box", "params": {"length": 30, "width": 20, "height": 10}, "expect": {"volume_mm3": 6000, "bodies": 1}}),
    );
    println!("solid.box: {}", text(&made).chars().take(300).collect::<String>());
    let measured = text(&client.call("measure", json!({})));
    assert!(measured.contains("6000"), "{measured}");

    // The window's picture, drawn by the browser and brought back through the server.
    let shot = client.call("screenshot", json!({}));
    assert_eq!(shot["content"][0]["type"], "image", "{shot}");
    let png = base64::engine::general_purpose::STANDARD.decode(shot["content"][0]["data"].as_str().unwrap()).unwrap();
    assert!(png.starts_with(b"\x89PNG\r\n\x1a\n") && png.len() > 1000, "{} bytes", png.len());
    println!("screenshot: {} bytes, {}", png.len(), text(&shot));
    let kept = data.join("box.png");
    client.call("screenshot", json!({"path": path_text(&kept), "source": "model"}));
    assert!(std::fs::read(&kept).unwrap().starts_with(b"\x89PNG"));

    // The design and an export land in the data folder on disk.
    let design = data.join("box.sc");
    client.call("save", json!({"path": path_text(&design)}));
    assert!(std::fs::metadata(&design).unwrap().len() > 0, "the saved design");
    let step = data.join("out").join("box.step");
    client.call("export", json!({"path": path_text(&step)}));
    assert!(std::fs::read_to_string(&step).unwrap().starts_with("ISO-10303-21"), "the STEP export");

    // And a design on disk opens in the tab: a new design, then the saved one, with its box.
    client.call("new_design", json!({}));
    let empty = text(&client.call("measure", json!({})));
    assert!(!empty.contains("6000"), "{empty}");
    client.call("open", json!({"path": path_text(&design)}));
    let reopened = text(&client.call("measure", json!({})));
    assert!(reopened.contains("6000"), "{reopened}");
    println!("end to end in {:.1} s", started.elapsed().as_secs_f32());
}
