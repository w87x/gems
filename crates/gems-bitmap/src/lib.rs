//! A trimmed roaring bitmap: a `u32` domain, split into 65536-value
//! "chunks" keyed by the high 16 bits, each chunk stored as either a
//! sorted array of low 16-bit values (small chunks) or a dense 8 KiB
//! bitset (large chunks). See ARCHITECTURE.md §3 for how this is used —
//! one bitmap per distinct value of a bitmap-indexed field, over *ordinal*
//! positions rather than TUIDs directly.
//!
//! This implements the standard roaring container model's two simplest
//! containers (array + bitmap); the upstream format additionally has a
//! run-length container for long contiguous ranges, which is skipped here
//! as a deliberate v1 scope cut — worth adding once a workload shows
//! long runs are common (sequential ordinal assignment during a bulk load
//! is exactly that case), but not needed for the set operations the query
//! planner needs today.

mod container;
mod roaring;

pub use roaring::RoaringBitmap;
