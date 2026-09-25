//! `gems-cluster-node`: the standalone deployable binary `OPERATIONS.md`
//! §1/§4 names as a real gap — until now, `raft_net::spawn`/
//! `swim_net::spawn` were library entry points a Rust program called, not
//! something `cargo run`/a container could start directly. This wraps
//! `raft_net::spawn` (gossip/SWIM membership is intentionally not wired in
//! here — see `ARCHITECTURE.md` §6's note that shard-map-over-gossip
//! integration is separate, unbuilt work; a Raft node alone is everything
//! a shard's consensus group needs).
//!
//! Three subcommands:
//!
//! - `gems-cluster-node serve` — runs one Raft node forever (until
//!   `SIGTERM`/`SIGINT`), configured entirely from environment variables
//!   (a container's natural configuration surface). See `ServeConfig`'s
//!   doc for the exact variables.
//! - `gems-cluster-node propose --name <name> [--status <value>]` — a
//!   thin `ShardedClient` wrapper: builds one `Data` entity and proposes
//!   it against whichever shard its (randomly generated) id routes to,
//!   printing the assigned id/shard/log index. Exists so a deployment can
//!   actually put data into the cluster to test with, without writing a
//!   throwaway Rust program to call `ShardedClient` directly.
//! - `gems-cluster-node install [--admin-name <name>]` — one-time cluster
//!   bootstrap: seeds the starter roles/policies/admin subject
//!   `gems_catalog::bootstrap` defines and prints an admin bearer token.
//!   Run once, after the cluster's Raft nodes are up. See `run_install`'s
//!   doc.
//!
//! Both subcommands read the same `GEMS_SHARD_MAP` JSON — the shape is
//! deliberately array-of-objects, not a JSON object keyed by shard/node
//! id, so it round-trips through `gems_json::Value`'s public API (which
//! has no dynamic-object-key iteration, only array iteration plus
//! known-key lookups):
//!
//! ```json
//! {"shards":[
//!   {"shard":0,"members":[
//!     {"node":1,"peer":"shard0-node1:7000","client":"shard0-node1:7001"},
//!     {"node":2,"peer":"shard0-node2:7000","client":"shard0-node2:7001"}
//!   ]},
//!   {"shard":1,"members":[
//!     {"node":11,"peer":"shard1-node1:7000","client":"shard1-node1:7001"}
//!   ]}
//! ]}
//! ```
//!
//! `peer` is the address other Raft group members use to reach this node
//! (`raft::wire` traffic); `client` is the separate port
//! `raft_net::propose_remote`/`ShardedClient` use. Both are `host:port`
//! strings resolved via the system resolver (so Docker Compose service
//! names work directly) — `serve` only needs its own shard's entry (to
//! find its own bind port and its peers' addresses); `propose` needs
//! every shard's `client` addresses (to route to whichever shard an
//! entity's id lands in).

use std::collections::HashMap;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use gems_abac::SubjectContext;
use gems_catalog::{EntityFlags, EntityHeader, EntityKind};
use gems_cluster::raft::NodeId;
use gems_cluster::raft_net::{self, RaftTiming};
use gems_cluster::shard::{ShardId, ShardMap, ShardRouter, ShardedClient};
use gems_cluster::LogRecord;
use gems_codec::{GbvBuilder, TypeTag};
use gems_common::{log_error, log_info, log_warn, Tuid};
use gems_json::Value;

