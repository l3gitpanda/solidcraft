//! The web app's files (the folder `trunk build --release` makes), served as
//! `packaging/docker/nginx.conf` serves them: `index.html`, `sw.js` and `build.json` are asked
//! for again every time, or browsers keep an old build; Trunk's hashed files never change and are
//! kept for a year. A file with a `.gz` beside it goes out compressed to a browser that takes it.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::http::{Request, Response, head, write_response};

/// The site's folder, resolved once so that every file served can be checked to lie inside it.
pub struct Site {
    root: PathBuf,
}

/// A request path as the name of a file under the site: percent-decoded, and refused (`None`)
/// when it could name anything else (`..`, a backslash, a drive, a hidden file, a control
/// character).
pub fn relative(path: &str) -> Option<PathBuf> {
    let mut bytes = Vec::with_capacity(path.len());
    let mut rest = path.as_bytes().iter();
    while let Some(&b) = rest.next() {
        if b == b'%' {
            let hex = [*rest.next()?, *rest.next()?];
            bytes.push(u8::from_str_radix(std::str::from_utf8(&hex).ok()?, 16).ok()?);
        } else {
            bytes.push(b);
        }
    }
    let decoded = String::from_utf8(bytes).ok()?;
    let mut out = PathBuf::new();
    for part in decoded.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part.starts_with('.') || part.chars().any(|c| c.is_control() || matches!(c, '\\' | ':')) {
            return None;
        }
        out.push(part);
    }
    Some(out)
}

