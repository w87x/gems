//! `SwimCore`: cluster membership and failure detection, per
//! ARCHITECTURE.md §6 — "gossip (SWIM-style) for cluster membership,
//! failure detection, and disseminating the shard map." Same design
//! pattern as `raft.rs` and for the same reason: a pure, I/O-free state
//! machine (`tick()`/`receive()`) driven by explicit inputs, so the
//! failure-detection timing and gossip-propagation logic are testable
//! deterministically instead of through flaky real-clock/real-socket
//! tests. A real deployment wraps this in a thin shell — UDP or TCP
//! sockets, a real timer, randomized peer selection — the same kind of
//! follow-on work `raft.rs`'s module doc describes for its own core.
//!
//! Algorithm, from Das, Gupta & Motivala's SWIM paper: each protocol
//! period, a node pings one peer directly. No ack within the timeout marks
//! that peer `Suspect` (not immediately `Dead` — a suspicion window gives
//! it a chance to be seen alive by someone else, or to refute the
//! suspicion itself). Membership updates piggyback on ping/ack messages
//! (no separate gossip round) and are merged by an incarnation-number
//! rule: higher incarnation always wins; at equal incarnation, `Dead` >
//! `Suspect` > `Alive` (severity wins ties, so a suspicion isn't
//! accidentally overwritten by a stale "alive" report). A node that
//! learns *it itself* has been marked `Suspect` refutes by incrementing
//! its own incarnation and gossiping `Alive` at the new one — which is
//! the only thing that can clear a suspicion; nothing else can, an
//! important safety property covered in this module's tests.
//!
//! **Deliberate v1 scope cut: no indirect probing (SWIM's "ping-req").**
//! Real SWIM asks a handful of other members to probe a suspected node
//! before giving up on it, specifically to avoid false positives from an
//! asymmetric network problem between the pinger and that one target
//! (they're unreachable from *you* but fine from everyone else). Skipping
//! that means every failure detection here is a direct, unilateral
//! judgment call by whichever node happened to be probing — real
//! failures are still caught (that's what the tests below verify), but at
//! a higher false-positive rate than full SWIM would have. Worth adding
//! once there's a deployment actually seeing that tradeoff bite.
//!
//! Also a deliberate simplification: ping targets are chosen by
//! round-robin over the known peer list rather than SWIM's per-round
//! reshuffle. This still guarantees every peer gets probed within one
//! full cycle (round-robin's coverage is a superset of what randomization
//! guarantees), just without randomization's protection against an
//! adversary predicting probe order — an acceptable tradeoff in the
//! trusted-cluster context this crate targets, and it keeps the core free
//! of the `RaftCore`-style "no internal randomness" exception this module
//! doesn't otherwise need to make.

use std::collections::{HashMap, VecDeque};

