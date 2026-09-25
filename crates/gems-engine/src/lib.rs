//! `Store`: ties `gems-index` (primary `Tuid -> SlotPointer` B-tree, plus
//! an ordinal-assignment B-tree pair), `gems-storage::ExtentManager`
//! (entity byte storage), `gems-catalog` (entity header), and
//! `gems-bitmap` (secondary indexes) into an actual insert/get/delete/
//! query path — the "higher-level engine crate" `gems-query`'s doc calls
//! out as needed before its AST can compile against anything real.
//!
//! Scope for this pass, called out rather than left implicit:
//!
//! - **Two concrete secondary indexes**: by `entity_kind` and by
//!   `schema_ref` (the entity's `EntityType`), covering the
//!   `type IN (...)` half of ARCHITECTURE.md §7's example query. Arbitrary
//!   attribute-value indexing (the `attr.status = 'active'` half) needs the
//!   `EntityType`/`EntityAttribute` schema resolution the query planner
//!   will do — not implemented yet, kept as a distinct next step.
//! - **In-memory secondary indexes** (see `secondary.rs`): rebuilt from a
//!   full scan on `open()` rather than persisted to their own extent
//!   files.
//! - **Single primary index, single data file** (`file_id = 0`) — sharding
//!   is cluster-layer work (ARCHITECTURE.md §6).
//! - **No query planner.** `gems-query`'s AST isn't compiled against this
//!   yet; `query_by_kind`/`query_by_schema_ref` are direct, hand-called
//!   entry points proving the indexing behavior works end to end.

mod ordinal;
mod secondary;

use std::path::{Path, PathBuf};

use gems_catalog::{EntityHeader, EntityKind};
use gems_common::{Error, Result, Tuid};
use gems_index::BTree;
use gems_storage::{ExtentManager, SlotPointer};

use ordinal::Ordinal;
use secondary::SecondaryIndex;

/// Logical B-tree page size for every index file this store creates.
/// Fixed at file-creation time per ARCHITECTURE.md §0/§2.3 — independent
/// of the host's OS page size, so index files stay portable between
/// platforms with different native page sizes (4 KiB on Linux, 16 KiB on
/// Apple Silicon).
pub const DEFAULT_INDEX_PAGE_SIZE: u32 = 4096;

const PRIMARY_INDEX_FILE_NAME: &str = "primary.gemi";
const ORDINAL_INDEX_FILE_NAME: &str = "ordinal.gemi";
const REVERSE_ORDINAL_INDEX_FILE_NAME: &str = "ordinal_rev.gemi";
const DATA_FILE_NAME: &str = "data.gemx";

pub struct Store {
    primary: BTree<Tuid, SlotPointer>,
    ordinal_by_id: BTree<Tuid, Ordinal>,
    id_by_ordinal: BTree<Ordinal, Tuid>,
    next_ordinal: u32,
    data: ExtentManager,
    by_kind: SecondaryIndex,
    by_schema_ref: SecondaryIndex,
}

fn kind_key(kind: EntityKind) -> Vec<u8> {
    vec![kind as u8]
}

fn schema_ref_key(schema_ref: &Tuid) -> Vec<u8> {
    schema_ref.as_bytes().to_vec()
}

impl Store {
    /// Create a fresh store rooted at `dir` (created if it doesn't exist).
    pub fn create(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let primary = BTree::create(&primary_index_path(dir), DEFAULT_INDEX_PAGE_SIZE)?;
        let ordinal_by_id = BTree::create(&ordinal_index_path(dir), DEFAULT_INDEX_PAGE_SIZE)?;
        let id_by_ordinal =
            BTree::create(&reverse_ordinal_index_path(dir), DEFAULT_INDEX_PAGE_SIZE)?;
        let data = ExtentManager::create(&data_path(dir), 0)?;
        Ok(Store {
            primary,
            ordinal_by_id,
            id_by_ordinal,
            next_ordinal: 0,
            data,
            by_kind: SecondaryIndex::new(),
            by_schema_ref: SecondaryIndex::new(),
        })
    }

