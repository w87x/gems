//! A minimal, hand-rolled JSON value type, parser, and serializer —
//! shared by `gems-webui` (HTTP request/response bodies) and `gems-mcp`
//! (JSON-RPC framing), consistent with this workspace's "hand-roll rather
//! than add a dependency" approach elsewhere (the CLI's own arg parser,
//! `gems-query`'s own lexer/parser).
//!
//! **Scope for this pass**: strict-enough JSON (objects, arrays, strings
//! with standard escapes and `\uXXXX`, numbers as `f64`, `true`/`false`/
//! `null`), preserving object key order (a `Vec<(String, Value)>`, not a
//! `HashMap`) since JSON-RPC and REST APIs both care about that for
//! readability even though the spec doesn't require it. Not implemented:
//! surrogate-pair validation beyond what `char::from_u32` already rejects,
//! and streaming/incremental parsing — this parses a complete in-memory
//! string, which is what both callers need (an HTTP request body read
//! fully before parsing, an MCP JSON-RPC message framed by `Content-
//! Length` before parsing).

mod parse;
mod value;
mod write;

pub use parse::{parse, ParseError};
pub use value::Value;