pub type NodeId = u32;
pub type Incarnation = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Status {
    Alive,
    Suspect,
    Dead,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Member {
    pub status: Status,
    pub incarnation: Incarnation,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GossipItem {
    pub member: NodeId,
    pub status: Status,
    pub incarnation: Incarnation,
}

/// How many more outgoing messages a queued gossip item should be
/// attached to before it's dropped — bounds gossip traffic instead of
/// retransmitting every historical change forever. A real deployment
/// would scale this with cluster size (SWIM's paper suggests ~log(N));
/// fixed here since this module doesn't have a cluster-size concept of
/// its own to scale against.
const GOSSIP_RETRANSMIT_COUNT: u32 = 4;
const MAX_GOSSIP_PER_MESSAGE: usize = 8;

#[derive(Debug, Clone, PartialEq)]
pub enum SwimMessage {
    Ping {
        seq: u32,
        sender_incarnation: Incarnation,
        gossip: Vec<GossipItem>,
    },
    Ack {
        seq: u32,
        sender_incarnation: Incarnation,
        gossip: Vec<GossipItem>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Envelope {
    pub from: NodeId,
    pub to: NodeId,
    pub message: SwimMessage,
}

struct AwaitingAck {
    target: NodeId,
    seq: u32,
    ticks_waited: u32,
}

struct GossipQueueItem {
    item: GossipItem,
    remaining_sends: u32,
}

pub struct SwimCore {
    id: NodeId,
    incarnation: Incarnation,
    members: HashMap<NodeId, Member>,
    ping_order: Vec<NodeId>,
    next_ping_idx: usize,
    next_seq: u32,

    protocol_period: u32,
    period_elapsed: u32,
    ping_timeout: u32,
    suspicion_timeout: u32,

    awaiting_ack: Option<AwaitingAck>,
    suspect_elapsed: HashMap<NodeId, u32>,
    gossip_queue: VecDeque<GossipQueueItem>,
}

impl SwimCore {
    pub fn new(
        id: NodeId,
        peers: Vec<NodeId>,
        protocol_period: u32,
        ping_timeout: u32,
        suspicion_timeout: u32,
    ) -> Self {
        let mut members = HashMap::new();
        members.insert(
            id,
            Member {
                status: Status::Alive,
                incarnation: 0,
            },
        );
        for &p in &peers {
            members.insert(
                p,
                Member {
                    status: Status::Alive,
                    incarnation: 0,
                },
            );
        }
        SwimCore {
            id,
            incarnation: 0,
            members,
            ping_order: peers,
            next_ping_idx: 0,
            next_seq: 0,
            protocol_period,
            period_elapsed: 0,
            ping_timeout,
            suspicion_timeout,
            awaiting_ack: None,
            suspect_elapsed: HashMap::new(),
            gossip_queue: VecDeque::new(),
        }
    }

    pub fn id(&self) -> NodeId {
        self.id
    }

    pub fn status_of(&self, id: NodeId) -> Option<Status> {
        self.members.get(&id).map(|m| m.status)
    }

    pub fn incarnation(&self) -> Incarnation {
        self.incarnation
    }

    fn envelope_to(&self, to: NodeId, message: SwimMessage) -> Envelope {
        Envelope {
            from: self.id,
            to,
            message,
        }
    }

    fn queue_gossip(&mut self, item: GossipItem) {
        // Replace any queued update about the same member — only the
        // freshest matters, and it keeps the queue from growing with
        // superseded entries for a flapping member.
        self.gossip_queue.retain(|g| g.item.member != item.member);
        self.gossip_queue.push_back(GossipQueueItem {
            item,
            remaining_sends: GOSSIP_RETRANSMIT_COUNT,
        });
    }

    fn take_gossip(&mut self) -> Vec<GossipItem> {
        let mut out = Vec::new();
        let mut still_pending = VecDeque::new();
        while let Some(mut entry) = self.gossip_queue.pop_front() {
            if out.len() < MAX_GOSSIP_PER_MESSAGE {
                out.push(entry.item);
                entry.remaining_sends -= 1;
                if entry.remaining_sends > 0 {
                    still_pending.push_back(entry);
                }
            } else {
                still_pending.push_back(entry);
            }
        }
        self.gossip_queue = still_pending;
        out
    }

    /// The core merge rule: apply an incoming `(member, status,
    /// incarnation)` fact only if it's more authoritative than what's
    /// currently recorded. Returns `true` if it changed anything (and so
    /// is worth re-gossiping and, if it's about us, worth checking for
    /// self-refutation).
    fn merge(&mut self, incoming: GossipItem) -> bool {
        if incoming.member == self.id {
            return self.maybe_refute_self(incoming);
        }

        let current = self.members.get(&incoming.member).copied();
        let should_apply = match current {
            None => true,
            Some(c) if c.status == Status::Dead => false, // terminal
            Some(c) => {
                incoming.incarnation > c.incarnation
                    || (incoming.incarnation == c.incarnation && incoming.status > c.status)
            }
        };
        if !should_apply {
            return false;
        }

        self.members.insert(
            incoming.member,
            Member {
                status: incoming.status,
                incarnation: incoming.incarnation,
            },
        );
        if incoming.status == Status::Suspect {
            self.suspect_elapsed.insert(incoming.member, 0);
        } else {
            self.suspect_elapsed.remove(&incoming.member);
        }
        self.queue_gossip(incoming);
        true
    }

    /// If gossip claims *we* are `Suspect` (or worse) at an incarnation
    /// we haven't already superseded, the only correct response is to
    /// bump our own incarnation and broadcast `Alive` at the new one —
    /// nothing else can clear a suspicion about us, by design (see the
    /// module doc).
    fn maybe_refute_self(&mut self, incoming: GossipItem) -> bool {
        if incoming.status == Status::Alive || incoming.incarnation < self.incarnation {
            return false;
        }
        self.incarnation = incoming.incarnation + 1;
        self.members.insert(
            self.id,
            Member {
                status: Status::Alive,
                incarnation: self.incarnation,
            },
        );
        self.queue_gossip(GossipItem {
            member: self.id,
            status: Status::Alive,
            incarnation: self.incarnation,
        });
        true
    }

    fn next_ping_target(&mut self) -> Option<NodeId> {
        if self.ping_order.is_empty() {
            return None;
        }
        // Skip members already known Dead — no point probing them.
        for _ in 0..self.ping_order.len() {
            let candidate = self.ping_order[self.next_ping_idx];
            self.next_ping_idx = (self.next_ping_idx + 1) % self.ping_order.len();
            if self.status_of(candidate) != Some(Status::Dead) {
                return Some(candidate);
            }
        }
        None
    }

    /// Advance time by one tick: ages any in-flight ping (declaring
    /// `Suspect` on timeout) and any active suspicions (declaring `Dead`
    /// once the suspicion window elapses), and starts a new ping if the
    /// protocol period has elapsed and no ping is outstanding.
    pub fn tick(&mut self) -> Vec<Envelope> {
        let mut out = Vec::new();

        let timed_out: Vec<NodeId> = self
            .suspect_elapsed
            .iter_mut()
            .filter_map(|(&id, elapsed)| {
                *elapsed += 1;
                (*elapsed >= self.suspicion_timeout).then_some(id)
            })
            .collect();
        for id in timed_out {
            self.suspect_elapsed.remove(&id);
            let incarnation = self.members.get(&id).map(|m| m.incarnation);
            if let (Some(m), Some(incarnation)) = (self.members.get_mut(&id), incarnation) {
                if m.status == Status::Suspect {
                    m.status = Status::Dead;
                    self.queue_gossip(GossipItem {
                        member: id,
                        status: Status::Dead,
                        incarnation,
                    });
                }
            }
        }

        if let Some(awaiting) = &mut self.awaiting_ack {
            awaiting.ticks_waited += 1;
            if awaiting.ticks_waited >= self.ping_timeout {
                let target = awaiting.target;
                self.awaiting_ack = None;
                if let Some(m) = self.members.get(&target).copied() {
                    if m.status == Status::Alive {
                        self.merge(GossipItem {
                            member: target,
                            status: Status::Suspect,
                            incarnation: m.incarnation,
                        });
                    }
                }
            }
        }

        self.period_elapsed += 1;
        if self.period_elapsed >= self.protocol_period && self.awaiting_ack.is_none() {
            self.period_elapsed = 0;
            if let Some(target) = self.next_ping_target() {
                self.next_seq += 1;
                let seq = self.next_seq;
                self.awaiting_ack = Some(AwaitingAck {
                    target,
                    seq,
                    ticks_waited: 0,
                });
                let gossip = self.take_gossip();
                out.push(self.envelope_to(
                    target,
                    SwimMessage::Ping {
                        seq,
                        sender_incarnation: self.incarnation,
                        gossip,
                    },
                ));
            }
        }
        out
    }

    pub fn receive(&mut self, envelope: Envelope) -> Vec<Envelope> {
        let from = envelope.from;
        match envelope.message {
            SwimMessage::Ping {
                seq,
                sender_incarnation,
                gossip,
            } => {
                self.merge(GossipItem {
                    member: from,
                    status: Status::Alive,
                    incarnation: sender_incarnation,
                });
                for item in gossip {
                    self.merge(item);
                }
                let response_gossip = self.take_gossip();
                vec![self.envelope_to(
                    from,
                    SwimMessage::Ack {
                        seq,
                        sender_incarnation: self.incarnation,
                        gossip: response_gossip,
                    },
                )]
            }
            SwimMessage::Ack {
                seq,
                sender_incarnation,
                gossip,
            } => {
                self.merge(GossipItem {
                    member: from,
                    status: Status::Alive,
                    incarnation: sender_incarnation,
                });
                for item in gossip {
                    self.merge(item);
                }
                if let Some(awaiting) = &self.awaiting_ack {
                    if awaiting.target == from && awaiting.seq == seq {
                        self.awaiting_ack = None;
                    }
                }
                Vec::new()
            }
        }
    }
}

/// Wire encoding for `Envelope`/`SwimMessage`, used by `swim_net`'s real
/// TCP shell. Same `u32`-length-prefixed framing as `raft::wire` and
/// `record::LogRecord`, for the same reason: partial-buffer reads off a
/// real socket compose the same way everywhere in this workspace.
pub mod wire {
    use super::{Envelope, GossipItem, Incarnation, NodeId, Status, SwimMessage};
    use crate::frame::check_frame_len;
    use gems_common::{Error, Result};

    const PING: u8 = 1;
    const ACK: u8 = 2;

    pub fn encode(envelope: &Envelope) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&envelope.from.to_le_bytes());
        payload.extend_from_slice(&envelope.to.to_le_bytes());
        match &envelope.message {
            SwimMessage::Ping {
                seq,
                sender_incarnation,
                gossip,
            } => {
                payload.push(PING);
                encode_ping_or_ack_body(&mut payload, *seq, *sender_incarnation, gossip);
            }
            SwimMessage::Ack {
                seq,
                sender_incarnation,
                gossip,
            } => {
                payload.push(ACK);
                encode_ping_or_ack_body(&mut payload, *seq, *sender_incarnation, gossip);
            }
        }
        let mut framed = Vec::with_capacity(4 + payload.len());
        framed.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        framed.extend_from_slice(&payload);
        framed
    }

    fn encode_ping_or_ack_body(
        out: &mut Vec<u8>,
        seq: u32,
        sender_incarnation: Incarnation,
        gossip: &[GossipItem],
    ) {
        out.extend_from_slice(&seq.to_le_bytes());
        out.extend_from_slice(&sender_incarnation.to_le_bytes());
        out.extend_from_slice(&(gossip.len() as u32).to_le_bytes());
        for item in gossip {
            out.extend_from_slice(&item.member.to_le_bytes());
            out.push(encode_status(item.status));
            out.extend_from_slice(&item.incarnation.to_le_bytes());
        }
    }

    fn encode_status(status: Status) -> u8 {
        match status {
            Status::Alive => 0,
            Status::Suspect => 1,
            Status::Dead => 2,
        }
    }

    fn decode_status(b: u8) -> Result<Status> {
        Ok(match b {
            0 => Status::Alive,
            1 => Status::Suspect,
            2 => Status::Dead,
            _ => {
                return Err(Error::InvalidValue {
                    detail: "unknown SWIM status byte",
                })
            }
        })
    }

    /// Decode one envelope from the start of `buf`. `None` (not an error)
    /// if `buf` doesn't yet hold a complete envelope.
    pub fn decode(buf: &[u8]) -> Result<Option<(Envelope, usize)>> {
        if buf.len() < 4 {
            return Ok(None);
        }
        let payload_len = u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize;
        check_frame_len(payload_len, "SWIM envelope exceeds the maximum frame size")?;
        let total_len = 4 + payload_len;
        if buf.len() < total_len {
            return Ok(None);
        }
        let payload = &buf[4..total_len];
        let mut pos = 0;
        let from = read_node_id(payload, &mut pos)?;
        let to = read_node_id(payload, &mut pos)?;
        let tag = read_u8(payload, &mut pos)?;

        let (seq, sender_incarnation, gossip) = decode_ping_or_ack_body(payload, &mut pos)?;
        let message = match tag {
            PING => SwimMessage::Ping {
                seq,
                sender_incarnation,
                gossip,
            },
            ACK => SwimMessage::Ack {
                seq,
                sender_incarnation,
                gossip,
            },
            _ => {
                return Err(Error::InvalidValue {
                    detail: "unknown SWIM message tag",
                })
            }
        };

        Ok(Some((Envelope { from, to, message }, total_len)))
    }

    fn decode_ping_or_ack_body(
        buf: &[u8],
        pos: &mut usize,
    ) -> Result<(u32, Incarnation, Vec<GossipItem>)> {
        let seq = read_u32(buf, pos)?;
        let sender_incarnation = read_u64(buf, pos)?;
        let count = read_u32(buf, pos)? as usize;
        // Each gossip item is at least 13 bytes (4-byte node id + 1-byte
        // status + 8-byte incarnation); reject a claimed count bigger than
        // what could fit in the remainder of `buf` before trusting it to
        // size a `Vec::with_capacity` allocation.
        if count > buf.len().saturating_sub(*pos) / 13 {
            return Err(Error::InvalidValue {
                detail: "SWIM gossip item count exceeds what the payload could hold",
            });
        }
        let mut gossip = Vec::with_capacity(count);
        for _ in 0..count {
            let member = read_node_id(buf, pos)?;
            let status = decode_status(read_u8(buf, pos)?)?;
            let incarnation = read_u64(buf, pos)?;
            gossip.push(GossipItem {
                member,
                status,
                incarnation,
            });
        }
        Ok((seq, sender_incarnation, gossip))
    }

    fn read_u8(buf: &[u8], pos: &mut usize) -> Result<u8> {
        let b = *buf.get(*pos).ok_or(Error::InvalidValue {
            detail: "SWIM envelope truncated",
        })?;
        *pos += 1;
        Ok(b)
    }

    fn read_node_id(buf: &[u8], pos: &mut usize) -> Result<NodeId> {
        read_u32(buf, pos)
    }

    fn read_u32(buf: &[u8], pos: &mut usize) -> Result<u32> {
        if buf.len() < *pos + 4 {
            return Err(Error::InvalidValue {
                detail: "SWIM envelope truncated",
            });
        }
        let v = u32::from_le_bytes(buf[*pos..*pos + 4].try_into().unwrap());
        *pos += 4;
        Ok(v)
    }

    fn read_u64(buf: &[u8], pos: &mut usize) -> Result<u64> {
        if buf.len() < *pos + 8 {
            return Err(Error::InvalidValue {
                detail: "SWIM envelope truncated",
            });
        }
        let v = u64::from_le_bytes(buf[*pos..*pos + 8].try_into().unwrap());
        *pos += 8;
        Ok(v)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn decode_rejects_a_claimed_length_over_the_frame_cap() {
            let mut buf = ((crate::frame::MAX_FRAME_LEN as u32) + 1)
                .to_le_bytes()
                .to_vec();
            buf.push(1);
            assert!(decode(&buf).is_err());
        }

        #[test]
        fn decode_rejects_a_gossip_count_bigger_than_the_payload_could_hold() {
            let mut payload = Vec::new();
            payload.extend_from_slice(&1u32.to_le_bytes()); // from
            payload.extend_from_slice(&2u32.to_le_bytes()); // to
            payload.push(PING);
            payload.extend_from_slice(&1u32.to_le_bytes()); // seq
            payload.extend_from_slice(&1u64.to_le_bytes()); // sender_incarnation
            payload.extend_from_slice(&1_000_000_000u32.to_le_bytes()); // gossip count
            let mut framed = (payload.len() as u32).to_le_bytes().to_vec();
            framed.extend_from_slice(&payload);
            assert!(decode(&framed).is_err());
        }

        #[test]
        fn ping_roundtrip_with_gossip_items() {
            let env = Envelope {
                from: 1,
                to: 2,
                message: SwimMessage::Ping {
                    seq: 5,
                    sender_incarnation: 3,
                    gossip: vec![
                        GossipItem {
                            member: 4,
                            status: Status::Suspect,
                            incarnation: 2,
                        },
                        GossipItem {
                            member: 5,
                            status: Status::Dead,
                            incarnation: 1,
                        },
                    ],
                },
            };
            let encoded = encode(&env);
            let (decoded, consumed) = decode(&encoded).unwrap().unwrap();
            assert_eq!(consumed, encoded.len());
            assert_eq!(decoded, env);
        }

        #[test]
        fn ack_roundtrip_with_no_gossip() {
            let env = Envelope {
                from: 2,
                to: 1,
                message: SwimMessage::Ack {
                    seq: 5,
                    sender_incarnation: 0,
                    gossip: vec![],
                },
            };
            let encoded = encode(&env);
            let (decoded, _) = decode(&encoded).unwrap().unwrap();
            assert_eq!(decoded, env);
        }

        #[test]
        fn decode_returns_none_on_partial_buffer() {
            let env = Envelope {
                from: 1,
                to: 2,
                message: SwimMessage::Ack {
                    seq: 1,
                    sender_incarnation: 0,
                    gossip: vec![],
                },
            };
            let encoded = encode(&env);
            assert!(decode(&encoded[..encoded.len() - 1]).unwrap().is_none());
        }

        #[test]
        fn decodes_two_back_to_back_envelopes() {
            let a = Envelope {
                from: 1,
                to: 2,
                message: SwimMessage::Ack {
                    seq: 1,
                    sender_incarnation: 0,
                    gossip: vec![],
                },
            };
            let b = Envelope {
                from: 2,
                to: 1,
                message: SwimMessage::Ack {
                    seq: 2,
                    sender_incarnation: 0,
                    gossip: vec![],
                },
            };
            let mut buf = encode(&a);
            buf.extend_from_slice(&encode(&b));
            let (decoded_a, consumed_a) = decode(&buf).unwrap().unwrap();
            let (decoded_b, consumed_b) = decode(&buf[consumed_a..]).unwrap().unwrap();
            assert_eq!(decoded_a, a);
            assert_eq!(decoded_b, b);
            assert_eq!(consumed_a + consumed_b, buf.len());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap as StdHashMap, HashSet, VecDeque as StdVecDeque};

    struct Cluster {
        nodes: StdHashMap<NodeId, SwimCore>,
        blocked: HashSet<(NodeId, NodeId)>,
    }

    impl Cluster {
        fn new(
            ids: &[NodeId],
            protocol_period: u32,
            ping_timeout: u32,
            suspicion_timeout: u32,
        ) -> Self {
            let nodes = ids
                .iter()
                .map(|&id| {
                    let peers = ids.iter().copied().filter(|&x| x != id).collect();
                    (
                        id,
                        SwimCore::new(id, peers, protocol_period, ping_timeout, suspicion_timeout),
                    )
                })
                .collect();
            Cluster {
                nodes,
                blocked: HashSet::new(),
            }
        }

        fn block(&mut self, a: NodeId, b: NodeId) {
            self.blocked.insert((a, b));
            self.blocked.insert((b, a));
        }

        fn drain(&mut self, mut queue: StdVecDeque<Envelope>) {
            while let Some(env) = queue.pop_front() {
                if self.blocked.contains(&(env.from, env.to)) {
                    continue;
                }
                if let Some(node) = self.nodes.get_mut(&env.to) {
                    queue.extend(node.receive(env));
                }
            }
        }

        fn tick(&mut self) {
            let mut queue = StdVecDeque::new();
            for node in self.nodes.values_mut() {
                queue.extend(node.tick());
            }
            self.drain(queue);
        }

        fn run_ticks(&mut self, n: u32) {
            for _ in 0..n {
                self.tick();
            }
        }

        fn status_everywhere(&self, target: NodeId) -> Vec<(NodeId, Option<Status>)> {
            let mut v: Vec<_> = self
                .nodes
                .keys()
                .map(|&id| (id, self.nodes[&id].status_of(target)))
                .collect();
            v.sort_by_key(|(id, _)| *id);
            v
        }
    }

    #[test]
    fn stable_cluster_stays_all_alive() {
        let mut cluster = Cluster::new(&[1, 2, 3, 4], 3, 2, 5);
        cluster.run_ticks(50);
        for observer in [1, 2, 3, 4] {
            for target in [1, 2, 3, 4] {
                assert_eq!(
                    cluster.nodes[&observer].status_of(target),
                    Some(Status::Alive),
                    "observer {observer} thinks {target} is not alive"
                );
            }
        }
    }

    #[test]
    fn an_unreachable_node_is_eventually_marked_dead_everywhere() {
        let mut cluster = Cluster::new(&[1, 2, 3, 4], 3, 2, 5);
        cluster.run_ticks(10); // let things settle first

        // Node 4 goes dark: block it from everyone.
        for other in [1, 2, 3] {
            cluster.block(4, other);
        }
        cluster.run_ticks(60); // several full ping cycles plus suspicion window

        for observer in [1, 2, 3] {
            assert_eq!(
                cluster.nodes[&observer].status_of(4),
                Some(Status::Dead),
                "observer {observer} should have marked node 4 dead"
            );
        }
    }

    #[test]
    fn suspicion_precedes_death_rather_than_jumping_straight_there() {
        let mut cluster = Cluster::new(&[1, 2, 3], 3, 2, 20);
        cluster.run_ticks(10);
        for other in [2, 3] {
            cluster.block(1, other);
        }
        // Enough ticks for a ping timeout to fire (protocol_period +
        // ping_timeout) but nowhere near the long suspicion_timeout.
        cluster.run_ticks(8);

        let statuses = cluster.status_everywhere(1);
        assert!(
            statuses.iter().any(|(_, s)| *s == Some(Status::Suspect)),
            "node 1 should be Suspect somewhere by now, not yet Dead: {statuses:?}"
        );
        assert!(
            statuses.iter().all(|(_, s)| *s != Some(Status::Dead)),
            "node 1 must not be Dead yet: {statuses:?}"
        );
    }

    #[test]
    fn self_refutation_clears_a_suspicion_and_nothing_else_can() {
        let mut cluster = Cluster::new(&[1, 2, 3], 3, 2, 30);
        cluster.run_ticks(10);

        // Manually inject a Suspect claim about node 1 into node 2's
        // view (simulating node 2 having detected a timeout), without
        // actually blocking node 1's links — this isolates the test to
        // "does gossiping a Suspect about a live node get corrected by
        // that node's own refutation" rather than depending on timing.
        let incarnation = cluster.nodes[&1].incarnation();
        cluster.nodes.get_mut(&2).unwrap().merge(GossipItem {
            member: 1,
            status: Status::Suspect,
            incarnation,
        });
        assert_eq!(cluster.nodes[&2].status_of(1), Some(Status::Suspect));

        // The next ping/ack cycle must carry that gossip to node 1, which
        // must refute it (bump incarnation, broadcast Alive), which must
        // then propagate back out to everyone.
        cluster.run_ticks(15);

        for observer in [1, 2, 3] {
            assert_eq!(
                cluster.nodes[&observer].status_of(1),
                Some(Status::Alive),
                "observer {observer} should see node 1 as Alive again after self-refutation"
            );
        }
        assert!(
            cluster.nodes[&1].incarnation() > incarnation,
            "node 1 must have bumped its own incarnation to refute"
        );
    }

    #[test]
    fn dead_is_terminal_and_not_reverted_by_a_stale_alive_report() {
        let mut cluster = Cluster::new(&[1, 2, 3], 3, 2, 5);
        cluster.run_ticks(10);
        for other in [2, 3] {
            cluster.block(1, other);
        }
        cluster.run_ticks(60);
        assert_eq!(cluster.nodes[&2].status_of(1), Some(Status::Dead));

        // A stale "Alive" report at a lower-or-equal incarnation must not
        // resurrect a node already marked Dead.
        let changed = cluster.nodes.get_mut(&2).unwrap().merge(GossipItem {
            member: 1,
            status: Status::Alive,
            incarnation: 0,
        });
        assert!(!changed);
        assert_eq!(cluster.nodes[&2].status_of(1), Some(Status::Dead));
    }
}