    pub fn open(dir: &Path, writable: bool) -> Result<Self> {
        let primary = BTree::open(&primary_index_path(dir), writable)?;
        let ordinal_by_id = BTree::open(&ordinal_index_path(dir), writable)?;
        let id_by_ordinal = BTree::open(&reverse_ordinal_index_path(dir), writable)?;
        let data = ExtentManager::open(&data_path(dir), 0, writable)?;

        let mut store = Store {
            primary,
            ordinal_by_id,
            id_by_ordinal,
            next_ordinal: 0,
            data,
            by_kind: SecondaryIndex::new(),
            by_schema_ref: SecondaryIndex::new(),
        };
        store.rebuild_in_memory_indexes()?;
        Ok(store)
    }

    /// Recomputes `next_ordinal` and every in-memory secondary index from
    /// the persisted primary/ordinal indexes. Called once on `open()`; see
    /// the crate doc for why secondary indexes aren't persisted directly.
    fn rebuild_in_memory_indexes(&mut self) -> Result<()> {
        let mut max_ordinal: Option<u32> = None;
        for (id, ptr) in self.primary.scan_all()? {
            let Some(Ordinal(ord)) = self.ordinal_by_id.get(&id)? else {
                return Err(Error::CorruptPage {
                    detail: "entity in primary index has no assigned ordinal",
                });
            };
            max_ordinal = Some(max_ordinal.map_or(ord, |m| m.max(ord)));
            let (header, _) = self.read_at(ptr)?;
            self.by_kind.insert(kind_key(header.entity_kind), ord);
            self.by_schema_ref
                .insert(schema_ref_key(&header.schema_ref), ord);
        }
        self.next_ordinal = max_ordinal.map_or(0, |m| m + 1);
        Ok(())
    }

    fn ordinal_for(&mut self, id: Tuid) -> Result<Ordinal> {
        if let Some(existing) = self.ordinal_by_id.get(&id)? {
            return Ok(existing);
        }
        let ord = Ordinal(self.next_ordinal);
        self.next_ordinal += 1;
        self.ordinal_by_id.insert(id, ord)?;
        self.id_by_ordinal.insert(ord, id)?;
        Ok(ord)
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

        let ord = self.ordinal_for(header.id)?;

        if let Some(old_ptr) = self.primary.get(&header.id)? {
            let (old_header, _) = self.read_at(old_ptr)?;
            self.by_kind
                .remove(&kind_key(old_header.entity_kind), ord.0);
            self.by_schema_ref
                .remove(&schema_ref_key(&old_header.schema_ref), ord.0);
            self.data.free(old_ptr)?;
        }

        self.by_kind.insert(kind_key(header.entity_kind), ord.0);
        self.by_schema_ref
            .insert(schema_ref_key(&header.schema_ref), ord.0);

        self.primary.insert(header.id, ptr)
    }

