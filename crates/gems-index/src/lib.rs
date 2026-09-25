//! The primary/secondary index engine: a fixed-width-key B-tree over a
//! paged, mmap'd file, with copy-on-write updates published by a single
//! atomic root-pointer write. See ARCHITECTURE.md §2 for the design and
//! the scope notes at the top of `btree.rs` for what v1 does and doesn't
//! do yet (plain splits instead of full B* redistribution, no rebalancing
//! on delete).

pub mod btree;
mod codec;
pub mod node;
pub mod pager;

pub use btree::BTree;
pub use pager::Pager;
