# Operations Guide

This is the deployment, security, and runbook companion to
`ARCHITECTURE.md` — that document explains *how the system is built*,
this one explains *how to run it*. Where a limitation matters
operationally, it's stated plainly here even when it's already noted in
`ARCHITECTURE.md`, rather than assuming an operator has read every line
of the design doc first.

## 1. Building

```sh
cargo build --release --workspace
```

Binaries land in `target/release/`:

| Binary | Crate | Purpose |
|---|---|---|
| `gems` | `gems-cli` | Administrative CLI: init a store, manage types/entities/policies, run queries |
| `gems-tui` | `gems-tui` | Interactive terminal entity browser |
| `gems-webui` | `gems-webui` | HTTP query browser and JSON API |
| `gems-mcp` | `gems-mcp` | MCP (Model Context Protocol) server over stdio |

There is **no standalone binary for a `gems-cluster` Raft/gossip node**.
`raft_net::spawn`/`swim_net::spawn` are library entry points a Rust
program calls, not something `cargo run` starts directly — see §4 below
and `ARCHITECTURE.md` §9's own note on this. Running a real cluster today
means writing a small Rust program that calls these functions with your
deployment's node ids, addresses, and shared secret.

## 2. What a store directory contains

Every `gems_engine::Store` lives in a directory containing:

| File | What it is |
|---|---|
| `primary.gemi` | Primary index: `Tuid -> SlotPointer` B*-tree |
| `ordinal.gemi` / `ordinal_rev.gemi` | Ordinal assignment B-trees (secondary-index bookkeeping) |
| `data.gemx` | Entity byte storage (extent files) |
| `.gems.lock` | Exclusive lock file — held for the process lifetime by whichever process opened the store writable; see §3.4 |

A `PrimaryStore` (used by cluster replication) additionally has:

| File | What it is |
|---|---|
| `replication.gemlog` | Append-only log of every mutation, tailed by replicas |

A Raft node (via `raft_net::spawn`) additionally has:

| File | What it is |
|---|---|
| `raft_state.bin` | Persisted `current_term`/`voted_for`/log — written via write-temp-file/fsync/atomic-rename; a `.tmp` file next to it mid-write is normal and should not exist once the process is idle |

None of these are meant to be edited by hand. All of them are portable
between hosts of the same OS/architecture family (no host-specific
absolute paths embedded), so a directory copy is a valid way to move or
back up a store — see §5.1.

## 3. Running the frontends

### 3.1 `gems` (CLI)

```sh
gems init <dir>
gems type create <dir> <name>
gems type list <dir>
gems entity create <dir> --type <type-name> --name <name> [key=value ...]
gems entity get <dir> <id-hex> [--as <subject-hex>[,<role-hex>,...]]
gems policy create <dir> --effect <permit|deny> [--target-kind <kind>] \
    [--target-type <type-name>] [--subject <id-hex>] [--role <id-hex>] \
    [--redact <field-name> ...]
gems query <dir> "<query string>" [--as <subject-hex>[,<role-hex>,...]]
```

Reads default to **raw, unenforced access** — this is an administrative
tool operating directly on a store's files (if you can run `gems` against
a store, you already have filesystem-level access to it, so ABAC is
opt-in via `--as`, not a barrier this tool enforces). Its own errors go
to stderr as plain text (`error: ...`); its normal output on stdout is
the tool's actual scripting interface, not diagnostic logging, and is
never gated behind a log level.

### 3.2 `gems-tui`

```sh
gems-tui <store_dir>
```

Read-only, no ABAC subject context — same "administrative access"
posture as the CLI's default. Opens the store read-only (no lock taken;
see §3.4), so it can run alongside a writer.

### 3.3 `gems-webui`

```sh
GEMS_WEBUI_SECRET=<your-secret> gems-webui [bind_addr] [--insecure]
```

| Variable / flag | Meaning | Default |
|---|---|---|
| `GEMS_WEBUI_SECRET` | HMAC secret used to verify `Authorization: Bearer <token>` on every request | **required** unless `--insecure` |
| `GEMS_WEBUI_READ_TIMEOUT_SECS` | Per-connection read timeout | `10` |
| `GEMS_LOG` | Log level: `error`/`warn`/`info`/`debug` | `info` |
| `bind_addr` (positional arg) | Address to listen on | `127.0.0.1:8080` |
| `--insecure` | Disable authentication entirely | off |

