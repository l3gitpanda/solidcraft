//! The designs in the agent's folders, for the person at the linked tab (docs/web-mcp.md,
//! "Recent designs"). A tab has no disk, so its Home page cannot see what agents saved here. The
//! relay lists those designs in its welcome, so Home shows them under Recent designs; sends a
//! design's file when the person opens one there (`{"fetch": …}`); and writes back a design the
//! person saves in the tab under these folders (`{"put": …}`), so the save is on disk at once,
//! not only after the next agent call.
//!
//! Everything here is checked against the agent's folder scope, as the files around a call are:
//! a tab never reads or writes beyond the folders the relay was given.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use serde_json::{Value, json};
use solidcraft_engine::io::vfs::PathScope;
use solidcraft_engine::io::{is_design_container_path, is_design_path};

use crate::hub::{Hub, Writer};
use crate::sync::{CHUNK, MAX_FILE, digest, publish};
use crate::ws;

/// The most designs a welcome lists; the newest are kept.
pub const MAX_LISTED: usize = 50;
/// How deep below a folder designs are looked for.
const MAX_DEPTH: usize = 4;
/// The most folder entries looked at, so a huge folder cannot hold up a tab's link.
const MAX_SCANNED: usize = 20_000;
/// The most uploads one tab may have under way at once.
const MAX_UPLOADS: usize = 8;

/// A design in the agent's folders: its path, spelled under the folder as the relay was given it
/// (as agents name it, and as the tab keeps it), and when it last changed (seconds since 1970).
#[derive(Debug, PartialEq, Eq)]
pub struct Design {
    pub path: String,
    pub modified: u64,
}

/// The designs under `folders`, newest first, at most [`MAX_LISTED`]. Hidden files and folders
/// are left out, and links to folders are not followed (no loops, nothing outside).
pub fn designs(folders: &[PathBuf]) -> Vec<Design> {
    fn walk(dir: &Path, depth: usize, scanned: &mut usize, out: &mut Vec<Design>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            *scanned += 1;
            if *scanned > MAX_SCANNED {
                return;
            }
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let path = entry.path();
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() {
                if depth < MAX_DEPTH {
                    walk(&path, depth + 1, scanned, out);
                }
                continue;
            }
            if !is_design_container_path(&path) {
                continue;
            }
            let Ok(meta) = std::fs::metadata(&path) else { continue };
            if !meta.is_file() {
                continue;
            }
            let modified = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
            out.push(Design { path: path.to_string_lossy().into_owned(), modified });
        }
    }
    let mut out = Vec::new();
    let mut scanned = 0;
    for folder in folders {
        walk(folder, 0, &mut scanned, &mut out);
    }
    out.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.path.cmp(&b.path)));
    out.dedup_by(|a, b| a.path == b.path);
    out.truncate(MAX_LISTED);
    out
}

/// The welcome a tab gets once its token is taken: its id, the agent's folders and the designs
/// in them.
pub fn welcome(tab: u64, folders: &[PathBuf]) -> Value {
    let listed: Vec<Value> = designs(folders).into_iter().map(|d| json!({"path": d.path, "modified": d.modified})).collect();
    let folders: Vec<String> = folders.iter().map(|f| f.to_string_lossy().into_owned()).collect();
    json!({"welcome": {"tab": tab, "folders": folders, "designs": listed}})
}

/// Whether a tab may read or write `path` here: a design, inside the agent's folders.
fn allowed(scope: Option<&PathScope>, path: &str, new: bool) -> Result<(), String> {
    let scope = scope.ok_or("this server was given no folder for designs")?;
    if !is_design_path(path) {
        return Err(format!("{path}: not a design"));
    }
    if new { scope.check_new(path) } else { scope.check(path) }.map_err(|e| e.to_string())
}

