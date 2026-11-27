//! The relay's command line and environment.

use std::path::PathBuf;

use solidcraft_mcp::Options;

use crate::backend::Waits;
use crate::server::Config;

pub const USAGE: &str = "\
solidcraft-web-mcp: serve the SolidCraft web app, with MCP over HTTP for the tab a person has open (docs/web-mcp.md)

  solidcraft-web-mcp [--site DIR] [--listen ADDR:PORT] [--data DIR]... [--url URL] [--allow-origin ORIGIN]... [--open-link] [--compact] [--structured]
  solidcraft-web-mcp --healthcheck [--listen ADDR:PORT]

  --site DIR            the built web app (trunk build --release in apps/solidcraft-web); default dist/web
  --listen ADDR:PORT    where to listen; default 127.0.0.1:8080
  --data DIR            a folder agents may read and write designs, exports and pictures in; may be given more than once
  --url URL             where people open the app, for the messages that ask them to; default http://ADDR:PORT/
  --allow-origin ORIGIN an origin allowed besides the server's own, such as https://cad.example, for a proxy that changes Host
  --open-link           tabs link without the token (behind a proxy that signs people in itself)
  --compact, --structured   as solidcraft-cli mcp takes them
  --healthcheck         ask the server listening at --listen whether it is up; exit 0 when it is

Environment (a flag wins): SOLIDCRAFT_WEB_MCP_TOKEN (required unless listening on this computer only; at least 32 characters,
for example `openssl rand -hex 32`), SOLIDCRAFT_WEB_MCP_SITE, SOLIDCRAFT_WEB_MCP_LISTEN, SOLIDCRAFT_WEB_MCP_DATA,
SOLIDCRAFT_WEB_MCP_URL, SOLIDCRAFT_WEB_MCP_ORIGINS (comma-separated), SOLIDCRAFT_WEB_MCP_OPEN_LINK=1.

MCP clients connect to http://ADDR:PORT/mcp with `Authorization: Bearer <token>`. A tab links itself when opened at
URL#mcp-token=<token> once; the browser remembers the token after that.
";

/// What the command line asks for.
#[derive(Debug)]
pub enum Command {
    Serve { config: Config, data: Vec<PathBuf> },
    Healthcheck { listen: String },
    Help,
}

const DEFAULT_LISTEN: &str = "127.0.0.1:8080";

