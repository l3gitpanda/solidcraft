//! The browser tabs linked to the relay, and the requests in flight to them.
//!
//! Each tab holds one WebSocket open. The tab that linked last is the one driven: a person who
//! opens the app in a new window means that window. A request is one text message
//! `{"id", …}`; the tab answers with a message carrying the same `id`. A tab that goes away takes
//! its unanswered requests with it, and their callers are told so at once.

use std::collections::HashMap;
use std::net::TcpStream;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::sync::Synced;
use crate::ws;

/// Where the relay writes a tab's messages: its socket, one writer at a time.
pub type Writer = Arc<Mutex<TcpStream>>;

/// A linked tab.
struct Tab {
    id: u64,
    writer: Writer,
    /// Whether the page is hidden (a background tab, a minimised window): the browser pauses its
    /// frames then, and with them the requests.
    hidden: bool,
    /// The app's version, as its hello said.
    version: String,
    since: Instant,
}

/// A request sent to a tab and not yet collected by its caller.
struct Flight {
    tab: u64,
    what: String,
    since: Instant,
    reply: Option<Value>,
    /// The tab went away before answering.
    lost: bool,
}

#[derive(Default)]
struct State {
    tabs: Vec<Tab>,
    last_tab: u64,
    last_request: u64,
    flights: HashMap<u64, Flight>,
}

impl State {
    fn active(&self) -> Option<&Tab> {
        self.tabs.last()
    }
}

/// How a wait for a reply ended.
#[derive(Debug)]
pub enum Waited {
    Reply(Value),
    /// Nothing yet when the wait ran out; the request stays in flight.
    Running,
    /// The tab closed or reloaded before answering.
    Lost,
}

/// A request in flight: its id, and the tab it went to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ticket {
    pub id: u64,
    pub tab: u64,
}

/// Why a request was not sent.
#[derive(Debug, PartialEq, Eq)]
pub enum SendError {
    NoTab,
    /// The tab's socket refused the message; the tab is dropped.
    Write(String),
}

#[derive(Default)]
pub struct Hub {
    state: Mutex<State>,
    moved: Condvar,
    /// Held across one call's file copies and its request, so two agents' copies do not mix;
    /// it holds what has been copied so far.
    turn: Mutex<Synced>,
}

impl Hub {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// One caller at a time copies files and sends its request.
    pub fn turn(&self) -> MutexGuard<'_, Synced> {
        self.turn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A tab linked: it is the one driven from now on.
    pub fn attach(&self, writer: Writer, version: String, hidden: bool) -> u64 {
        let mut state = self.state();
        state.last_tab += 1;
        let id = state.last_tab;
        state.tabs.push(Tab { id, writer, hidden, version, since: Instant::now() });
        self.moved.notify_all();
        id
    }

    /// A tab's link ended: its unanswered requests are lost.
    pub fn detach(&self, tab: u64) {
        let mut state = self.state();
        state.tabs.retain(|t| t.id != tab);
        for flight in state.flights.values_mut().filter(|f| f.tab == tab && f.reply.is_none()) {
            flight.lost = true;
        }
        self.moved.notify_all();
    }

    /// A tab said whether its page is hidden.
    pub fn set_hidden(&self, tab: u64, hidden: bool) {
        if let Some(t) = self.state().tabs.iter_mut().find(|t| t.id == tab) {
            t.hidden = hidden;
        }
    }

    /// A tab's answer to request `id`. An answer nobody waits for (its caller gave up, or the id
    /// was never sent to this tab) is dropped.
    pub fn deliver(&self, tab: u64, id: u64, reply: Value) {
        let mut state = self.state();
        if let Some(flight) = state.flights.get_mut(&id).filter(|f| f.tab == tab && f.reply.is_none()) {
            flight.reply = Some(reply);
            self.moved.notify_all();
        }
    }

    /// The driven tab's id, if a tab is linked.
    pub fn active(&self) -> Option<u64> {
        self.state().active().map(|t| t.id)
    }

