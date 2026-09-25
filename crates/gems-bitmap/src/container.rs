//! A single 65536-value chunk: either a sorted `Vec<u16>` (cheap for
//! sparse chunks) or a dense bitset (`1024 x u64` = 65536 bits, cheap for
//! dense chunks). `ARRAY_TO_BITMAP_THRESHOLD` picks the crossover — above
//! it, a bitset is smaller and its operations are branch-free words-at-a-
//! time, which matters more than array's smaller footprint once a chunk
//! is that full.

pub const BITMAP_WORDS: usize = 1024; // 1024 * 64 bits = 65536
pub const ARRAY_TO_BITMAP_THRESHOLD: usize = 4096;

#[derive(Clone)]
pub enum Container {
    Array(Vec<u16>),
    Bitmap(Box<[u64; BITMAP_WORDS]>),
}

impl Container {
    pub fn new_array() -> Self {
        Container::Array(Vec::new())
    }

    pub fn len(&self) -> usize {
        match self {
            Container::Array(v) => v.len(),
            Container::Bitmap(bits) => bits.iter().map(|w| w.count_ones() as usize).sum(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn contains(&self, low: u16) -> bool {
        match self {
            Container::Array(v) => v.binary_search(&low).is_ok(),
            Container::Bitmap(bits) => {
                let (word, bit) = (low as usize / 64, low as usize % 64);
                (bits[word] >> bit) & 1 != 0
            }
        }
    }

    /// Insert `low`, promoting Array -> Bitmap if it grows past the
    /// threshold. Returns `true` if this was a new element.
    pub fn insert(&mut self, low: u16) -> bool {
        match self {
            Container::Array(v) => match v.binary_search(&low) {
                Ok(_) => false,
                Err(idx) => {
                    v.insert(idx, low);
                    if v.len() > ARRAY_TO_BITMAP_THRESHOLD {
                        self.promote_to_bitmap();
                    }
                    true
                }
            },
            Container::Bitmap(bits) => {
                let (word, bit) = (low as usize / 64, low as usize % 64);
                let mask = 1u64 << bit;
                let was_set = bits[word] & mask != 0;
                bits[word] |= mask;
                !was_set
            }
        }
    }

    /// Remove `low`. Never demotes Bitmap back to Array (a deliberate v1
    /// simplification — see the module doc's note on container choice
    /// being a one-way promotion for now).
    pub fn remove(&mut self, low: u16) -> bool {
        match self {
            Container::Array(v) => match v.binary_search(&low) {
                Ok(idx) => {
                    v.remove(idx);
                    true
                }
                Err(_) => false,
            },
            Container::Bitmap(bits) => {
                let (word, bit) = (low as usize / 64, low as usize % 64);
                let mask = 1u64 << bit;
                let was_set = bits[word] & mask != 0;
                bits[word] &= !mask;
                was_set
            }
        }
    }

    fn promote_to_bitmap(&mut self) {
        if let Container::Array(v) = self {
            let mut bits = Box::new([0u64; BITMAP_WORDS]);
            for &low in v.iter() {
                let (word, bit) = (low as usize / 64, low as usize % 64);
                bits[word] |= 1u64 << bit;
            }
            *self = Container::Bitmap(bits);
        }
    }

    pub fn iter(&self) -> Box<dyn Iterator<Item = u16> + '_> {
        match self {
            Container::Array(v) => Box::new(v.iter().copied()),
            Container::Bitmap(bits) => Box::new(bits.iter().enumerate().flat_map(|(w, &word)| {
                (0..64).filter_map(move |b| {
                    if word & (1u64 << b) != 0 {
                        Some((w * 64 + b) as u16)
                    } else {
                        None
                    }
                })
            })),
        }
    }

    pub fn union(&self, other: &Container) -> Container {
        let mut bits = Box::new([0u64; BITMAP_WORDS]);
        self.or_into(&mut bits);
        other.or_into(&mut bits);
        shrink_if_sparse(Container::Bitmap(bits))
    }

    pub fn intersect(&self, other: &Container) -> Container {
        // Iterate the smaller side for a cheap sparse fast-path; correctness
        // doesn't depend on which side we iterate.
        let (small, large) = if self.len() <= other.len() {
            (self, other)
        } else {
            (other, self)
        };
        let mut out = Container::new_array();
        for v in small.iter() {
            if large.contains(v) {
                out.insert(v);
            }
        }
        out
    }

    fn or_into(&self, bits: &mut [u64; BITMAP_WORDS]) {
        match self {
            Container::Array(v) => {
                for &low in v.iter() {
                    let (word, bit) = (low as usize / 64, low as usize % 64);
                    bits[word] |= 1u64 << bit;
                }
            }
            Container::Bitmap(b) => {
                for i in 0..BITMAP_WORDS {
                    bits[i] |= b[i];
                }
            }
        }
    }
}

/// After a bitmap-shaped union, demote back to an array if the result
/// turned out sparse — a union of two sparse containers should stay
/// cheap to iterate/serialize.
fn shrink_if_sparse(c: Container) -> Container {
    if let Container::Bitmap(bits) = &c {
        let count = bits.iter().map(|w| w.count_ones() as usize).sum::<usize>();
        if count <= ARRAY_TO_BITMAP_THRESHOLD {
            return Container::Array(c.iter().collect());
        }
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn array_insert_contains_remove() {
        let mut c = Container::new_array();
        assert!(c.insert(5));
        assert!(!c.insert(5));
        assert!(c.contains(5));
        assert!(c.remove(5));
        assert!(!c.contains(5));
    }

    #[test]
    fn promotes_to_bitmap_past_threshold() {
        let mut c = Container::new_array();
        for i in 0..=(ARRAY_TO_BITMAP_THRESHOLD as u16 + 1) {
            c.insert(i);
        }
        assert!(matches!(c, Container::Bitmap(_)));
        assert!(c.contains(0));
        assert!(c.contains(ARRAY_TO_BITMAP_THRESHOLD as u16 + 1));
    }

    #[test]
    fn union_and_intersect() {
        let mut a = Container::new_array();
        let mut b = Container::new_array();
        for v in [1, 2, 3] {
            a.insert(v);
        }
        for v in [3, 4, 5] {
            b.insert(v);
        }
        let u: Vec<u16> = a.union(&b).iter().collect();
        assert_eq!(u, vec![1, 2, 3, 4, 5]);
        let i: Vec<u16> = a.intersect(&b).iter().collect();
        assert_eq!(i, vec![3]);
    }
}
