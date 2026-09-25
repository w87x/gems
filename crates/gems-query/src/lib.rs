//! The SQL-subset query language from ARCHITECTURE.md §7: a hand-written
//! lexer + recursive-descent parser (no parser-generator dependency,
//! consistent with the rest of this workspace) producing an AST.
//!
//! This crate is deliberately scoped to lexing/parsing only. The planner
//! (compiling `Expr` into roaring-bitmap set operations over
//! `gems-bitmap`/`gems-index`, per §7) and the ABAC enforcement point
//! (§8) both need a live catalog/index to compile against, which belongs
//! in a higher-level engine crate that ties `gems-storage`, `gems-index`,
//! `gems-bitmap`, and `gems-catalog` together — a separate, larger piece
//! of work than the language itself.

pub mod ast;
mod lexer;
mod parser;

pub use ast::{CompareOp, Expr, Literal, OrderItem, Query, SelectList};
pub use parser::{parse, ParseError};
