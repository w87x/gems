//! `Ordinal`: the dense `u32` position roaring bitmaps index over
//! (ARCHITECTURE.md §3 — "each distinct value maps to a bitmap of
//! *ordinal* entity positions, not TUIDs directly"). Assigned once per
//! entity at first insert and never reused, even after delete (a v1
//! simplification: ordinal space can have gaps, which costs nothing since
//! it's never serialized as a dense array).
//!
//! Defined as a newtype rather than implementing `gems_index`'s
//! `FixedCodec` directly for `u32` because Rust's orphan rule forbids
//! implementing a foreign trait (`FixedCodec`, from `gems-index`) for a
//! foreign type (`u32`) from a third crate (`gems-engine`) — a local
//! wrapper type sidesteps that.

use gems_index::node::FixedCodec;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Ordinal(pub u32);

impl FixedCodec for Ordinal {
    const LEN: usize = 4;

    fn encode_into(&self, out: &mut [u8]) {
        out.copy_from_slice(&self.0.to_le_bytes());
    }

    fn decode_from(bytes: &[u8]) -> Self {
        Ordinal(u32::from_le_bytes(bytes.try_into().unwrap()))
    }
}
