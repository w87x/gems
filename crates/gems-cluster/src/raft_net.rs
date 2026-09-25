//! `RaftNode`: the real-network shell `raft.rs`'s module doc calls for —
//! TCP sockets carrying `raft::wire`-encoded envelopes, a timer thread
//! driving `tick()`, and committed entries applied to a real
//! `gems_engine::Store`. One thread per accepted connection (consistent
//! with `server.rs`'s style), plus one "engine" thread that exclusively
//! owns the `RaftCore` and the `Store` — every other thread only ever
//! reaches them by sending a message, so there's no locking to get wrong
//! around the core algorithm itself.
//!
//! **Simplification versus a from-scratch production shell, worth naming
//! rather than leaving implicit:** each outbound RPC opens a fresh,
//! short-lived TCP connection rather than keeping persistent per-peer
//! connections open. Simple and correct — small clusters at heartbeat-ish
//! message rates don't need connection reuse — but it does mean a
//! `connect()` (with a short timeout) is on the critical path of every
//! tick, so a genuinely slow-to-refuse peer could delay a tick. Acceptable
//! for proving the wiring out; worth revisiting under real deployment
//! latency requirements.
//!
//! **Also simplified: election timeout jitter.** `RaftCore` deliberately
//! doesn't randomize its own timeout (see its module doc); the "proper"
//! shell behavior is to draw a fresh random timeout every time the
//! election timer resets, which `RaftCore` doesn't expose a hook for. This
//! shell instead draws *one* random timeout per node at startup (via
//! `gems_common::rand`), which still staggers the first election across
//! nodes — the scenario that actually needs randomness to avoid a
//! guaranteed tie — but doesn't re-jitter afterward. A livelock from
//! *that* gap specifically would need two nodes to keep re-timing-out in
//! lockstep after the first election ever, which the fixed stagger from
//! startup already makes very unlikely for a long-running node; full
//! jitter-on-every-reset is real follow-on work, not implemented here.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use gems_common::{Error, Result};
use gems_engine::Store;

use crate::raft::{wire, Envelope, NodeId, RaftCore, RaftError, Role, Term};
use crate::record::LogRecord;

enum EngineMsg {
    Inbound(Envelope),
    Propose(LogRecord, Sender<std::result::Result<u64, RaftError>>),
    Status(Sender<(Role, Term)>),
    Shutdown,
}

pub struct RaftNodeHandle {
    to_engine: Sender<EngineMsg>,
    join: Option<thread::JoinHandle<()>>,
}

impl RaftNodeHandle {
    /// Append `command` to the log if this node is currently the leader.
    /// Returns the log index it was assigned; the caller finds out it was
    /// actually committed by polling `status`/watching the local `Store`,
    /// same as any async-replicated write.
    pub fn propose(&self, command: LogRecord) -> Result<u64> {
        let (tx, rx) = mpsc::channel();
        self.to_engine
            .send(EngineMsg::Propose(command, tx))
            .map_err(|_| Error::InvalidValue {
                detail: "Raft engine thread is gone",
            })?;
        rx.recv()
            .map_err(|_| Error::InvalidValue {
                detail: "Raft engine thread dropped the propose response",
            })?
            .map_err(|_| Error::InvalidValue {
                detail: "this node is not the Raft leader",
            })
    }

    pub fn status(&self) -> Result<(Role, Term)> {
        let (tx, rx) = mpsc::channel();
        self.to_engine
            .send(EngineMsg::Status(tx))
            .map_err(|_| Error::InvalidValue {
                detail: "Raft engine thread is gone",
            })?;
        rx.recv().map_err(|_| Error::InvalidValue {
            detail: "Raft engine thread dropped the status response",
        })
    }

