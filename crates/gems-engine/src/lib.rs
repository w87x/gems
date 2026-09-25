//! `Store`: the first piece of the "higher-level engine crate" that
//! `gems-query`'s crate doc calls out as needed before the planner or ABAC
//! can be real — ties `gems-index` (primary `Tuid -> SlotPointer` index),
//! `gems-storage::ExtentManager` (entity byte storage), and `gems-catalog`
//! (entity header) together into an actual insert/get/delete path.
//!
//! Scope for this pass: single primary index, single data file
//! (`file_id = 0`), no secondary indexes yet (that's `gems-bitmap`'s
//! integration, layered on top of this once it exists), and no query
//! planner (this only supports point lookups by `Tuid` — `gems-query`'s
//! AST isn't compiled against anything here yet). Each of those is a
//! distinct, sizable next step, not folded into this one.

use std::path::{Path, PathBuf};

use gems_catalog::EntityHeader;
use gems_common::{Error, Result, Tuid};
use gems_index::BTree;
use gems_storage::{ExtentManager, SlotPointer};

/// Logical B-tree page size for the primary index file. Fixed at
/// file-creation time per ARCHITECTURE.md §0/§2.3 — independent of the
/// host's OS page size, so index files stay portable between platforms
/// with different native page sizes (4 KiB on Linux, 16 KiB on Apple
/// Silicon).
pub const DEFAULT_INDEX_PAGE_SIZE: u32 = 4096;

const PRIMARY_INDEX_FILE_NAME: &str = "primary.gemi";
const DATA_FILE_NAME: &str = "data.gemx";

pub struct Store {
    primary: BTree<Tuid, SlotPointer>,
    data: ExtentManager,
}

impl Store {
    /// Create a fresh store rooted at `dir` (created if it doesn't exist).
    pub fn create(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let primary = BTree::create(&index_path(dir), DEFAULT_INDEX_PAGE_SIZE)?;
        let data = ExtentManager::create(&data_path(dir), 0)?;
        Ok(Store { primary, data })
    }

    pub fn open(dir: &Path, writable: bool) -> Result<Self> {
        let primary = BTree::open(&index_path(dir), writable)?;
        let data = ExtentManager::open(&data_path(dir), 0, writable)?;
        Ok(Store { primary, data })
    }

    /// Insert (or overwrite, by `header.id`) an entity. `header.body_offset`
    /// and `header.body_len` are set here from `body`'s actual placement
    /// immediately after the encoded header — callers don't need to (and
    /// shouldn't) compute them by hand.
    pub fn insert(&mut self, mut header: EntityHeader, body: &[u8]) -> Result<()> {
        header.body_offset = gems_catalog::header::ENCODED_LEN as u32;
        header.body_len = body.len() as u32;

        let mut blob = header.encode();
        blob.extend_from_slice(body);

        let ptr = self.data.allocate(blob.len())?;
        self.data.write(ptr, &blob)?;

        if let Some(old_ptr) = self.primary.get(&header.id)? {
            self.data.free(old_ptr)?;
        }
        self.primary.insert(header.id, ptr)
    }

    pub fn get(&self, id: &Tuid) -> Result<Option<(EntityHeader, Vec<u8>)>> {
        let Some(ptr) = self.primary.get(id)? else {
            return Ok(None);
        };
        let (header, body) = self.read_at(ptr)?;
        Ok(Some((header, body)))
    }

    fn read_at(&self, ptr: SlotPointer) -> Result<(EntityHeader, Vec<u8>)> {
        let class = gems_storage::BlockClass::from_index(ptr.block_class)?;
        let blob = self.data.read(ptr, class.block_size() as usize)?;
        let header = EntityHeader::decode(blob)?;
        let body_start = header.body_offset as usize;
        let body_end = body_start + header.body_len as usize;
        if blob.len() < body_end {
            return Err(Error::CorruptPage {
                detail: "entity body extends past its stored slot",
            });
        }
        let body = blob[body_start..body_end].to_vec();
        Ok((header, body))
    }

