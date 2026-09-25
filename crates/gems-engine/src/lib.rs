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
//! - **A first, narrow query planner** (`Store::query`): compiles a
//!   `gems_query::Query` against the indexes above, but only understands a
//!   top-level `type IN (...)` filter (or no filter at all) — exactly
//!   ARCHITECTURE.md §7's example's `type IN (xx, yy, zz)` clause, resolved
//!   by a new name index over `EntityType`-kind entities so type names in
//!   the query resolve to the TUIDs `query_by_schema_ref` needs. Compound
//!   filters (`AND`/`OR` with `attr.*`/`layer.*` predicates), `ORDER BY`,
//!   and non-type `IN`/comparison predicates all return an explicit
//!   "unsupported" error rather than silently ignoring part of the query —
//!   the rest of the planner (attribute lookups needing `EntityType`
//!   resolution, layer membership) is real, separate work, not something
//!   to fake here.
//! - **ABAC is wired in as an opt-in enforced path** (`get_enforced`/
//!   `query_enforced`, using `gems-abac`'s PDP/PEP over `load_policies`),
//!   not as the only way to read. `get`/`query` stay unenforced — this
//!   crate doesn't force every caller through a subject context (an
//!   internal migration job, a CLI running as an administrator, or a
//!   replication stream all legitimately need raw access), so ABAC is
//!   something a caller opts into per read, not a mode the whole store is
//!   switched into.
//!
//! **Concurrency contract**: a `Store` is not `Sync` in any load-bearing
//! sense — every mutating method takes `&mut self`, so within one process
//! the borrow checker already forces external synchronization (a
//! `Mutex<Store>`, or a single owning thread) around concurrent access.
//! Across *processes*, nothing before this comment stopped two of them
//! from opening the same directory writable at once — the copy-on-write
//! page allocator and in-memory freelist state in `gems-index::Pager` and
//! `gems-storage::ExtentManager` both assume they're the only writer, so
//! two independent writers racing would corrupt the store (one reusing a
//! page or extent slot the other just allocated, both computing an
//! inconsistent freelist). `create`/`open(_, writable: true)` now take an
//! exclusive, non-blocking `gems_common::filelock` on the store directory
//! and hold it for the `Store`'s lifetime, so a second writable open from
//! any process — including a stray second instance of the same process —
//! fails fast with `Error::AlreadyLocked` instead of corrupting data.
//! `open(_, writable: false)` does not lock: concurrent read-only opens
//! are not the corruption scenario this guards against, and forbidding
//! them would break the (already-relied-on, e.g. in
//! `gems-cluster`'s integration tests) pattern of peeking at a store's
//! persisted state from a second, short-lived, read-only `Store` while
//! another process's writer keeps it open.

mod ordinal;
mod secondary;

use std::path::{Path, PathBuf};

use gems_catalog::{EntityHeader, EntityKind};
use gems_common::filelock::FileLock;
use gems_common::{Error, Result, Tuid};
use gems_index::BTree;
use gems_query::{Expr, Literal, Query};
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
    /// Name -> ordinal, populated only for `EntityKind::EntityType`
    /// entities — how the planner resolves a bare `type IN (widget, ...)`
    /// name to the TUID `by_schema_ref` is keyed on.
    entity_type_by_name: SecondaryIndex,
    /// Held only for a writable `Store` (see the crate doc's concurrency
    /// contract) — `None` for a read-only open. Releases automatically on
    /// `Drop`.
    _lock: Option<FileLock>,
}

fn kind_key(kind: EntityKind) -> Vec<u8> {
    vec![kind as u8]
}

fn schema_ref_key(schema_ref: &Tuid) -> Vec<u8> {
    schema_ref.as_bytes().to_vec()
}

fn name_key(name: &str) -> Vec<u8> {
    name.as_bytes().to_vec()
}

