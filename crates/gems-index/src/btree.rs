//! The B-tree engine described in ARCHITECTURE.md §2: fixed-width keys and
//! values, pages allocated via `Pager`, updates built bottom-up as fresh
//! pages and published with a single atomic root-pointer write.
//!
//! Deliberate v1 scope decisions, called out here rather than left
//! implicit:
//!
//! - **Splits are plain 50/50 splits, not the 2-into-3 sibling
//!   redistribution a "true" B*-tree does.** ARCHITECTURE.md §2.2 explains
//!   why B* matters for this workload (random UUID-half keys fragmenting a
//!   plain B+-tree toward 50% fill); a straight split already gets the
//!   crash-safety and CoW behavior right, and sibling redistribution can be
//!   layered on top of `insert_rec`'s split path later without touching the
//!   on-disk format. Don't block the rest of the stack on it.
//! - **Delete does not rebalance.** A leaf (or internal node) is allowed to
//!   underflow after a removal; no borrow-from-sibling or merge is
//!   implemented. Correctness (find/insert/delete/scan) matters more than
//!   fill factor after heavy deletion for the crates that build on this one
//!   next; revisit once there's a workload that actually deletes enough to
//!   care.
//! - **No leaf sibling chain.** See `node.rs`'s module doc for why: under
//!   copy-on-write, maintaining a `next_leaf` pointer would require
//!   rewriting an untouched left sibling (and everything above it) on
//!   every update. `scan_all` walks the tree structure instead.

use gems_common::Result;
use std::marker::PhantomData;
use std::path::Path;

use crate::node::{
    encode_internal, encode_leaf, internal_capacity, leaf_capacity, node_type, FixedCodec,
    InternalView, LeafView, INTERNAL, LEAF,
};
use crate::pager::Pager;

pub struct BTree<K, V> {
    pager: Pager,
    pending_free: Vec<u32>,
    _marker: PhantomData<(K, V)>,
}

enum InsertOutcome<K> {
    Replaced(u32),
    Split { left: u32, sep: K, right: u32 },
}

impl<K: FixedCodec, V: FixedCodec> BTree<K, V> {
    pub fn create(path: &Path, page_size: u32) -> Result<Self> {
        let pager = Pager::create(path, page_size, K::LEN as u16, V::LEN as u16)?;
        Ok(BTree {
            pager,
            pending_free: Vec::new(),
            _marker: PhantomData,
        })
    }

    pub fn open(path: &Path, writable: bool) -> Result<Self> {
        let pager = Pager::open(path, writable)?;
        Ok(BTree {
            pager,
            pending_free: Vec::new(),
            _marker: PhantomData,
        })
    }

    fn leaf_capacity(&self) -> usize {
        leaf_capacity(self.pager.page_size() as usize, K::LEN, V::LEN)
    }

    fn internal_capacity(&self) -> usize {
        internal_capacity(self.pager.page_size() as usize, K::LEN)
    }

    pub fn get(&self, key: &K) -> Result<Option<V>> {
        let mut page_id = self.pager.root_page();
        if page_id == 0 {
            return Ok(None);
        }
        loop {
            let buf = self.pager.page(page_id)?;
            match node_type(buf)? {
                LEAF => {
                    let view = LeafView::<K, V>::decode(buf)?;
                    return Ok(view
                        .entries
                        .binary_search_by(|(k, _)| k.cmp(key))
                        .ok()
                        .map(|idx| view.entries[idx].1.clone()));
                }
                INTERNAL => {
                    let view = InternalView::<K>::decode(buf)?;
                    let idx = view.keys.partition_point(|k| k <= key);
                    page_id = view.children[idx];
                }
                _ => unreachable!("node_type validated by decode"),
            }
        }
    }

    pub fn insert(&mut self, key: K, value: V) -> Result<()> {
        self.pending_free.clear();
        let root = self.pager.root_page();
        let new_root = if root == 0 {
            let leaf_id = self.pager.allocate_page()?;
            encode_leaf(self.pager.page_mut(leaf_id)?, &[(key, value)]);
            leaf_id
        } else {
            match self.insert_rec(root, &key, &value)? {
                InsertOutcome::Replaced(id) => id,
                InsertOutcome::Split { left, sep, right } => {
                    let new_root_id = self.pager.allocate_page()?;
                    encode_internal(self.pager.page_mut(new_root_id)?, &[sep], &[left, right]);
                    new_root_id
                }
            }
        };
        self.pager.publish_root(new_root)?;
        for old in self.pending_free.drain(..) {
            self.pager.free_page(old)?;
        }
        Ok(())
    }