    pub fn shutdown(mut self) {
        let _ = self.to_engine.send(EngineMsg::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Tuning knobs bundled together mainly to keep `spawn`'s argument count
/// sane; see `raft::RaftCore::new` for what `heartbeat_interval_ticks` and
/// `election_timeout_ticks_range` mean (the latter is sampled once per
/// node at startup — see this module's doc for why that's a fixed draw
/// rather than continuous jitter).
#[derive(Debug, Clone, Copy)]
pub struct RaftTiming {
    pub tick_interval: Duration,
    pub heartbeat_interval_ticks: u32,
    pub election_timeout_ticks_range: (u32, u32),
}

/// Start a Raft node: opens `store_dir` as a `gems_engine::Store`, binds
/// `listen_addr` for inbound peer RPCs and `client_listen_addr` for
/// inbound client propose requests (see `propose_remote`), and spawns
/// both accept loops plus the engine thread. `peers` maps every *other*
/// node's id to its (peer) address. `secret` authenticates peer traffic —
/// see `raft::wire::encode`'s doc — and must be the same across every
/// node in the cluster.
pub fn spawn(
    id: NodeId,
    listen_addr: &str,
    client_listen_addr: &str,
    peers: HashMap<NodeId, SocketAddr>,
    store_dir: &std::path::Path,
    timing: RaftTiming,
    secret: Arc<Vec<u8>>,
) -> Result<RaftNodeHandle> {
    let RaftTiming {
        tick_interval,
        heartbeat_interval_ticks,
        election_timeout_ticks_range,
    } = timing;
    let store = Store::open(store_dir, true).or_else(|_| Store::create(store_dir))?;
    let listener = TcpListener::bind(listen_addr)?;
    let client_listener = TcpListener::bind(client_listen_addr)?;

    let (to_engine, from_network) = mpsc::channel::<EngineMsg>();

    // Peer accept loop: one thread per connection, each decoding envelopes
    // off its socket and forwarding them into the engine's inbound channel.
    let accept_sender = to_engine.clone();
    let accept_secret = Arc::clone(&secret);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let sender = accept_sender.clone();
            let secret = Arc::clone(&accept_secret);
            thread::spawn(move || {
                let _ = handle_connection(stream, sender, &secret);
            });
        }
    });

    // Client accept loop: a separate port and a separate, much simpler
    // protocol (see `propose_remote`) — a client isn't a Raft peer and
    // shouldn't need to speak `raft::wire`'s Envelope format just to ask
    // "please propose this command."
    let client_sender = to_engine.clone();
    thread::spawn(move || {
        for stream in client_listener.incoming() {
            let Ok(stream) = stream else { continue };
            let sender = client_sender.clone();
            thread::spawn(move || {
                let _ = handle_client_connection(stream, sender);
            });
        }
    });

    let (lo, hi) = election_timeout_ticks_range;
    let span = hi.saturating_sub(lo).max(1);
    let jittered = lo + (u32::from_le_bytes(gems_common::rand::random_bytes()) % span);

    // Restore whatever term/vote/log this node persisted before its last
    // crash or shutdown — see `raft_state`'s module doc for why forgetting
    // this on restart would be a real Raft safety violation, not just data
    // loss.
    let (term, voted_for, log) = crate::raft_state::load(store_dir)?;
    let peer_ids: Vec<NodeId> = peers.keys().copied().collect();
    let core = RaftCore::restore(
        id,
        peer_ids,
        jittered,
        heartbeat_interval_ticks,
        term,
        voted_for,
        log,
    );

    let state_dir = store_dir.to_path_buf();
    let join = thread::spawn(move || {
        run_engine(
            core,
            store,
            peers,
            from_network,
            tick_interval,
            secret,
            state_dir,
        );
    });

    Ok(RaftNodeHandle {
        to_engine,
        join: Some(join),
    })
}

fn handle_connection(stream: TcpStream, sender: Sender<EngineMsg>, secret: &[u8]) -> Result<()> {
    let mut reader = stream;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = reader.read(&mut chunk)?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        while let Some((envelope, consumed)) = wire::decode(&buf, secret)? {
            let _ = sender.send(EngineMsg::Inbound(envelope));
            buf.drain(..consumed);
        }
    }
}

/// Client protocol: the client sends one `LogRecord::encode()`-framed
/// request (that framing is already self-describing, so it composes with
/// the same partial-read accumulation loop as everywhere else in this
/// workspace) and reads back a fixed 9-byte response: `u8 status (0 =
/// not leader, 1 = ok)` followed by `u64 index` (meaningful only when
/// `status == 1`). One request per connection — simple, and proposals are
/// rare enough relative to peer traffic that connection setup cost doesn't
/// matter here either.
const CLIENT_STATUS_NOT_LEADER: u8 = 0;
const CLIENT_STATUS_OK: u8 = 1;