    /// Remove an entity. Returns `true` if it existed. This is a hard
    /// delete of the storage slot — the `PENDING_DELETE`/`TOMBSTONE` soft-
    /// delete flow from ARCHITECTURE.md §5.2 is a higher-level policy for
    /// callers to implement on top of `insert`/`get`, not something this
    /// crate enforces.
    pub fn delete(&mut self, id: &Tuid) -> Result<bool> {
        match self.primary.delete(id)? {
            Some(ptr) => {
                self.data.free(ptr)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// All entities currently stored, in primary-key order. A full scan,
    /// not a planner-driven query (see the crate doc) — useful for tests
    /// and simple tooling until `gems-query`'s AST is wired up here.
    pub fn scan_all(&self) -> Result<Vec<(EntityHeader, Vec<u8>)>> {
        self.primary
            .scan_all()?
            .into_iter()
            .map(|(_, ptr)| self.read_at(ptr))
            .collect()
    }
}

fn index_path(dir: &Path) -> PathBuf {
    dir.join(PRIMARY_INDEX_FILE_NAME)
}

fn data_path(dir: &Path) -> PathBuf {
    dir.join(DATA_FILE_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gems_catalog::{EntityFlags, EntityKind};
    use gems_codec::{GbvBuilder, GbvReader, TypeTag};

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("gems-engine-test")
            .join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn sample_header(id: Tuid, name: &str) -> EntityHeader {
        EntityHeader {
            id,
            created_by: [1u8; 16],
            modified_by: [1u8; 16],
            modified_at_ns: 1000,
            name: name.to_string(),
            description: "a test entity".to_string(),
            flags: EntityFlags::NONE,
            entity_kind: EntityKind::Data,
            schema_ref: Tuid::new([9u8; 16], 1),
            body_offset: 0, // overwritten by Store::insert
            body_len: 0,
        }
    }

    fn gbv_body(status: &str) -> Vec<u8> {
        let mut b = GbvBuilder::new();
        b.push(1, TypeTag::Str, status.as_bytes());
        b.finish()
    }

    #[test]
    fn insert_and_get_roundtrip_through_the_whole_stack() {
        let dir = tmp_dir("roundtrip");
        let mut store = Store::create(&dir).unwrap();

        let id = Tuid::new([1u8; 16], 1);
        let header = sample_header(id, "widget-1");
        let body = gbv_body("active");
        store.insert(header.clone(), &body).unwrap();

        let (got_header, got_body) = store.get(&id).unwrap().unwrap();
        assert_eq!(got_header.id, header.id);
        assert_eq!(got_header.name, "widget-1");
        assert_eq!(got_body, body);

        // and the body is genuinely a readable GBV buffer end to end
        let reader = GbvReader::new(&got_body).unwrap();
        let (tag, val) = reader.get(1).unwrap();
        assert_eq!(tag, TypeTag::Str);
        assert_eq!(val, b"active");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn get_missing_returns_none() {
        let dir = tmp_dir("missing");
        let store = Store::create(&dir).unwrap();
        assert!(store.get(&Tuid::new([2u8; 16], 2)).unwrap().is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn insert_same_id_overwrites_and_frees_old_slot() {
        let dir = tmp_dir("overwrite");
        let mut store = Store::create(&dir).unwrap();
        let id = Tuid::new([3u8; 16], 3);

        store
            .insert(sample_header(id, "v1"), &gbv_body("draft"))
            .unwrap();
        store
            .insert(sample_header(id, "v2"), &gbv_body("active"))
            .unwrap();

        let (header, body) = store.get(&id).unwrap().unwrap();
        assert_eq!(header.name, "v2");
        let reader = GbvReader::new(&body).unwrap();
        assert_eq!(reader.get(1).unwrap().1, b"active");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn delete_removes_entity() {
        let dir = tmp_dir("delete");
        let mut store = Store::create(&dir).unwrap();
        let id = Tuid::new([4u8; 16], 4);
        store
            .insert(sample_header(id, "gone-soon"), &gbv_body("active"))
            .unwrap();

        assert!(store.delete(&id).unwrap());
        assert!(store.get(&id).unwrap().is_none());
        assert!(!store.delete(&id).unwrap(), "already deleted");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scan_all_returns_every_entity_in_key_order() {
        let dir = tmp_dir("scan");
        let mut store = Store::create(&dir).unwrap();
        let mut ids = Vec::new();
        for n in 0..50u64 {
            let id = Tuid::new([n as u8; 16], n);
            ids.push(id);
            store
                .insert(sample_header(id, &format!("e{n}")), &gbv_body("active"))
                .unwrap();
        }
        let scanned = store.scan_all().unwrap();
        assert_eq!(scanned.len(), 50);
        let mut sorted_ids = ids.clone();
        sorted_ids.sort();
        let scanned_ids: Vec<Tuid> = scanned.iter().map(|(h, _)| h.id).collect();
        assert_eq!(scanned_ids, sorted_ids);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reopen_after_close_preserves_everything() {
        let dir = tmp_dir("reopen");
        let id = Tuid::new([5u8; 16], 5);
        {
            let mut store = Store::create(&dir).unwrap();
            store
                .insert(sample_header(id, "persisted"), &gbv_body("active"))
                .unwrap();
        }
        {
            let store = Store::open(&dir, false).unwrap();
            let (header, body) = store.get(&id).unwrap().unwrap();
            assert_eq!(header.name, "persisted");
            let reader = GbvReader::new(&body).unwrap();
            assert_eq!(reader.get(1).unwrap().1, b"active");
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
