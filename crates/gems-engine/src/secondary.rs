//! `SecondaryIndex`: a bitmap-indexed field, per ARCHITECTURE.md §3 —
//! `HashMap<value bytes, RoaringBitmap of ordinals>`. Kept in memory only
//! for this pass rather than persisted to its own extent file; `Store`
//! rebuilds every index from a full scan on `open()`, which is fine at the
//! scale this crate is tested at and is a clearly separable piece of work
//! (giving `RoaringBitmap` an on-disk page format) from getting the
//! indexing *behavior* — insert/delete/query — right first.

use gems_bitmap::RoaringBitmap;
use std::collections::HashMap;

#[derive(Default)]
pub struct SecondaryIndex {
    by_value: HashMap<Vec<u8>, RoaringBitmap>,
}

impl SecondaryIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, value: Vec<u8>, ordinal: u32) {
        self.by_value.entry(value).or_default().insert(ordinal);
    }

    pub fn remove(&mut self, value: &[u8], ordinal: u32) {
        if let Some(bitmap) = self.by_value.get_mut(value) {
            bitmap.remove(ordinal);
            if bitmap.is_empty() {
                self.by_value.remove(value);
            }
        }
    }

    /// The bitmap of ordinals whose indexed field equals `value`, or an
    /// empty bitmap if nothing matches.
    pub fn get(&self, value: &[u8]) -> RoaringBitmap {
        self.by_value.get(value).cloned().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_query_remove() {
        let mut idx = SecondaryIndex::new();
        idx.insert(b"data".to_vec(), 1);
        idx.insert(b"data".to_vec(), 2);
        idx.insert(b"schema".to_vec(), 3);

        assert_eq!(idx.get(b"data").to_vec(), vec![1, 2]);
        assert_eq!(idx.get(b"schema").to_vec(), vec![3]);
        assert!(idx.get(b"missing").is_empty());

        idx.remove(b"data", 1);
        assert_eq!(idx.get(b"data").to_vec(), vec![2]);
    }

    #[test]
    fn removing_last_member_drops_the_bucket() {
        let mut idx = SecondaryIndex::new();
        idx.insert(b"x".to_vec(), 1);
        idx.remove(b"x", 1);
        assert!(idx.by_value.is_empty());
    }
}
