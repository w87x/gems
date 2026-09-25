//! Extent-file storage engine: fixed 16 MiB extents, each dedicated to one
//! block-size class and prefixed by a 4 KiB allocation bitmap. See
//! ARCHITECTURE.md §1 for the full rationale. This crate owns the
//! file/mmap plumbing (via `rustix`) and the slab allocator; it knows
//! nothing about entities, indexes, or queries — those live in
//! `gems-index` and `gems-catalog`.

pub mod extent;
pub mod file;
pub mod slot;

pub use extent::{BlockClass, ExtentHeader, EXTENT_SIZE};
pub use slot::SlotPointer;
