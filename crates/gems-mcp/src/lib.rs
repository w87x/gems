//! `gems-mcp`: an MCP (Model Context Protocol) server over stdio, per
//! ARCHITECTURE.md §9 — "JSON-RPC over stdio (and optionally HTTP),
//! exposing tools like `query`, `get_entity`, `list_entity_types`." Uses
//! `gems-json` (this workspace's own hand-rolled JSON) rather than a
//! JSON-RPC crate, consistent with everything else here.
//!
//! Split into a testable core (this crate's lib) and a thin stdio loop
//! (`main.rs`, one line in, one line out — the standard MCP stdio
//! transport framing: newline-delimited JSON-RPC 2.0 messages, no
//! `Content-Length` headers). `dispatch` takes one already-parsed request
//! line and returns the response line to write, so the protocol logic
//! itself never touches `std::io`.
//!
//! **Scope for this pass**: three tools (`query`, `get_entity`,
//! `list_entity_types`), each opening the `gems_engine::Store` named by
//! its `store_dir` argument fresh on every call rather than holding a
//! long-lived, cached handle — simple and correct, at the cost of paying
//! `Store::open`'s cost on every tool call. Fine for how infrequently an
//! MCP client actually calls a tool; worth revisiting if that stops being
//! true.
//!
//! **Authentication is required by default** — see `tools.rs`'s module
//! doc. Every tool call must carry a valid `auth_token` argument, unless
//! the server was started with `--insecure` (see `main.rs`).

mod protocol;
mod tools;

pub use protocol::dispatch;
