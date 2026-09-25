//! `RoaringBitmap`: a sorted map from the high 16 bits of a `u32` to the
//! `Container` holding its low 16 bits. Keeping chunks in a `BTreeMap`
//! means both value order and chunk order fall out of the same structure,
//! which is what makes `iter()` a simple ordered walk and `union`/
//! `intersect` a merge over both maps' keys.

use crate::container::Container;
use std::collections::BTreeMap;

#[derive(Clone, Default)]
pub struct RoaringBitmap {
    chunks: BTreeMap<u16, Container>,
}

fn split(value: u32) -> (u16, u16) {
    ((value >> 16) as u16, (value & 0xffff) as u16)
}

fn join(high: u16, low: u16) -> u32 {
    ((high as u32) << 16) | low as u32
}

impl RoaringBitmap {
    pub fn new() -> Self {
        RoaringBitmap {
            chunks: BTreeMap::new(),
        }
    }

    pub fn insert(&mut self, value: u32) -> bool {
        let (high, low) = split(value);
        self.chunks
            .entry(high)
            .or_insert_with(Container::new_array)
            .insert(low)
    }

    pub fn remove(&mut self, value: u32) -> bool {
        let (high, low) = split(value);
        match self.chunks.get_mut(&high) {
            Some(container) => {
                let removed = container.remove(low);
                if container.is_empty() {
                    self.chunks.remove(&high);
                }
                removed
            }
            None => false,
        }
    }

    pub fn contains(&self, value: u32) -> bool {
        let (high, low) = split(value);
        self.chunks
            .get(&high)
            .map(|c| c.contains(low))
            .unwrap_or(false)
    }

    pub fn len(&self) -> u64 {
        self.chunks.values().map(|c| c.len() as u64).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        self.chunks
            .iter()
            .flat_map(|(&high, c)| c.iter().map(move |low| join(high, low)))
    }

    pub fn to_vec(&self) -> Vec<u32> {
        self.iter().collect()
    }

    pub fn union(&self, other: &RoaringBitmap) -> RoaringBitmap {
        let mut out = RoaringBitmap::new();
        for high in self
            .chunks
            .keys()
            .chain(other.chunks.keys())
            .collect::<std::collections::BTreeSet<_>>()
        {
            let merged = match (self.chunks.get(high), other.chunks.get(high)) {
                (Some(a), Some(b)) => a.union(b),
                (Some(a), None) => a.clone(),
                (None, Some(b)) => b.clone(),
                (None, None) => unreachable!("key came from one of the two maps"),
            };
            if !merged.is_empty() {
                out.chunks.insert(*high, merged);
            }
        }
        out
    }

    pub fn intersect(&self, other: &RoaringBitmap) -> RoaringBitmap {
        let mut out = RoaringBitmap::new();
        for (high, a) in &self.chunks {
            if let Some(b) = other.chunks.get(high) {
                let merged = a.intersect(b);
                if !merged.is_empty() {
                    out.chunks.insert(*high, merged);
                }
            }
        }
        out
    }

    /// `self` with every element also present in `other` removed
    /// (`self AND NOT other`) — the ABAC PEP's "drop denied entities from
    /// the candidate set" operation (ARCHITECTURE.md §8).
    pub fn and_not(&self, other: &RoaringBitmap) -> RoaringBitmap {
        let mut out = RoaringBitmap::new();
        for (high, a) in &self.chunks {
            match other.chunks.get(high) {
                None => {
                    out.chunks.insert(*high, a.clone());
                }
                Some(b) => {
                    let mut kept = Container::new_array();
                    for v in a.iter() {
                        if !b.contains(v) {
                            kept.insert(v);
                        }
                    }
                    if !kept.is_empty() {
                        out.chunks.insert(*high, kept);
                    }
                }
            }
        }
        out
    }
}

impl FromIterator<u32> for RoaringBitmap {
    fn from_iter<I: IntoIterator<Item = u32>>(values: I) -> Self {
        let mut bm = RoaringBitmap::new();
        for v in values {
            bm.insert(v);
        }
        bm
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn insert_contains_remove_roundtrip() {
        let mut bm = RoaringBitmap::new();
        assert!(bm.insert(70000)); // forces a second chunk (high=1)
        assert!(bm.insert(5));
        assert!(!bm.insert(5));
        assert!(bm.contains(5));
        assert!(bm.contains(70000));
        assert!(bm.remove(5));
        assert!(!bm.contains(5));
        assert_eq!(bm.len(), 1);
    }

    #[test]
    fn iteration_is_sorted() {
        let bm = RoaringBitmap::from_iter([500, 5, 70000, 6, 100000]);
        let v = bm.to_vec();
        let mut sorted = v.clone();
        sorted.sort();
        assert_eq!(v, sorted);
    }

    #[test]
    fn set_ops_match_reference() {
        let a_vals: Vec<u32> = (0..5000).step_by(3).collect();
        let b_vals: Vec<u32> = (0..5000).step_by(5).collect();
        let a = RoaringBitmap::from_iter(a_vals.iter().copied());
        let b = RoaringBitmap::from_iter(b_vals.iter().copied());

        let ref_a: BTreeSet<u32> = a_vals.into_iter().collect();
        let ref_b: BTreeSet<u32> = b_vals.into_iter().collect();

        let union: BTreeSet<u32> = a.union(&b).to_vec().into_iter().collect();
        assert_eq!(union, ref_a.union(&ref_b).copied().collect());

        let inter: BTreeSet<u32> = a.intersect(&b).to_vec().into_iter().collect();
        assert_eq!(inter, ref_a.intersection(&ref_b).copied().collect());

        let diff: BTreeSet<u32> = a.and_not(&b).to_vec().into_iter().collect();
        assert_eq!(diff, ref_a.difference(&ref_b).copied().collect());
    }

    #[test]
    fn set_ops_across_bitmap_containers() {
        // Push both sides past the array->bitmap promotion threshold in
        // overlapping and non-overlapping chunks.
        let a = RoaringBitmap::from_iter(0..10_000);
        let b = RoaringBitmap::from_iter(5_000..15_000);

        assert_eq!(a.union(&b).len(), 15_000);
        assert_eq!(a.intersect(&b).len(), 5_000);
        assert_eq!(a.and_not(&b).len(), 5_000);
    }
}
