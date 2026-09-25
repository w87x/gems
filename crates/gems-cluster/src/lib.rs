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

mod frame;
pub mod gossip;
mod log;
mod primary;
pub mod raft;
pub mod raft_net;
mod record;
mod replica;
mod server;
pub mod shard;
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

    /// The full sharding story: two independent 3-node Raft clusters (two
    /// shards), a `ShardedClient` configured with both, and a proposal per
    /// shard that must land — via real TCP, real elections, real
    /// `gems_engine::Store` writes — only in the cluster the router
    /// actually assigned it to.
    #[test]
    fn sharded_client_routes_each_entity_to_its_own_raft_group() {
        use crate::raft::NodeId;
        use crate::raft_net;
        use crate::shard::{ShardMap, ShardRouter, ShardedClient};
        use std::collections::HashMap;
        use std::net::{SocketAddr, TcpListener};

        fn free_port() -> u16 {
            TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port()
        }

        fn spawn_shard(
            shard_ids: &[NodeId],
            dir: &std::path::Path,
        ) -> (Vec<raft_net::RaftNodeHandle>, Vec<SocketAddr>) {
            let peer_ports: Vec<u16> = shard_ids.iter().map(|_| free_port()).collect();
            let client_ports: Vec<u16> = shard_ids.iter().map(|_| free_port()).collect();
            let peer_addrs: HashMap<NodeId, SocketAddr> = shard_ids
                .iter()
                .zip(&peer_ports)
                .map(|(&id, &port)| (id, format!("127.0.0.1:{port}").parse().unwrap()))
                .collect();
            let client_addrs: Vec<SocketAddr> = client_ports
                .iter()
                .map(|&port| format!("127.0.0.1:{port}").parse().unwrap())
                .collect();

            let mut handles = Vec::new();
            for (i, &id) in shard_ids.iter().enumerate() {
                let peers: HashMap<NodeId, SocketAddr> = peer_addrs
                    .iter()
                    .filter(|(&pid, _)| pid != id)
                    .map(|(&pid, &addr)| (pid, addr))
                    .collect();
                let handle = raft_net::spawn(
                    id,
                    &format!("127.0.0.1:{}", peer_ports[i]),
                    &format!("127.0.0.1:{}", client_ports[i]),
                    peers,
                    &dir.join(format!("node{id}")),
                    raft_net::RaftTiming {
                        tick_interval: Duration::from_millis(20),
                        heartbeat_interval_ticks: 3,
                        election_timeout_ticks_range: (6, 10),
                    },
                )
                .unwrap();
                handles.push(handle);
            }
            (handles, client_addrs)
        }

        fn wait_for_leader(handles: &[raft_net::RaftNodeHandle], timeout: Duration) -> bool {
            let deadline = std::time::Instant::now() + timeout;
            while std::time::Instant::now() < deadline {
                if handles
                    .iter()
                    .any(|h| matches!(h.status(), Ok((crate::raft::Role::Leader, _))))
                {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            false
        }

        let dir = tmp_dir("sharded");
        let shard0_ids: Vec<NodeId> = vec![1, 2, 3];
        let shard1_ids: Vec<NodeId> = vec![11, 12, 13];

        let (shard0_handles, shard0_client_addrs) = spawn_shard(&shard0_ids, &dir.join("shard0"));
        let (shard1_handles, shard1_client_addrs) = spawn_shard(&shard1_ids, &dir.join("shard1"));

        assert!(
            wait_for_leader(&shard0_handles, Duration::from_secs(5)),
            "shard 0 must elect a leader"
        );
        assert!(
            wait_for_leader(&shard1_handles, Duration::from_secs(5)),
            "shard 1 must elect a leader"
        );

        let mut map = ShardMap::new();
        for (&id, &addr) in shard0_ids.iter().zip(&shard0_client_addrs) {
            map.add_member(0, id, addr);
        }
        for (&id, &addr) in shard1_ids.iter().zip(&shard1_client_addrs) {
            map.add_member(1, id, addr);
        }
        let client = ShardedClient::new(ShardRouter::new(2), map, Duration::from_secs(2));

        // Leading UUID byte 0x00 routes to shard 0, 0xFF to shard 1, for a
        // 2-way router (see ShardRouter::shard_of's arithmetic).
        let mut uuid_a = [0u8; 16];
        uuid_a[0] = 0x00;
        let entity_a = Tuid::new(uuid_a, 1);
        let mut uuid_b = [0u8; 16];
        uuid_b[0] = 0xFF;
        let entity_b = Tuid::new(uuid_b, 2);

        assert_eq!(client.shard_of(&entity_a), 0);
        assert_eq!(client.shard_of(&entity_b), 1);

        client
            .propose(
                &entity_a,
                LogRecord::Insert {
                    header: header(entity_a, "in-shard-0"),
                    body: b"a".to_vec(),
                },
            )
            .unwrap();
        client
            .propose(
                &entity_b,
                LogRecord::Insert {
                    header: header(entity_b, "in-shard-1"),
                    body: b"b".to_vec(),
                },
            )
            .unwrap();

        // Give each shard's cluster time to apply, then check every node's
        // on-disk state directly.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let shard0_has_a = shard0_ids.iter().all(|&id| {
                gems_engine::Store::open(&dir.join("shard0").join(format!("node{id}")), false)
                    .ok()
                    .and_then(|s| s.get(&entity_a).ok().flatten())
                    .is_some()
            });
            let shard1_has_b = shard1_ids.iter().all(|&id| {
                gems_engine::Store::open(&dir.join("shard1").join(format!("node{id}")), false)
                    .ok()
                    .and_then(|s| s.get(&entity_b).ok().flatten())
                    .is_some()
            });
            if shard0_has_a && shard1_has_b {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "both shards should have applied their respective entity within 5 seconds"
            );
            std::thread::sleep(Duration::from_millis(50));
        }

        // And cross-check: shard 0 must never have received entity_b, nor
        // shard 1 entity_a — that's the actual point of sharding.
        for &id in &shard0_ids {
            let store =
                gems_engine::Store::open(&dir.join("shard0").join(format!("node{id}")), false)
                    .unwrap();
            assert!(store.get(&entity_b).unwrap().is_none());
        }
        for &id in &shard1_ids {
            let store =
                gems_engine::Store::open(&dir.join("shard1").join(format!("node{id}")), false)
                    .unwrap();
            assert!(store.get(&entity_a).unwrap().is_none());
        }

        for handle in shard0_handles {
            handle.shutdown();
        }
        for handle in shard1_handles {
            handle.shutdown();
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