/// The media type of a file, by its extension.
fn media_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase().as_str() {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "application/javascript",
        "wasm" => "application/wasm",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "webmanifest" => "application/manifest+json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "ico" => "image/x-icon",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "woff2" => "font/woff2",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// Whether Trunk named the file after its content (`name-0123abcd.js`, `name-0123abcd_bg.wasm`):
/// such a file never changes.
pub fn content_named(name: &str) -> bool {
    let Some((stem, extension)) = name.rsplit_once('.') else { return false };
    if !matches!(extension, "js" | "wasm" | "css") {
        return false;
    }
    let stem = stem.strip_suffix("_bg").unwrap_or(stem);
    let Some((_, hash)) = stem.rsplit_once('-') else { return false };
    (8..=16).contains(&hash.len()) && hash.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl Site {
    /// The site in `dir`, which must hold an `index.html`.
    pub fn open(dir: &Path) -> Result<Self, String> {
        let root = std::fs::canonicalize(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        if !root.join("index.html").is_file() {
            return Err(format!("{} holds no index.html: build the web app first (trunk build --release in apps/solidcraft-web)", root.display()));
        }
        Ok(Site { root })
    }

    /// The file a request path names: a regular file inside the site, links followed.
    fn resolve(&self, path: &str) -> Option<PathBuf> {
        let mut file = self.root.join(relative(path)?);
        if file.is_dir() {
            file.push("index.html");
        }
        let file = std::fs::canonicalize(file).ok()?;
        (file.starts_with(&self.root) && file.is_file()).then_some(file)
    }

    /// Answer a `GET` or `HEAD` for one of the site's files.
    pub fn serve(&self, request: &Request, output: &mut impl Write) -> std::io::Result<()> {
        let head_only = request.method == "HEAD";
        let Some(file) = self.resolve(&request.path) else {
            return write_response(output, &Response::text(404, "not found").with("X-Content-Type-Options", "nosniff"), head_only);
        };
        let name = file.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        let cache = if content_named(name) { "public, max-age=31536000, immutable" } else { "no-cache" };
        let mut headers = vec![
            ("Content-Type", media_type(&file).to_string()),
            ("Cache-Control", cache.to_string()),
            ("X-Content-Type-Options", "nosniff".to_string()),
        ];
        // The compressed copy the image's build made, for a browser that takes gzip.
        let mut gz = file.clone().into_os_string();
        gz.push(".gz");
        let gz = PathBuf::from(gz);
        let body = if request.header_lists("accept-encoding", "gzip") && gz.is_file() {
            headers.push(("Content-Encoding", "gzip".to_string()));
            headers.push(("Vary", "Accept-Encoding".to_string()));
            gz
        } else {
            file
        };
        let mut source = std::fs::File::open(&body)?;
        let length = source.metadata()?.len();
        output.write_all(&head(200, &headers, Some(length)))?;
        if !head_only {
            std::io::copy(&mut source, output)?;
        }
        output.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_paths_name_only_files_under_the_site() {
        assert_eq!(relative("/"), Some(PathBuf::new()));
        assert_eq!(relative("/fonts/a%20b.ttf"), Some(PathBuf::from("fonts").join("a b.ttf")));
        assert_eq!(relative("//fonts/./x.js"), Some(PathBuf::from("fonts").join("x.js")));
        for hostile in ["/../etc/passwd", "/a/%2e%2e/b", "/.git/config", "/a\\b", "/C:/x", "/a%00b", "/%zz", "/%2", "/%ff"] {
            assert_eq!(relative(hostile), None, "{hostile}");
        }
        // An encoded slash is a separator like any other; what it reaches is still under the site.
        assert_eq!(relative("/a%2fb"), Some(PathBuf::from("a").join("b")));
    }

    #[test]
    fn only_trunks_content_named_files_are_kept_forever() {
        assert!(content_named("solidcraft-web-0123456789abcdef_bg.wasm"));
        assert!(content_named("solidcraft-web-0123abcd.js"));
        assert!(content_named("loading-89abcdef.css"));
        for other in
            ["index.html", "sw.js", "build.json", "shared.js", "app-0123.js", "app-0123ABCD.js", "app-0123abcd.png", "x-0123456789abcdef0.js"]
        {
            assert!(!content_named(other), "{other}");
        }
    }

    #[test]
    fn files_go_out_with_their_type_and_caching_and_nothing_outside_does() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), "<html>").unwrap();
        std::fs::write(dir.path().join("app-0123abcd_bg.wasm"), b"\0asm-plain").unwrap();
        std::fs::write(dir.path().join("app-0123abcd_bg.wasm.gz"), b"gz").unwrap();
        #[cfg(unix)]
        let outside = tempfile::NamedTempFile::new().unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), dir.path().join("leak.txt")).unwrap();
        let site = Site::open(dir.path()).unwrap();
        let get = |method: &str, path: &str, gzip: bool| {
            let headers = if gzip { vec![("Accept-Encoding".to_string(), "br, gzip".to_string())] } else { Vec::new() };
            let request = Request { method: method.into(), path: path.into(), headers, body: Vec::new() };
            let mut out = Vec::new();
            site.serve(&request, &mut out).unwrap();
            String::from_utf8_lossy(&out).into_owned()
        };
        let index = get("GET", "/", false);
        assert!(index.starts_with("HTTP/1.1 200 OK\r\n") && index.contains("Cache-Control: no-cache\r\n") && index.ends_with("<html>"), "{index}");
        let wasm = get("GET", "/app-0123abcd_bg.wasm", false);
        assert!(wasm.contains("Content-Type: application/wasm\r\n") && wasm.contains("immutable") && wasm.ends_with("asm-plain"), "{wasm}");
        let packed = get("GET", "/app-0123abcd_bg.wasm", true);
        assert!(
            packed.contains("Content-Encoding: gzip\r\n") && packed.contains("Content-Type: application/wasm\r\n") && packed.ends_with("\r\n\r\ngz"),
            "{packed}"
        );
        let headed = get("HEAD", "/", false);
        assert!(headed.contains("Content-Length: 6\r\n") && headed.ends_with("\r\n\r\n"), "{headed}");
        for missing in ["/nope.js", "/../index.html", "/leak.txt"] {
            assert!(get("GET", missing, false).starts_with("HTTP/1.1 404 "), "{missing}");
        }
    }
}
