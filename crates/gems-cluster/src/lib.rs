//! Clustering, per ARCHITECTURE.md §6/§11's staged plan:
//!
//! - **Stage 2 — async log-shipping read replicas** (`log`, `primary`,
//!   `record`, `replica`, `server`): a `PrimaryStore` wraps
//!   `gems_engine::Store` and appends every mutation to a
//!   `ReplicationLog`; a `ReplicationServer` streams that log to any
//!   number of connecting `ReplicaClient`s over a plain TCP socket, each
//!   applying records to its own local `Store`. Gives you horizontal read
//!   scaling and a warm standby, cheaply. Gives you **no** consensus,
//!   leader election, or split-brain protection — exactly one primary,
//!   chosen out of band, and promoting a replica after it dies is a
//!   manual step this stage doesn't arbitrate.
//! - **Stage 3 — full Raft** (`raft`): closes that gap. `RaftCore` is a
//!   pure, I/O-free consensus state machine (see its module doc for why);
//!   its committed log entries are `LogRecord`s, the exact type stage 2's
//!   `ReplicaClient` already knows how to apply to a `Store` — the two
//!   stages share the same apply-record format by design, per
//!   ARCHITECTURE.md's note that getting stage 2's format right isn't
//!   wasted work ahead of stage 3. Wiring `RaftCore` to real sockets and a
//!   real timer (the "shell" its module doc describes) is the remaining
//!   piece before this closes the loop into an actual replicated
//!   `gems_engine::Store` end to end.
//! - **Cluster membership and failure detection** (`gossip`): the other
//!   half of §6 — "gossip (SWIM-style) for cluster membership, failure
//!   detection, and disseminating the shard map." `SwimCore` is the same
//!   pure-state-machine shape as `RaftCore`, for the same testability
//!   reason; see its module doc for what's simplified relative to full
//!   SWIM (no indirect probing, round-robin rather than randomized ping
//!   order) and why those are reasonable cuts for a trusted-cluster
//!   context rather than gaps to paper over. This is deliberately
//!   independent of the Raft module — real deployments would use gossip
//!   to disseminate *which nodes exist and are reachable*, separately
//!   from Raft's job of getting a specific shard's replicas to agree on
//!   its log.

pub mod gossip;
mod log;
mod primary;
pub mod raft;
pub mod raft_net;
mod record;
mod replica;
mod server;
pub mod swim_net;

pub use gossip::SwimCore;
pub use log::ReplicationLog;
pub use primary::PrimaryStore;
pub use raft::{RaftCore, RaftError};
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
