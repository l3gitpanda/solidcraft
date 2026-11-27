//! SolidCraft's web app with MCP beside it (docs/web-mcp.md).
//!
//! One server does three things on one port: it serves the built web app, it speaks MCP over
//! HTTP at `/mcp`, and it holds a WebSocket to each open tab at `/mcp-link`. A tool call from an
//! MCP client becomes control-channel requests, as in `solidcraft-cli mcp`, and each request goes
//! to the tab, which runs it in its own engine and draws the result on that computer's GPU: an
//! agent models while a person watches in the browser. The tools are `solidcraft-mcp`'s,
//! unchanged; this crate adds the HTTP transport ([`mcp`]) and the tab as their backend
//! ([`backend`], [`hub`], [`sync`]).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]
#![forbid(unsafe_code)]

pub mod backend;
pub mod cli;
pub mod http;
pub mod hub;
pub mod mcp;
pub mod server;
pub mod site;
pub mod sync;
pub mod ws;

pub use server::Config;
pub use server::Relay;

#[cfg(test)]
mod tests;
