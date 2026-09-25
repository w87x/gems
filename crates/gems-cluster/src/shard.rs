//! Sharding: partition entities across independent Raft groups by TUID
//! prefix, per ARCHITECTURE.md §6 ("partition by TUID prefix... since it's
//! uniformly random, this gives even distribution without needing a
//! separate hash step").
//!
//! **Scope for this pass**, consistent with how every other piece of this
//! workspace started narrow: the shard map (which shard owns which node
//! addresses) is **static configuration**, not dynamically negotiated —
//! there's no rebalancing, no adding/removing shards at runtime, and no
//! use of `gossip`'s dissemination for it yet, even though
//! ARCHITECTURE.md §6 names that as gossip's eventual job. A node hosting
//! more than one shard just means calling `raft_net::spawn` once per shard
//! it participates in, on different ports and store directories — nothing
//! about `raft_net`/`raft` needed to change to support that, since each
//! spawned node is already fully independent.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;

use gems_common::{Error, Result, Tuid};

use crate::raft::NodeId;
use crate::raft_net::propose_remote;
use crate::record::LogRecord;

pub type ShardId = u32;

/// Partitions TUIDs into `num_shards` shards by the leading byte of their
/// UUID half — uniform since that byte is random, and cheap (no hashing
/// needed). `num_shards` need not be a power of two; the leading byte's
/// range (0..=255) is divided as evenly as integer division allows.
#[derive(Debug, Clone, Copy)]
pub struct ShardRouter {
    num_shards: u32,
}

impl ShardRouter {
    pub fn new(num_shards: u32) -> Self {
        assert!(num_shards > 0, "a router needs at least one shard");
        ShardRouter { num_shards }
    }

    pub fn shard_of(&self, id: &Tuid) -> ShardId {
        let leading_byte = id.uuid()[0] as u32;
        // 256 possible byte values mapped onto num_shards buckets.
        (leading_byte * self.num_shards) / 256
    }

    pub fn num_shards(&self) -> u32 {
        self.num_shards
    }
}

/// Static config: which node addresses (their *client* ports, per
/// `raft_net::propose_remote`) make up each shard's Raft group.
#[derive(Debug, Clone, Default)]
pub struct ShardMap {
    shards: HashMap<ShardId, Vec<(NodeId, SocketAddr)>>,
}

impl ShardMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_member(&mut self, shard: ShardId, node: NodeId, client_addr: SocketAddr) {
        self.shards
            .entry(shard)
            .or_default()
            .push((node, client_addr));
    }

    pub fn members(&self, shard: ShardId) -> &[(NodeId, SocketAddr)] {
        self.shards.get(&shard).map(Vec::as_slice).unwrap_or(&[])
    }
}

/// Routes a write to the right shard's Raft group and finds its current
/// leader by trying each member's client port in turn — `propose_remote`
/// fails fast against a non-leader (see its doc), so this is a simple
/// linear retry, not a smarter "ask who the leader is" protocol. Good
/// enough for small Raft groups; a real deployment would want to remember
/// the last-known leader per shard rather than re-discovering it on every
/// call.
pub struct ShardedClient {
    router: ShardRouter,
    map: ShardMap,
    timeout: Duration,
}

impl ShardedClient {
    pub fn new(router: ShardRouter, map: ShardMap, timeout: Duration) -> Self {
        ShardedClient {
            router,
            map,
            timeout,
        }
    }

    pub fn shard_of(&self, id: &Tuid) -> ShardId {
        self.router.shard_of(id)
    }

    /// Propose `command` (whose entity id determines the shard) against
    /// whichever member of that shard's Raft group turns out to be
    /// leader. Errors only if every member was tried and none accepted
    /// it — most commonly because the shard is mid-election, which a
    /// caller can simply retry.
    pub fn propose(&self, entity_id: &Tuid, command: LogRecord) -> Result<u64> {
        let shard = self.router.shard_of(entity_id);
        let members = self.map.members(shard);
        if members.is_empty() {
            return Err(Error::InvalidValue {
                detail: "no configured members for this entity's shard",
            });
        }
        for &(_, addr) in members {
            if let Ok(index) = propose_remote(addr, &command, self.timeout) {
                return Ok(index);
            }
        }
        Err(Error::InvalidValue {
            detail: "no member of this entity's shard accepted the proposal \
                     (likely mid-election or the shard is unreachable)",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn router_distributes_across_shards_roughly_evenly() {
        let router = ShardRouter::new(4);
        let mut counts = [0u32; 4];
        for byte in 0u32..256 {
            let mut uuid = [0u8; 16];
            uuid[0] = byte as u8;
            let id = Tuid::new(uuid, 0);
            counts[router.shard_of(&id) as usize] += 1;
        }
        // 256 / 4 = exactly 64 each, since 256 divides evenly.
        assert_eq!(counts, [64, 64, 64, 64]);
    }

    #[test]
    fn router_is_deterministic_for_the_same_id() {
        let router = ShardRouter::new(3);
        let id = Tuid::new([42u8; 16], 7);
        assert_eq!(router.shard_of(&id), router.shard_of(&id));
    }

    #[test]
    fn shard_map_tracks_members_per_shard() {
        let mut map = ShardMap::new();
        let addr: SocketAddr = "127.0.0.1:9000".parse().unwrap();
        map.add_member(0, 1, addr);
        assert_eq!(map.members(0), &[(1, addr)]);
        assert!(map.members(1).is_empty());
    }

    #[test]
    fn sharded_client_errors_cleanly_with_no_configured_members() {
        let client = ShardedClient::new(
            ShardRouter::new(2),
            ShardMap::new(),
            Duration::from_millis(100),
        );
        let id = Tuid::new([1u8; 16], 1);
        let result = client.propose(&id, LogRecord::Delete { id });
        assert!(result.is_err());
    }

    #[test]
    fn sharded_client_errors_when_no_member_is_reachable() {
        let mut map = ShardMap::new();
        // Nothing is listening on this port.
        map.add_member(0, 1, "127.0.0.1:1".parse().unwrap());
        let client = ShardedClient::new(ShardRouter::new(1), map, Duration::from_millis(200));
        let id = Tuid::new([1u8; 16], 1);
        let result = client.propose(&id, LogRecord::Delete { id });
        assert!(result.is_err());
    }
}
