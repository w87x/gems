//! Change notifications: "entity Z deleted," "entity X's attribute Y
//! changed," and "count(entities matching P) changed," all driven by the
//! `LogRecord` stream `gems-cluster`'s replication layer already produces.
//! A subscriber is architecturally a replica that doesn't write to a
//! `gems_engine::Store` — it evaluates a predicate against each record
//! instead of applying it, which is why this crate depends on
//! `gems-cluster` for `LogRecord`/`ReplicationLog` rather than
//! duplicating them.
//!
//! Two tiers, deliberately different in cost and kept as separate engines:
//!
//! - **Point-level** (`watch`): "did this specific entity get deleted, or
//!   this specific attribute change." Cheap — a `LogRecord::Delete` or a
//!   diff against one cached prior value, no scanning.
//! - **Aggregate** (`view`): "did the count of entities matching this
//!   predicate change." Genuinely harder — incremental view maintenance,
//!   tracking a matching set so a membership transition can be detected at
//!   all, not just event filtering. `COUNT` only and a single-condition
//!   predicate in this pass; see `view`'s module doc for why.
//!
//! `hub::NotificationHub` is the entry point most callers want: it owns
//! both engines and turns a view's count change into notifications for
//! any subscription watching that view. `tailer::LogTailer` is the thin,
//! I/O-owning shell that reads a `ReplicationLog` file and drives a hub —
//! everything else in this crate is a pure function of its inputs, same
//! design pattern as `gems_cluster::raft`/`gossip`, and for the same
//! reason: deterministic tests instead of ones that depend on real time
//! or real sockets.

mod hub;
mod tailer;
mod view;
mod watch;

pub use hub::NotificationHub;
pub use tailer::LogTailer;
pub use view::{ViewEngine, ViewId, ViewPredicate};
pub use watch::{Notification, NotificationKind, SubscriptionEngine, SubscriptionId, Watch};