    pub fn get(&self, id: &Tuid) -> Result<Option<(EntityHeader, Vec<u8>)>> {
        let Some(ptr) = self.primary.get(id)? else {
            return Ok(None);
        };
        Ok(Some(self.read_at(ptr)?))
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
    /// crate enforces. The entity's ordinal is retired, not reused (see
    /// `ordinal.rs`).
    pub fn delete(&mut self, id: &Tuid) -> Result<bool> {
        let Some(ptr) = self.primary.delete(id)? else {
            return Ok(false);
        };
        let (header, _) = self.read_at(ptr)?;
        if let Some(ord) = self.ordinal_by_id.delete(id)? {
            self.id_by_ordinal.delete(&ord)?;
            self.by_kind.remove(&kind_key(header.entity_kind), ord.0);
            self.by_schema_ref
                .remove(&schema_ref_key(&header.schema_ref), ord.0);
        }
        self.data.free(ptr)?;
        Ok(true)
    }

    /// All entities currently stored, in primary-key order. A full scan,
    /// not a planner-driven query — useful for tests and simple tooling
    /// until `gems-query`'s AST is wired up here.
    pub fn scan_all(&self) -> Result<Vec<(EntityHeader, Vec<u8>)>> {
        self.primary
            .scan_all()?
            .into_iter()
            .map(|(_, ptr)| self.read_at(ptr))
            .collect()
    }

    fn resolve_ordinals(&self, ordinals: gems_bitmap::RoaringBitmap) -> Result<Vec<Tuid>> {
        ordinals
            .to_vec()
            .into_iter()
            .map(|ord| {
                self.id_by_ordinal
                    .get(&Ordinal(ord))?
                    .ok_or(Error::CorruptPage {
                        detail: "secondary index referenced an ordinal with no reverse mapping",
                    })
            })
            .collect()
    }

    /// Every entity of a given `EntityKind`, via the in-memory bitmap
    /// index (ARCHITECTURE.md §3).
    pub fn query_by_kind(&self, kind: EntityKind) -> Result<Vec<Tuid>> {
        self.resolve_ordinals(self.by_kind.get(&kind_key(kind)))
    }

    /// Every `Data` entity whose `schema_ref` is `type_id` — the piece
    /// behind ARCHITECTURE.md §7's `type IN (...)` example.
    pub fn query_by_schema_ref(&self, type_id: &Tuid) -> Result<Vec<Tuid>> {
        self.resolve_ordinals(self.by_schema_ref.get(&schema_ref_key(type_id)))
    }
}

fn primary_index_path(dir: &Path) -> PathBuf {
    dir.join(PRIMARY_INDEX_FILE_NAME)
}

fn ordinal_index_path(dir: &Path) -> PathBuf {
    dir.join(ORDINAL_INDEX_FILE_NAME)
}

fn reverse_ordinal_index_path(dir: &Path) -> PathBuf {
    dir.join(REVERSE_ORDINAL_INDEX_FILE_NAME)
}

fn data_path(dir: &Path) -> PathBuf {
    dir.join(DATA_FILE_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gems_catalog::EntityFlags;
    use gems_codec::{GbvBuilder, GbvReader, TypeTag};

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("gems-engine-test")
            .join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn sample_header(id: Tuid, name: &str, kind: EntityKind, schema_ref: Tuid) -> EntityHeader {
        EntityHeader {
            id,
            created_by: [1u8; 16],
            modified_by: [1u8; 16],
            modified_at_ns: 1000,
            name: name.to_string(),
            description: "a test entity".to_string(),
            flags: EntityFlags::NONE,
            entity_kind: kind,
            schema_ref,
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
        let type_id = Tuid::new([9u8; 16], 1);
        let header = sample_header(id, "widget-1", EntityKind::Data, type_id);
        let body = gbv_body("active");
        store.insert(header.clone(), &body).unwrap();

        let (got_header, got_body) = store.get(&id).unwrap().unwrap();
        assert_eq!(got_header.id, header.id);
        assert_eq!(got_header.name, "widget-1");
        assert_eq!(got_body, body);

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
        let type_id = Tuid::new([9u8; 16], 1);

        store
            .insert(
                sample_header(id, "v1", EntityKind::Data, type_id),
                &gbv_body("draft"),
            )
            .unwrap();
        store
            .insert(
                sample_header(id, "v2", EntityKind::Data, type_id),
                &gbv_body("active"),
            )
            .unwrap();

        let (header, body) = store.get(&id).unwrap().unwrap();
        assert_eq!(header.name, "v2");
        let reader = GbvReader::new(&body).unwrap();
        assert_eq!(reader.get(1).unwrap().1, b"active");

        // Overwriting must not double-count this entity's ordinal in the
        // secondary index.
        assert_eq!(store.query_by_schema_ref(&type_id).unwrap(), vec![id]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn delete_removes_entity() {
        let dir = tmp_dir("delete");
        let mut store = Store::create(&dir).unwrap();
        let id = Tuid::new([4u8; 16], 4);
        let type_id = Tuid::new([9u8; 16], 1);
        store
            .insert(
                sample_header(id, "gone-soon", EntityKind::Data, type_id),
                &gbv_body("active"),
            )
            .unwrap();

        assert!(store.delete(&id).unwrap());
        assert!(store.get(&id).unwrap().is_none());
        assert!(!store.delete(&id).unwrap(), "already deleted");
        assert!(store.query_by_schema_ref(&type_id).unwrap().is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scan_all_returns_every_entity_in_key_order() {
        let dir = tmp_dir("scan");
        let mut store = Store::create(&dir).unwrap();
        let type_id = Tuid::new([9u8; 16], 1);
        let mut ids = Vec::new();
        for n in 0..50u64 {
            let id = Tuid::new([n as u8; 16], n);
            ids.push(id);
            store
                .insert(
                    sample_header(id, &format!("e{n}"), EntityKind::Data, type_id),
                    &gbv_body("active"),
                )
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
    fn query_by_kind_and_schema_ref() {
        let dir = tmp_dir("secondary");
        let mut store = Store::create(&dir).unwrap();
        let type_a = Tuid::new([1u8; 16], 1);
        let type_b = Tuid::new([2u8; 16], 2);

        let data1 = Tuid::new([10u8; 16], 10);
        let data2 = Tuid::new([11u8; 16], 11);
        let attr1 = Tuid::new([12u8; 16], 12);

        store
            .insert(
                sample_header(data1, "d1", EntityKind::Data, type_a),
                &gbv_body("active"),
            )
            .unwrap();
        store
            .insert(
                sample_header(data2, "d2", EntityKind::Data, type_b),
                &gbv_body("active"),
            )
            .unwrap();
        store
            .insert(
                sample_header(attr1, "a1", EntityKind::EntityAttribute, Tuid::NIL),
                &gbv_body("n/a"),
            )
            .unwrap();

        let mut data_ids = store.query_by_kind(EntityKind::Data).unwrap();
        data_ids.sort();
        let mut expected = vec![data1, data2];
        expected.sort();
        assert_eq!(data_ids, expected);

        assert_eq!(
            store.query_by_kind(EntityKind::EntityAttribute).unwrap(),
            vec![attr1]
        );
        assert_eq!(store.query_by_schema_ref(&type_a).unwrap(), vec![data1]);
        assert_eq!(store.query_by_schema_ref(&type_b).unwrap(), vec![data2]);
        assert!(store
            .query_by_schema_ref(&Tuid::new([99u8; 16], 99))
            .unwrap()
            .is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reopen_after_close_rebuilds_secondary_indexes() {
        let dir = tmp_dir("reopen");
        let id = Tuid::new([5u8; 16], 5);
        let type_id = Tuid::new([9u8; 16], 1);
        {
            let mut store = Store::create(&dir).unwrap();
            store
                .insert(
                    sample_header(id, "persisted", EntityKind::Data, type_id),
                    &gbv_body("active"),
                )
                .unwrap();
        }
        {
            let mut store = Store::open(&dir, true).unwrap();
            let (header, body) = store.get(&id).unwrap().unwrap();
            assert_eq!(header.name, "persisted");
            let reader = GbvReader::new(&body).unwrap();
            assert_eq!(reader.get(1).unwrap().1, b"active");

            assert_eq!(store.query_by_schema_ref(&type_id).unwrap(), vec![id]);
            assert_eq!(store.query_by_kind(EntityKind::Data).unwrap(), vec![id]);

            // A fresh insert after reopen must get a new ordinal, not
            // collide with the rebuilt one.
            let id2 = Tuid::new([6u8; 16], 6);
            store
                .insert(
                    sample_header(id2, "second", EntityKind::Data, type_id),
                    &gbv_body("active"),
                )
                .unwrap();
            let mut ids = store.query_by_schema_ref(&type_id).unwrap();
            ids.sort();
            let mut expected = vec![id, id2];
            expected.sort();
            assert_eq!(ids, expected);
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
