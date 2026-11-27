//! The browser tab as an MCP backend: where `solidcraft-mcp`'s tools end up when the app they
//! drive is the web app in a tab linked to this relay.
//!
//! It stands where `solidcraft_mcp::Remote` stands for the desktop app. A tool's control-channel
//! call (`engine.execute`, `ui.screenshot`, …) goes to the tab as one message and its reply comes
//! back as one; the tab runs it through the same queue its `window.solidcraft` API feeds. The
//! methods the desktop's loopback server answers itself (`control.running`, `control.pending`,
//! `control.capabilities`, `control.cancel`) are answered here, with the same replies.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use solidcraft_control::STILL_RUNNING;
use solidcraft_control::client::outcome;
use solidcraft_engine::control_pending;
use solidcraft_engine::io::vfs;
use solidcraft_mcp::Backend;
use solidcraft_mcp_server::ProgressReporter;

use crate::hub::{Hub, SendError, Ticket, Waited};
use crate::sync::{Synced, Tab};

/// How long the backend waits on the tab.
#[derive(Clone, Copy, Debug)]
pub struct Waits {
    /// For a request's reply, before the call returns `still running:`.
    pub call: Duration,
    /// For a request still running from an earlier call: before a new call, and per `pending`.
    pub settle: Duration,
    /// For one file message: the tab answers those at once, whatever it is doing.
    pub file: Duration,
    /// How often a wait reports progress and looks at its cancellation flag.
    pub step: Duration,
}

impl Default for Waits {
    fn default() -> Self {
        Waits { call: Duration::from_secs(240), settle: Duration::from_secs(60), file: Duration::from_secs(60), step: Duration::from_millis(250) }
    }
}

/// A request still running in the tab when its call returned.
struct Outstanding {
    ticket: Ticket,
    what: String,
    since: Instant,
    /// Whether its files are still to be brought back when it ends.
    syncs: bool,
}

/// What became of an outstanding request a later call waited out: kept for `pending`.
enum Settled {
    Finished { what: String, reply: Value },
    Lost { what: String },
}

/// Why a tool error says no tab is linked. It starts as `solidcraft-mcp`'s error for an app that
/// is not running does, so `batch` reports its steps as never sent, not as possibly run.
fn no_tab(url: &str) -> String {
    format!(
        "no SolidCraft app is listening at {url}: no browser tab is linked to this server. Open {url} in a browser, keep that tab \
         visible, and send the call again"
    )
}

/// The methods whose requests may read or write files.
fn touches_files(method: &str) -> bool {
    matches!(method, "engine.execute" | "command" | "engine.batch" | "engine.script" | "ui.render" | "ui.screenshot")
}

pub struct TabBackend {
    hub: Arc<Hub>,
    waits: Waits,
    /// Where a person opens the app, for the errors that ask them to.
    url: String,
    /// Set from outside when the MCP client cancels the call in progress.
    cancel: Arc<AtomicBool>,
    /// The agent's folders as they were spelled when given, beside the scope's resolved ones.
    folders: Vec<String>,
    progress: Option<ProgressReporter>,
    outstanding: Option<Outstanding>,
    settled: Option<Settled>,
}

/// One tab, as the file copies reach it.
struct Link<'a> {
    hub: &'a Hub,
    tab: u64,
    wait: Duration,
    step: Duration,
}

impl Tab for Link<'_> {
    fn id(&self) -> u64 {
        self.tab
    }

    fn file(&mut self, message: Value) -> Result<Value, String> {
        let what = format!("a file {}", message.get("fs").and_then(Value::as_str).unwrap_or("message"));
        let ticket = self.hub.send(self.tab, message, &what).map_err(|e| match e {
            SendError::NoTab => "the browser tab closed or reloaded while its files were being copied".to_string(),
            SendError::Write(e) => format!("the browser tab stopped answering while its files were being copied ({e})"),
        })?;
        match self.hub.wait(ticket, self.wait, self.step, |_| true) {
            Waited::Reply(reply) => outcome(&reply),
            Waited::Running => {
                self.hub.forget(ticket.id);
                Err(format!("the browser tab did not answer {what} within {} s", self.wait.as_secs()))
            }
            Waited::Lost => Err("the browser tab closed or reloaded while its files were being copied".to_string()),
        }
    }
}

impl TabBackend {
    pub fn new(hub: Arc<Hub>, url: impl Into<String>, cancel: Arc<AtomicBool>) -> Self {
        TabBackend { hub, waits: Waits::default(), url: url.into(), cancel, folders: Vec::new(), progress: None, outstanding: None, settled: None }
    }

    pub fn with_waits(mut self, waits: Waits) -> Self {
        self.waits = waits;
        self
    }