    fn insert_rec(&mut self, page_id: u32, key: &K, value: &V) -> Result<InsertOutcome<K>> {
        let node_ty = node_type(self.pager.page(page_id)?)?;
        if node_ty == LEAF {
            let mut view = LeafView::<K, V>::decode(self.pager.page(page_id)?)?;
            match view.entries.binary_search_by(|(k, _)| k.cmp(key)) {
                Ok(idx) => view.entries[idx].1 = value.clone(),
                Err(idx) => view.entries.insert(idx, (key.clone(), value.clone())),
            }
            let capacity = self.leaf_capacity();
            if view.entries.len() <= capacity {
                let new_id = self.pager.allocate_page()?;
                encode_leaf(self.pager.page_mut(new_id)?, &view.entries);
                self.pending_free.push(page_id);
                Ok(InsertOutcome::Replaced(new_id))
            } else {
                let mid = view.entries.len() / 2;
                let right_entries = view.entries.split_off(mid);
                let left_entries = view.entries;
                let sep = right_entries[0].0.clone();

                let right_id = self.pager.allocate_page()?;
                let left_id = self.pager.allocate_page()?;
                encode_leaf(self.pager.page_mut(right_id)?, &right_entries);
                encode_leaf(self.pager.page_mut(left_id)?, &left_entries);
                self.pending_free.push(page_id);
                Ok(InsertOutcome::Split {
                    left: left_id,
                    sep,
                    right: right_id,
                })
            }
        } else {
            let view = InternalView::<K>::decode(self.pager.page(page_id)?)?;
            let child_idx = view.keys.partition_point(|k| k <= key);
            let outcome = self.insert_rec(view.children[child_idx], key, value)?;
            match outcome {
                InsertOutcome::Replaced(new_child_id) => {
                    let mut children = view.children.clone();
                    children[child_idx] = new_child_id;
                    let new_id = self.pager.allocate_page()?;
                    encode_internal(self.pager.page_mut(new_id)?, &view.keys, &children);
                    self.pending_free.push(page_id);
                    Ok(InsertOutcome::Replaced(new_id))
                }
                InsertOutcome::Split { left, sep, right } => {
                    let mut keys = view.keys.clone();
                    let mut children = view.children.clone();
                    children[child_idx] = left;
                    children.insert(child_idx + 1, right);
                    keys.insert(child_idx, sep);

                    let capacity = self.internal_capacity();
                    if keys.len() <= capacity {
                        let new_id = self.pager.allocate_page()?;
                        encode_internal(self.pager.page_mut(new_id)?, &keys, &children);
                        self.pending_free.push(page_id);
                        Ok(InsertOutcome::Replaced(new_id))
                    } else {
                        let mid = keys.len() / 2;
                        let sep_up = keys[mid].clone();
                        let right_keys = keys.split_off(mid + 1);
                        let mut left_keys = keys;
                        left_keys.truncate(mid);
                        let right_children = children.split_off(mid + 1);
                        let left_children = children;

                        let left_id = self.pager.allocate_page()?;
                        let right_id = self.pager.allocate_page()?;
                        encode_internal(self.pager.page_mut(left_id)?, &left_keys, &left_children);
                        encode_internal(
                            self.pager.page_mut(right_id)?,
                            &right_keys,
                            &right_children,
                        );
                        self.pending_free.push(page_id);
                        Ok(InsertOutcome::Split {
                            left: left_id,
                            sep: sep_up,
                            right: right_id,
                        })
                    }
                }
            }
        }
    }

    /// Remove `key` if present. See the module-level note: no rebalancing
    /// on underflow in v1.
    pub fn delete(&mut self, key: &K) -> Result<Option<V>> {
        self.pending_free.clear();
        let root = self.pager.root_page();
        if root == 0 {
            return Ok(None);
        }
        let (new_root, removed) = self.delete_rec(root, key)?;
        if new_root != root {
            self.pager.publish_root(new_root)?;
            for old in self.pending_free.drain(..) {
                self.pager.free_page(old)?;
            }
        }
        Ok(removed)
    }

    fn delete_rec(&mut self, page_id: u32, key: &K) -> Result<(u32, Option<V>)> {
        let node_ty = node_type(self.pager.page(page_id)?)?;
        if node_ty == LEAF {
            let mut view = LeafView::<K, V>::decode(self.pager.page(page_id)?)?;
            match view.entries.binary_search_by(|(k, _)| k.cmp(key)) {
                Err(_) => Ok((page_id, None)),
                Ok(idx) => {
                    let (_, removed_value) = view.entries.remove(idx);
                    let new_id = self.pager.allocate_page()?;
                    encode_leaf(self.pager.page_mut(new_id)?, &view.entries);
                    self.pending_free.push(page_id);
                    Ok((new_id, Some(removed_value)))
                }
            }
        } else {
            let view = InternalView::<K>::decode(self.pager.page(page_id)?)?;
            let child_idx = view.keys.partition_point(|k| k <= key);
            let (new_child_id, removed) = self.delete_rec(view.children[child_idx], key)?;
            if new_child_id == view.children[child_idx] {
                return Ok((page_id, removed));
            }
            let mut children = view.children.clone();
            children[child_idx] = new_child_id;
            let new_id = self.pager.allocate_page()?;
            encode_internal(self.pager.page_mut(new_id)?, &view.keys, &children);
            self.pending_free.push(page_id);
            Ok((new_id, removed))
        }
    }