impl Store {
    /// Create a fresh store rooted at `dir` (created if it doesn't exist).
    pub fn create(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        // Acquired before any file in `dir` is touched, so a second
        // process racing to `create`/`open(_, true)` the same directory
        // fails here rather than after already allocating pages.
        let lock = gems_common::filelock::acquire_exclusive_in_dir(dir)?;
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
            entity_type_by_name: SecondaryIndex::new(),
            _lock: Some(lock),
        })
    }

    pub fn open(dir: &Path, writable: bool) -> Result<Self> {
        // See the crate doc's concurrency contract: only a writable open
        // takes the exclusive lock. A read-only open is intentionally
        // lock-free.
        let lock = writable
            .then(|| gems_common::filelock::acquire_exclusive_in_dir(dir))
            .transpose()?;
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
            entity_type_by_name: SecondaryIndex::new(),
            _lock: lock,
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
            self.index_header(&header, ord);
        }
        self.next_ordinal = max_ordinal.map_or(0, |m| m + 1);
        Ok(())
    }

    fn index_header(&mut self, header: &EntityHeader, ord: u32) {
        self.by_kind.insert(kind_key(header.entity_kind), ord);
        self.by_schema_ref
            .insert(schema_ref_key(&header.schema_ref), ord);
        if header.entity_kind == EntityKind::EntityType {
            self.entity_type_by_name.insert(name_key(&header.name), ord);
        }
    }

    fn deindex_header(&mut self, header: &EntityHeader, ord: u32) {
        self.by_kind.remove(&kind_key(header.entity_kind), ord);
        self.by_schema_ref
            .remove(&schema_ref_key(&header.schema_ref), ord);
        if header.entity_kind == EntityKind::EntityType {
            self.entity_type_by_name
                .remove(&name_key(&header.name), ord);
        }
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
            self.deindex_header(&old_header, ord.0);
            self.data.free(old_ptr)?;
        }

        self.index_header(&header, ord.0);

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
            self.deindex_header(&header, ord.0);
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

    /// Resolve an `EntityType`'s name (as written unquoted in a query, e.g.
    /// `type IN (widget, gadget)`) to its TUID. `None` if no `EntityType`
    /// entity has that name.
    pub fn find_entity_type_by_name(&self, name: &str) -> Result<Option<Tuid>> {
        let ordinals = self.entity_type_by_name.get(&name_key(name));
        let ids = self.resolve_ordinals(ordinals)?;
        Ok(ids.into_iter().next())
    }

    /// Compile and execute a `gems_query::Query` against this store. See
    /// the crate doc for exactly what's supported in this pass: no filter,
    /// or a single top-level `type IN (...)`; `LIMIT` is applied to the
    /// (Tuid-ordered) result, everything else is an explicit error rather
    /// than a silently partial answer.
    pub fn query(&self, query: &Query) -> Result<Vec<Tuid>> {
        if !query.from.eq_ignore_ascii_case("entities") {
            return Err(Error::InvalidValue {
                detail: "the only queryable source is `entities`",
            });
        }
        if !query.order_by.is_empty() {
            return Err(Error::InvalidValue {
                detail: "ORDER BY is not supported by this v1 planner",
            });
        }

        let mut ids = match &query.filter {
            None => self.scan_all()?.into_iter().map(|(h, _)| h.id).collect(),
            Some(filter) => self.eval_type_in_filter(filter)?,
        };

        ids.sort();
        if let Some(limit) = query.limit {
            ids.truncate(limit as usize);
        }
        Ok(ids)
    }

    fn eval_type_in_filter(&self, expr: &Expr) -> Result<Vec<Tuid>> {
        let Expr::In { field, values } = expr else {
            return Err(Error::InvalidValue {
                detail: "this v1 planner only supports a top-level `type IN (...)` filter",
            });
        };
        if !field.eq_ignore_ascii_case("type") {
            return Err(Error::InvalidValue {
                detail: "this v1 planner only supports filtering on `type`",
            });
        }

        let mut matched = gems_bitmap::RoaringBitmap::new();
        for value in values {
            let name = match value {
                Literal::Ident(s) | Literal::Str(s) => s.as_str(),
                _ => {
                    return Err(Error::InvalidValue {
                        detail: "type names in `type IN (...)` must be identifiers or strings",
                    })
                }
            };
            if let Some(type_id) = self.find_entity_type_by_name(name)? {
                matched = matched.union(&self.by_schema_ref.get(&schema_ref_key(&type_id)));
            }
        }
        self.resolve_ordinals(matched)
    }

    /// Every `Policy` entity currently stored, decoded. Policies are
    /// ordinary entities (ARCHITECTURE.md §8), so this is just
    /// `query_by_kind(Policy)` plus a decode of each body — no separate
    /// policy store to keep in sync.
    pub fn load_policies(&self) -> Result<Vec<gems_catalog::Policy>> {
        self.query_by_kind(EntityKind::Policy)?
            .into_iter()
            .map(|id| {
                let (_, body) = self.get(&id)?.ok_or(Error::CorruptPage {
                    detail: "policy id from secondary index missing from storage",
                })?;
                gems_catalog::Policy::decode(&body)
            })
            .collect()
    }

    /// `get`, filtered/redacted through the ABAC PEP (`gems_abac::enforce`)
    /// for `subject`. `None` both when the entity doesn't exist and when it
    /// exists but no policy permits `subject` to see it — the two are
    /// indistinguishable by design (ARCHITECTURE.md §8's PEP never reveals
    /// *that* a denied entity exists, only that the result has nothing for
    /// this id).
    pub fn get_enforced(
        &self,
        id: &Tuid,
        subject: &gems_abac::SubjectContext,
    ) -> Result<Option<(EntityHeader, Vec<u8>)>> {
        let Some(pair) = self.get(id)? else {
            return Ok(None);
        };
        let policies = self.load_policies()?;
        Ok(gems_abac::enforce(&policies, subject, [pair])
            .into_iter()
            .next())
    }

    /// `query`, filtered/redacted through the ABAC PEP for `subject`. This
    /// is the "query result can be partial and some fields or whole
    /// objects won't show" behavior from ARCHITECTURE.md §8: candidates
    /// come from the same planner as `query`, then each one is decided and
    /// possibly redacted before being returned.
    pub fn query_enforced(
        &self,
        query: &Query,
        subject: &gems_abac::SubjectContext,
    ) -> Result<Vec<(EntityHeader, Vec<u8>)>> {
        let ids = self.query(query)?;
        let policies = self.load_policies()?;
        let mut entities = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(pair) = self.get(&id)? {
                entities.push(pair);
            }
        }
        Ok(gems_abac::enforce(&policies, subject, entities))
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
    fn a_second_writable_open_of_the_same_store_is_refused() {
        let dir = tmp_dir("second_writer_refused");
        let _first = Store::create(&dir).unwrap();
        let second = Store::open(&dir, true);
        assert!(
            matches!(second, Err(Error::AlreadyLocked { .. })),
            "a second writable open must fail fast instead of risking corrupting the store"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_writable_open_succeeds_again_once_the_first_is_dropped() {
        let dir = tmp_dir("writer_after_drop");
        {
            let _first = Store::create(&dir).unwrap();
        }
        assert!(Store::open(&dir, true).is_ok());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_only_opens_do_not_take_the_lock_and_can_coexist_with_a_writer() {
        let dir = tmp_dir("readers_coexist_with_writer");
        let id = Tuid::new([3u8; 16], 3);
        let type_id = Tuid::new([4u8; 16], 4);
        let writer = {
            let mut store = Store::create(&dir).unwrap();
            store
                .insert(
                    sample_header(id, "w", EntityKind::Data, type_id),
                    &gbv_body("active"),
                )
                .unwrap();
            store
        };
        // The writer is still open (not dropped) — a read-only open of the
        // same directory must still succeed, unlike a writable one.
        let reader1 = Store::open(&dir, false).unwrap();
        let reader2 = Store::open(&dir, false).unwrap();
        assert!(reader1.get(&id).unwrap().is_some());
        assert!(reader2.get(&id).unwrap().is_some());
        drop(writer);
        std::fs::remove_dir_all(&dir).ok();
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

    #[test]
    fn query_parses_and_executes_type_in_end_to_end() {
        let dir = tmp_dir("query_type_in");
        let mut store = Store::create(&dir).unwrap();

        // The EntityType entities that give "widget" and "gadget" meaning.
        let widget_type = Tuid::new([20u8; 16], 20);
        let gadget_type = Tuid::new([21u8; 16], 21);
        let other_type = Tuid::new([22u8; 16], 22);
        store
            .insert(
                sample_header(widget_type, "widget", EntityKind::EntityType, Tuid::NIL),
                &[],
            )
            .unwrap();
        store
            .insert(
                sample_header(gadget_type, "gadget", EntityKind::EntityType, Tuid::NIL),
                &[],
            )
            .unwrap();
        store
            .insert(
                sample_header(other_type, "other", EntityKind::EntityType, Tuid::NIL),
                &[],
            )
            .unwrap();

        let w1 = Tuid::new([30u8; 16], 30);
        let g1 = Tuid::new([31u8; 16], 31);
        let o1 = Tuid::new([32u8; 16], 32);
        store
            .insert(
                sample_header(w1, "w1", EntityKind::Data, widget_type),
                &gbv_body("active"),
            )
            .unwrap();
        store
            .insert(
                sample_header(g1, "g1", EntityKind::Data, gadget_type),
                &gbv_body("active"),
            )
            .unwrap();
        store
            .insert(
                sample_header(o1, "o1", EntityKind::Data, other_type),
                &gbv_body("active"),
            )
            .unwrap();

        let query =
            gems_query::parse("SELECT * FROM entities WHERE type IN (widget, gadget)").unwrap();
        let mut result = store.query(&query).unwrap();
        result.sort();
        let mut expected = vec![w1, g1];
        expected.sort();
        assert_eq!(result, expected);

        // A LIMIT clause truncates the (Tuid-ordered) result.
        let limited =
            gems_query::parse("SELECT * FROM entities WHERE type IN (widget, gadget) LIMIT 1")
                .unwrap();
        assert_eq!(store.query(&limited).unwrap().len(), 1);

        // Unsupported constructs are an explicit error, not a silently
        // partial answer.
        let unsupported =
            gems_query::parse("SELECT * FROM entities WHERE attr.status = 'active'").unwrap();
        assert!(store.query(&unsupported).is_err());

        let ordered =
            gems_query::parse("SELECT * FROM entities ORDER BY modified_at DESC").unwrap();
        assert!(store.query(&ordered).is_err());

        std::fs::remove_dir_all(&dir).ok();
    }

    fn insert_policy(store: &mut Store, policy: &gems_catalog::Policy, name: &str) -> Tuid {
        let id = Tuid::generate();
        store
            .insert(
                sample_header(id, name, EntityKind::Policy, Tuid::NIL),
                &policy.encode(),
            )
            .unwrap();
        id
    }

    #[test]
    fn no_policies_means_enforced_reads_see_nothing() {
        let dir = tmp_dir("abac_default_deny");
        let mut store = Store::create(&dir).unwrap();
        let type_id = Tuid::new([9u8; 16], 1);
        let id = Tuid::new([1u8; 16], 1);
        store
            .insert(
                sample_header(id, "w1", EntityKind::Data, type_id),
                &gbv_body("active"),
            )
            .unwrap();

        let anonymous = gems_abac::SubjectContext {
            subject_id: Tuid::NIL,
            roles: vec![],
        };
        assert!(store.get_enforced(&id, &anonymous).unwrap().is_none());

        let query = gems_query::parse("SELECT * FROM entities").unwrap();
        assert!(store.query_enforced(&query, &anonymous).unwrap().is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_permit_policy_makes_enforced_reads_visible() {
        let dir = tmp_dir("abac_permit");
        let mut store = Store::create(&dir).unwrap();
        let type_id = Tuid::new([9u8; 16], 1);
        let id = Tuid::new([1u8; 16], 1);
        store
            .insert(
                sample_header(id, "w1", EntityKind::Data, type_id),
                &gbv_body("active"),
            )
            .unwrap();

        insert_policy(
            &mut store,
            &gems_catalog::Policy {
                target: gems_catalog::TargetPredicate::ANY,
                subject: gems_catalog::SubjectPredicate::ANY,
                effect: gems_catalog::Effect::Permit,
                redact_attributes: vec![],
            },
            "allow-all",
        );

        let anonymous = gems_abac::SubjectContext {
            subject_id: Tuid::NIL,
            roles: vec![],
        };
        let (header, _) = store.get_enforced(&id, &anonymous).unwrap().unwrap();
        assert_eq!(header.id, id);

        let query = gems_query::parse("SELECT * FROM entities").unwrap();
        let results = store.query_enforced(&query, &anonymous).unwrap();
        // The permit-all policy is itself a Policy-kind entity with no
        // matching target restriction, so it's visible too.
        assert!(results.iter().any(|(h, _)| h.id == id));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn query_enforced_redacts_fields_per_permit_obligation() {
        let dir = tmp_dir("abac_redact");
        let mut store = Store::create(&dir).unwrap();
        let type_id = Tuid::new([9u8; 16], 1);
        let id = Tuid::new([1u8; 16], 1);

        let mut body = GbvBuilder::new();
        body.push(1, TypeTag::Str, b"public-value");
        body.push(2, TypeTag::Str, b"secret-value");
        let body = body.finish();
        store
            .insert(sample_header(id, "w1", EntityKind::Data, type_id), &body)
            .unwrap();

        insert_policy(
            &mut store,
            &gems_catalog::Policy {
                target: gems_catalog::TargetPredicate {
                    entity_kind: Some(EntityKind::Data),
                    schema_ref: type_id,
                },
                subject: gems_catalog::SubjectPredicate::ANY,
                effect: gems_catalog::Effect::Permit,
                redact_attributes: vec![2],
            },
            "redact-field-2",
        );

        let anonymous = gems_abac::SubjectContext {
            subject_id: Tuid::NIL,
            roles: vec![],
        };
        let (_, redacted_body) = store.get_enforced(&id, &anonymous).unwrap().unwrap();
        let reader = GbvReader::new(&redacted_body).unwrap();
        assert_eq!(reader.get(1).unwrap().1, b"public-value");
        assert!(reader.get(2).is_none());

        std::fs::remove_dir_all(&dir).ok();
    }
}
