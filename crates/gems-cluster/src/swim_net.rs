//! `SwimNode`: the real-network shell for `SwimCore` — TCP sockets
//! carrying `gossip::wire`-encoded envelopes and a timer thread driving
//! `tick()`, mirroring `raft_net.rs`'s structure and simplifications for
//! the same reasons (see that module's doc for the fuller rationale, which
//! applies here too: one fresh short-lived TCP connection per outbound
//! message rather than persistent per-peer connections).
//!
//! **Worth naming explicitly: real SWIM deployments typically use UDP.**
//! Packet loss is part of SWIM's own failure-detection signal (a dropped
//! ping *is* the timeout it's designed to notice), so UDP's unreliability
//! isn't a problem to route around, it's expected input. Using TCP here
//! instead is a deliberate consistency choice with the rest of this
//! workspace's transport code, not an attempt to reproduce UDP's exact
//! deployment characteristics: a TCP connect failure or timeout gets
//! treated as a failed ping exactly the way a UDP timeout would be, so the
//! algorithm's behavior carries over, just implemented over a different
//! transport than SWIM's own paper assumes.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use gems_common::{Error, Result};

use crate::gossip::{wire, Envelope, NodeId, Status, SwimCore};

enum EngineMsg {
    Inbound(Envelope),
    StatusOf(NodeId, Sender<Option<Status>>),
    Shutdown,
}

pub struct SwimNodeHandle {
    to_engine: Sender<EngineMsg>,
    join: Option<thread::JoinHandle<()>>,
}

impl SwimNodeHandle {
    pub fn status_of(&self, id: NodeId) -> Result<Option<Status>> {
        let (tx, rx) = mpsc::channel();
        self.to_engine
            .send(EngineMsg::StatusOf(id, tx))
            .map_err(|_| Error::InvalidValue {
                detail: "SWIM engine thread is gone",
            })?;
        rx.recv().map_err(|_| Error::InvalidValue {
            detail: "SWIM engine thread dropped the status response",
        })
    }

    pub fn shutdown(mut self) {
        let _ = self.to_engine.send(EngineMsg::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Start a SWIM node: binds `listen_addr` for inbound ping/ack traffic and
/// spawns the accept loop plus the engine thread. `peers` maps every
/// *other* node's id to its address.
pub fn spawn(
    id: NodeId,
    listen_addr: &str,
    peers: HashMap<NodeId, SocketAddr>,
    protocol_period: Duration,
    ping_timeout_ticks: u32,
    suspicion_timeout_ticks: u32,
) -> Result<SwimNodeHandle> {
    let listener = TcpListener::bind(listen_addr)?;
    let (to_engine, from_network) = mpsc::channel::<EngineMsg>();

    let accept_sender = to_engine.clone();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let sender = accept_sender.clone();
            thread::spawn(move || {
                let _ = handle_connection(stream, sender);
            });
        }
    });

    let peer_ids: Vec<NodeId> = peers.keys().copied().collect();
    // The protocol period counts in ticks the same way RaftCore does;
    // one tick per `protocol_period` here (ping_timeout/suspicion_timeout
    // are already expressed in that same tick unit by the caller).
    let core = SwimCore::new(id, peer_ids, 1, ping_timeout_ticks, suspicion_timeout_ticks);

    let join = thread::spawn(move || {
        run_engine(core, peers, from_network, protocol_period);
    });

    Ok(SwimNodeHandle {
        to_engine,
        join: Some(join),
    })
}

fn handle_connection(stream: TcpStream, sender: Sender<EngineMsg>) -> Result<()> {
    let mut reader = stream;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = reader.read(&mut chunk)?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        while let Some((envelope, consumed)) = wire::decode(&buf)? {
            let _ = sender.send(EngineMsg::Inbound(envelope));
            buf.drain(..consumed);
        }
    }
}

fn send_envelope(addr: SocketAddr, envelope: &Envelope) {
    // Best-effort, same reasoning as raft_net::send_envelope: a failed
    // send here is exactly the "ping didn't get through" signal SWIM's
    // failure detector is built to notice on its own, via the timeout
    // that already fires when no Ack arrives.
    if let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(200)) {
        let _ = stream.write_all(&wire::encode(envelope));
    }
}

fn run_engine(
    mut core: SwimCore,
    peers: HashMap<NodeId, SocketAddr>,
    inbound: Receiver<EngineMsg>,
    protocol_period: Duration,
) {
    loop {
        let outgoing = match inbound.recv_timeout(protocol_period) {
            Ok(EngineMsg::Inbound(envelope)) => core.receive(envelope),
            Ok(EngineMsg::StatusOf(id, respond)) => {
                let _ = respond.send(core.status_of(id));
                Vec::new()
            }
            Ok(EngineMsg::Shutdown) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => core.tick(),
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        };

        for envelope in outgoing {
            if let Some(&addr) = peers.get(&envelope.to) {
                send_envelope(addr, &envelope);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn free_port() -> u16 {
        TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    fn wait_for<F: Fn() -> bool>(condition: F, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if condition() {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn three_real_nodes_converge_on_everyone_alive() {
        let ids: Vec<NodeId> = vec![1, 2, 3];
        let ports: Vec<u16> = (0..3).map(|_| free_port()).collect();
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
                peers,
                Duration::from_millis(20),
                3,
                10,
            )
            .unwrap();
            handles.push((id, handle));
        }

        let converged = wait_for(
            || {
                ids.iter().all(|&observer| {
                    let handle = &handles.iter().find(|(id, _)| *id == observer).unwrap().1;
                    ids.iter().all(|&target| {
                        handle.status_of(target).ok().flatten() == Some(Status::Alive)
                    })
                })
            },
            Duration::from_secs(5),
        );
        assert!(
            converged,
            "all three real nodes should see each other Alive"
        );

        for (_, handle) in handles {
            handle.shutdown();
        }
    }

    #[test]
    fn an_unreachable_real_node_is_eventually_marked_dead() {
        let ids: Vec<NodeId> = vec![1, 2, 3];
        let ports: Vec<u16> = (0..3).map(|_| free_port()).collect();
        let addrs: HashMap<NodeId, SocketAddr> = ids
            .iter()
            .zip(&ports)
            .map(|(&id, &port)| (id, format!("127.0.0.1:{port}").parse().unwrap()))
            .collect();

        // Node 3 never actually starts listening — its "peer" address
        // points at a port nothing is bound to, simulating a node that's
        // down from the moment the cluster starts.
        let mut handles = Vec::new();
        for &id in &[1u32, 2] {
            let peers: HashMap<NodeId, SocketAddr> = addrs
                .iter()
                .filter(|(&pid, _)| pid != id)
                .map(|(&pid, &addr)| (pid, addr))
                .collect();
            let idx = ids.iter().position(|&x| x == id).unwrap();
            let handle = spawn(
                id,
                &format!("127.0.0.1:{}", ports[idx]),
                peers,
                Duration::from_millis(20),
                3,
                10,
            )
            .unwrap();
            handles.push((id, handle));
        }

        let node1 = &handles[0].1;
        let converged = wait_for(
            || node1.status_of(3).ok().flatten() == Some(Status::Dead),
            Duration::from_secs(5),
        );
        assert!(
            converged,
            "node 1 should mark the never-started node 3 as Dead"
        );

        for (_, handle) in handles {
            handle.shutdown();
        }
    }
}
