//! GBV (Gems Binary Value): the zero-copy field-map format used for data
//! entity bodies. See ARCHITECTURE.md §4. Encoding is a sorted directory of
//! `(key_id, type_tag, offset, len)` entries followed by raw value bytes;
//! reading a single field is a binary search plus a slice, with no parsing
//! of unrelated fields and no allocation.

pub mod gbv;

pub use gbv::{GbvBuilder, GbvReader, TypeTag};
