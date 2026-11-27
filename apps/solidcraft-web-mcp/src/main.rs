//! `solidcraft-web-mcp`: the web app and MCP over HTTP on one port (docs/web-mcp.md).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]
#![forbid(unsafe_code)]

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::process::ExitCode;
use std::time::Duration;

use solidcraft_engine::note;
use solidcraft_web_mcp::Relay;
use solidcraft_web_mcp::cli::{Command, USAGE, parse};

/// Ask the server at `listen` whether it is up (the image's health check).
fn healthcheck(listen: &str) -> Result<(), String> {
    // A server listening on every address is asked on this computer's own.
    let address = listen.replace("0.0.0.0", "127.0.0.1").replace("[::]", "[::1]");
    let socket = address.to_socket_addrs().map_err(|e| format!("{address}: {e}"))?.next().ok_or_else(|| format!("{address}: no address"))?;
    let timeout = Duration::from_secs(4);
    let mut stream = TcpStream::connect_timeout(&socket, timeout).map_err(|e| format!("{address}: {e}"))?;
    stream.set_read_timeout(Some(timeout)).and_then(|()| stream.set_write_timeout(Some(timeout))).map_err(|e| e.to_string())?;
    write!(stream, "GET /healthz HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n").map_err(|e| e.to_string())?;
    let mut reply = String::new();
    let _ = stream.take(256).read_to_string(&mut reply);
    if reply.starts_with("HTTP/1.1 200 ") {
        Ok(())
    } else {
        Err(format!("{address}: unexpected reply `{}`", reply.lines().next().unwrap_or_default()))
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match parse(&args, |name| std::env::var(name).ok())? {
        Command::Help => {
            let _ = std::io::stdout().write_all(USAGE.as_bytes());
            Ok(())
        }
        Command::Healthcheck { listen } => healthcheck(&listen),
        Command::Serve { mut config, data } => {
            for dir in &data {
                std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            }
            // Made from the folders as they resolve; a call's paths are matched against the
            // spellings given here too, since a tab keeps a file under the path it was told.
            let scope = solidcraft_mcp::agent_scope(data.iter().cloned());
            let folders: Vec<String> = scope.roots().iter().map(|r| r.display().to_string()).collect();
            config.folders = data.iter().filter_map(|d| std::path::absolute(d).ok()).collect();
            config.scope = Some(scope);
            let (token, open_link) = (config.token.is_some(), config.open_link);
            let relay = Relay::bind(*config)?;
            let address = relay.local_addr().map_err(|e| e.to_string())?;
            note!("solidcraft-web-mcp: the app is at http://{address}/ and MCP at http://{address}/mcp");
            note!("solidcraft-web-mcp: agents' files are limited to {}", folders.join(", "));
            match (token, open_link) {
                (false, _) => note!("solidcraft-web-mcp: no token is set: reachable from this computer only"),
                (true, true) => {
                    note!("solidcraft-web-mcp: MCP clients send the token as `Authorization: Bearer`; tabs link without it (--open-link)")
                }
                (true, false) => {
                    note!(
                        "solidcraft-web-mcp: MCP clients send the token as `Authorization: Bearer`; open the app once at http://{address}/#mcp-token=<token> to link a browser"
                    )
                }
            }
            relay.run().map_err(|e| e.to_string())
        }
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            note!("solidcraft-web-mcp: {e}");
            ExitCode::FAILURE
        }
    }
}
