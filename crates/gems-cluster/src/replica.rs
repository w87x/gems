//! `ReplicaClient`: connects to a `ReplicationServer`, requests everything
//! since its own last-applied offset, and applies each record to a local
//! `gems_engine::Store` as it arrives. The offset is persisted to a
//! sidecar file so a restarted replica resumes rather than re-applying (or
//! silently dropping) records — inserts and deletes are both idempotent by
//! id, so a record applied twice after a crash mid-write is harmless, but
//! there's no reason to redo work a replica already durably recorded.
//!
//! This is explicitly **not** Raft: no consensus, no leader election, no
//! protection against a split-brain primary. It's one-way, best-effort,
//! eventually-consistent replication from a single fixed primary address —
//! exactly the "async log-shipping read replicas" stage ARCHITECTURE.md
//! §6/§11 calls for *before* attempting full Raft, not a substitute for it.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::Duration;

use gems_common::Result;
use gems_engine::Store;

use crate::record::LogRecord;

const OFFSET_FILE_NAME: &str = "replica_offset";

pub struct ReplicaClient {
    offset_path: PathBuf,
}

impl ReplicaClient {
    pub fn new(store_dir: &Path) -> Self {
        ReplicaClient {
            offset_path: store_dir.join(OFFSET_FILE_NAME),
        }
    }

    fn load_offset(&self) -> u64 {
        std::fs::read_to_string(&self.offset_path)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    }

    fn save_offset(&self, offset: u64) -> Result<()> {
        std::fs::write(&self.offset_path, offset.to_string())?;
        Ok(())
    }

    /// Connect to `addr`, request everything since this replica's
    /// persisted offset, and apply records to `store` as they arrive.
    /// Blocks on the socket, so this naturally keeps tailing new records
    /// the primary appends while the connection stays open. Returns once
    /// `stop_after` records have been applied (for callers — tests, or a
    /// batch-catch-up mode — that want a bound) or the connection closes;
    /// pass `None` to keep applying until the connection drops, then
    /// reconnect in a loop for a real long-running replica process.
    pub fn sync(
        &self,
        addr: impl ToSocketAddrs,
        store: &mut Store,
        read_timeout: Option<Duration>,
        stop_after: Option<usize>,
    ) -> Result<usize> {
        let mut stream = TcpStream::connect(addr)?;
        stream.set_read_timeout(read_timeout)?;

        let mut offset = self.load_offset();
        stream.write_all(format!("SYNC {offset}\n").as_bytes())?;

        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let mut applied = 0;

        loop {
            let n = stream.read(&mut chunk)?;
            if n == 0 {
                break; // primary closed the connection
            }
            buf.extend_from_slice(&chunk[..n]);

            while let Some((record, consumed)) = LogRecord::decode(&buf)? {
                Self::apply(store, &record)?;
                offset += consumed as u64;
                self.save_offset(offset)?;
                applied += 1;
                buf.drain(..consumed);

                if stop_after.is_some_and(|max| applied >= max) {
                    return Ok(applied);
                }
            }
        }
        Ok(applied)
    }

