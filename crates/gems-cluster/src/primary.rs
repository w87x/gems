//! `PrimaryStore`: a `gems_engine::Store` that appends a `LogRecord` to its
//! `ReplicationLog` after every successful mutation, durably and in the
//! same order they were applied locally — the log a replica tails.

use std::path::Path;

use gems_catalog::EntityHeader;
use gems_common::{Result, Tuid};
use gems_engine::Store;

use crate::log::ReplicationLog;
use crate::record::LogRecord;

const REPLICATION_LOG_FILE_NAME: &str = "replication.gemlog";

pub struct PrimaryStore {
    pub store: Store,
    log: ReplicationLog,
}

impl PrimaryStore {
    pub fn create(dir: &Path) -> Result<Self> {
        let store = Store::create(dir)?;
        let log = ReplicationLog::open(&dir.join(REPLICATION_LOG_FILE_NAME))?;
        Ok(PrimaryStore { store, log })
    }

    pub fn open(dir: &Path) -> Result<Self> {
        let store = Store::open(dir, true)?;
        let log = ReplicationLog::open(&dir.join(REPLICATION_LOG_FILE_NAME))?;
        Ok(PrimaryStore { store, log })
    }

    /// Insert, then log the record. Re-reads the entity back out of the
    /// store rather than logging the caller's header verbatim, since
    /// `Store::insert` fills in `body_offset`/`body_len` internally — the
    /// logged record needs to be the actual stored shape, byte-for-byte,
    /// or a replica applying it would end up with a header whose body
    /// offsets don't match what it wrote.
    pub fn insert(&mut self, header: EntityHeader, body: &[u8]) -> Result<()> {
        let id = header.id;
        self.store.insert(header, body)?;
        let (stored_header, stored_body) = self.store.get(&id)?.expect("just inserted");
        self.log.append(&LogRecord::Insert {
            header: stored_header,
            body: stored_body,
        })
    }

    pub fn delete(&mut self, id: &Tuid) -> Result<bool> {
        let existed = self.store.delete(id)?;
        if existed {
            self.log.append(&LogRecord::Delete { id: *id })?;
        }
        Ok(existed)
    }

    pub fn log_len(&self) -> Result<u64> {
        self.log.len()
    }

    /// The replication log's file path — what to pass to
    /// `ReplicationServer::spawn` to serve this primary's log.
    pub fn log_path(&self) -> &Path {
        self.log.path()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gems_catalog::{EntityFlags, EntityKind};
    use std::path::PathBuf;

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("gems-cluster-primary-test")
            .join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn header(id: Tuid, name: &str) -> EntityHeader {
        EntityHeader {
            id,
            created_by: [0u8; 16],
            modified_by: [0u8; 16],
            modified_at_ns: 0,
            name: name.to_string(),
            description: String::new(),
            flags: EntityFlags::NONE,
            entity_kind: EntityKind::Data,
            schema_ref: Tuid::NIL,
            body_offset: 0,
            body_len: 0,
        }
    }

    #[test]
    fn insert_appends_a_record_matching_what_was_stored() {
        let dir = tmp_dir("insert_logs");
        let mut primary = PrimaryStore::create(&dir).unwrap();
        let id = Tuid::new([1u8; 16], 1);
        primary.insert(header(id, "w1"), b"hello").unwrap();

        let (records, _) = primary.log.read_from(0).unwrap();
        assert_eq!(records.len(), 1);
        let LogRecord::Insert {
            header: logged_header,
            body: logged_body,
        } = &records[0]
        else {
            panic!("expected an Insert record");
        };
        assert_eq!(logged_header.id, id);
        assert_eq!(logged_body, b"hello");

        // The logged header must match the actually-stored one, including
        // the body_offset/body_len the raw caller-supplied header didn't
        // have set.
        let (stored_header, _) = primary.store.get(&id).unwrap().unwrap();
        assert_eq!(logged_header.body_offset, stored_header.body_offset);
        assert_eq!(logged_header.body_len, stored_header.body_len);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn delete_appends_a_record_only_when_the_entity_existed() {
        let dir = tmp_dir("delete_logs");
        let mut primary = PrimaryStore::create(&dir).unwrap();
        let id = Tuid::new([2u8; 16], 2);

        assert!(!primary.delete(&id).unwrap());
        assert!(primary.log.is_empty().unwrap());

        primary.insert(header(id, "w1"), b"x").unwrap();
        assert!(primary.delete(&id).unwrap());

        let (records, _) = primary.log.read_from(0).unwrap();
        assert_eq!(records.len(), 2);
        assert!(matches!(records[1], LogRecord::Delete { id: logged_id } if logged_id == id));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reopen_continues_appending_to_the_same_log() {
        let dir = tmp_dir("reopen");
        let id1 = Tuid::new([3u8; 16], 3);
        let id2 = Tuid::new([4u8; 16], 4);
        {
            let mut primary = PrimaryStore::create(&dir).unwrap();
            primary.insert(header(id1, "w1"), b"a").unwrap();
        }
        {
            let mut primary = PrimaryStore::open(&dir).unwrap();
            primary.insert(header(id2, "w2"), b"b").unwrap();
            let (records, _) = primary.log.read_from(0).unwrap();
            assert_eq!(records.len(), 2);
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