fn handle_client_connection(mut stream: TcpStream, sender: Sender<EngineMsg>) -> Result<()> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let command = loop {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Ok(()); // client disconnected before sending a full request
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some((record, _)) = LogRecord::decode(&buf)? {
            break record;
        }
    };

    let (tx, rx) = mpsc::channel();
    if sender.send(EngineMsg::Propose(command, tx)).is_err() {
        return Ok(());
    }
    let mut response = [0u8; 9];
    match rx.recv() {
        Ok(Ok(index)) => {
            response[0] = CLIENT_STATUS_OK;
            response[1..9].copy_from_slice(&index.to_le_bytes());
        }
        _ => response[0] = CLIENT_STATUS_NOT_LEADER,
    }
    stream.write_all(&response)?;
    Ok(())
}

/// The other half of the client protocol above: connect to `addr` (a
/// node's *client* port, not its peer port) and ask it to propose
/// `command`. Returns the assigned log index on success, or an error if
/// this node isn't the leader (or wasn't reachable at all) — a caller
/// wanting to actually get a command committed retries against a
/// different member of the same Raft group, since this function doesn't
/// know who else is in it. See `shard.rs`'s `ShardedClient` for that retry
/// loop.
pub fn propose_remote(addr: SocketAddr, command: &LogRecord, timeout: Duration) -> Result<u64> {
    let mut stream = TcpStream::connect_timeout(&addr, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.write_all(&command.encode())?;

    let mut response = [0u8; 9];
    stream.read_exact(&mut response)?;
    if response[0] != CLIENT_STATUS_OK {
        return Err(Error::InvalidValue {
            detail: "target node is not the Raft leader",
        });
    }
    Ok(u64::from_le_bytes(response[1..9].try_into().unwrap()))
}

fn send_envelope(addr: SocketAddr, envelope: &Envelope, secret: &[u8]) {
    // Best-effort: a send failure (peer down, network partition) is
    // exactly the condition Raft is designed to tolerate — the tick loop
    // will simply retry on the next heartbeat/election timeout. Logging
    // it is a real deployment's job (this shell has no logging story
    // yet); silently dropping is the correct *algorithmic* response.
    if let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(200)) {
        let _ = stream.write_all(&wire::encode(envelope, secret));
    }
}

fn apply_to_store(store: &mut Store, command: &LogRecord) {
    let result = match command {
        LogRecord::Insert { header, body } => store.insert(header.clone(), body),
        LogRecord::Delete { id } => store.delete(id).map(|_| ()),
    };
    // A failure applying an already-committed, already-validated command
    // indicates local corruption, not a Raft-level problem — nothing in
    // this shell's scope to do about it beyond not crashing the engine
    // loop over one bad apply.
    let _ = result;
}

/// Persists `core`'s current term/vote/log to `state_dir`, per the Raft
/// paper's requirement that this happen before acting on the state change
/// (here: before this loop iteration's outgoing RPCs are sent). A failure
/// here is treated as fatal (panics, taking the node down) rather than
/// logged and ignored: continuing to run as a Raft node whose vote or log
/// entry silently failed to reach durable storage risks exactly the
/// safety violation this whole module exists to prevent, and a crashed
/// node is something the rest of the cluster already tolerates by design,
/// unlike a node that's up but lying about its own history.
fn persist_state(core: &RaftCore, state_dir: &std::path::Path) {
    let (term, voted_for, log) = core.persistent_state();
    crate::raft_state::save(state_dir, term, voted_for, log)
        .expect("failed to persist Raft state durably");
}