    fn apply(store: &mut Store, record: &LogRecord) -> Result<()> {
        match record {
            LogRecord::Insert { header, body } => store.insert(header.clone(), body),
            LogRecord::Delete { id } => store.delete(id).map(|_| ()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::ReplicationLog;
    use crate::server::ReplicationServer;
    use gems_catalog::{EntityFlags, EntityHeader, EntityKind};
    use gems_common::Tuid;
    use std::path::PathBuf;
    use std::thread;

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("gems-cluster-replica-test")
            .join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
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
    fn catches_up_on_records_written_before_the_connection() {
        let dir = tmp_dir("catchup");
        let log_path = dir.join("primary.gemlog");
        let log = ReplicationLog::open(&log_path).unwrap();
        let id = Tuid::new([1u8; 16], 1);
        log.append(&LogRecord::Insert {
            header: {
                let mut h = header(id, "w1");
                h.body_offset = gems_catalog::header::ENCODED_LEN as u32;
                h.body_len = 5;
                h
            },
            body: b"hello".to_vec(),
        })
        .unwrap();

        let addr = ReplicationServer::spawn(log_path, "127.0.0.1:0").unwrap();

        let replica_dir = dir.join("replica");
        let mut replica_store = Store::create(&replica_dir).unwrap();
        let client = ReplicaClient::new(&replica_dir);
        let applied = client
            .sync(
                addr,
                &mut replica_store,
                Some(Duration::from_secs(5)),
                Some(1),
            )
            .unwrap();
        assert_eq!(applied, 1);

        let (got_header, got_body) = replica_store.get(&id).unwrap().unwrap();
        assert_eq!(got_header.name, "w1");
        assert_eq!(got_body, b"hello");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn keeps_tailing_records_written_after_the_connection_opens() {
        let dir = tmp_dir("tail");
        let log_path = dir.join("primary.gemlog");
        let addr = ReplicationServer::spawn(log_path.clone(), "127.0.0.1:0").unwrap();

        let replica_dir = dir.join("replica");
        let mut replica_store = Store::create(&replica_dir).unwrap();
        let client = ReplicaClient::new(&replica_dir);

        let id = Tuid::new([2u8; 16], 2);
        let log_for_writer = log_path.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            let log = ReplicationLog::open(&log_for_writer).unwrap();
            let mut h = header(id, "w2");
            h.body_offset = gems_catalog::header::ENCODED_LEN as u32;
            h.body_len = 3;
            log.append(&LogRecord::Insert {
                header: h,
                body: b"abc".to_vec(),
            })
            .unwrap();
        });

        let applied = client
            .sync(
                addr,
                &mut replica_store,
                Some(Duration::from_secs(5)),
                Some(1),
            )
            .unwrap();
        assert_eq!(applied, 1);
        assert!(replica_store.get(&id).unwrap().is_some());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resumes_from_persisted_offset_across_two_sync_calls() {
        let dir = tmp_dir("resume");
        let log_path = dir.join("primary.gemlog");
        let log = ReplicationLog::open(&log_path).unwrap();
        let id1 = Tuid::new([3u8; 16], 3);
        let id2 = Tuid::new([4u8; 16], 4);

        let mut h1 = header(id1, "w1");
        h1.body_offset = gems_catalog::header::ENCODED_LEN as u32;
        h1.body_len = 1;
        log.append(&LogRecord::Insert {
            header: h1,
            body: b"a".to_vec(),
        })
        .unwrap();

        let addr = ReplicationServer::spawn(log_path.clone(), "127.0.0.1:0").unwrap();
        let replica_dir = dir.join("replica");
        let mut replica_store = Store::create(&replica_dir).unwrap();
        let client = ReplicaClient::new(&replica_dir);

        let applied = client
            .sync(
                addr,
                &mut replica_store,
                Some(Duration::from_secs(5)),
                Some(1),
            )
            .unwrap();
        assert_eq!(applied, 1);

        // A second entity appended after the first sync call returned.
        let mut h2 = header(id2, "w2");
        h2.body_offset = gems_catalog::header::ENCODED_LEN as u32;
        h2.body_len = 1;
        log.append(&LogRecord::Insert {
            header: h2,
            body: b"b".to_vec(),
        })
        .unwrap();

        // A fresh sync call must resume from the persisted offset, not
        // from zero (which would just re-apply id1 harmlessly, but should
        // still correctly pick up id2 either way — this asserts it does).
        let addr2 = ReplicationServer::spawn(log_path, "127.0.0.1:0").unwrap();
        let applied2 = client
            .sync(
                addr2,
                &mut replica_store,
                Some(Duration::from_secs(5)),
                Some(1),
            )
            .unwrap();
        assert_eq!(applied2, 1);
        assert!(replica_store.get(&id2).unwrap().is_some());

        std::fs::remove_dir_all(&dir).ok();
    }
}
