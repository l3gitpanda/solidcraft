//! Files between the relay's disk and the tab's memory.
//!
//! The MCP tools assume the app shares their file system: a screenshot is written by the app and
//! read back by the tool, `open` and `export` name paths. A browser tab has no disk, only files in
//! its memory (`solidcraft_engine::io::vfs` on wasm32), so the relay copies around each call:
//! before it, the files the call names that exist here go to the tab; after it, what the tab
//! wrote under the agent's folders comes back. Every read and write on this side goes through
//! `vfs`, so the agent's folder scope confines it; a path the scope refuses is left alone.

use std::collections::HashMap;
use std::path::Path;
use std::time::SystemTime;

use base64::Engine as _;
use serde_json::{Value, json};
use solidcraft_engine::io::vfs;

/// The bytes of a file sent in one message.
pub const CHUNK: usize = 4 << 20;
/// The largest file copied either way.
pub const MAX_FILE: usize = 512 << 20;
/// The most files one call may name, and the most a listing may bring back.
const MAX_FILES: usize = 512;

/// FNV-1a, 64 bits, as 16 hex digits: how the tab and the relay tell a changed file from an
/// unchanged one. Not a security measure; the tab is trusted with the files it is sent.
pub fn digest(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// A file as both sides last had it.
struct Entry {
    /// The file here when it was copied: its modification time and size.
    here: Option<(SystemTime, u64)>,
    /// Its digest in the tab.
    digest: String,
}

/// What has been copied to and from one tab. A new tab starts with empty memory, so the record
/// is dropped when the driven tab changes.
#[derive(Default)]
pub struct Synced {
    tab: u64,
    files: HashMap<String, Entry>,
}

/// The absolute paths `params` names, anywhere in it, in order and without repeats.
pub fn named_paths(params: &Value) -> Vec<String> {
    fn walk(value: &Value, depth: usize, out: &mut Vec<String>) {
        if depth > 32 || out.len() >= MAX_FILES {
            return;
        }
        match value {
            Value::String(s) if s.len() <= 4096 && Path::new(s).is_absolute() && !out.contains(s) => out.push(s.clone()),
            Value::Array(items) => items.iter().for_each(|v| walk(v, depth + 1, out)),
            Value::Object(map) => map.values().for_each(|v| walk(v, depth + 1, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(params, 0, &mut out);
    out
}

/// How the copies reach the tab: one file message and its answer.
pub trait Tab {
    /// The tab's id.
    fn id(&self) -> u64;
    /// Send a file message (`{"fs": …}`) and wait for its result.
    fn file(&mut self, message: Value) -> Result<Value, String>;
}

impl Synced {
    fn for_tab(&mut self, tab: u64) {
        if self.tab != tab {
            self.tab = tab;
            self.files.clear();
        }
    }

    /// Before a call: send the tab each file `params` names that exists here and that the tab
    /// does not have as it is now.
    pub fn push(&mut self, tab: &mut dyn Tab, params: &Value) -> Result<(), String> {
        self.for_tab(tab.id());
        for path in named_paths(params) {
            // Not one of the agent's files here: a path of the tab's own (browser storage, an upload).
            if vfs::check(&path).is_err() || !vfs::is_file(&path) {
                continue;
            }
            let here = vfs::stamp(&path);
            if here.is_some() && self.files.get(&path).is_some_and(|e| e.here == here) {
                continue;
            }
            let bytes = vfs::read(&path, MAX_FILE).map_err(|e| format!("{path}: {e}"))?;
            let mut chunks = bytes.chunks(CHUNK);
            let first = chunks.next().unwrap_or_default();
            tab.file(json!({"fs": "write", "path": path, "data": base64::engine::general_purpose::STANDARD.encode(first), "append": false}))?;
            for chunk in chunks {
                tab.file(json!({"fs": "write", "path": path, "data": base64::engine::general_purpose::STANDARD.encode(chunk), "append": true}))?;
            }
            self.files.insert(path, Entry { here, digest: digest(&bytes) });
        }
        Ok(())
    }

    /// After a call: bring back each file the tab holds under `roots` that is new or changed
    /// there, then drop the tab's copies of the files under `transient` (the scratch folder:
    /// screenshots the tool reads once).
    pub fn pull(&mut self, tab: &mut dyn Tab, roots: &[String], transient: Option<&str>) -> Result<(), String> {
        self.for_tab(tab.id());
        if roots.is_empty() {
            return Ok(());
        }
        let listing = tab.file(json!({"fs": "list", "roots": roots}))?;
        let files = listing.get("files").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
        let mut spent = Vec::new();
        for file in files.iter().take(MAX_FILES) {
            let (Some(path), Some(size), Some(digest)) =
                (file.get("path").and_then(Value::as_str), file.get("size").and_then(Value::as_u64), file.get("digest").and_then(Value::as_str))
            else {
                continue;
            };
            if transient.is_some_and(|t| Path::new(path).starts_with(t)) {
                spent.push(path.to_string());
            }
            if self.files.get(path).is_some_and(|e| e.digest == digest) {
                continue;
            }
            // The tab names the path; the scope decides whether it is one of the agent's.
            if size > MAX_FILE as u64 || vfs::check_new(path).is_err() {
                continue;
            }
            let mut bytes = Vec::new();
            while (bytes.len() as u64) < size {
                let part = tab.file(json!({"fs": "read", "path": path, "offset": bytes.len(), "len": CHUNK}))?;
                let data = part.get("data").and_then(Value::as_str).unwrap_or_default();
                let chunk = base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .map_err(|e| format!("{path}: the tab sent bytes that do not decode ({e})"))?;
                if chunk.is_empty() || bytes.len() + chunk.len() > MAX_FILE {
                    return Err(format!("{path}: the tab sent {} of {size} bytes", bytes.len()));
                }
                bytes.extend_from_slice(&chunk);
            }
            if let Some(parent) = Path::new(path).parent().and_then(Path::to_str).filter(|p| !p.is_empty()) {
                vfs::create_new_dir_all(parent).map_err(|e| format!("{parent}: {e}"))?;
            }
            // Whole or not at all: a copy cut short (a full disk, the server stopped) leaves the
            // file that was there, a saved design above all, as it was.
            vfs::check(path)
                .and_then(|()| solidcraft_engine::io::write_atomic(Path::new(path), &bytes, false))
                .map_err(|e| format!("{path}: {e}"))?;
            self.files.insert(path.to_string(), Entry { here: vfs::stamp(path), digest: digest.to_string() });
        }
        if !spent.is_empty() {
            for path in &spent {
                self.files.remove(path);
            }
            tab.file(json!({"fs": "remove", "paths": spent}))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_digest_is_fnv_1a() {
        // The published test vectors.
        assert_eq!(digest(b""), "cbf29ce484222325");
        assert_eq!(digest(b"a"), "af63dc4c8601ec8c");
        assert_eq!(digest(b"foobar"), "85944171f73967e8");
    }

    #[test]
    fn named_paths_are_the_absolute_strings_anywhere_in_the_params() {
        let root = if cfg!(windows) { "C:\\data" } else { "/data" };
        let (a, b) = (format!("{root}/a.step"), format!("{root}/b.png"));
        let params = json!({"command": "file.export", "params": {"path": a, "name": "relative.sc", "n": 3}, "commands": [{"params": {"overlay": b}}, {"params": {"path": a}}]});
        let mut found = named_paths(&params);
        found.sort();
        assert_eq!(found, vec![a, b]);
        assert!(named_paths(&json!("part.sc")).is_empty());
    }
}
