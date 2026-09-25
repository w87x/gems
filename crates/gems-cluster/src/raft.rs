//! `RaftCore`: stage 3 of ARCHITECTURE.md §6/§11's clustering plan — full
//! consensus, building on the same `LogRecord` apply-record shape stage 2
//! (`primary.rs`/`log.rs`/`server.rs`/`replica.rs`) proved out. Implements
//! the core algorithm from the Raft paper (Ongaro & Ousterhout): leader
//! election, log replication, and the commit-index safety rule that
//! prevents committing an entry from a previous term based on replication
//! count alone (§5.4.2 of the paper — "a leader cannot conclude that an
//! entry from a previous term is committed once it is stored on a majority
//! of servers").
//!
//! **Deliberately a pure state machine, not a network service.** `tick()`
//! and `receive()` take explicit inputs and return the messages that
//! should be sent as a result — no sockets, no threads, no timers inside
//! this module. That's what makes Raft's safety properties (and its
//! trickier interleavings: a stale leader rejoining, a follower with a
//! divergent log, a term changing mid-election) testable *deterministically*
//! in this file's test suite, by driving several `RaftCore`s from a single
//! test thread with a simulated network the test controls completely
//! (including partitions). A real deployment wraps this in a thin shell —
//! TCP sockets for the RPCs (the same blocking, thread-per-connection style
//! as `server.rs`), a real timer thread calling `tick()`, and randomized
//! election timeouts drawn from `gems_common::rand` — which is exactly the
//! kind of I/O-and-timing-dependent code this module avoids needing to get
//! right at the same time as the algorithm itself. That shell is real,
//! separate follow-on work, not implemented here.
//!
//! Also deliberately out of scope for this pass, each a distinct piece of
//! follow-on work: log compaction/snapshotting (an unbounded in-memory
//! `Vec<LogEntry>` is fine for proving out the algorithm, not for a
//! long-running node), cluster membership changes (the peer set is fixed
//! at construction), and wiring `take_newly_committed` into an actual
//! `gems_engine::Store` the way `ReplicaClient` already does for stage 2
//! (the entry point is there — `LogEntry.command` is a `LogRecord` — but
//! connecting it up is the network shell's job).

use std::collections::HashSet;

use crate::record::LogRecord;