/// Read the command line (`args`, without the program's name) and the environment (`env`).
pub fn parse(args: &[String], env: impl Fn(&str) -> Option<String>) -> Result<Command, String> {
    let env = |name: &str| env(name).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let (mut site, mut listen, mut url) = (None, None, None);
    let (mut data, mut origins): (Vec<PathBuf>, Vec<String>) = (Vec::new(), Vec::new());
    let (mut open_link, mut healthcheck) = (false, false);
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        let mut value = || rest.next().cloned().ok_or_else(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--site" => site = Some(value()?),
            "--listen" => listen = Some(value()?),
            "--data" => data.push(value()?.into()),
            "--url" => url = Some(value()?),
            "--allow-origin" => origins.push(value()?),
            "--open-link" => open_link = true,
            "--healthcheck" => healthcheck = true,
            "-h" | "--help" => return Ok(Command::Help),
            // Read by `Options::from_args_and_env` below.
            solidcraft_mcp::COMPACT_FLAG | solidcraft_mcp::STRUCTURED_FLAG => {}
            other => return Err(format!("unknown option `{other}`")),
        }
    }
    let listen = listen.or_else(|| env("SOLIDCRAFT_WEB_MCP_LISTEN")).unwrap_or_else(|| DEFAULT_LISTEN.to_string());
    if healthcheck {
        return Ok(Command::Healthcheck { listen });
    }
    if data.is_empty() {
        data.extend(env("SOLIDCRAFT_WEB_MCP_DATA").map(PathBuf::from));
    }
    origins.extend(env("SOLIDCRAFT_WEB_MCP_ORIGINS").iter().flat_map(|list| list.split(',')).map(|o| o.trim().to_string()).filter(|o| !o.is_empty()));
    if let Some(bad) = origins.iter().find(|o| !o.starts_with("http://") && !o.starts_with("https://") || o.trim_end_matches('/') != o.as_str()) {
        return Err(format!("an allowed origin is a scheme and a host with no path, such as https://cad.example, not `{bad}`"));
    }
    let open_link =
        open_link || env("SOLIDCRAFT_WEB_MCP_OPEN_LINK").is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"));
    let config = Config {
        listen,
        site: site.or_else(|| env("SOLIDCRAFT_WEB_MCP_SITE")).unwrap_or_else(|| "dist/web".to_string()).into(),
        token: env("SOLIDCRAFT_WEB_MCP_TOKEN"),
        open_link,
        origins,
        url: url.or_else(|| env("SOLIDCRAFT_WEB_MCP_URL")),
        // The folders are made into a scope by the caller, which may create them.
        scope: None,
        folders: Vec::new(),
        options: Options::from_args_and_env(args)?,
        waits: Waits::default(),
    };
    config.check()?;
    Ok(Command::Serve { config, data })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn flags_win_over_the_environment_and_defaults_stay_on_this_computer() {
        let Command::Serve { config, data } = parse(&[], |_| None).unwrap() else { panic!("serve") };
        assert_eq!(
            (config.listen.as_str(), config.site.to_str(), config.token.as_deref(), config.open_link),
            ("127.0.0.1:8080", Some("dist/web"), None, false)
        );
        assert!(data.is_empty());
        let token = "t".repeat(32);
        let env = |name: &str| match name {
            "SOLIDCRAFT_WEB_MCP_TOKEN" => Some(token.clone()),
            "SOLIDCRAFT_WEB_MCP_LISTEN" => Some("0.0.0.0:8080".into()),
            "SOLIDCRAFT_WEB_MCP_DATA" => Some("/data".into()),
            "SOLIDCRAFT_WEB_MCP_ORIGINS" => Some("https://cad.example, https://other.example".into()),
            "SOLIDCRAFT_WEB_MCP_OPEN_LINK" => Some("1".into()),
            _ => None,
        };
        let Command::Serve { config, data } = parse(&args(&["--site", "/srv/site", "--data", "/a", "--data", "/b", "--compact"]), env).unwrap()
        else {
            panic!("serve")
        };
        assert_eq!((config.listen.as_str(), config.site.to_str()), ("0.0.0.0:8080", Some("/srv/site")));
        assert_eq!(data, vec![PathBuf::from("/a"), PathBuf::from("/b")], "--data replaces the variable");
        assert_eq!(config.origins, vec!["https://cad.example", "https://other.example"]);
        assert!(config.open_link && config.options.compact && config.token.is_some());
    }

    #[test]
    fn a_network_address_needs_a_token_of_some_length() {
        let listen = |addr: &str, token: Option<&str>| {
            let token = token.map(str::to_string);
            parse(&args(&["--listen", addr]), move |name| (name == "SOLIDCRAFT_WEB_MCP_TOKEN").then(|| token.clone()).flatten()).map(|_| ())
        };
        assert!(listen("127.0.0.1:9000", None).is_ok());
        assert!(listen("[::1]:9000", None).is_ok());
        assert!(listen("localhost:9000", None).is_ok());
        assert!(listen("0.0.0.0:9000", None).unwrap_err().contains("SOLIDCRAFT_WEB_MCP_TOKEN"));
        assert!(listen("192.168.1.5:9000", None).is_err());
        assert!(listen("0.0.0.0:9000", Some("short")).unwrap_err().contains("at least 32"));
        assert!(listen("0.0.0.0:9000", Some(&"x".repeat(32))).is_ok());
    }

    #[test]
    fn mistakes_are_named() {
        assert!(parse(&args(&["--port", "1"]), |_| None).unwrap_err().contains("--port"));
        assert!(parse(&args(&["--site"]), |_| None).unwrap_err().contains("needs a value"));
        assert!(parse(&args(&["--allow-origin", "cad.example"]), |_| None).unwrap_err().contains("https://cad.example"));
        assert!(parse(&args(&["--allow-origin", "https://cad.example/"]), |_| None).is_err());
        assert!(
            matches!(parse(&args(&["--healthcheck", "--listen", "0.0.0.0:1"]), |_| None), Ok(Command::Healthcheck { listen }) if listen == "0.0.0.0:1")
        );
        assert!(matches!(parse(&args(&["--help"]), |_| None), Ok(Command::Help)));
    }
}
