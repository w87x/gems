//! Stage 2 of ARCHITECTURE.md §6/§11's clustering plan: async log-shipping
//! read replicas, deliberately *before* attempting full Raft. A
//! `PrimaryStore` wraps `gems_engine::Store` and appends every mutation to
//! a `ReplicationLog`; a `ReplicationServer` streams that log to any
//! number of connecting `ReplicaClient`s over a plain TCP socket, each
//! applying records to its own local `Store`.
//!
//! What this gives you: horizontal read scaling and a warm standby, cheap
//! to build and reason about. What it explicitly does **not** give you:
//! consensus, leader election, or split-brain protection — there is
//! exactly one primary, chosen out of band (a human, a config file), and
//! if it dies, promoting a replica to primary is a manual/operational
//! step, not something this crate arbitrates. That gap is precisely what
//! full Raft closes, and precisely why ARCHITECTURE.md recommends proving
//! out this simpler stage first: the apply-record format
//! (`LogRecord`/`ReplicationLog`) is the same shape a Raft log entry would
//! need, so getting it right here isn't wasted work — it's what Raft would
//! build on, not something Raft replaces.

mod log;
mod primary;
mod record;
mod replica;
mod server;

pub use log::ReplicationLog;
pub use primary::PrimaryStore;
pub use record::LogRecord;
pub use replica::ReplicaClient;
pub use server::ReplicationServer;

#[cfg(test)]
mod integration_tests {
    use super::*;
    use gems_catalog::{EntityFlags, EntityHeader, EntityKind};
    use gems_common::Tuid;
    use std::path::PathBuf;
    use std::time::Duration;

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("gems-cluster-integration-test")
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

    /// The end-to-end story: a primary that real callers write through
    /// (`PrimaryStore::insert`/`delete`, going through the same
    /// `gems_engine::Store` as a single-node deployment), a replica that
    /// starts from nothing, catches up, keeps tailing live writes, and
    /// ends up with equivalent state — including a delete, not just
    /// inserts.
    #[test]
    fn replica_converges_with_primary_across_inserts_and_a_delete() {
        let dir = tmp_dir("converge");
        let primary_dir = dir.join("primary");
        let mut primary = PrimaryStore::create(&primary_dir).unwrap();

        let w1 = Tuid::new([1u8; 16], 1);
        let w2 = Tuid::new([2u8; 16], 2);
        primary.insert(header(w1, "w1"), b"first").unwrap();

        let addr =
            ReplicationServer::spawn(primary.log_path().to_path_buf(), "127.0.0.1:0").unwrap();

        let replica_dir = dir.join("replica");
        let mut replica_store = gems_engine::Store::create(&replica_dir).unwrap();
        let client = ReplicaClient::new(&replica_dir);

        // Catch up on the pre-existing insert.
        let applied = client
            .sync(
                addr,
                &mut replica_store,
                Some(Duration::from_secs(5)),
                Some(1),
            )
            .unwrap();
        assert_eq!(applied, 1);
        assert_eq!(
            replica_store.get(&w1).unwrap().unwrap().1,
            primary.store.get(&w1).unwrap().unwrap().1
        );

        // A second insert and a delete, applied on the primary after the
        // replica already caught up once.
        primary.insert(header(w2, "w2"), b"second").unwrap();
        primary.delete(&w1).unwrap();

        let addr2 =
            ReplicationServer::spawn(primary.log_path().to_path_buf(), "127.0.0.1:0").unwrap();
        let applied2 = client
            .sync(
                addr2,
                &mut replica_store,
                Some(Duration::from_secs(5)),
                Some(2),
            )
            .unwrap();
        assert_eq!(applied2, 2);

        assert!(
            replica_store.get(&w1).unwrap().is_none(),
            "w1 was deleted on the primary"
        );
        assert!(primary.store.get(&w1).unwrap().is_none());
        assert_eq!(
            replica_store.get(&w2).unwrap().unwrap().1,
            primary.store.get(&w2).unwrap().unwrap().1
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