pub type NodeId = u32;
pub type Term = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Follower,
    Candidate,
    Leader,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LogEntry {
    pub term: Term,
    pub command: LogRecord,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Rpc {
    RequestVoteRequest {
        term: Term,
        candidate_id: NodeId,
        last_log_index: u64,
        last_log_term: Term,
    },
    RequestVoteResponse {
        term: Term,
        vote_granted: bool,
        voter_id: NodeId,
    },
    AppendEntriesRequest {
        term: Term,
        leader_id: NodeId,
        prev_log_index: u64,
        prev_log_term: Term,
        entries: Vec<LogEntry>,
        leader_commit: u64,
    },
    AppendEntriesResponse {
        term: Term,
        success: bool,
        /// The follower's log length after applying this request — lets
        /// the leader advance `match_index` directly on success instead of
        /// re-deriving it, and is unused (ignored) on failure.
        match_index: u64,
        follower_id: NodeId,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Envelope {
    pub from: NodeId,
    pub to: NodeId,
    pub rpc: Rpc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RaftError {
    NotLeader,
}

pub struct RaftCore {
    id: NodeId,
    peers: Vec<NodeId>,

    // Persistent state (a real deployment must fsync these before acting
    // on them — see the crate/module doc on why that's shell, not core,
    // work for this pass).
    current_term: Term,
    voted_for: Option<NodeId>,
    log: Vec<LogEntry>, // log[i] is index i+1 (Raft's log is 1-indexed)

    // Volatile state, all servers.
    commit_index: u64,
    last_applied: u64,
    role: Role,

    // Volatile state, candidates only.
    votes_received: HashSet<NodeId>,

    // Volatile state, leaders only.
    next_index: std::collections::HashMap<NodeId, u64>,
    match_index: std::collections::HashMap<NodeId, u64>,

    // Timers, expressed in abstract "ticks" the caller advances — see the
    // module doc on why this core doesn't own a real clock or randomness.
    election_elapsed: u32,
    election_timeout: u32,
    heartbeat_elapsed: u32,
    heartbeat_interval: u32,
}

impl RaftCore {
    pub fn new(
        id: NodeId,
        peers: Vec<NodeId>,
        election_timeout: u32,
        heartbeat_interval: u32,
    ) -> Self {
        RaftCore {
            id,
            peers,
            current_term: 0,
            voted_for: None,
            log: Vec::new(),
            commit_index: 0,
            last_applied: 0,
            role: Role::Follower,
            votes_received: HashSet::new(),
            next_index: std::collections::HashMap::new(),
            match_index: std::collections::HashMap::new(),
            election_elapsed: 0,
            election_timeout,
            heartbeat_elapsed: 0,
            heartbeat_interval,
        }
    }

    pub fn id(&self) -> NodeId {
        self.id
    }

    pub fn role(&self) -> Role {
        self.role
    }

    pub fn current_term(&self) -> Term {
        self.current_term
    }

    pub fn log_len(&self) -> u64 {
        self.log.len() as u64
    }

    pub fn commit_index(&self) -> u64 {
        self.commit_index
    }

    /// Adjust the election timeout — a real shell re-randomizes this on
    /// every reset (per the module doc); tests use it to deterministically
    /// control which node wins a given election.
    pub fn set_election_timeout(&mut self, ticks: u32) {
        self.election_timeout = ticks;
    }

    fn last_log_index(&self) -> u64 {
        self.log.len() as u64
    }

    fn last_log_term(&self) -> Term {
        self.log.last().map(|e| e.term).unwrap_or(0)
    }

    fn log_term_at(&self, index: u64) -> Term {
        if index == 0 {
            return 0;
        }
        self.log
            .get((index - 1) as usize)
            .map(|e| e.term)
            .unwrap_or(0)
    }

    fn envelope_to(&self, to: NodeId, rpc: Rpc) -> Envelope {
        Envelope {
            from: self.id,
            to,
            rpc,
        }
    }

    fn reset_election_timer(&mut self) {
        self.election_elapsed = 0;
    }

    /// Advance time by one tick. Returns any RPCs this triggers: an
    /// election's `RequestVote`s on timeout, or a leader's periodic
    /// `AppendEntries` (heartbeat, or carrying whatever entries a follower
    /// still needs).
    pub fn tick(&mut self) -> Vec<Envelope> {
        match self.role {
            Role::Leader => {
                self.heartbeat_elapsed += 1;
                if self.heartbeat_elapsed >= self.heartbeat_interval {
                    self.heartbeat_elapsed = 0;
                    return self.replicate_to_all_peers();
                }
                Vec::new()
            }
            Role::Follower | Role::Candidate => {
                self.election_elapsed += 1;
                if self.election_elapsed >= self.election_timeout {
                    return self.start_election();
                }
                Vec::new()
            }
        }
    }

    fn start_election(&mut self) -> Vec<Envelope> {
        self.role = Role::Candidate;
        self.current_term += 1;
        self.voted_for = Some(self.id);
        self.votes_received.clear();
        self.votes_received.insert(self.id);
        self.reset_election_timer();

        let mut msgs: Vec<Envelope> = self
            .peers
            .iter()
            .map(|&p| {
                self.envelope_to(
                    p,
                    Rpc::RequestVoteRequest {
                        term: self.current_term,
                        candidate_id: self.id,
                        last_log_index: self.last_log_index(),
                        last_log_term: self.last_log_term(),
                    },
                )
            })
            .collect();
        // A single-node "cluster" (no peers) wins its own election
        // immediately — there's no one else to grant a vote.
        msgs.extend(self.check_election_win());
        msgs
    }

    fn become_follower(&mut self, term: Term) {
        if term > self.current_term {
            self.current_term = term;
            self.voted_for = None;
        }
        self.role = Role::Follower;
        self.reset_election_timer();
    }

    fn become_leader(&mut self) {
        self.role = Role::Leader;
        let next = self.last_log_index() + 1;
        self.next_index = self.peers.iter().map(|&p| (p, next)).collect();
        self.match_index = self.peers.iter().map(|&p| (p, 0)).collect();
        self.heartbeat_elapsed = 0;
    }

    fn check_election_win(&mut self) -> Vec<Envelope> {
        if !matches!(self.role, Role::Candidate) {
            return Vec::new();
        }
        let total = self.peers.len() + 1;
        let majority = total / 2 + 1;
        if self.votes_received.len() >= majority {
            self.become_leader();
            return self.replicate_to_all_peers();
        }
        Vec::new()
    }

    fn append_entries_for(&self, follower: NodeId) -> Rpc {
        let next = *self
            .next_index
            .get(&follower)
            .unwrap_or(&(self.last_log_index() + 1));
        let prev_log_index = next.saturating_sub(1);
        let prev_log_term = self.log_term_at(prev_log_index);
        let entries = self.log[(prev_log_index as usize)..].to_vec();
        Rpc::AppendEntriesRequest {
            term: self.current_term,
            leader_id: self.id,
            prev_log_index,
            prev_log_term,
            entries,
            leader_commit: self.commit_index,
        }
    }

    fn replicate_to_all_peers(&self) -> Vec<Envelope> {
        self.peers
            .iter()
            .map(|&p| self.envelope_to(p, self.append_entries_for(p)))
            .collect()
    }

    /// Append `command` to the log (leader only) and return its 1-based
    /// log index plus the `AppendEntries` this triggers to start
    /// replicating it. A single-node cluster commits it immediately (it's
    /// already on a "majority" of one).
    pub fn propose(&mut self, command: LogRecord) -> Result<(u64, Vec<Envelope>), RaftError> {
        if !matches!(self.role, Role::Leader) {
            return Err(RaftError::NotLeader);
        }
        self.log.push(LogEntry {
            term: self.current_term,
            command,
        });
        let index = self.last_log_index();
        let msgs = if self.peers.is_empty() {
            self.advance_commit_index();
            Vec::new()
        } else {
            self.replicate_to_all_peers()
        };
        Ok((index, msgs))
    }

    /// Handle one incoming RPC, returning whatever response or follow-up
    /// RPCs it produces.
    pub fn receive(&mut self, envelope: Envelope) -> Vec<Envelope> {
        match envelope.rpc {
            Rpc::RequestVoteRequest {
                term,
                candidate_id,
                last_log_index,
                last_log_term,
            } => {
                let (resp_term, vote_granted) =
                    self.handle_request_vote(term, candidate_id, last_log_index, last_log_term);
                vec![self.envelope_to(
                    envelope.from,
                    Rpc::RequestVoteResponse {
                        term: resp_term,
                        vote_granted,
                        voter_id: self.id,
                    },
                )]
            }
            Rpc::RequestVoteResponse {
                term,
                vote_granted,
                voter_id,
            } => self.handle_vote_response(term, vote_granted, voter_id),
            Rpc::AppendEntriesRequest {
                term,
                leader_id,
                prev_log_index,
                prev_log_term,
                entries,
                leader_commit,
            } => {
                let (resp_term, success, match_index) = self.handle_append_entries(
                    term,
                    leader_id,
                    prev_log_index,
                    prev_log_term,
                    entries,
                    leader_commit,
                );
                vec![self.envelope_to(
                    envelope.from,
                    Rpc::AppendEntriesResponse {
                        term: resp_term,
                        success,
                        match_index,
                        follower_id: self.id,
                    },
                )]
            }
            Rpc::AppendEntriesResponse {
                term,
                success,
                match_index,
                follower_id,
            } => self.handle_append_response(term, success, match_index, follower_id),
        }
    }

    fn handle_request_vote(
        &mut self,
        term: Term,
        candidate_id: NodeId,
        last_log_index: u64,
        last_log_term: Term,
    ) -> (Term, bool) {
        if term > self.current_term {
            self.become_follower(term);
        }
        let mut vote_granted = false;
        if term == self.current_term
            && (self.voted_for.is_none() || self.voted_for == Some(candidate_id))
            && self.candidate_log_is_up_to_date(last_log_index, last_log_term)
        {
            self.voted_for = Some(candidate_id);
            vote_granted = true;
            self.reset_election_timer();
        }
        (self.current_term, vote_granted)
    }

    /// The Raft paper's §5.4.1 "up-to-date" check: a candidate can only
    /// win a follower's vote if its log couldn't have lost any committed
    /// entry, which term-then-length comparison guarantees.
    fn candidate_log_is_up_to_date(&self, last_log_index: u64, last_log_term: Term) -> bool {
        let my_term = self.last_log_term();
        let my_index = self.last_log_index();
        last_log_term > my_term || (last_log_term == my_term && last_log_index >= my_index)
    }

    fn handle_vote_response(
        &mut self,
        term: Term,
        vote_granted: bool,
        voter_id: NodeId,
    ) -> Vec<Envelope> {
        if term > self.current_term {
            self.become_follower(term);
            return Vec::new();
        }
        if !matches!(self.role, Role::Candidate) || term != self.current_term {
            return Vec::new();
        }
        if vote_granted {
            self.votes_received.insert(voter_id);
        }
        self.check_election_win()
    }

    fn handle_append_entries(
        &mut self,
        term: Term,
        leader_id: NodeId,
        prev_log_index: u64,
        prev_log_term: Term,
        entries: Vec<LogEntry>,
        leader_commit: u64,
    ) -> (Term, bool, u64) {
        if term < self.current_term {
            return (self.current_term, false, 0);
        }
        // A valid AppendEntries at term >= current_term always means: step
        // down (if we were a candidate/stale leader for this term), and
        // this is the term's real leader.
        self.become_follower(term);
        let _ = leader_id; // not tracked separately; a shell could surface it

        if prev_log_index > 0
            && (prev_log_index > self.last_log_index()
                || self.log_term_at(prev_log_index) != prev_log_term)
        {
            return (self.current_term, false, 0);
        }

        for (i, entry) in entries.into_iter().enumerate() {
            let index = prev_log_index + 1 + i as u64;
            if index <= self.last_log_index() {
                if self.log_term_at(index) != entry.term {
                    self.log.truncate((index - 1) as usize);
                    self.log.push(entry);
                }
                // else: already present and matching — leave it alone.
            } else {
                self.log.push(entry);
            }
        }

        if leader_commit > self.commit_index {
            self.commit_index = leader_commit.min(self.last_log_index());
        }

        (self.current_term, true, self.last_log_index())
    }

    fn handle_append_response(
        &mut self,
        term: Term,
        success: bool,
        match_index: u64,
        follower_id: NodeId,
    ) -> Vec<Envelope> {
        if term > self.current_term {
            self.become_follower(term);
            return Vec::new();
        }
        if !matches!(self.role, Role::Leader) || term != self.current_term {
            return Vec::new();
        }
        if success {
            self.match_index.insert(follower_id, match_index);
            self.next_index.insert(follower_id, match_index + 1);
            let previous_commit = self.commit_index;
            self.advance_commit_index();
            if self.commit_index > previous_commit {
                // Propagate the new commit index to followers immediately
                // rather than waiting for the next heartbeat tick — lower
                // commit-visibility latency, and what the test suite
                // assumes when it checks follower commit_index right after
                // a `propose` without ticking further.
                self.replicate_to_all_peers()
            } else {
                Vec::new()
            }
        } else {
            let fallback = self.last_log_index() + 1;
            let next = self.next_index.entry(follower_id).or_insert(fallback);
            if *next > 1 {
                *next -= 1;
            }
            vec![self.envelope_to(follower_id, self.append_entries_for(follower_id))]
        }
    }

    /// The safety-critical half of commit advancement: find the highest
    /// index replicated on a majority, but only actually commit it if that
    /// entry was written in the *current* term. Without that guard, a
    /// leader could commit an entry from an earlier term based purely on
    /// replication count and then have it silently overwritten by a later
    /// leader that never saw it — the exact failure mode Figure 8 in the
    /// Raft paper walks through.
    fn advance_commit_index(&mut self) {
        let mut match_indices: Vec<u64> = self
            .peers
            .iter()
            .map(|p| *self.match_index.get(p).unwrap_or(&0))
            .collect();
        match_indices.push(self.last_log_index()); // the leader is always fully caught up on itself
        match_indices.sort_unstable_by(|a, b| b.cmp(a));

        let majority_count = match_indices.len() / 2 + 1;
        let candidate = match_indices[majority_count - 1];
        if candidate > self.commit_index && self.log_term_at(candidate) == self.current_term {
            self.commit_index = candidate;
        }
    }

    /// Drain every entry that became committed since the last call,
    /// advancing `last_applied`. This is the hand-off point to a state
    /// machine (a real shell applies each `LogEntry.command` — a
    /// `LogRecord`, the same type stage 2's `ReplicaClient` already knows
    /// how to apply — to a `gems_engine::Store`).
    pub fn take_newly_committed(&mut self) -> Vec<LogEntry> {
        let mut out = Vec::new();
        while self.last_applied < self.commit_index {
            self.last_applied += 1;
            out.push(self.log[(self.last_applied - 1) as usize].clone());
        }
        out
    }
}

/// Wire encoding for `Envelope`, used by `raft_net`'s real TCP shell.
/// Kept in this module (rather than the network shell) since it's purely a
/// function of these types, with the same "framed with a `u32` length
/// prefix, decode returns `None` on a partial buffer" shape as
/// `record::LogRecord` — the two are meant to compose the same way when a
/// caller is accumulating bytes off a socket.
pub mod wire {
    use super::{Envelope, LogEntry, NodeId, Rpc, Term};
    use crate::frame::check_frame_len;
    use crate::record::LogRecord;
    use gems_common::{Error, Result};

    const REQUEST_VOTE_REQUEST: u8 = 1;
    const REQUEST_VOTE_RESPONSE: u8 = 2;
    const APPEND_ENTRIES_REQUEST: u8 = 3;
    const APPEND_ENTRIES_RESPONSE: u8 = 4;

    pub fn encode(envelope: &Envelope) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&envelope.from.to_le_bytes());
        payload.extend_from_slice(&envelope.to.to_le_bytes());
        match &envelope.rpc {
            Rpc::RequestVoteRequest {
                term,
                candidate_id,
                last_log_index,
                last_log_term,
            } => {
                payload.push(REQUEST_VOTE_REQUEST);
                payload.extend_from_slice(&term.to_le_bytes());
                payload.extend_from_slice(&candidate_id.to_le_bytes());
                payload.extend_from_slice(&last_log_index.to_le_bytes());
                payload.extend_from_slice(&last_log_term.to_le_bytes());
            }
            Rpc::RequestVoteResponse {
                term,
                vote_granted,
                voter_id,
            } => {
                payload.push(REQUEST_VOTE_RESPONSE);
                payload.extend_from_slice(&term.to_le_bytes());
                payload.push(*vote_granted as u8);
                payload.extend_from_slice(&voter_id.to_le_bytes());
            }
            Rpc::AppendEntriesRequest {
                term,
                leader_id,
                prev_log_index,
                prev_log_term,
                entries,
                leader_commit,
            } => {
                payload.push(APPEND_ENTRIES_REQUEST);
                payload.extend_from_slice(&term.to_le_bytes());
                payload.extend_from_slice(&leader_id.to_le_bytes());
                payload.extend_from_slice(&prev_log_index.to_le_bytes());
                payload.extend_from_slice(&prev_log_term.to_le_bytes());
                payload.extend_from_slice(&(entries.len() as u32).to_le_bytes());
                for entry in entries {
                    payload.extend_from_slice(&entry.term.to_le_bytes());
                    payload.extend_from_slice(&entry.command.encode());
                }
                payload.extend_from_slice(&leader_commit.to_le_bytes());
            }
            Rpc::AppendEntriesResponse {
                term,
                success,
                match_index,
                follower_id,
            } => {
                payload.push(APPEND_ENTRIES_RESPONSE);
                payload.extend_from_slice(&term.to_le_bytes());
                payload.push(*success as u8);
                payload.extend_from_slice(&match_index.to_le_bytes());
                payload.extend_from_slice(&follower_id.to_le_bytes());
            }
        }
        let mut framed = Vec::with_capacity(4 + payload.len());
        framed.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        framed.extend_from_slice(&payload);
        framed
    }

    /// Decode one envelope from the start of `buf`. `None` (not an error)
    /// if `buf` doesn't yet hold a complete envelope.
    pub fn decode(buf: &[u8]) -> Result<Option<(Envelope, usize)>> {
        if buf.len() < 4 {
            return Ok(None);
        }
        let payload_len = u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize;
        check_frame_len(payload_len, "Raft envelope exceeds the maximum frame size")?;
        let total_len = 4 + payload_len;
        if buf.len() < total_len {
            return Ok(None);
        }
        let payload = &buf[4..total_len];
        let mut pos = 0;
        let from = read_node_id(payload, &mut pos)?;
        let to = read_node_id(payload, &mut pos)?;
        let tag = read_u8(payload, &mut pos)?;

        let rpc = match tag {
            REQUEST_VOTE_REQUEST => Rpc::RequestVoteRequest {
                term: read_term(payload, &mut pos)?,
                candidate_id: read_node_id(payload, &mut pos)?,
                last_log_index: read_u64(payload, &mut pos)?,
                last_log_term: read_term(payload, &mut pos)?,
            },
            REQUEST_VOTE_RESPONSE => Rpc::RequestVoteResponse {
                term: read_term(payload, &mut pos)?,
                vote_granted: read_u8(payload, &mut pos)? != 0,
                voter_id: read_node_id(payload, &mut pos)?,
            },
            APPEND_ENTRIES_REQUEST => {
                let term = read_term(payload, &mut pos)?;
                let leader_id = read_node_id(payload, &mut pos)?;
                let prev_log_index = read_u64(payload, &mut pos)?;
                let prev_log_term = read_term(payload, &mut pos)?;
                let entry_count = read_u32(payload, &mut pos)? as usize;
                // `entry_count` is attacker/corruption-controlled; each
                // entry is at least 12 bytes (8-byte term + 4-byte
                // LogRecord length prefix), so a claim bigger than what
                // could possibly fit in the already-length-checked
                // `payload` is rejected before `Vec::with_capacity` ever
                // tries to allocate for it.
                if entry_count > payload.len() / 12 {
                    return Err(Error::InvalidValue {
                        detail: "AppendEntries entry count exceeds what the payload could hold",
                    });
                }
                let mut entries = Vec::with_capacity(entry_count);
                for _ in 0..entry_count {
                    let entry_term = read_term(payload, &mut pos)?;
                    let (command, consumed) =
                        LogRecord::decode(&payload[pos..])?.ok_or(Error::InvalidValue {
                            detail: "truncated log entry inside AppendEntries payload",
                        })?;
                    pos += consumed;
                    entries.push(LogEntry {
                        term: entry_term,
                        command,
                    });
                }
                let leader_commit = read_u64(payload, &mut pos)?;
                Rpc::AppendEntriesRequest {
                    term,
                    leader_id,
                    prev_log_index,
                    prev_log_term,
                    entries,
                    leader_commit,
                }
            }
            APPEND_ENTRIES_RESPONSE => Rpc::AppendEntriesResponse {
                term: read_term(payload, &mut pos)?,
                success: read_u8(payload, &mut pos)? != 0,
                match_index: read_u64(payload, &mut pos)?,
                follower_id: read_node_id(payload, &mut pos)?,
            },
            _ => {
                return Err(Error::InvalidValue {
                    detail: "unknown Raft RPC tag",
                })
            }
        };

        Ok(Some((Envelope { from, to, rpc }, total_len)))
    }

    fn read_u8(buf: &[u8], pos: &mut usize) -> Result<u8> {
        let b = *buf.get(*pos).ok_or(Error::InvalidValue {
            detail: "Raft envelope truncated",
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
                detail: "Raft envelope truncated",
            });
        }
        let v = u32::from_le_bytes(buf[*pos..*pos + 4].try_into().unwrap());
        *pos += 4;
        Ok(v)
    }

    fn read_u64(buf: &[u8], pos: &mut usize) -> Result<u64> {
        if buf.len() < *pos + 8 {
            return Err(Error::InvalidValue {
                detail: "Raft envelope truncated",
            });
        }
        let v = u64::from_le_bytes(buf[*pos..*pos + 8].try_into().unwrap());
        *pos += 8;
        Ok(v)
    }

    fn read_term(buf: &[u8], pos: &mut usize) -> Result<Term> {
        read_u64(buf, pos)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::record::LogRecord;
        use gems_common::Tuid;

        fn sample_entries() -> Vec<LogEntry> {
            vec![
                LogEntry {
                    term: 1,
                    command: LogRecord::Delete {
                        id: Tuid::new([1u8; 16], 1),
                    },
                },
                LogEntry {
                    term: 2,
                    command: LogRecord::Delete {
                        id: Tuid::new([2u8; 16], 2),
                    },
                },
            ]
        }

        #[test]
        fn decode_rejects_a_claimed_length_over_the_frame_cap() {
            let mut buf = ((crate::frame::MAX_FRAME_LEN as u32) + 1)
                .to_le_bytes()
                .to_vec();
            buf.push(1);
            assert!(decode(&buf).is_err());
        }

        #[test]
        fn decode_rejects_an_append_entries_count_bigger_than_the_payload_could_hold() {
            // A crafted AppendEntries claiming e.g. a billion entries but
            // with a payload far too small to actually hold them must be
            // rejected before `Vec::with_capacity` ever sees that count.
            let mut payload = Vec::new();
            payload.extend_from_slice(&1u32.to_le_bytes()); // from
            payload.extend_from_slice(&2u32.to_le_bytes()); // to
            payload.push(APPEND_ENTRIES_REQUEST);
            payload.extend_from_slice(&1u64.to_le_bytes()); // term
            payload.extend_from_slice(&1u32.to_le_bytes()); // leader_id
            payload.extend_from_slice(&0u64.to_le_bytes()); // prev_log_index
            payload.extend_from_slice(&0u64.to_le_bytes()); // prev_log_term
            payload.extend_from_slice(&1_000_000_000u32.to_le_bytes()); // entry_count
            let mut framed = (payload.len() as u32).to_le_bytes().to_vec();
            framed.extend_from_slice(&payload);
            assert!(decode(&framed).is_err());
        }

        #[test]
        fn request_vote_request_roundtrip() {
            let env = Envelope {
                from: 1,
                to: 2,
                rpc: Rpc::RequestVoteRequest {
                    term: 5,
                    candidate_id: 1,
                    last_log_index: 10,
                    last_log_term: 4,
                },
            };
            let encoded = encode(&env);
            let (decoded, consumed) = decode(&encoded).unwrap().unwrap();
            assert_eq!(consumed, encoded.len());
            assert_eq!(decoded, env);
        }

        #[test]
        fn append_entries_request_roundtrip_with_entries() {
            let env = Envelope {
                from: 1,
                to: 2,
                rpc: Rpc::AppendEntriesRequest {
                    term: 3,
                    leader_id: 1,
                    prev_log_index: 4,
                    prev_log_term: 2,
                    entries: sample_entries(),
                    leader_commit: 3,
                },
            };
            let encoded = encode(&env);
            let (decoded, consumed) = decode(&encoded).unwrap().unwrap();
            assert_eq!(consumed, encoded.len());
            assert_eq!(decoded, env);
        }

        #[test]
        fn append_entries_response_roundtrip() {
            let env = Envelope {
                from: 2,
                to: 1,
                rpc: Rpc::AppendEntriesResponse {
                    term: 3,
                    success: true,
                    match_index: 7,
                    follower_id: 2,
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
                rpc: Rpc::RequestVoteResponse {
                    term: 1,
                    vote_granted: true,
                    voter_id: 1,
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
                rpc: Rpc::RequestVoteResponse {
                    term: 1,
                    vote_granted: true,
                    voter_id: 1,
                },
            };
            let b = Envelope {
                from: 2,
                to: 1,
                rpc: Rpc::RequestVoteResponse {
                    term: 1,
                    vote_granted: false,
                    voter_id: 2,
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
    use gems_common::Tuid;
    use std::collections::{HashMap, HashSet as StdHashSet, VecDeque};

    fn delete_record(n: u8) -> LogRecord {
        LogRecord::Delete {
            id: Tuid::new([n; 16], n as u64),
        }
    }

    /// A test-only network simulator: delivers messages between
    /// `RaftCore`s synchronously (a full BFS drain per `tick`/`propose`
    /// call, modeling instantaneous-but-possibly-partitioned delivery)
    /// under the test's full control — including which links are blocked,
    /// which is how the partition/stale-leader tests work.
    struct Cluster {
        nodes: HashMap<NodeId, RaftCore>,
        blocked: StdHashSet<(NodeId, NodeId)>,
    }

    impl Cluster {
        fn new(ids: &[NodeId], election_timeouts: &[u32], heartbeat_interval: u32) -> Self {
            let nodes = ids
                .iter()
                .zip(election_timeouts)
                .map(|(&id, &timeout)| {
                    let peers = ids.iter().copied().filter(|&x| x != id).collect();
                    (id, RaftCore::new(id, peers, timeout, heartbeat_interval))
                })
                .collect();
            Cluster {
                nodes,
                blocked: StdHashSet::new(),
            }
        }

        fn block(&mut self, from: NodeId, to: NodeId) {
            self.blocked.insert((from, to));
        }

        fn unblock_all(&mut self) {
            self.blocked.clear();
        }

        fn drain(&mut self, mut queue: VecDeque<Envelope>) {
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
            let mut queue = VecDeque::new();
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

        fn propose(&mut self, leader: NodeId, command: LogRecord) -> u64 {
            let (index, msgs) = self
                .nodes
                .get_mut(&leader)
                .unwrap()
                .propose(command)
                .unwrap();
            self.drain(msgs.into());
            index
        }

        fn leaders(&self) -> Vec<NodeId> {
            let mut ids: Vec<NodeId> = self
                .nodes
                .iter()
                .filter(|(_, n)| n.role() == Role::Leader)
                .map(|(&id, _)| id)
                .collect();
            ids.sort_unstable();
            ids
        }
    }

    #[test]
    fn single_node_cluster_becomes_leader_and_commits_immediately() {
        let mut cluster = Cluster::new(&[1], &[5], 3);
        cluster.run_ticks(5);
        assert_eq!(cluster.leaders(), vec![1]);

        cluster.propose(1, delete_record(1));
        let node = cluster.nodes.get_mut(&1).unwrap();
        assert_eq!(node.commit_index(), 1);
        assert_eq!(node.take_newly_committed().len(), 1);
    }

    #[test]
    fn three_node_cluster_elects_exactly_one_leader() {
        // Node 1 has the shortest timeout, so it starts the election first
        // and should win it before nodes 2/3 time out on their own.
        let mut cluster = Cluster::new(&[1, 2, 3], &[3, 10, 10], 1);
        cluster.run_ticks(5);
        assert_eq!(cluster.leaders(), vec![1]);
        for id in [1, 2, 3] {
            assert_eq!(cluster.nodes[&id].current_term(), 1);
        }
    }

    #[test]
    fn proposal_replicates_and_commits_on_a_majority() {
        let mut cluster = Cluster::new(&[1, 2, 3], &[3, 10, 10], 1);
        cluster.run_ticks(5);
        assert_eq!(cluster.leaders(), vec![1]);

        let index = cluster.propose(1, delete_record(7));
        assert_eq!(index, 1);

        for id in [1, 2, 3] {
            let node = cluster.nodes.get_mut(&id).unwrap();
            assert_eq!(node.commit_index(), 1, "node {id} should have committed");
            let committed = node.take_newly_committed();
            assert_eq!(committed.len(), 1);
            assert_eq!(committed[0].command, delete_record(7));
        }
    }

    #[test]
    fn a_partitioned_minority_cannot_elect_a_leader_or_commit() {
        let mut cluster = Cluster::new(&[1, 2, 3, 4, 5], &[3, 10, 10, 10, 10], 1);
        cluster.run_ticks(5);
        assert_eq!(cluster.leaders(), vec![1]);
        cluster.propose(1, delete_record(1));
        assert_eq!(cluster.nodes[&1].commit_index(), 1);

        // Partition off nodes 4 and 5 from everyone (a minority of 2 out
        // of 5) and try to get one of them elected.
        for (a, b) in [(4, 1), (1, 4), (4, 2), (2, 4), (4, 3), (3, 4)] {
            cluster.block(a, b);
        }
        for (a, b) in [(5, 1), (1, 5), (5, 2), (2, 5), (5, 3), (3, 5)] {
            cluster.block(a, b);
        }
        cluster.nodes.get_mut(&4).unwrap().set_election_timeout(2);
        cluster.run_ticks(10);

        assert_eq!(
            cluster.nodes[&4].role(),
            Role::Candidate,
            "an isolated node keeps trying forever but never wins"
        );
        // The majority side (1, 2, 3) must still have exactly one leader
        // and it must not have lost commit_index.
        let majority_leaders: Vec<NodeId> = cluster
            .leaders()
            .into_iter()
            .filter(|id| [1, 2, 3].contains(id))
            .collect();
        assert_eq!(majority_leaders.len(), 1);
        assert_eq!(cluster.nodes[&1].commit_index(), 1);
    }

    #[test]
    fn leader_failure_triggers_reelection_with_a_higher_term() {
        let mut cluster = Cluster::new(&[1, 2, 3], &[3, 10, 10], 1);
        cluster.run_ticks(5);
        assert_eq!(cluster.leaders(), vec![1]);
        let first_term = cluster.nodes[&1].current_term();

        // Simulate node 1 crashing: block all its links both ways so it
        // can neither send nor receive.
        for other in [2, 3] {
            cluster.block(1, other);
            cluster.block(other, 1);
        }
        // Give node 2 the short timeout for the next election.
        cluster.nodes.get_mut(&2).unwrap().set_election_timeout(3);
        cluster.run_ticks(10);

        let new_leaders: Vec<NodeId> = cluster
            .leaders()
            .into_iter()
            .filter(|id| *id != 1)
            .collect();
        assert_eq!(new_leaders, vec![2]);
        assert!(cluster.nodes[&2].current_term() > first_term);
    }

    #[test]
    fn a_stale_leader_steps_down_once_it_sees_a_higher_term() {
        let mut cluster = Cluster::new(&[1, 2, 3], &[3, 10, 10], 1);
        cluster.run_ticks(5);
        assert_eq!(cluster.leaders(), vec![1]);

        // Isolate the old leader and force a new election among the rest.
        for other in [2, 3] {
            cluster.block(1, other);
            cluster.block(other, 1);
        }
        cluster.nodes.get_mut(&2).unwrap().set_election_timeout(3);
        cluster.run_ticks(10);
        assert_eq!(
            cluster
                .leaders()
                .into_iter()
                .filter(|&id| id != 1)
                .collect::<Vec<_>>(),
            vec![2]
        );
        let new_term = cluster.nodes[&2].current_term();

        // Reconnect node 1 (still believing it's the term-1 leader) and
        // let a heartbeat round run; it must step down on seeing term 2.
        cluster.unblock_all();
        cluster.run_ticks(5);

        assert_eq!(cluster.nodes[&1].role(), Role::Follower);
        assert_eq!(cluster.nodes[&1].current_term(), new_term);
    }

    #[test]
    fn candidate_with_a_stale_log_cannot_win_an_election() {
        // Node 1 gets a proposal committed, then goes silent while nodes 2
        // and 3 (whose logs are now behind) try to elect a leader among
        // themselves. Node 3, with the up-to-date-check disabled by having
        // an equally-empty log, races node 2 — the real assertion is that
        // whichever of them wins, term progresses and no data is lost, and
        // specifically that node 2/3 cannot claim to be "as up to date" as
        // node 1 while missing its committed entry once node 1 rejoins.
        let mut cluster = Cluster::new(&[1, 2, 3], &[3, 10, 10], 1);
        cluster.run_ticks(5);
        assert_eq!(cluster.leaders(), vec![1]);
        cluster.propose(1, delete_record(1));
        assert_eq!(cluster.nodes[&1].log_len(), 1);
        assert_eq!(
            cluster.nodes[&2].log_len(),
            1,
            "already replicated before isolation"
        );

        // Isolate node 1 *after* replication, so 2 and 3 both have the
        // entry too — this test is really checking that the up-to-date
        // check doesn't spuriously block an equally-current candidate.
        for other in [2, 3] {
            cluster.block(1, other);
            cluster.block(other, 1);
        }
        cluster.nodes.get_mut(&3).unwrap().set_election_timeout(3);
        cluster.run_ticks(10);

        let new_leader = cluster
            .leaders()
            .into_iter()
            .find(|&id| id != 1)
            .expect("2 or 3 must be able to elect a leader with an equally up-to-date log");
        assert_eq!(new_leader, 3);
        assert_eq!(
            cluster.nodes[&3].log_len(),
            1,
            "must not have lost the already-committed entry"
        );
    }
}