    pub fn with_folders(mut self, folders: impl IntoIterator<Item = std::path::PathBuf>) -> Self {
        self.folders = folders.into_iter().map(|f| f.to_string_lossy().into_owned()).collect();
        self
    }

    fn link(&self, tab: u64, wait: Duration) -> Link<'_> {
        Link { hub: &self.hub, tab, wait, step: self.waits.step }
    }

    /// The folders whose files come back from the tab: the agent's, as the call's scope has
    /// them and as they were spelled when given. The tab keeps a file under the path it was
    /// told, so both spellings are asked for; the scope still decides what is written here.
    fn roots(&self, scratch: Option<&str>) -> Vec<String> {
        let mut roots: Vec<String> = vfs::current_scope()
            .map_or_else(Vec::new, |scope| scope.roots().iter().chain(scope.design_roots()).map(|r| r.to_string_lossy().into_owned()).collect());
        for folder in self.folders.iter().map(String::as_str).chain(scratch) {
            if !roots.iter().any(|r| r == folder) {
                roots.push(folder.to_string());
            }
        }
        roots
    }

    /// Bring back what a finished request wrote, from the tab that ran it.
    fn pull(&self, synced: &mut Synced, tab: u64) -> Result<(), String> {
        let scratch = solidcraft_mcp::scratch_dir().map(|d| d.to_string_lossy().into_owned());
        synced.pull(&mut self.link(tab, self.waits.file), &self.roots(scratch.as_deref()), scratch.as_deref())
    }

    /// Wait for `ticket` up to `timeout`, reporting progress and minding the cancellation flag.
    /// The second value: the wait was cancelled.
    fn wait(&self, ticket: Ticket, what: &str, since: Instant, timeout: Duration) -> (Waited, bool) {
        let mut reported = 0;
        let mut cancelled = false;
        let waited = self.hub.wait(ticket, timeout, self.waits.step, |_| {
            if self.cancel.load(Ordering::Relaxed) {
                cancelled = true;
                return false;
            }
            let secs = since.elapsed().as_secs();
            // Every ten seconds: enough to show life, and to keep a proxy from closing the stream.
            if secs / 10 > reported {
                reported = secs / 10;
                if let Some(progress) = &self.progress {
                    let hidden =
                        if self.hub.hidden(ticket.tab) { "; the tab is hidden, so the browser has paused it: bring it to the front" } else { "" };
                    progress.report(secs as f64, None, Some(&format!("{what} is running in the browser tab ({secs} s{hidden})")));
                }
            }
            true
        });
        (waited, cancelled)
    }

    /// The error for a request still running after its wait, in the desktop bridge's words.
    fn still_running(&self, o: &Outstanding, earlier: bool) -> String {
        let secs = o.since.elapsed().as_secs();
        let hidden = if self.hub.hidden(o.ticket.tab) {
            " The tab is hidden, and a browser pauses a hidden tab: bring it to the front (a window of its own is enough)."
        } else {
            ""
        };
        if earlier {
            format!(
                "{STILL_RUNNING} an earlier request, {}, is still running in the browser tab ({secs} s; it was not cancelled and may still \
                 change the design when it finishes), so this call was not sent. Call the `pending` tool to wait for its result, then send \
                 this call again.{hidden}",
                o.what
            )
        } else {
            format!(
                "{STILL_RUNNING} {} is still running in the browser tab after {secs} s (it was not cancelled and may still change the \
                 design when it finishes); don't send it again. Call the `pending` tool to wait for its result (up to {} s per call); \
                 other calls wait for it first.{hidden}",
                o.what,
                self.waits.settle.as_secs()
            )
        }
    }

    /// The reply of a request that ended after its call returned, with its files brought back. A
    /// request that ran but whose files could not be copied did not do what was asked: its reply
    /// becomes an error that says so.
    fn collect(&self, o: &Outstanding, reply: Value) -> Value {
        if o.syncs
            && let Err(e) = self.pull(&mut self.hub.turn(), o.ticket.tab)
            && reply.get("ok").and_then(Value::as_bool) == Some(true)
        {
            return json!({"ok": false, "error": format!("it ran, but its files could not be copied from the browser tab: {e}")});
        }
        reply
    }

    /// Before a new request: wait out one still running from an earlier call, keeping its end for
    /// `pending`. Still running after the wait: the new request is not sent.
    fn settle_earlier(&mut self) -> Result<(), String> {
        let Some(o) = self.outstanding.take() else { return Ok(()) };
        match self.wait(o.ticket, &o.what, o.since, self.waits.settle) {
            (Waited::Reply(reply), _) => {
                // Its files, late. A failure to copy them is the earlier request's, not this
                // call's: it is kept with the reply for `pending` to report.
                let reply = self.collect(&o, reply);
                self.settled = Some(Settled::Finished { what: o.what, reply });
                Ok(())
            }
            (Waited::Lost, _) => {
                self.settled = Some(Settled::Lost { what: o.what });
                Ok(())
            }
            (Waited::Running, true) => {
                let error = format!("cancelled: the wait for {} was cancelled by the client", o.what);
                self.outstanding = Some(o);
                Err(error)
            }
            (Waited::Running, false) => {
                let error = self.still_running(&o, true);
                self.outstanding = Some(o);
                Err(error)
            }
        }
    }

    /// `control.pending`: the end of the request still running from an earlier call, waited for
    /// up to the settle wait: `{pending: what | null, done?, reply?, lost?, running_s?}`.
    fn pending(&mut self) -> Value {
        if let Some(settled) = self.settled.take() {
            return match settled {
                Settled::Finished { what, reply } => json!({"pending": what, "done": true, "reply": reply}),
                Settled::Lost { what } => json!({"pending": what, "lost": true}),
            };
        }
        let Some(o) = self.outstanding.take() else { return json!({"pending": null}) };
        match self.wait(o.ticket, &o.what, o.since, self.waits.settle) {
            (Waited::Reply(reply), _) => json!({"pending": o.what, "done": true, "reply": self.collect(&o, reply)}),
            (Waited::Lost, _) => json!({"pending": o.what, "lost": true}),
            (Waited::Running, cancelled) => {
                let reply = match cancelled {
                    true => json!({"pending": o.what, "done": false, "cancelled": true}),
                    false => json!({"pending": o.what, "done": false, "running_s": o.since.elapsed().as_secs()}),
                };
                self.outstanding = Some(o);
                reply
            }
        }
    }

    /// Send `method` to the tab and wait for its result, with its files copied around it.
    fn forward(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.settle_earlier()?;
        let what = control_pending::describe(method, &params);
        let syncs = touches_files(method);
        let hub = self.hub.clone();
        let mut synced = hub.turn();
        // The tab driven now is this call's tab to its end: its files go there, its request runs
        // there and its files come back from there, whatever links meanwhile.
        let Some(tab) = self.hub.active() else { return Err(no_tab(&self.url)) };
        if syncs {
            synced.push(&mut self.link(tab, self.waits.file), &params).map_err(|e| format!("{what} was not sent: {e}"))?;
        }
        let ticket = self.hub.send(tab, json!({"method": method, "params": params}), &what).map_err(|e| match e {
            SendError::NoTab => no_tab(&self.url),
            SendError::Write(e) => format!("{}; the tab's link broke as the call was sent ({e})", no_tab(&self.url)),
        })?;
        let since = Instant::now();
        match self.wait(ticket, &what, since, self.waits.call) {
            (Waited::Reply(reply), _) => {
                let result = outcome(&reply)?;
                if syncs {
                    self.pull(&mut synced, tab).map_err(|e| format!("{what} ran, but its files could not be copied from the browser tab: {e}"))?;
                }
                Ok(result)
            }
            (Waited::Lost, _) => Err(format!(
                "the browser tab closed or reloaded while {what} was running, and its result with it: inspect the design (inspect_design) \
                 to see whether it applied before sending it again"
            )),
            (Waited::Running, cancelled) => {
                let o = Outstanding { ticket, what, since, syncs };
                let error = match cancelled {
                    true => format!(
                        "cancelled: the wait for {} was cancelled by the client. The browser tab still runs it, and it may still change \
                         the design when it finishes; the `pending` tool waits for its result",
                        o.what
                    ),
                    false => self.still_running(&o, false),
                };
                self.outstanding = Some(o);
                Err(error)
            }
        }
    }
}

impl Backend for TabBackend {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        match method {
            "control.running" => Ok(json!({"running": self.hub.running()})),
            "control.pending" => Ok(self.pending()),
            // No request level is checked in the tab, and plugins' tools are not looked up there.
            "control.capabilities" => Ok(json!({"param_check": [], "plugin_tools": false})),
            "control.cancel" => Err("cannot be cancelled: the browser tab runs each request to its end on the page's one thread".into()),
            _ => self.forward(method, params),
        }
    }

    fn has_ui(&self) -> bool {
        true
    }

    fn describe(&self) -> String {
        format!("the SolidCraft web app in the browser tab linked at {}", self.url)
    }

    fn notice(&self) -> Option<String> {
        self.hub.active_hidden().then(|| {
            "The linked browser tab is hidden, and a browser pauses a hidden tab: calls wait until it is brought to the front (a window of its own is enough)."
                .to_string()
        })
    }

    fn set_progress(&mut self, progress: Option<ProgressReporter>) {
        self.progress = progress;
    }
}