fn run_engine(
    mut core: RaftCore,
    mut store: Store,
    peers: HashMap<NodeId, SocketAddr>,
    inbound: Receiver<EngineMsg>,
    tick_interval: Duration,
    secret: Arc<Vec<u8>>,
    state_dir: std::path::PathBuf,
) {
    loop {
        let outgoing = match inbound.recv_timeout(tick_interval) {
            Ok(EngineMsg::Inbound(envelope)) => {
                let outgoing = core.receive(envelope);
                persist_state(&core, &state_dir);
                outgoing
            }
            Ok(EngineMsg::Propose(command, respond)) => {
                let result = core.propose(command);
                let outgoing = match &result {
                    Ok((_, msgs)) => msgs.clone(),
                    Err(_) => Vec::new(),
                };
                if result.is_ok() {
                    persist_state(&core, &state_dir);
                }
                let _ = respond.send(result.map(|(index, _)| index));
                outgoing
            }
            Ok(EngineMsg::Status(respond)) => {
                let _ = respond.send((core.role(), core.current_term()));
                Vec::new()
            }
            Ok(EngineMsg::Shutdown) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // `tick()` never touches the log, only term/role/vote (on
                // an election timeout firing), so comparing just those is
                // a safe, cheap way to skip a redundant fsync on the
                // common case (a heartbeat tick that changes nothing).
                let before = (core.current_term(), core.role(), core.voted_for());
                let outgoing = core.tick();
                let after = (core.current_term(), core.role(), core.voted_for());
                if before != after {
                    persist_state(&core, &state_dir);
                }
                outgoing
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        };

        for envelope in outgoing {
            if let Some(&addr) = peers.get(&envelope.to) {
                send_envelope(addr, &envelope, &secret);
            }
        }

        for entry in core.take_newly_committed() {
            apply_to_store(&mut store, &entry.command);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gems_catalog::{EntityFlags, EntityHeader, EntityKind};
    use gems_common::Tuid;
    use std::path::PathBuf;

    fn test_secret() -> Arc<Vec<u8>> {
        Arc::new(b"test-cluster-secret".to_vec())
    }

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("gems-cluster-raft-net-test")
            .join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn header(id: Tuid) -> EntityHeader {
        EntityHeader {
            id,
            created_by: [0u8; 16],
            modified_by: [0u8; 16],
            modified_at_ns: 0,
            name: "w1".to_string(),
            description: String::new(),
            flags: EntityFlags::NONE,
            entity_kind: EntityKind::Data,
            schema_ref: Tuid::NIL,
            body_offset: 0,
            body_len: 0,
        }
    }

    fn free_port() -> u16 {
        TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    fn wait_for_leader(handles: &[(NodeId, RaftNodeHandle)], timeout: Duration) -> Option<NodeId> {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            for (id, handle) in handles {
                if let Ok((Role::Leader, _)) = handle.status() {
                    return Some(*id);
                }
            }
            thread::sleep(Duration::from_millis(20));
        }
        None
    }

    /// Env var naming the store directory: its presence is how this test
    /// tells its own re-exec'd child process "you're the crash-simulating
    /// helper, not the launcher" (see the test below).
    const CRASH_HELPER_ENV: &str = "GEMS_RAFT_CRASH_HELPER_DIR";

    #[test]
    fn a_hard_killed_node_recovers_its_persisted_term_and_log_on_restart() {
        if let Ok(dir) = std::env::var(CRASH_HELPER_ENV) {
            run_crash_helper(std::path::PathBuf::from(dir));
        }

        let dir = tmp_dir("crash_recovery");
        std::fs::create_dir_all(&dir).unwrap();
        let exe = std::env::current_exe().unwrap();
        let test_name =
            "raft_net::tests::a_hard_killed_node_recovers_its_persisted_term_and_log_on_restart";

        // Re-exec this same test binary, filtered to just this test, with
        // the helper env var set — the recursive call above takes the
        // helper branch instead of getting here again.
        let mut child = std::process::Command::new(&exe)
            .args(["--exact", test_name, "--nocapture"])
            .env(CRASH_HELPER_ENV, &dir)
            .spawn()
            .expect("failed to spawn the crash-test helper subprocess");

        // Poll the on-disk state (not the process, which we have no IPC
        // into) until the helper has persisted the entries it proposes
        // right after becoming leader.
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            if let Ok((_, _, log)) = crate::raft_state::load(&dir) {
                if log.len() >= 3 {
                    break;
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "helper process never persisted the proposed entries in time"
            );
            thread::sleep(Duration::from_millis(20));
        }
        let (term_before_kill, _, log_before_kill) = crate::raft_state::load(&dir).unwrap();

        // SIGKILL: no clean shutdown, no Drop code, no chance to flush
        // anything — exactly what a hard `kill -9` (or a host power loss)
        // does to a real deployed process. If durability here depended on
        // graceful-shutdown code ever running, this is where that would
        // show up as lost or corrupted state.
        let pid = rustix::process::Pid::from_raw(child.id() as i32)
            .expect("child pid should be a valid nonzero pid");
        rustix::process::kill_process(pid, rustix::process::Signal::KILL)
            .expect("failed to SIGKILL the helper process");
        let status = child
            .wait()
            .expect("failed to reap the killed helper process");
        assert!(
            !status.success(),
            "the helper must have been killed, not exited on its own"
        );

        // What was already durable before the kill must still be exactly
        // what it was — the crash must not have corrupted or rolled back
        // anything already fsync'd.
        let (term_after, _voted_for, log_after) = crate::raft_state::load(&dir).unwrap();
        assert!(term_after >= term_before_kill);
        assert!(log_after.len() >= log_before_kill.len());
        assert_eq!(log_after[..log_before_kill.len()], log_before_kill[..]);

        // The actual point of this test: a fresh RaftCore restored from
        // this on-disk state — exactly what `spawn()` does whenever a
        // node restarts — sees the term this node had reached and the
        // entries it had logged, not a reset-to-zero state. Forgetting
        // either after a restart is a real Raft safety violation (a node
        // that forgets its term/vote can vote twice in the same term),
        // not just data loss.
        assert!(
            term_after >= 1,
            "term must have advanced past the initial single-node election"
        );
        assert_eq!(
            log_after.len(),
            3,
            "all three proposed entries must have survived the crash"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The crash-simulating helper: runs as a re-exec'd child process (see
    /// above). Starts a single-node Raft "cluster" (no peers, so it
    /// becomes its own leader immediately), proposes a few entries so
    /// there's real persisted state to check, then parks forever —
    /// deliberately never calling `shutdown()` or running any cleanup —
    /// until the parent test sends it SIGKILL.
    fn run_crash_helper(dir: std::path::PathBuf) -> ! {
        let peer_port = free_port();
        let client_port = free_port();
        let handle = spawn(
            1,
            &format!("127.0.0.1:{peer_port}"),
            &format!("127.0.0.1:{client_port}"),
            HashMap::new(),
            &dir,
            RaftTiming {
                tick_interval: Duration::from_millis(20),
                heartbeat_interval_ticks: 3,
                election_timeout_ticks_range: (6, 10),
            },
            test_secret(),
        )
        .expect("crash-test helper failed to start its Raft node");

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !matches!(handle.status(), Ok((Role::Leader, _))) {
            assert!(
                std::time::Instant::now() < deadline,
                "crash-test helper's single-node cluster never became leader"
            );
            thread::sleep(Duration::from_millis(20));
        }

        for i in 0..3u8 {
            handle
                .propose(LogRecord::Insert {
                    header: header(Tuid::new([i; 16], i as u64)),
                    body: b"x".to_vec(),
                })
                .expect("crash-test helper failed to propose an entry");
        }

        loop {
            thread::sleep(Duration::from_secs(3600));
        }
    }

    #[test]
    fn three_real_nodes_elect_a_leader_and_replicate_a_proposal() {
        let dir = tmp_dir("three_nodes");
        let ports: Vec<u16> = (0..3).map(|_| free_port()).collect();
        let client_ports: Vec<u16> = (0..3).map(|_| free_port()).collect();
        let ids: Vec<NodeId> = vec![1, 2, 3];
        let addrs: HashMap<NodeId, SocketAddr> = ids
            .iter()
            .zip(&ports)
            .map(|(&id, &port)| (id, format!("127.0.0.1:{port}").parse().unwrap()))
            .collect();

        let mut handles = Vec::new();
        for (i, &id) in ids.iter().enumerate() {
            let peers: HashMap<NodeId, SocketAddr> = addrs
                .iter()
                .filter(|(&pid, _)| pid != id)
                .map(|(&pid, &addr)| (pid, addr))
                .collect();
            let handle = spawn(
                id,
                &format!("127.0.0.1:{}", ports[i]),
                &format!("127.0.0.1:{}", client_ports[i]),
                peers,
                &dir.join(format!("node{id}")),
                RaftTiming {
                    tick_interval: Duration::from_millis(20),
                    heartbeat_interval_ticks: 3,
                    election_timeout_ticks_range: (6, 10),
                },
                test_secret(),
            )
            .unwrap();
            handles.push((id, handle));
        }

        let leader_id = wait_for_leader(&handles, Duration::from_secs(5))
            .expect("a leader must be elected within 5 seconds");
        let leader = &handles.iter().find(|(id, _)| *id == leader_id).unwrap().1;

        let entity_id = Tuid::new([7u8; 16], 7);
        let index = leader
            .propose(LogRecord::Insert {
                header: header(entity_id),
                body: b"hello".to_vec(),
            })
            .unwrap();
        assert_eq!(index, 1);

        // Give the cluster time to replicate and apply, then check every
        // node's local Store — this is the real end-to-end assertion:
        // actual TCP sockets, actual timer-driven ticks, actual
        // gems_engine::Store writes on three separate node instances.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let all_applied = ids.iter().all(|&id| {
                let store_dir = dir.join(format!("node{id}"));
                gems_engine::Store::open(&store_dir, false)
                    .ok()
                    .and_then(|s| s.get(&entity_id).ok().flatten())
                    .is_some()
            });
            if all_applied {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "not every node applied the committed entry within 5 seconds"
            );
            thread::sleep(Duration::from_millis(50));
        }

        for (_, handle) in handles {
            handle.shutdown();
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn propose_remote_succeeds_against_the_leader_and_fails_against_a_follower() {
        let dir = tmp_dir("propose_remote");
        let ports: Vec<u16> = (0..3).map(|_| free_port()).collect();
        let client_ports: Vec<u16> = (0..3).map(|_| free_port()).collect();
        let ids: Vec<NodeId> = vec![1, 2, 3];
        let addrs: HashMap<NodeId, SocketAddr> = ids
            .iter()
            .zip(&ports)
            .map(|(&id, &port)| (id, format!("127.0.0.1:{port}").parse().unwrap()))
            .collect();
        let client_addrs: HashMap<NodeId, SocketAddr> = ids
            .iter()
            .zip(&client_ports)
            .map(|(&id, &port)| (id, format!("127.0.0.1:{port}").parse().unwrap()))
            .collect();

        let mut handles = Vec::new();
        for (i, &id) in ids.iter().enumerate() {
            let peers: HashMap<NodeId, SocketAddr> = addrs
                .iter()
                .filter(|(&pid, _)| pid != id)
                .map(|(&pid, &addr)| (pid, addr))
                .collect();
            let handle = spawn(
                id,
                &format!("127.0.0.1:{}", ports[i]),
                &format!("127.0.0.1:{}", client_ports[i]),
                peers,
                &dir.join(format!("node{id}")),
                RaftTiming {
                    tick_interval: Duration::from_millis(20),
                    heartbeat_interval_ticks: 3,
                    election_timeout_ticks_range: (6, 10),
                },
                test_secret(),
            )
            .unwrap();
            handles.push((id, handle));
        }

        let leader_id = wait_for_leader(&handles, Duration::from_secs(5))
            .expect("a leader must be elected within 5 seconds");
        let follower_id = ids.iter().copied().find(|&id| id != leader_id).unwrap();

        // A remote client, over the network, talking only to the client
        // port — no in-process channel, no shared memory with the node.
        let entity_id = Tuid::new([9u8; 16], 9);
        let index = propose_remote(
            client_addrs[&leader_id],
            &LogRecord::Insert {
                header: header(entity_id),
                body: b"via-network".to_vec(),
            },
            Duration::from_secs(2),
        )
        .expect("proposing to the actual leader's client port must succeed");
        assert_eq!(index, 1);

        let follower_result = propose_remote(
            client_addrs[&follower_id],
            &LogRecord::Delete { id: entity_id },
            Duration::from_secs(2),
        );
        assert!(
            follower_result.is_err(),
            "proposing to a follower's client port must fail, not silently redirect"
        );

        for (_, handle) in handles {
            handle.shutdown();
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
