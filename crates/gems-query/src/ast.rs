//! The query AST. `Query` is what `parser::parse` produces; a planner
//! (future work, see the crate doc) walks `filter` to compile it into
//! bitmap set operations.

#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    pub select: SelectList,
    /// The source name after `FROM` — always `"entities"` today, kept as a
    /// field rather than hardcoded so a future planner can validate it
    /// (and so the grammar doesn't have to special-case the one keyword).
    pub from: String,
    pub filter: Option<Expr>,
    pub order_by: Vec<OrderItem>,
    pub limit: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SelectList {
    All,
    Fields(Vec<String>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrderItem {
    pub field: String,
    pub descending: bool,
}

/// A field reference, kept as its full dotted path (e.g. `"layer.layergroup"`,
/// `"attr.status"`, or a bare built-in like `"type"`/`"modified_at"`). A
/// planner is what gives the leading segment meaning (`attr.` = a data
/// field looked up through the entity's `EntityType`, `layer.` = a
/// relation, no dot = a header/built-in field) — the parser doesn't
/// interpret it.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
    Compare {
        field: String,
        op: CompareOp,
        value: Literal,
    },
    In {
        field: String,
        values: Vec<Literal>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOp {
    Eq,
    Neq,
    Lt,
    Lte,
    Gt,
    Gte,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Str(String),
    Num(f64),
    Bool(bool),
    /// A bare, unquoted identifier used as a value — e.g. the `xx, yy, zz`
    /// in `type IN (xx, yy, zz)` from the ARCHITECTURE.md §7 example,
    /// where entity type names are written unquoted.
    Ident(String),
}