/// The modification time and size of a file here, as the copies around a call record it.
fn stamp(path: &str) -> Option<(SystemTime, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// Read the design a tab asked for.
fn read(scope: Option<&PathScope>, path: &str) -> Result<Vec<u8>, String> {
    allowed(scope, path, false)?;
    let meta = std::fs::metadata(path).map_err(|e| format!("{path}: {e}"))?;
    if !meta.is_file() {
        return Err(format!("{path}: not a file"));
    }
    if meta.len() > MAX_FILE as u64 {
        return Err(format!("{path}: larger than {} MiB", MAX_FILE >> 20));
    }
    std::fs::read(path).map_err(|e| format!("{path}: {e}"))
}

fn write(writer: &Writer, message: &Value) -> std::io::Result<()> {
    ws::write_text(&mut *writer.lock().unwrap_or_else(PoisonError::into_inner), &message.to_string())
}

/// Answer `{"fetch": {"path"}}`: the design's bytes as `{"file": {path, data, append}}` messages,
/// in pieces, then `{"fetched": {path}}`, or `{"fetched": {path, error}}` alone. Run on a thread
/// of its own, so the link goes on reading the tab's answers meanwhile.
pub fn fetch(hub: &Hub, tab: u64, writer: &Writer, scope: Option<&PathScope>, path: &str) -> std::io::Result<()> {
    let bytes = match read(scope, path) {
        Ok(bytes) => bytes,
        Err(error) => return write(writer, &json!({"fetched": {"path": path, "error": error}})),
    };
    let b64 = base64::engine::general_purpose::STANDARD;
    let mut chunks = bytes.chunks(CHUNK);
    write(writer, &json!({"file": {"path": path, "data": b64.encode(chunks.next().unwrap_or_default()), "append": false}}))?;
    for chunk in chunks {
        write(writer, &json!({"file": {"path": path, "data": b64.encode(chunk), "append": true}}))?;
    }
    // The tab now holds the file as it is here: the next call's copies must not take it for one
    // the tab wrote, and write it back over a newer file.
    hub.note(tab, path, stamp(path), digest(&bytes));
    write(writer, &json!({"fetched": {"path": path}}))
}

/// The designs a tab is uploading, by the tab's number for each: the path and the bytes so far,
/// or why the upload was refused (said once, when its last piece comes).
#[derive(Default)]
pub struct Uploads(Mutex<HashMap<u64, Result<(String, Vec<u8>), String>>>);

impl Uploads {
    /// Take one piece of `{"put": {ref, path, data, append, last}}`. On the last piece the design
    /// is written (whole, replacing the file only once complete), and the answer to send back is
    /// returned: `{"saved": {ref}}` or `{"saved": {ref, error}}`.
    pub fn put(&self, hub: &Hub, tab: u64, scope: Option<&PathScope>, put: &Value) -> Option<Value> {
        let number = put.get("ref").and_then(Value::as_u64)?;
        let path = put.get("path").and_then(Value::as_str).unwrap_or_default();
        let append = put.get("append").and_then(Value::as_bool).unwrap_or(false);
        let last = put.get("last").and_then(Value::as_bool).unwrap_or(true);
        let data = put.get("data").and_then(Value::as_str).unwrap_or_default();
        let piece = base64::engine::general_purpose::STANDARD.decode(data).map_err(|_| "a piece of the design is not base64".to_string());
        let mut uploads = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if !append {
            let started = if uploads.len() >= MAX_UPLOADS && !uploads.contains_key(&number) {
                Err("too many designs are being saved at once".to_string())
            } else {
                allowed(scope, path, true).and(piece).map(|bytes| (path.to_string(), bytes))
            };
            uploads.insert(number, started);
        } else if let Some(Ok((held, bytes))) = uploads.get_mut(&number) {
            let more = piece.and_then(|piece| {
                if held.as_str() == path && bytes.len() + piece.len() <= MAX_FILE {
                    Ok(piece)
                } else {
                    Err(format!("{path}: larger than {} MiB, or not the file the upload began with", MAX_FILE >> 20))
                }
            });
            match more {
                Ok(piece) => bytes.extend_from_slice(&piece),
                Err(e) => {
                    uploads.insert(number, Err(e));
                }
            }
        } else {
            uploads.entry(number).or_insert_with(|| Err("a piece came without the start of its design".to_string()));
        }
        if !last {
            return None;
        }
        let finished = uploads.remove(&number)?;
        drop(uploads);
        let written = finished.and_then(|(path, bytes)| {
            publish(&path, &bytes).map_err(|e| format!("{path}: {e}"))?;
            hub.note(tab, &path, stamp(&path), digest(&bytes));
            Ok(())
        });
        Some(match written {
            Ok(()) => json!({"saved": {"ref": number}}),
            Err(error) => json!({"saved": {"ref": number, "error": error}}),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn designs_are_listed_newest_first_without_hidden_files_or_other_kinds() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let at = |name: &str, secs: u64| {
            let path = root.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "x").unwrap();
            let file = std::fs::File::options().write(true).open(&path).unwrap();
            file.set_modified(UNIX_EPOCH + std::time::Duration::from_secs(secs)).unwrap();
            path.to_string_lossy().into_owned()
        };
        let old = at("old.sc", 1_000);
        let new = at("parts/new.sc", 3_000);
        at("parts/new.step", 4_000);
        at("notes.json", 5_000);
        at(".hidden/secret.sc", 6_000);
        at("a/b/c/d/e/too-deep.sc", 7_000);
        let listed = designs(std::slice::from_ref(&root));
        assert_eq!(listed, [Design { path: new, modified: 3_000 }, Design { path: old, modified: 1_000 }]);
        assert!(designs(&[root.join("nowhere")]).is_empty());
    }
}