    /// Whether the driven tab's page is hidden.
    pub fn active_hidden(&self) -> bool {
        self.state().active().is_some_and(|t| t.hidden)
    }

    /// Send `message` (an object; its `id` is set here) to the driven tab. `what` names the
    /// request for people (`control.running`, the errors).
    pub fn send(&self, mut message: Value, what: &str) -> Result<Ticket, SendError> {
        let (ticket, writer) = {
            let mut state = self.state();
            let Some((tab, writer)) = state.active().map(|t| (t.id, t.writer.clone())) else { return Err(SendError::NoTab) };
            state.last_request += 1;
            let id = state.last_request;
            state.flights.insert(id, Flight { tab, what: what.to_string(), since: Instant::now(), reply: None, lost: false });
            (Ticket { id, tab }, writer)
        };
        if let Some(o) = message.as_object_mut() {
            o.insert("id".into(), json!(ticket.id));
        }
        let written = {
            let mut socket = writer.lock().unwrap_or_else(PoisonError::into_inner);
            ws::write_text(&mut *socket, &message.to_string())
        };
        if let Err(e) = written {
            self.forget(ticket.id);
            // A socket that takes no more is a tab that is gone; its reader will say so too.
            self.detach(ticket.tab);
            return Err(SendError::Write(e.to_string()));
        }
        Ok(ticket)
    }

    /// Wait up to `timeout` for the answer to `ticket`, in steps of at most `step`, calling
    /// `tick` after each step (progress, cancellation); `tick` answers false to stop waiting.
    /// A reply or a loss ends the flight; a request still running stays in flight.
    pub fn wait(&self, ticket: Ticket, timeout: Duration, step: Duration, mut tick: impl FnMut(Duration) -> bool) -> Waited {
        let started = Instant::now();
        let mut state = self.state();
        loop {
            match state.flights.get(&ticket.id) {
                None => return Waited::Lost,
                Some(f) if f.reply.is_some() => {
                    return match state.flights.remove(&ticket.id).and_then(|f| f.reply) {
                        Some(reply) => Waited::Reply(reply),
                        None => Waited::Lost,
                    };
                }
                Some(f) if f.lost => {
                    state.flights.remove(&ticket.id);
                    return Waited::Lost;
                }
                Some(_) => {}
            }
            let elapsed = started.elapsed();
            if elapsed >= timeout {
                return Waited::Running;
            }
            let (next, _) = self.moved.wait_timeout(state, step.min(timeout - elapsed)).unwrap_or_else(PoisonError::into_inner);
            state = next;
            drop(state);
            if !tick(started.elapsed()) {
                return Waited::Running;
            }
            state = self.state();
        }
    }

    /// Stop keeping a flight (its caller will not ask for it again).
    pub fn forget(&self, id: u64) {
        self.state().flights.remove(&id);
    }

    /// The requests the tabs have not answered yet, oldest first, as `control.running` lists them.
    pub fn running(&self) -> Value {
        let state = self.state();
        let mut flights: Vec<(&u64, &Flight)> = state.flights.iter().filter(|(_, f)| f.reply.is_none() && !f.lost).collect();
        flights.sort_by_key(|(id, _)| **id);
        json!(flights.iter().map(|(_, f)| json!({"what": f.what, "running_s": f.since.elapsed().as_secs()})).collect::<Vec<_>>())
    }

    /// What `/mcp-link/status` shows: how many tabs are linked, and the driven one.
    pub fn status(&self) -> Value {
        let state = self.state();
        json!({
            "tabs": state.tabs.len(),
            "active": state.active().map(|t| json!({"version": t.version, "hidden": t.hidden, "linked_s": t.since.elapsed().as_secs()})),
            "running": state.flights.values().filter(|f| f.reply.is_none() && !f.lost).count(),
        })
    }

    /// Every linked tab's writer, for the keep-alive pings.
    pub fn writers(&self) -> Vec<(u64, Writer)> {
        self.state().tabs.iter().map(|t| (t.id, t.writer.clone())).collect()
    }
}