const LOG_TARGET: &str = "gems-cluster-node";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("serve") => run_serve(),
        Some("propose") => run_propose(&args[1..]),
        Some("install") => run_install(&args[1..]),
        _ => {
            eprintln!(
                "usage:\n  gems-cluster-node serve\n  gems-cluster-node propose --name <name> [--status <value>]\n  gems-cluster-node install [--admin-name <name>]"
            );
            std::process::exit(2);
        }
    };
    if let Err(e) = result {
        log_error!(LOG_TARGET, "{e}");
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------------
// Shard map parsing, shared by both subcommands.
// ---------------------------------------------------------------------

struct Member {
    node: NodeId,
    peer: String,
    client: String,
}

fn parse_shard_map(json: &str) -> Result<HashMap<ShardId, Vec<Member>>, String> {
    let root = gems_json::parse(json).map_err(|e| format!("GEMS_SHARD_MAP: {e}"))?;
    let shards = root
        .get("shards")
        .and_then(Value::as_array)
        .ok_or("GEMS_SHARD_MAP: missing top-level \"shards\" array")?;

    let mut map = HashMap::new();
    for entry in shards {
        let shard_id = entry
            .get("shard")
            .and_then(Value::as_u64)
            .ok_or("GEMS_SHARD_MAP: a shard entry is missing an integer \"shard\" id")?
            as ShardId;
        let members = entry
            .get("members")
            .and_then(Value::as_array)
            .ok_or("GEMS_SHARD_MAP: a shard entry is missing a \"members\" array")?;

        let mut parsed_members = Vec::new();
        for member in members {
            let node = member
                .get("node")
                .and_then(Value::as_u64)
                .ok_or("GEMS_SHARD_MAP: a member is missing an integer \"node\" id")?
                as NodeId;
            let peer = member
                .get("peer")
                .and_then(Value::as_str)
                .ok_or("GEMS_SHARD_MAP: a member is missing its \"peer\" address")?
                .to_string();
            let client = member
                .get("client")
                .and_then(Value::as_str)
                .ok_or("GEMS_SHARD_MAP: a member is missing its \"client\" address")?
                .to_string();
            parsed_members.push(Member { node, peer, client });
        }
        map.insert(shard_id, parsed_members);
    }
    Ok(map)
}

/// Resolves a `host:port` string via the system resolver, retrying for up
/// to `deadline` — a Docker Compose peer's hostname may not be resolvable
/// yet in the first moments after `docker compose up` starts every
/// service roughly in parallel, so a node must not give up on its first
/// failed lookup.
fn resolve_with_retry(host_port: &str, deadline: Duration) -> Result<SocketAddr, String> {
    let start = std::time::Instant::now();
    loop {
        if let Ok(mut addrs) = host_port.to_socket_addrs() {
            if let Some(addr) = addrs.next() {
                return Ok(addr);
            }
        }
        if start.elapsed() > deadline {
            return Err(format!("could not resolve {host_port} within {deadline:?}"));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn env_var(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|_| format!("${name} must be set"))
}

fn env_var_parsed<T: std::str::FromStr>(name: &str, default: T) -> Result<T, String> {
    match std::env::var(name) {
        Ok(raw) => raw
            .parse()
            .map_err(|_| format!("${name} is set but not a valid value: {raw:?}")),
        Err(_) => Ok(default),
    }
}

fn port_of(host_port: &str) -> Result<u16, String> {
    host_port
        .rsplit_once(':')
        .and_then(|(_, port)| port.parse().ok())
        .ok_or_else(|| format!("not a valid host:port string: {host_port:?}"))
}

// ---------------------------------------------------------------------
// `serve`
// ---------------------------------------------------------------------

/// Environment variables `serve` reads. All but the timing knobs are
/// required; a missing one is a startup error, not a silently-assumed
/// default — a misconfigured cluster node should refuse to start, not
/// come up in some unintended shape.
///
/// | Variable | Meaning |
/// |---|---|
/// | `GEMS_NODE_SHARD` | Which shard (an index into `GEMS_SHARD_MAP`'s `shards` array) this node belongs to |
/// | `GEMS_NODE_ID` | This node's id within that shard (must match one of its `members`) |
/// | `GEMS_SHARD_MAP` | The JSON shape documented in this file's module doc |
/// | `GEMS_STORE_DIR` | Where this node's `gems_engine::Store` lives |
/// | `GEMS_CLUSTER_SECRET` | HMAC secret shared by every node in the cluster (see `ARCHITECTURE.md` §6) |
/// | `GEMS_TICK_MS` (optional, default `200`) | Milliseconds per Raft tick |
/// | `GEMS_HEARTBEAT_TICKS` (optional, default `3`) | Leader heartbeat interval, in ticks |
/// | `GEMS_ELECTION_MIN_TICKS` / `GEMS_ELECTION_MAX_TICKS` (optional, default `8`/`15`) | Election timeout range, in ticks |
fn run_serve() -> Result<(), String> {
    gems_common::shutdown::install_handler();

    let shard_id: ShardId = env_var_parsed("GEMS_NODE_SHARD", 0)?;
    let node_id: NodeId = env_var("GEMS_NODE_ID")?
        .parse()
        .map_err(|_| "GEMS_NODE_ID must be a valid node id (u32)".to_string())?;
    let shard_map = parse_shard_map(&env_var("GEMS_SHARD_MAP")?)?;
    let store_dir = env_var("GEMS_STORE_DIR")?;
    let secret = env_var("GEMS_CLUSTER_SECRET")?;
    if secret.is_empty() {
        return Err("$GEMS_CLUSTER_SECRET must not be empty".to_string());
    }
    let tick_ms: u64 = env_var_parsed("GEMS_TICK_MS", 200)?;
    let heartbeat_ticks: u32 = env_var_parsed("GEMS_HEARTBEAT_TICKS", 3)?;
    let election_min: u32 = env_var_parsed("GEMS_ELECTION_MIN_TICKS", 8)?;
    let election_max: u32 = env_var_parsed("GEMS_ELECTION_MAX_TICKS", 15)?;

    let members = shard_map
        .get(&shard_id)
        .ok_or_else(|| format!("GEMS_SHARD_MAP has no entry for shard {shard_id}"))?;
    let self_member = members
        .iter()
        .find(|m| m.node == node_id)
        .ok_or_else(|| format!("shard {shard_id} has no member with node id {node_id}"))?;
    let peer_port = port_of(&self_member.peer)?;
    let client_port = port_of(&self_member.client)?;

    log_info!(
        LOG_TARGET,
        "resolving {} peer(s) for shard {shard_id}...",
        members.len() - 1
    );
    let mut peers: HashMap<NodeId, SocketAddr> = HashMap::new();
    for member in members {
        if member.node == node_id {
            continue;
        }
        let addr = resolve_with_retry(&member.peer, Duration::from_secs(60))?;
        peers.insert(member.node, addr);
    }
    log_info!(
        LOG_TARGET,
        "node {node_id} (shard {shard_id}): binding peer port {peer_port}, client port {client_port}, {} peer(s) resolved",
        peers.len()
    );

    let handle = raft_net::spawn(
        node_id,
        &format!("0.0.0.0:{peer_port}"),
        &format!("0.0.0.0:{client_port}"),
        peers,
        std::path::Path::new(&store_dir),
        RaftTiming {
            tick_interval: Duration::from_millis(tick_ms),
            heartbeat_interval_ticks: heartbeat_ticks,
            election_timeout_ticks_range: (election_min, election_max),
        },
        Arc::new(secret.into_bytes()),
    )
    .map_err(|e| format!("failed to start the Raft node: {e}"))?;

    log_info!(LOG_TARGET, "node {node_id} (shard {shard_id}) is running");

    // Checked every 200ms so SIGTERM is noticed promptly (a container
    // orchestrator's stop grace period is a real budget to stay well
    // under), but status is only logged on an actual change, so this
    // doesn't spam the log at 5x/sec.
    const POLL_INTERVAL: Duration = Duration::from_millis(200);
    const STATUS_LOG_INTERVAL: Duration = Duration::from_secs(1);
    let mut last_logged = None;
    let mut last_status_check = std::time::Instant::now() - STATUS_LOG_INTERVAL;
    loop {
        if gems_common::shutdown::shutdown_requested() {
            log_info!(LOG_TARGET, "shutdown signal received, stopping");
            handle.shutdown();
            return Ok(());
        }
        if last_status_check.elapsed() >= STATUS_LOG_INTERVAL {
            last_status_check = std::time::Instant::now();
            if let Ok(status) = handle.status() {
                if last_logged != Some(status) {
                    log_info!(LOG_TARGET, "status: {:?}, term {}", status.0, status.1);
                    last_logged = Some(status);
                }
            }
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

// ---------------------------------------------------------------------
// `propose`
// ---------------------------------------------------------------------

/// Builds a `ShardedClient` from `$GEMS_SHARD_MAP`, resolving every shard's
/// member client addresses — the setup shared by `propose` and `install`,
/// the two subcommands that submit writes to an already-running cluster
/// rather than running a Raft node themselves.
fn build_sharded_client() -> Result<ShardedClient, String> {
    let shard_map = parse_shard_map(&env_var("GEMS_SHARD_MAP")?)?;
    let num_shards = shard_map.len() as u32;
    if num_shards == 0 {
        return Err("GEMS_SHARD_MAP has no shards".to_string());
    }

    let mut map = ShardMap::new();
    for (&shard_id, members) in &shard_map {
        for member in members {
            let addr = resolve_with_retry(&member.client, Duration::from_secs(30))?;
            map.add_member(shard_id, member.node, addr);
        }
    }
    Ok(ShardedClient::new(
        ShardRouter::new(num_shards),
        map,
        Duration::from_secs(5),
    ))
}

fn run_propose(args: &[String]) -> Result<(), String> {
    let mut name = None;
    let mut status_field = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--name" => {
                name = args.get(i + 1).cloned();
                i += 2;
            }
            "--status" => {
                status_field = args.get(i + 1).cloned();
                i += 2;
            }
            other => return Err(format!("propose: unknown argument {other:?}")),
        }
    }
    let name = name.ok_or("propose: --name <name> is required")?;

    let client = build_sharded_client()?;

    let id = Tuid::generate();
    let shard = client.shard_of(&id);

    let mut body = GbvBuilder::new();
    if let Some(status) = &status_field {
        body.push(1, TypeTag::Str, status.as_bytes());
    }
    let header = EntityHeader {
        id,
        created_by: [0u8; 16],
        modified_by: [0u8; 16],
        modified_at_ns: 0,
        name: name.clone(),
        description: String::new(),
        flags: EntityFlags::NONE,
        entity_kind: EntityKind::Data,
        schema_ref: Tuid::NIL,
        body_offset: 0,
        body_len: 0,
    };

    log_info!(
        LOG_TARGET,
        "proposing {name:?} (id {}) to shard {shard}...",
        id.to_hex_string()
    );
    let index = client
        .propose(
            &id,
            LogRecord::Insert {
                header,
                body: body.finish(),
            },
        )
        .map_err(|e| format!("propose failed: {e}"))?;

    println!("id:    {}", id.to_hex_string());
    println!("shard: {shard}");
    println!("index: {index}");
    log_warn!(
        LOG_TARGET,
        "note: the log index above is when the leader accepted the proposal, not proof of \
         replication to a majority yet — query the entity back to confirm it committed."
    );
    Ok(())
}

// ---------------------------------------------------------------------
// `install`
// ---------------------------------------------------------------------

/// One-time cluster bootstrap: seeds the `admin`/`analyst`/`consumer`
/// `Role` entities, a starter policy set, and one admin `Subject` (see
/// `gems_catalog::bootstrap`'s doc for exactly what that seeds and why),
/// then issues that subject a never-expiring admin bearer token signed
/// with `$GEMS_WEBUI_SECRET` — the same secret `gems-webui` verifies
/// tokens against, so the printed token can be pasted straight into the
/// webui's login screen. Run this once, after the cluster's Raft nodes are
/// up (it proposes through the same `ShardedClient` path `propose` uses,
/// so it needs a leader elected in every shard, not a store to write to
/// directly — see `gems_catalog::bootstrap`'s doc for why installs never
/// bypass the cluster's single-writer Raft log).
fn run_install(args: &[String]) -> Result<(), String> {
    let mut admin_name = "admin".to_string();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--admin-name" => {
                admin_name = args
                    .get(i + 1)
                    .cloned()
                    .ok_or("--admin-name needs a value")?;
                i += 2;
            }
            other => return Err(format!("install: unknown argument {other:?}")),
        }
    }

    let webui_secret = env_var("GEMS_WEBUI_SECRET")?;
    if webui_secret.is_empty() {
        return Err("$GEMS_WEBUI_SECRET must not be empty".to_string());
    }
    let client = build_sharded_client()?;
    let seed = gems_catalog::bootstrap::bootstrap(&admin_name);

    log_info!(
        LOG_TARGET,
        "proposing {} seed entities (roles, policies, admin subject)...",
        seed.entities.len()
    );
    for entity in seed.entities {
        let id = entity.header.id;
        client
            .propose(
                &id,
                LogRecord::Insert {
                    header: entity.header,
                    body: entity.body,
                },
            )
            .map_err(|e| format!("install: failed to propose a seed entity: {e}"))?;
    }

    let token = gems_abac::token::issue(
        webui_secret.as_bytes(),
        &SubjectContext {
            subject_id: seed.admin_subject_id,
            roles: vec![seed.admin_role_id],
        },
        None,
    );

    println!(
        "admin_subject_id:   {}",
        seed.admin_subject_id.to_hex_string()
    );
    println!("admin_role_id:      {}", seed.admin_role_id.to_hex_string());
    println!(
        "analyst_role_id:    {}",
        seed.analyst_role_id.to_hex_string()
    );
    println!(
        "consumer_role_id:   {}",
        seed.consumer_role_id.to_hex_string()
    );
    println!("admin_token:        {token}");
    log_warn!(
        LOG_TARGET,
        "the printed token grants full admin access and never expires — store it like any \
         other credential, not in shell history or a committed file."
    );
    Ok(())
}
