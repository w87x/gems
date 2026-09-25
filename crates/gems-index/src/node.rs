//! Leaf/internal node encoding within a fixed-size page. Keys and values
//! are fixed-width (the `FixedCodec` trait), which keeps node capacity a
//! simple function of page size and lets us binary-search a page without
//! parsing it first.
//!
//! Leaf page:
//! `u8 node_type(1) | u16 key_count | (key,value)*`
//!
//! Internal page:
//! `u8 node_type(2) | u16 key_count | (key)* | (child: u32)*`  — n keys, n+1 children.
//!
//! Leaves deliberately do **not** carry a sibling ("next leaf") pointer.
//! Under copy-on-write, replacing a leaf changes its page id; a sibling
//! chain would require also rewriting the *left* sibling's next-pointer to
//! follow it, which cascades into rewriting an otherwise-untouched
//! subtree on every update — defeating the point of only copying the
//! root-to-leaf path. `BTree::scan_all` instead walks the tree structure
//! (recurse into every child of an internal node, in order) to produce a
//! sorted traversal; a real streaming range-cursor can revisit this once
//! there's a workload that needs O(1) leaf-to-leaf stepping rather than a
//! full materialized scan.

use gems_common::{Error, Result};

pub const LEAF: u8 = 1;
pub const INTERNAL: u8 = 2;

const LEAF_HEADER_LEN: usize = 1 + 2;
const INTERNAL_HEADER_LEN: usize = 1 + 2;

/// A fixed-width key or value that can be encoded into/decoded from a byte
/// slice of exactly `LEN` bytes. Implemented for `Tuid` (index keys) and
/// `SlotPointer` (index values) in `codec.rs`.
pub trait FixedCodec: Ord + Clone {
    const LEN: usize;
    fn encode_into(&self, out: &mut [u8]);
    fn decode_from(bytes: &[u8]) -> Self;
}

pub fn leaf_capacity(page_size: usize, key_len: usize, value_len: usize) -> usize {
    (page_size - LEAF_HEADER_LEN) / (key_len + value_len)
}

pub fn internal_capacity(page_size: usize, key_len: usize) -> usize {
    // n*key_len + (n+1)*4 <= page_size - header
    let budget = page_size - INTERNAL_HEADER_LEN;
    if budget < 4 {
        return 0;
    }
    (budget - 4) / (key_len + 4)
}

pub struct LeafView<K, V> {
    pub entries: Vec<(K, V)>,
}

impl<K: FixedCodec, V: FixedCodec> LeafView<K, V> {
    pub fn decode(buf: &[u8]) -> Result<Self> {
        if buf.is_empty() || buf[0] != LEAF {
            return Err(Error::CorruptPage {
                detail: "expected leaf node",
            });
        }
        if buf.len() < 3 {
            return Err(Error::CorruptPage {
                detail: "leaf node shorter than its header",
            });
        }
        let key_count = u16::from_le_bytes(buf[1..3].try_into().unwrap()) as usize;
        // `key_count` is an on-disk byte an OS crash mid-write or disk
        // corruption can leave inconsistent with the rest of the page (no
        // per-page checksum exists to catch it earlier, unlike
        // `EntityHeader`'s crc32c). Without this check, a corrupted count
        // bigger than what the page could actually hold walks `pos` past
        // `buf.len()` and panics on the first out-of-bounds slice — this
        // check turns that into a normal `CorruptPage` error instead.
        let max_entries = (buf.len() - LEAF_HEADER_LEN) / (K::LEN + V::LEN);
        if key_count > max_entries {
            return Err(Error::CorruptPage {
                detail: "leaf node key_count exceeds what the page could hold",
            });
        }
        let mut entries = Vec::with_capacity(key_count);
        let mut pos = LEAF_HEADER_LEN;
        for _ in 0..key_count {
            let key = K::decode_from(&buf[pos..pos + K::LEN]);
            pos += K::LEN;
            let value = V::decode_from(&buf[pos..pos + V::LEN]);
            pos += V::LEN;
            entries.push((key, value));
        }
        Ok(LeafView { entries })
    }
}

pub fn encode_leaf<K: FixedCodec, V: FixedCodec>(out: &mut [u8], entries: &[(K, V)]) {
    out.fill(0);
    out[0] = LEAF;
    out[1..3].copy_from_slice(&(entries.len() as u16).to_le_bytes());
    let mut pos = LEAF_HEADER_LEN;
    for (k, v) in entries {
        k.encode_into(&mut out[pos..pos + K::LEN]);
        pos += K::LEN;
        v.encode_into(&mut out[pos..pos + V::LEN]);
        pos += V::LEN;
    }
}

pub struct InternalView<K> {
    pub keys: Vec<K>,
    pub children: Vec<u32>,
}