Refuses to start without `GEMS_WEBUI_SECRET` set, unless `--insecure` is
passed — there is deliberately no default secret (a shipped default
would give every deployment the same effective password). Handles
`SIGTERM`/`SIGINT` gracefully: stops accepting new connections within
~100ms and lets in-flight requests finish; see `ARCHITECTURE.md` §9.

Issuing a token (from any Rust code linking `gems-abac`, or via a small
throwaway program — there is no CLI subcommand for this yet, a real gap
noted in §6):

```rust
let subject = gems_abac::SubjectContext {
    subject_id: /* the acting subject's Tuid */,
    roles: vec![/* role Tuids */],
};
let token = gems_abac::token::issue(secret.as_bytes(), &subject, /* expires_at_unix_secs */ None);
```

Send it as `Authorization: Bearer <token>` on every request (the WebUI's
own page has a token field that does this for you via `fetch`).

### 3.4 `gems-mcp`

```sh
GEMS_MCP_SECRET=<your-secret> gems-mcp [--insecure]
```

Same secret/`--insecure`/`GEMS_LOG` posture as the WebUI, but the secret
verifies each tool call's `auth_token` argument rather than a header
(MCP has no headers — it's JSON-RPC over stdio). No special shutdown
handling needed: it holds no store lock between calls (every tool call
opens its `Store` read-only) and already exits cleanly on stdin EOF.

### 3.5 Store locking — what it means for you operationally

`Store::create`/`Store::open(_, writable: true)` take an exclusive lock
on the store directory (`.gems.lock`) for the life of the process. A
second process trying to open the same store writable at the same time
gets a clear `AlreadyLocked` error instead of silently corrupting data.
Practical consequences:

- **Never run two writers against the same store directory** — `gems
  entity create` twice concurrently against the same dir, or a `gems`
  command against a store a Raft node is actively serving, will have one
  of them fail fast with `AlreadyLocked` rather than race.
- **Read-only opens are unaffected** — `gems-tui`, `gems-webui`, and
  `gems query`/`gems entity get` (without a concurrent write happening
  through the same tool) never take this lock, so they coexist freely
  with a running writer.
- If you see `AlreadyLocked` unexpectedly, check for a leftover process
  still holding the store open (a crashed shell job, an orphaned
  background process) before assuming corruption — the lock is doing its
  job.

## 4. Cluster deployment (Raft + gossip)

There is no packaged deployment story for this yet — see §1's note.
Concretely, standing up a cluster today means writing a small program
(or, per §6, building the still-missing standalone binary) that:

1. Assigns each node a stable `NodeId` (a `u32`) and reads a **shared
   secret** from its own configuration (an env var you define, a secrets
   file — `gems-cluster` doesn't read one itself) — **the same secret
   value on every node**, passed as `Arc<Vec<u8>>` into both
   `raft_net::spawn` and `swim_net::spawn`. A node with the wrong secret
   can't be tricked into joining: its frames simply fail HMAC
   verification and get dropped.
2. Calls `raft_net::spawn(id, listen_addr, client_listen_addr, peers, store_dir, timing, secret)` once per shard-group member, where `listen_addr` is this node's own peer-RPC address, `client_listen_addr` is its separate client-proposal port, and `peers` maps every *other* member's `NodeId` to its peer address.
3. Separately, if you want membership/failure-detection, calls
   `swim_net::spawn(id, listen_addr, peers, protocol_period, ping_timeout_ticks, suspicion_timeout_ticks, secret)` — independent of the Raft group; there is no shard-map-over-gossip integration yet (`ARCHITECTURE.md` §6 notes this).
4. On restart (planned or after a crash), calling `raft_net::spawn` again
   with the **same `store_dir`** automatically restores the node's
   persisted term/vote/log (§2's `raft_state.bin`) — you do not need to
   do anything special to "resume" a node; just start it the same way.

`RaftNodeHandle::status()` returns `(Role, Term)` and
`SwimNodeHandle::status_of(id)` returns a peer's last-known `Status` —
these are your only programmatic health signals today (see §7 on
monitoring).

## 5. Runbook

### 5.1 Backup and restore

**Cold backup** (the only supported method today — there is no
hot/online backup API):

1. Stop whatever process holds the store open writable (a clean shutdown
   is enough; `Store`'s own CoW+fsync discipline means there is no
   in-memory state you need to explicitly flush first).
2. Copy the entire store directory (see §2's file list) to your backup
   location — a plain `tar`/`cp -r`/`rsync` is sufficient; there is no
   special backup tool or format.
3. To restore, copy the directory back and start the process against it
   normally.

**While a store is open read-only** (e.g. `gems-tui` or `gems-webui`
pointed at it), a filesystem-level snapshot (LVM, ZFS, a cloud disk
snapshot) taken at a consistent point is safe to use as a backup source,
since reads never take the lock and the underlying files are only ever
mutated by the one writer holding `.gems.lock`. Don't copy files out of a
directory a writer currently holds open with a plain `cp`/`tar` (no
snapshot) — you risk copying a torn, in-progress write; use a real
snapshot or stop the writer first.

### 5.2 Cluster bootstrap (fresh cluster)

1. Decide the shard's member `NodeId`s and addresses; generate a shared
   secret (e.g. `openssl rand -base64 32`) and distribute it to every
   node's configuration out of band (not over this cluster's own network
   — there's no bootstrap handshake to exchange it safely).
2. Start every member's `raft_net::spawn` roughly together (order doesn't
   matter — a node with no persisted state starts at term 0 as a
   follower and will time out into an election if it doesn't hear from a
   leader).
3. Confirm a leader was elected: poll `status()` on each `RaftNodeHandle`
   until exactly one reports `Role::Leader`.
4. Propose through `RaftNodeHandle::propose` (in-process) or
   `raft_net::propose_remote`/`shard::ShardedClient` (from outside the
   cluster process) — see `ARCHITECTURE.md` §6.

### 5.3 Adding/replacing a cluster node

There is no dynamic membership-change support (`ARCHITECTURE.md` §6
names this as a known scope cut — the peer set is fixed at each node's
own construction). Practically, changing membership today means:

1. Stop every node in the shard.
2. Restart all of them with the updated `peers` map (including the
   new/removed member) passed to `raft_net::spawn`.
3. A brand-new node's `store_dir` starts empty (term 0, empty log) — it
   will catch up via normal `AppendEntries` replication from whichever
   node becomes leader, the same way any follower catches up after being
   behind.

This is a full-shard restart, not a rolling one — plan for a brief
unavailability window. A true rolling add/remove needs the membership-
change protocol from the Raft paper (§6 of it), which isn't implemented.

### 5.4 Rolling upgrade of a running cluster

1. Stop one follower (never the current leader first — check `status()`).
2. Replace its binary/deployment artifact.
3. Restart it against the same `store_dir` — it resumes from its
   persisted state (§2, §4.4) and catches up on anything it missed.
4. Confirm it's caught up (compare its status/log length against the
   leader's) before moving to the next node.
5. Do the leader last; stepping it down (stopping it) triggers a normal
   election among the already-upgraded followers.

There is no on-disk format version negotiation — all nodes in a shard
should run the same `gems-cluster` version. `gems-engine`'s own on-disk
formats (index page size, GBV layout) are fixed at store-creation time
(`ARCHITECTURE.md` §0/§2.3); there is no live migration tool for changing
them, so a format change requires a new store and a data migration you
write yourself (dump via `query`, reload into a fresh store).

### 5.5 Rotating secrets

There is no key-rotation support — each of `GEMS_WEBUI_SECRET`,
`GEMS_MCP_SECRET`, and a cluster's shared HMAC secret is a single static
value with no overlap/grace period for two valid secrets at once.
Rotating one means:

- **WebUI/MCP secret**: issue new tokens signed with the new secret,
  restart the service with the new `GEMS_*_SECRET` value. Every
  previously issued token becomes invalid the instant the service
  restarts (verification is exact-match against the current secret) —
  there is no way to honor old and new tokens simultaneously during a
  rollout. Plan for a brief window where in-flight old tokens fail.
- **Cluster peer secret**: requires the full-shard restart from §5.3
  (every node restarted with the new secret at once) — a node running
  the old secret and a node running the new one cannot authenticate each
  other's frames at all, so there is no rolling option here either.

### 5.6 Diagnosing an `AlreadyLocked` error

See §3.5. In order: (1) confirm no other intended process is running
against the same store directory; (2) check for a leftover/orphaned
process (`ps` for the binary, or check for the store directory's path in
open-file listings, e.g. `lsof <store_dir>/.gems.lock` on Linux); (3) if
you're certain nothing legitimate holds it (e.g. the whole host was
power-cycled and the lock file is stale), the lock releases automatically
once no process holds the fd — a stale `.gems.lock` *file* left over from
before a hard crash does **not** by itself block a new open (flock
locks don't survive the process that held them), so if you're still
seeing `AlreadyLocked` after confirming no process holds it, that's worth
treating as a bug report, not a manual fix (deleting the lock file is
never necessary or recommended).

## 6. Security model summary

(See `ARCHITECTURE.md` §8 for the full ABAC design and §6 for cluster
peer authentication; this is the operator-facing summary.)

- **Default posture is deny-by-default, auth-required-by-default.**
  `gems-webui`/`gems-mcp` refuse to start without an explicit secret
  configured; ABAC itself is default-deny (an entity is visible only
  because some `Policy` explicitly permits it).
- **Trust boundary**: `gems-cli`/`gems-tui` assume whoever can run them
  already has filesystem access to the store and are therefore
  administrative tools, not access-controlled clients — don't expose
  either of them to an untrusted user via, say, a shared shell account
  without OS-level access controls doing the actual enforcement.
- **Transport is not encrypted.** HMAC tags (cluster peer traffic) and
  JWT signatures (WebUI/MCP tokens) give integrity and authenticity —
  tampering and impersonation are detected — but none of `raft::wire`,
  `gossip::wire`, or the WebUI's plain HTTP carry any confidentiality.
  Anyone who can observe the network can read cluster traffic and
  request/response bodies (including tokens in flight, unless a
  reverse proxy adds TLS in front of `gems-webui`). Run cluster and
  WebUI/MCP traffic over a network you already trust for confidentiality
  (a private VPC, a WireGuard/VPN overlay), or put a TLS-terminating
  reverse proxy in front of `gems-webui` — this codebase does not
  implement TLS itself.
- **No rate limiting** on any frontend beyond the frame-size/header-count
  caps from `ARCHITECTURE.md`'s hardening notes (those bound memory per
  connection, not request rate). A determined client can still make many
  requests quickly; put a reverse proxy or firewall in front if that's a
  concern for your deployment.
- **`propose_remote`'s client port is unauthenticated** (`ARCHITECTURE.md`
  §6) — anyone who can reach a Raft node's client port can submit
  proposals. Firewall that port to trusted callers only until this gets
  its own auth story.

## 7. Monitoring

There is no metrics endpoint (no Prometheus `/metrics`, no
structured-metrics export) — a real gap, not implemented. What exists
today:

- **Logs**: `gems-webui`/`gems-mcp` emit leveled lines to stderr
  (`$GEMS_LOG`); aggregate these with whatever your deployment already
  uses for process stderr (systemd journal, a container log driver,
  etc.).
- **Cluster health, polled programmatically**: `RaftNodeHandle::status()`
  → `(Role, Term)` per node; `SwimNodeHandle::status_of(id)` → a peer's
  last-known `Status` (`Alive`/`Suspect`/`Dead`). There is no built-in
  poller/exporter for these — wire them into your own monitoring by
  calling them periodically from whatever program holds the handles.
- **Process-level**: standard OS process supervision (systemd, a
  container orchestrator's liveness probe) is your restart-on-crash
  mechanism — there is no self-healing or automatic failover beyond
  what Raft/SWIM themselves provide at the consensus/membership layer.