    /// Collect every `(key, value)` pair in ascending key order. Leaves have
    /// no sibling chain (see the module doc), so this recurses through the
    /// tree structure itself — visiting every child of an internal node in
    /// order visits every leaf in order. Sufficient for the query planner's
    /// full/range scans until there's a reason to make this a lazy cursor
    /// instead of a `Vec`.
    pub fn scan_all(&self) -> Result<Vec<(K, V)>> {
        let mut out = Vec::new();
        let root = self.pager.root_page();
        if root != 0 {
            self.scan_rec(root, &mut out)?;
        }
        Ok(out)
    }

    fn scan_rec(&self, page_id: u32, out: &mut Vec<(K, V)>) -> Result<()> {
        let buf = self.pager.page(page_id)?;
        match node_type(buf)? {
            LEAF => {
                let view = LeafView::<K, V>::decode(buf)?;
                out.extend(view.entries);
                Ok(())
            }
            INTERNAL => {
                let view = InternalView::<K>::decode(buf)?;
                for child in view.children {
                    self.scan_rec(child, out)?;
                }
                Ok(())
            }
            _ => unreachable!("node_type validated by decode"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gems_common::Tuid;
    use gems_storage::SlotPointer;
    use std::collections::BTreeMap;

    fn tmp_path(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("gems-index-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let _ = std::fs::remove_file(&path);
        path
    }

    fn ptr(n: u32) -> SlotPointer {
        SlotPointer {
            file_id: 0,
            extent_index: 0,
            slot_index: n,
            block_class: 0,
        }
    }

    fn key(n: u64) -> Tuid {
        // Vary the "uuid" bytes so keys are not just timestamp-ordered,
        // exercising real B-tree branching rather than an always-appending
        // pattern.
        let mut uuid = [0u8; 16];
        uuid[0..8].copy_from_slice(&(n.wrapping_mul(2654435761)).to_be_bytes());
        Tuid::new(uuid, n)
    }

    #[test]
    fn insert_and_get_small() {
        let path = tmp_path("small.gemi");
        let mut tree: BTree<Tuid, SlotPointer> = BTree::create(&path, 512).unwrap();
        tree.insert(key(1), ptr(1)).unwrap();
        tree.insert(key(2), ptr(2)).unwrap();
        assert_eq!(tree.get(&key(1)).unwrap(), Some(ptr(1)));
        assert_eq!(tree.get(&key(2)).unwrap(), Some(ptr(2)));
        assert_eq!(tree.get(&key(3)).unwrap(), None);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn insert_many_forces_splits_and_matches_reference() {
        let path = tmp_path("many.gemi");
        // Small page size so this test actually exercises leaf/internal
        // splits without inserting an enormous number of keys.
        let mut tree: BTree<Tuid, SlotPointer> = BTree::create(&path, 256).unwrap();
        let mut reference = BTreeMap::new();

        for n in 0..2000u64 {
            tree.insert(key(n), ptr(n as u32)).unwrap();
            reference.insert(key(n), ptr(n as u32));
        }

        for (k, v) in &reference {
            assert_eq!(tree.get(k).unwrap(), Some(*v));
        }

        let scanned = tree.scan_all().unwrap();
        let expected: Vec<_> = reference.into_iter().collect();
        assert_eq!(scanned, expected);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn update_existing_key_overwrites_value() {
        let path = tmp_path("update.gemi");
        let mut tree: BTree<Tuid, SlotPointer> = BTree::create(&path, 512).unwrap();
        tree.insert(key(1), ptr(1)).unwrap();
        tree.insert(key(1), ptr(99)).unwrap();
        assert_eq!(tree.get(&key(1)).unwrap(), Some(ptr(99)));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn delete_removes_key() {
        let path = tmp_path("delete.gemi");
        let mut tree: BTree<Tuid, SlotPointer> = BTree::create(&path, 256).unwrap();
        for n in 0..500u64 {
            tree.insert(key(n), ptr(n as u32)).unwrap();
        }
        for n in (0..500u64).step_by(2) {
            assert_eq!(tree.delete(&key(n)).unwrap(), Some(ptr(n as u32)));
        }
        for n in 0..500u64 {
            let expected = if n % 2 == 0 {
                None
            } else {
                Some(ptr(n as u32))
            };
            assert_eq!(tree.get(&key(n)).unwrap(), expected);
        }
        assert_eq!(tree.delete(&key(0)).unwrap(), None, "already deleted");

        let scanned = tree.scan_all().unwrap();
        let expected_count = 500 - (0..500u64).step_by(2).count();
        assert_eq!(scanned.len(), expected_count);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn reopen_after_close_preserves_data() {
        let path = tmp_path("reopen.gemi");
        {
            let mut tree: BTree<Tuid, SlotPointer> = BTree::create(&path, 256).unwrap();
            for n in 0..300u64 {
                tree.insert(key(n), ptr(n as u32)).unwrap();
            }
        }
        {
            let tree: BTree<Tuid, SlotPointer> = BTree::open(&path, false).unwrap();
            for n in 0..300u64 {
                assert_eq!(tree.get(&key(n)).unwrap(), Some(ptr(n as u32)));
            }
            assert_eq!(tree.scan_all().unwrap().len(), 300);
        }
        std::fs::remove_file(&path).ok();
    }
}