impl<K: FixedCodec> InternalView<K> {
    pub fn decode(buf: &[u8]) -> Result<Self> {
        if buf.is_empty() || buf[0] != INTERNAL {
            return Err(Error::CorruptPage {
                detail: "expected internal node",
            });
        }
        if buf.len() < 3 {
            return Err(Error::CorruptPage {
                detail: "internal node shorter than its header",
            });
        }
        let key_count = u16::from_le_bytes(buf[1..3].try_into().unwrap()) as usize;
        // Same reasoning as `LeafView::decode`: validate the on-disk count
        // against what the page could actually hold (n keys + (n+1)
        // 4-byte children) before using it to walk `pos`, so a corrupted
        // page returns `CorruptPage` instead of panicking on an
        // out-of-bounds slice.
        let budget = buf.len().saturating_sub(INTERNAL_HEADER_LEN + 4);
        let max_keys = budget / (K::LEN + 4);
        if key_count > max_keys {
            return Err(Error::CorruptPage {
                detail: "internal node key_count exceeds what the page could hold",
            });
        }
        let mut pos = INTERNAL_HEADER_LEN;
        let mut keys = Vec::with_capacity(key_count);
        for _ in 0..key_count {
            keys.push(K::decode_from(&buf[pos..pos + K::LEN]));
            pos += K::LEN;
        }
        let mut children = Vec::with_capacity(key_count + 1);
        for _ in 0..=key_count {
            children.push(u32::from_le_bytes(buf[pos..pos + 4].try_into().unwrap()));
            pos += 4;
        }
        Ok(InternalView { keys, children })
    }
}

pub fn encode_internal<K: FixedCodec>(out: &mut [u8], keys: &[K], children: &[u32]) {
    debug_assert_eq!(children.len(), keys.len() + 1);
    out.fill(0);
    out[0] = INTERNAL;
    out[1..3].copy_from_slice(&(keys.len() as u16).to_le_bytes());
    let mut pos = INTERNAL_HEADER_LEN;
    for k in keys {
        k.encode_into(&mut out[pos..pos + K::LEN]);
        pos += K::LEN;
    }
    for &c in children {
        out[pos..pos + 4].copy_from_slice(&c.to_le_bytes());
        pos += 4;
    }
}

pub fn node_type(buf: &[u8]) -> Result<u8> {
    if buf.is_empty() {
        return Err(Error::CorruptPage {
            detail: "empty node buffer",
        });
    }
    Ok(buf[0])
}

#[cfg(test)]
mod tests {
    use super::*;
    use gems_common::Tuid;
    use gems_storage::SlotPointer;

    const PAGE_SIZE: usize = 256;

    fn slot(slot_index: u32) -> SlotPointer {
        SlotPointer {
            file_id: 1,
            extent_index: 0,
            slot_index,
            block_class: 0,
        }
    }

    #[test]
    fn leaf_roundtrip() {
        let mut buf = vec![0u8; PAGE_SIZE];
        let entries = vec![
            (Tuid::new([1u8; 16], 1), slot(0)),
            (Tuid::new([2u8; 16], 2), slot(1)),
        ];
        encode_leaf(&mut buf, &entries);
        let decoded = LeafView::<Tuid, SlotPointer>::decode(&buf).unwrap();
        assert_eq!(decoded.entries, entries);
    }

    #[test]
    fn leaf_decode_rejects_a_key_count_bigger_than_the_page_could_hold() {
        // Corrupt just the key_count field to claim far more entries than
        // a page this size could ever store — the shape a torn write or
        // bit-rot could produce. Before the fix this walked `pos` past
        // `buf.len()` and panicked on the first out-of-bounds slice.
        let mut buf = vec![0u8; PAGE_SIZE];
        buf[0] = LEAF;
        buf[1..3].copy_from_slice(&u16::MAX.to_le_bytes());
        assert!(LeafView::<Tuid, SlotPointer>::decode(&buf).is_err());
    }

    #[test]
    fn internal_roundtrip() {
        let mut buf = vec![0u8; PAGE_SIZE];
        let keys = vec![Tuid::new([1u8; 16], 1), Tuid::new([2u8; 16], 2)];
        let children = vec![10u32, 20, 30];
        encode_internal(&mut buf, &keys, &children);
        let decoded = InternalView::<Tuid>::decode(&buf).unwrap();
        assert_eq!(decoded.keys, keys);
        assert_eq!(decoded.children, children);
    }

    #[test]
    fn internal_decode_rejects_a_key_count_bigger_than_the_page_could_hold() {
        let mut buf = vec![0u8; PAGE_SIZE];
        buf[0] = INTERNAL;
        buf[1..3].copy_from_slice(&u16::MAX.to_le_bytes());
        assert!(InternalView::<Tuid>::decode(&buf).is_err());
    }
}
