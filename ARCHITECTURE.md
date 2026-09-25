# gems — entity storage database

Design notes for a from-scratch, dependency-averse entity store: mmap'd extent
files, a B*-tree primary index, roaring-bitmap secondary indexes, ABAC, and a
Raft+gossip cluster. This document is the reference for the crate layout in
this repo and the rationale behind each choice. Where "avoid 3rd-party"
collides with something you really don't want to hand-roll (crypto), that
tradeoff is called out explicitly.

## 0. Guiding constraints

- **No dependency graph.** We link `rustix` (thin syscall wrapper, no libc
  dependency chain) and essentially nothing else. Anything else we want
  (UUIDs, msgpack-ish encoding, roaring bitmaps, a B-tree, JSON for MCP, an
  HTTP/1.1 server, a TUI renderer) gets **vendored**: copy the relevant
  algorithm/source into `vendor/` or reimplement the subset we need, with a
  comment crediting the origin and license. This keeps the supply chain to
  "us + rustix" and lets us tune every format to our on-disk layout instead
  of fighting someone else's serialization model.
- **One exception worth taking on purpose: cryptography.** Password hashing
  (Argon2id) and TOTP (HMAC-SHA1/256) are the one place where "just
  reimplement it" is a real security risk (timing side channels, subtle spec
  bugs). Vendor a small, well-known reference implementation (e.g. the
  RustCrypto `sha2`/`hmac`/`argon2` crates' source, pulled in as vendored
  files, not as a Cargo dependency) rather than writing your own from the
  spec. Everything else in this design is fair game to hand-roll.
- **Linux + macOS/ARM.** Page size is *not* a constant (4 KiB on Linux
  x86_64, 16 KiB on Apple Silicon). Query it at runtime via
  `rustix::param::page_size()`. Decouple the *logical* B-tree page size
  (fixed at file-creation time, stored in the file header) from the *OS mmap
  granularity* (only relevant to how you round `mmap()` offsets/lengths) —
  never assume they're equal, or files won't be portable between the two
  platforms.

## 1. Storage layer: extent files over mmap

### 1.1 File access

All I/O goes through `rustix`: `rustix::fs::{open, ftruncate, fstat, fdatasync}`
and `rustix::mm::{mmap, munmap, msync, mprotect}`. No `std::fs` (its `File`
is fine for opening, but growth/mmap should be explicit). Writers mmap
`PROT_READ | PROT_WRITE`; read replicas / secondary readers can map
`PROT_READ` only. Every mutation to a mapped page is followed by `msync`
(or batched + `fdatasync` on the fd) before the mutation is considered
durable — the primary index and header pages need write-ahead framing (see
§1.4) so a crash mid-`msync` can't leave a torn page.

### 1.2 Extents and the 16 MiB / 4 KiB relationship

A data file is a sequence of fixed-size **extents**. Every extent is
dedicated to **one block-size class** — this is a segregated free-list
allocator (a slab allocator, not a buddy allocator), not a variable-length
heap:

```
extent
┌────────────┬──────────────┬────────────────────────────────────────┐
│ 16 B header │ 4 KiB bitmap │  16 MiB of data slots, all the same size │
└────────────┴──────────────┴────────────────────────────────────────┘
```

Block-size classes are powers of two from **512 B to 16 MiB**: 512, 1K, 2K,
4K, 8K, ..., up to 16M (a class of exactly 16 MiB means "one slot per
extent", used for oversized entities that spill into their own extent chain).

The elegant bit: a 4 KiB bitmap is **exactly** 32768 bits, and
16 MiB / 512 B = 32768. So the smallest block class fills the bitmap
perfectly; every larger class uses only the leading `16MiB/blocksize` bits
of the same 4 KiB bitmap and leaves the rest zeroed/unused. One bitmap
format serves every class without variable-length headers. **This only
holds if the header lives in its own space ahead of the bitmap** — an
earlier draft of this design (and its first implementation) carved the
16-byte header out of the same 4 KiB page as the bitmap, which quietly
broke the fit on both sides at once: 16 bytes less bitmap capacity than
the smallest class needs, computed against a data region that itself
was wrongly assumed to be the full 16 MiB extent minus that same page.
`gems-storage`'s test suite caught it (an out-of-bounds bitmap access
once an extent actually filled up); the fix is the header/bitmap/data
layout shown above — a 16-byte header, then a full untouched 4 KiB
bitmap, then a genuine 16 MiB data region, making the extent's total
on-disk footprint 16 MiB + 4 KiB + 16 B rather than an exact 16 MiB.

Extent header (16 bytes, immediately followed by the bitmap):
- `magic: u32`, `format_version: u16`
- `block_size_class: u8` (log2(block_size) - 9, so 0..=15 covers 512B..16MiB)
- `slot_count: u32`, `free_count: u32` (cached, recomputed on scrub if in doubt)

Bitmap (4 KiB, immediately following the header): 1 bit per slot, 1 =
allocated. Only the leading `slot_count` bits are meaningful for classes
above the smallest; the rest stay zeroed.

A data file's own header (offset 0, one OS page) records: file magic/version,
extent size (fixed 16 MiB), page size the B-tree in this file was built with,
and a free-extent-class summary (small array of "first extent with a free
slot" per class, so allocation doesn't have to scan) — essentially a slab
allocator's per-size-class freelist head, persisted.

Growing a file = `ftruncate` to `old_len + 16MiB`, format the new extent's
bitmap for whatever class is needed, `mmap` (or extend the existing mapping)
and go. Slot allocation = find a free bit in the size class's active extent
(cached hint), flip it, write. Freeing = clear the bit; no compaction needed
since slots are fixed-size within an extent.

Oversized values (bigger than 16 MiB) chain multiple whole-extent slots
together via a small continuation header in the first slot — rare path, keep
it simple (linked list of extent-slot pointers), don't over-engineer this.

### 1.3 Slot address / pointer format

A pointer to a stored record is `(file_id: u32, extent_index: u32,
slot_index: u16, block_class: u8)` — 11 bytes, pad to 12. This is what the
primary and secondary indexes store as their "value". `file_id` indexes into
a small, cluster-wide table of open extent files (data is sharded across
multiple files per §6, plus files roll over at some size to keep individual
mmaps manageable).

### 1.4 Crash safety

Keep this simple rather than building a general WAL for v1:
- Extent bitmap flips and header writes are single machine words (`u64`)
  wherever possible → atomic on both target platforms, so a torn write
  during a crash can't corrupt the bitmap itself (worst case: a slot is
  marked used but never gets its payload — a background scrubber can find
  "allocated but header magic missing" slots and reclaim them).
- The B*-tree (§2) uses copy-on-write page allocation + a single atomic
  "root pointer" swap (see below) so a crash never leaves a half-updated
  tree visible — this is the same trick LMDB uses, and it's the reason we
  don't need a separate WAL for index consistency.
- A per-file monotonic `commit_sequence` in the header, bumped after the
  root-pointer swap and `msync`'d, gives replicas/CLI tools a cheap way to
  detect "this file is mid-write, retry".

## 2. Primary index: B*-tree keyed by TUID

### 2.1 Entity IDs

"UUID with created-at timestamp at the end" → a 24-byte **TUID**
(Timestamped UID): `[16 bytes random UUIDv4][8 bytes: created_at, ns since
epoch, big-endian]`. Two properties fall out of this for free:
- The UUID half stays a normal, opaque, globally-unique 128-bit ID for
  external references (links, `created_by`, API responses can still print
  just the 16-byte UUID when the timestamp isn't needed).
- Comparing the full 24 bytes gives you creation order without a separate
  index — useful for the B*-tree's insert pattern (see below) and for
  "recently created" scans.

### 2.2 Why B\* and not plain B+

A B*-tree keeps internal (and here, leaf) nodes at ~2/3 full minimum instead
of ~1/2, by redistributing across two sibling nodes before splitting into
three, instead of splitting one node into two at 1/2 full. For a primary
index whose keys are largely insert-order-random (UUID half) but
time-clustered (timestamp half), this matters: pure-random UUID keys would
otherwise fragment a plain B+-tree down to ~50% fill under heavy insert
churn. Implement it as: leaf nodes hold `(TUID, slot_pointer)` pairs sorted
by key; internal nodes hold separator keys + child page pointers; on
overflow, try to shift into a sibling first, only 2-way-split-into-3 when
both siblings are also full.

### 2.3 On-disk page layout

- Page size fixed per-file at creation (recommend: match the *smaller* of
  the two platforms' native page size, 4 KiB, so both platforms mmap it
  without straddling concerns — see §0 on decoupling logical vs OS page
  size).
- Copy-on-write: a page is never mutated in place once published. An
  update allocates a fresh page (bump allocator + freelist over the index
  file's own logical pages — the `gems-index` crate's `Pager`, distinct
  from the §1 data-extent allocator since index pages are much smaller and
  fixed-size) for every node along the root-to-leaf path, writes the new
  version, and the change propagates up to a new root. Publishing a new
  root is a single aligned 4-byte write of the root page id into the file
  header (atomic on both target platforms — no need for a two-slot
  `root_a`/`root_b` double buffer, since the write itself is already
  word-atomic) followed by `msync`. Only *after* that publish are the old
  (now-superseded) pages along the path returned to the freelist —
  reachable-only-from-the-old-root pages are the exact set of pages a
  single-writer, no-long-lived-snapshot design can free immediately.
- **No leaf sibling ("next leaf") chain.** A tempting optimization for
  range scans, but incompatible with copying only the root-to-leaf path:
  replacing a leaf changes its page id, so a linked-list predecessor would
  need updating too — cascading a supposedly-localized CoW update into an
  unrelated sibling subtree. `gems-index` learned this the hard way (an
  early implementation had a leaf silently point at a freed page after its
  sibling was updated); scans instead recurse through the tree structure
  itself, which stays correct under CoW for free. Revisit only if a
  workload needs O(1) leaf-to-leaf stepping badly enough to justify
  rewriting the whole predecessor chain on every leaf update.
- If you want consistent point-in-time reads for query execution (you do,
  for ABAC + query correctness), take the current root pointer once at
  query start and read through it — CoW guarantees it stays valid even as
  writers publish new roots concurrently, at the cost of only reclaiming a
  freed page once every open snapshot has passed it. A simple epoch counter
  (reader announces the root generation it's using, GC waits for the
  minimum) is enough; you don't need full MVCC.

## 3. Secondary indexes

Two structures, chosen per field by cardinality/query pattern:

- **Roaring bitmaps** for categorical/low-cardinality fields: entity kind,
  entity type membership, layer membership, flags, role assignment, link
  source/target postings. Each distinct value maps to a bitmap of *ordinal*
  entity positions (not TUIDs — keep a dense `ordinal -> slot_pointer`
  side-table, since roaring bitmaps are a `u32` domain and TUIDs are 24
  bytes). Query planner intersects/unions these directly, which is exactly
  what "entities of types (xx,yy,zz) in layers under layergroup ff" compiles
  to: `(type_xx | type_yy | type_zz) & (layer_a | layer_b | ...)`.
  Implement (or vendor, trimmed) the standard roaring container model:
  array container (<4096 elements), bitmap container (dense, 8 KiB) and a
  run-length container, chosen automatically per 16-bit chunk. This is a
  well-specified format (worth writing docs/roaring-format.md citing the
  spec) — a few hundred lines, no need for the full upstream crate's API
  surface.
- **Plain secondary B-tree** (same engine as §2, different key type) for
  ordered/range fields: numeric attributes, string prefixes, timestamps,
  CIDR prefixes (store as (prefix_len, network_bytes) sorted so range scans
  match subnet containment). Value = either a single ordinal/slot pointer
  (unique index) or a pointer to a roaring bitmap "posting list" page
  (non-unique).

Which structure a given `EntityAttribute` gets is a flag on the attribute
definition (`Indexed::None | Indexed::Bitmap | Indexed::BTree`), chosen at
schema-design time based on expected cardinality — expose this as guidance
in the CLI/TUI schema editor (warn if someone bitmap-indexes a
high-cardinality field like `description`).

## 4. Serialization: a zero-copy binary value format, not msgpack

Plain MessagePack requires a linear scan to find a field inside a map —
that's not zero-copy in the sense you want (zero-copy *read of a specific
field*). Instead, use a small custom format, call it **GBV** (Gems Binary
Value), that borrows MessagePack's type-tag vocabulary but adds a directory:

```
GBV value:
  u16  field_count
  [field_count] x { u32 key_id, u8 type_tag, u32 offset, u32 len }   // directory, sorted by key_id
  ...raw bytes for each field, referenced by (offset,len) into this same buffer...
```

- For **data entities**: `key_id` is the `EntityAttribute`'s TUID reduced to
  a 4-byte id (attributes get a dense u32 id at creation, resolved through
  the schema, not stored as strings — this is also what keeps the directory
  binary-searchable and compact). Reading field `x`: binary-search the
  directory (sorted by key_id) → slice `[offset..offset+len]` directly out
  of the mmap'd page, no allocation, no parse of unrelated fields.
- For **schema/aux entities** (EntityAttribute, EntityType, Subject, Layer,
  LayerGroup, VariantList, Role, LinkType, Link): these have a *fixed*
  Rust-defined shape per kind, so skip the directory entirely and use a
  positional array of typed values (a plain struct-of-fields layout with a
  small fixed header of offsets) — faster and simpler, no key lookup needed
  since the code always knows which field is which.
- Type tags mirror msgpack's fixed set plus the domain types requested:
  `Int64, Float64, Bool, Str, Bin, Point2D, Point3D, Poly2D, Poly3D, Geo2D,
  Geo3D, IPv4, IPv6, Cidr4, Cidr6, Regex, Currency(scale:u8), DateTime,
  Duration, Uuid, EnumRef, Array<T>`. `Array<T>` (for `Multiple` attributes)
  is stored as its own small directory of same-typed elements, same
  zero-copy slicing trick.

## 5. Entity model

### 5.1 Common header (fixed-size, every entity)

```
magic/version        u32
id (TUID)             24 bytes   // uuid || created_at_ns
created_by (uuid)      16 bytes   // subject TUID's uuid half
modified_by (uuid)     16 bytes
modified_at           i64        // ns since epoch
name_len              u8   ; name              63 bytes  (UTF-8, truncated, len-prefixed)
desc_len              u16  ; description       254 bytes (UTF-8, truncated, len-prefixed)
flags                 u32
entity_kind           u8         // Data | EntityAttribute | EntityType | Subject | ...
schema_ref (TUID)      24 bytes   // EntityType for Data entities; unused (zeroed) for schema/aux
body_offset, body_len u32, u32   // where the GBV/positional body starts within this slot
checksum (crc32c)     u32
```

Total comes in comfortably under 512 B, i.e. the header always fits in even
the smallest block class; the body follows in the same slot for small
entities, or in a chained oversized slot for large ones (§1.2).

### 5.2 Flags

```
PENDING_DELETE   // soft-delete in flight; excluded from default query results
PENDING_RENAME   // name change queued (see below)
DISABLED         // administratively inactive, kept for audit/history
SYSTEM           // builtin, not user-deletable (builtin subjects, builtin types)
IMMUTABLE        // schema entities that are locked once referenced by data
HIDDEN           // excluded from default listings, still directly addressable
TOMBSTONE        // fully deleted, header retained for tombstone-replication/audit
CONFLICT         // multi-writer conflict marker (cluster reconciliation, §6)
```
`PENDING_*` flags exist because renames/deletes on schema entities
(`EntityAttribute`, `EntityType`) need to fan out to every data entity that
references them before becoming final — model this as a background
migration job that clears the pending flag when the fan-out completes, not
as a synchronous multi-entity transaction.

### 5.3 Data vs schema/aux bodies

- **Data** entities: body = GBV map (§4), keyed by attribute id, validated
  against the `EntityType` referenced in `schema_ref` at write time.
- **Schema/aux** entities: body = positional typed array (§4), one Rust
  struct per `entity_kind`. Concretely:

**EntityAttribute**
```
prototype: Prototype (Int64|Float64|String|Binary|Bool|Point2D|Point3D|
           Poly2D|Poly3D|Geo2D|Geo3D|Ipv4|Ipv6|Cidr4|Cidr6|Regex|Currency|
           DateTime|Duration|EnumRef|EntityRef)
cardinality: Single | Multiple
nullable: bool
indexed: None | Bitmap | BTree
validation: ValidationRule   // min/max, regex-id, enum-ref TUID, precision/scale
format: FormatRule           // display template, e.g. currency "${1}" style
                              // placeholder substitution against the raw
                              // canonical value (store canonical, e.g. cents
                              // as i64; format is purely presentational)
```

**EntityType** — model this on LDAP's objectClass, which already solved
"attributes required by this type, optionally augmented by other types,
with some combinations forbidden":
```
kind: Structural | Auxiliary   // exactly one Structural type per entity,
                                // any number of compatible Auxiliary types
attributes: [(attribute_id, required: bool, default: Option<GBV>)]
compatible_with: [EntityType TUID]     // auxiliary types allowed alongside this one
incompatible_with: [EntityType TUID]   // explicit exclusion, wins over compatible
```

### 5.4 Aux types

**Subject** — `kind: Builtin(System|Anonymous|Everyone|Nobody|Authorized) |
User | Group | Organization | Service`. For `User`/`Service`: credential
block (Argon2id password hash + TOTP secret — see §0's crypto exception),
plus `member_of: [Subject TUID]` (groups/orgs this subject belongs to).
For `Group`/`Organization`: same `member_of` field (groups can nest in
orgs/other groups), no credentials.

Don't store the reverse edge (`members: [...]`) physically — that's the
classic LDAP `memberOf` overlay problem: a physically bidirectional edge
needs a distributed transaction on every membership change. Instead, derive
membership-of-group as a roaring-bitmap secondary index (§3) keyed by group
TUID → bitmap of member ordinals, rebuilt incrementally from the forward
`member_of` writes. One direction of truth, one direction of index.

**Layer / LayerGroup** — plain containment (`layer.group_ref: LayerGroup
TUID`). "Dynamic layer" = a `Layer` whose body additionally carries a stored
query string (§7); membership is materialized into the same roaring-bitmap
index as static layers, refreshed on a TTL or on write-triggered
invalidation of the tables the query touches (track this as a dependency
set on the compiled query, same way materialized-view invalidation works in
SQL engines).

**VariantList** — ordered list of allowed values, referenced by
`EntityAttribute.validation` when `prototype = EnumRef`.

**Role** — a tag entity; attach to a Subject via a `Multiple, EntityRef`
attribute on Subject pointing at Role TUIDs, with the "who has role X"
direction served by a roaring-bitmap index exactly like group membership.
Same one-direction-of-truth pattern throughout the aux model.

**LinkType / Link** — this is a lightweight property-graph edge, modeled
after RDF's subject–predicate–object triple (`LinkType` = predicate):
```
LinkType: name, flags (directional: bool, temporal_allowed: bool)
Link: source: [TUID], target: [TUID], link_type: LinkType TUID,
      from: Option<timestamp>, to: Option<timestamp>,
      flags: Temporal | Constant
```
Index every `Link` in both directions via roaring bitmaps (`source TUID →
bitmap of link ordinals`, `target TUID → bitmap of link ordinals`) so graph
traversal ("everything this entity links to/from") is a bitmap lookup, not
a scan. A secondary B-tree on `(link_type, from)` serves temporal range
queries ("links of type 'was seen' active during [t0,t1]").

## 6. Clustering: Raft per shard, gossip for the rest

Don't pick one algorithm for everything — CockroachDB/TiKV's split (Raft
per data range + gossip for cluster-wide metadata) is proven and maps
cleanly onto this design:

- **Sharding**: partition by TUID prefix (range-partitioned on the UUID
  half, since it's uniformly random — this gives even distribution without
  needing a separate hash step, and keeps a shard's key range contiguous
  for the B*-tree). Each shard is a set of extent files + its own primary
  index file + the secondary indexes scoped to that shard.
- **Per-shard Raft group**: the B*-tree's CoW root-swap (§2.3) *is* the
  natural Raft log entry — "apply this batch of slot writes + publish this
  new root" is a single, idempotent, replayable operation. This gives you
  linearizable writes per shard without needing a general-purpose
  distributed transaction protocol; cross-shard operations (e.g. a Link
  whose source and target land in different shards) go through a simple
  two-phase commit only when they must (most Link/query workloads don't
  need cross-shard atomicity — reads tolerate the bitmap indexes being
  "eventually" in sync within a shard's own Raft-replicated apply).
- **Gossip (SWIM-style)** for: cluster membership, failure detection, and
  disseminating the shard map (which nodes hold which shard's Raft group,
  who's leader). Also a good fit for propagating roaring-bitmap secondary
  index deltas across read replicas that don't need Raft-level
  consistency — roaring bitmaps OR-merge cleanly, which makes them
  naturally CRDT-like for this purpose.
- **Scope for v1**: implementing Raft + SWIM from scratch is the single
  biggest engineering risk in this project. Recommend sequencing: (1)
  single-node with the CoW/mmap engine fully working and tested, (2)
  async log-shipping read replicas (ship the Raft-log-shaped apply records
  over a plain TCP stream, no consensus yet — gets you HA reads and a
  warm standby cheaply), (3) full Raft once the apply-record format has
  proven stable under (2). Don't build the hard distributed-systems part
  against a storage format that's still moving.
- **Stage (2) is implemented**, in `gems-cluster`: `PrimaryStore` wraps
  `gems-engine::Store` and appends a `LogRecord` (Insert/Delete, at the
  logical-operation level rather than a physical page diff — the CoW
  B-tree/extent internals stay private to `gems-engine`) to a
  `ReplicationLog` after every mutation; `ReplicationServer` streams that
  log to any number of connecting `ReplicaClient`s over a plain blocking
  TCP socket (one thread per connection, a short poll loop to notice newly
  appended records — no OS-specific file-watching needed); each replica
  applies records to its own local `Store` and persists its progress so a
  restart resumes rather than replays. Verified with real sockets: a
  replica catches up on pre-existing writes, keeps tailing writes made
  while it's connected, and converges with the primary across both inserts
  and a delete. What it deliberately does not have — consensus, leader
  election, split-brain protection, automatic failover — is stage (3)'s
  job, not a gap in stage (2).
- **Stage (3), the Raft half, is implemented as a pure state machine**:
  `gems-cluster::raft::RaftCore` (leader election, log replication, and the
  §5.4.2 commit-index safety rule) takes explicit `tick()`/`receive()`
  inputs and returns the messages to send, with no sockets, timers, or
  threads inside it — what makes the tricky interleavings (a stale leader
  rejoining, a partitioned minority, a candidate with a stale log)
  deterministically testable, driving several instances from one test
  thread over a simulated, fully test-controlled network. Its committed
  entries are `LogRecord`s, the same type stage (2)'s `ReplicaClient`
  already applies to a `Store` — the two stages share the apply-record
  format by design. Wiring `RaftCore` to real sockets and a real timer (the
  network "shell") is the remaining piece before this closes the loop into
  an actual replicated `Store` end to end; log compaction, cluster
  membership changes, and connecting to `gems-engine` are each separate
  follow-on work, not implemented here.
- **The gossip/SWIM half is also implemented**, as `gems-cluster::gossip::
  SwimCore` — same pure-state-machine shape, for the same testability
  reason. Covers membership and failure detection (Alive/Suspect/Dead per
  member, incarnation-numbered so only a member's own refutation can clear
  a suspicion about it, piggybacked on ping/ack rather than a separate
  gossip round) but not yet indirect probing (SWIM's "ping-req" — asking
  other members to double-check a suspect before giving up on it, which
  reduces false positives from an asymmetric network problem between one
  pair of nodes) or disseminating the shard map specifically — the
  membership layer is there, what a shard map disseminated over it would
  look like is unbuilt.
- **Both cores now have real network shells**: `raft_net`/`swim_net` bind
  actual TCP sockets (`raft::wire`/`gossip::wire` handle the encode/decode,
  same `u32`-length-prefixed framing as `record::LogRecord` throughout this
  workspace) and run a timer-driven engine thread that exclusively owns
  the core — everything else reaches it by message, so there's no locking
  around the algorithm itself. `raft_net` also exposes a small client
  protocol (`propose_remote`, a separate port and format from peer RPCs)
  so a proposal can come from outside the cluster process entirely, not
  just from code sharing memory with a node. Verified with real sockets,
  real threads, real ports: three actual node processes elect a leader and
  replicate a proposal into separate `gems_engine::Store` instances (Raft),
  and three actual nodes converge on membership and detect an unreachable
  one (SWIM) — both stable across repeated runs.
- **A node's persistent Raft state now survives a crash**: `gems-cluster::
  raft_state` durably persists `current_term`/`voted_for`/`log[]` — the
  three fields the Raft paper (§5.1) requires be on stable storage before
  a node acts on them — and `raft_net::spawn` restores them on startup via
  `RaftCore::restore`. Before this, `RaftCore`'s persistent state lived
  only in the in-memory `Vec<LogEntry>`/fields its own module doc already
  flagged as the network shell's responsibility to persist — a real gap,
  not just a documented scope cut: a restarted node forgetting its term or
  vote can vote twice in the same term, the exact safety violation Raft's
  voting rule exists to prevent. Written as a full-snapshot
  write-temp-file/fsync/atomic-rename (plus an fsync of the containing
  directory, since POSIX doesn't guarantee a rename's durability without
  one) rather than an incremental log, persisted after every `receive()`/
  successful `propose()` and after any `tick()` that actually changed
  term/role/vote — before, not after, that iteration's outgoing RPCs are
  sent, per the paper's ordering requirement. Volatile state
  (`commit_index`/`last_applied`) is deliberately *not* persisted — Raft
  recovers it through normal protocol operation, and re-applying an
  already-applied `LogRecord` to a `Store` is safe by construction
  (insert overwrites; delete-of-already-deleted is a no-op).
  **Verified with an actual `SIGKILL`**, not just a unit test of the
  encode/decode format: a real single-node cluster process proposes
  several entries, then gets a hard `kill -9` (via `rustix::process::
  kill_process`) with no chance to run any shutdown/cleanup code: the
  persisted term/log survive intact, exactly as if a real deployed node's
  host had lost power. That same investigation also surfaced a second,
  related bug this fixed: `gems_common::filelock`'s lock file wasn't
  opened `O_CLOEXEC`, so a process that spawned a child while holding a
  store's lock leaked a live duplicate of that lock into the child via
  `fork`+`exec` — the lock stayed effectively held for as long as the
  *child* ran, well past the original owner dropping it, causing spurious
  `AlreadyLocked` failures for anyone else. Both bugs were found by the
  same crash-recovery test, not by inspection — concrete evidence for why
  this kind of test belongs in the suite rather than being satisfied by
  reasoning about the code.
- **Peer traffic is authenticated**: every `raft::wire`/`gossip::wire`
  frame carries an HMAC-SHA256 tag over its payload, keyed by a secret
  shared across the cluster (`raft_net::spawn`/`swim_net::spawn` both take
  it as an `Arc<Vec<u8>>`). Without this, any TCP client that could reach
  a node's peer port could inject Raft/SWIM messages directly —
  forge votes, propose bogus committed entries, or manipulate another
  node's membership view — without being a configured cluster member at
  all. A shared secret is a deliberately simple scheme relative to real
  mTLS/PKI (this workspace's "avoid third-party crates" rule extends to
  crypto — `gems-common::sha256`/`hmac` are hand-rolled, verified against
  NIST/RFC test vectors), reasonable for an operator-trusted cluster where
  every node already reads the same secret from its own deployment config.
  **Not yet covered**: the separate client-facing propose port
  (`propose_remote`) an external caller uses to submit a proposal without
  being a peer at all — that's a distinct authNz surface (a caller, not a
  cluster member) and is real follow-on work, not silently assumed safe.
- **Sharding is implemented at the "static config" scope named above**:
  `gems-cluster::shard::ShardRouter` partitions TUIDs by their UUID
  half's leading byte (uniform, no hashing needed, per this section's
  design), `ShardMap` is the static shard -> node-address config, and
  `ShardedClient` routes a proposal to the right shard's Raft group and
  finds its current leader by trying each member's client port in turn.
  No dynamic rebalancing, no gossip-disseminated shard map yet (still real
  follow-on work) — but verified end to end with two actual independent
  3-node Raft clusters: a client proposes one entity per shard, both
  commit and apply on their respective cluster only, and each shard is
  confirmed to have never received the other's entity.

## 7. Query language

Hand-written recursive-descent parser (no parser-generator dependency) for
a SQL subset:

```sql
SELECT [fields...]
FROM entities
WHERE type IN (xx, yy, zz)
  AND layer.layergroup = 'ff'
  AND attr.status = 'active'
ORDER BY modified_at DESC
LIMIT 50
```

Planner compiles the `WHERE` tree into roaring-bitmap set operations where
possible (`type IN (...)`, layer/layergroup membership, flag checks, role
checks) and falls back to secondary-B-tree range scans or, last resort, a
header/body scan for unindexed predicates. A simple tree-walking evaluator
is enough for v1 (skip building a bytecode VM à la SQLite until profiling
says otherwise — the win from bitmap-indexed predicates dwarfs
interpreter overhead at this scale). The final bitmap of matching ordinals
is what gets handed to the ABAC enforcement point (§8) before
materialization.

## 8. Access control: ABAC

- **Policy** is itself just another schema-backed entity kind (reuses the
  whole storage/entity machinery — no separate policy store): attributes
  = target predicate (entity kind/type/layer this policy applies to,
  itself expressed as a small query-language fragment), subject predicate
  (which subjects/roles/groups it applies to), effect (`Permit | Deny`),
  and optional field-level obligations (redact/mask specific attribute
  ids rather than denying the whole entity).
- **PDP (decision point)**: policies are indexed the same way entities are
  (bitmap-indexed by target type/layer) so evaluating "which policies
  apply to this query" is itself a bitmap lookup, not a linear scan of
  every policy in the system.
- **PEP (enforcement point)**: sits between the query planner's candidate
  bitmap and result materialization. For each candidate entity: evaluate
  applicable policies against `(subject attributes, entity attributes,
  environment)`; a `Deny` at the whole-entity level drops it from the
  result set (candidate bitmap AND-NOT'd, cheap); a field-level obligation
  redacts specific fields when materializing that entity's GBV body,
  producing a genuinely partial object rather than an error — exactly the
  "partial results" behavior you asked for.
- Compile each policy's predicate once (into the same bitmap/AST
  representation as user queries) and cache it; re-evaluate the cache only
  when the policy entity itself changes (it's a normal entity, so this is
  just "invalidate on write to a Policy-kind entity").
- **Authentication (establishing *who* the `SubjectContext` is) is
  separate from the PDP/PEP above (deciding what that subject may see),
  and lives in `gems-abac::token`**: JWT-shaped (RFC 7519), HS256
  (HMAC-SHA256) signed and verified with a shared secret, built on this
  workspace's own hand-rolled `sha256`/`hmac`/`base64url` primitives
  (`gems-common`) rather than a crypto or JWT crate — deliberately minimal
  relative to the full JWT spec (one fixed header, one algorithm, three
  claims: `sub`, `roles`, optional `exp`; no algorithm negotiation, which
  closes off the classic JWT "alg confusion" attack by construction, since
  `verify` never branches on what the token itself claims). `gems-webui`
  and `gems-mcp` both require a valid, unexpired token by default
  (`AuthMode::Enforced`) and use the `SubjectContext` it verifies to for
  every read — replacing an earlier scheme where a caller could just pass
  a `subject`/`roles` parameter directly with nothing verifying they
  actually *were* that subject (i.e. anyone could read as anyone by
  editing a query parameter). `AuthMode::Insecure` (an explicit
  `--insecure` startup flag) restores raw, unauthenticated access, for
  local testing only — never the default, and a server refuses to start
  under `Enforced` without its secret configured (fail closed, not a
  silently-empty default secret every deployment would share).

## 8a. Change notifications: subscriptions and materialized views

A natural feature given §6's replication log already exists — added on
request, not originally scoped, so noted as its own section rather than
folded silently into §6. **Implemented in `gems-subscribe`.** The key
insight: a subscriber is architecturally just a replica that doesn't write
to a `Store` — it evaluates a predicate against each `LogRecord` instead of
applying it, so this crate reuses `gems-cluster`'s `LogRecord`/
`ReplicationLog` rather than inventing a parallel change-capture mechanism.

Two tiers, deliberately kept separate because their cost is genuinely
different:

- **Point-level** (`"entity Z deleted"`, `"entity X's attribute Y
  changed"`): cheap. A delete is just `LogRecord::Delete`. An attribute
  change needs a "before" to diff against, which a single `Insert` record
  doesn't carry (it's the new state, not a diff) — solved with a small
  cache of last-observed values, but only for `(entity, attribute)` pairs
  someone actually subscribed to, not a full shadow copy of every entity.
  First sighting of a watched attribute seeds the cache without firing (no
  prior value to have changed from).
- **Aggregate** (`"count(entities matching P) changed"`): genuinely
  harder — real incremental view maintenance, not event filtering. A
  `LogRecord` says an entity was written or deleted, not whether that
  pushed it in or out of some predicate's matching set, so a view has to
  evaluate the predicate itself and track the full matching-id set (not
  just a counter) to correctly detect a membership transition. `COUNT`
  only and a single-condition predicate (entity kind, type, or one
  attribute equality) for this pass — compiling a richer predicate from
  `gems-query`'s `Expr` tree, or other aggregates (`SUM`/`MIN`/`MAX`), is
  real follow-on integration work, not a small addition.

Both tiers are pure, I/O-free state machines (`SubscriptionEngine`,
`ViewEngine`), same design pattern as §6's `RaftCore`/`SwimCore` and for
the same reason — deterministic tests instead of ones depending on real
time. `NotificationHub` combines them so a `ViewChanged` watch fires
correctly off a view's count transition. `LogTailer` is the thin shell
that actually reads a `ReplicationLog` file and drives a hub; it's
local/same-machine for this pass (a remote subscriber would need a small
client analogous to `ReplicaClient` but feeding a `NotificationHub`
instead of a `Store` — not needed yet since pointing `LogTailer` at a
replica's already-synced local copy of the log works today).

## 9. Frontends

All four talk to the same query/ABAC engine as a library — no frontend
gets a shortcut around policy enforcement.

- **CLI**: hand-rolled arg parsing (the surface area is small enough that a
  dependency isn't worth it) over the query engine; scriptable, prints
  the same SQL-like query language results as table/JSON/GBV-passthrough.
- **TUI** (`gems-tui`, built): raw terminal control via `rustix`'s termios
  ioctls (`tcgetattr`/`tcsetattr`/`make_raw` for raw mode, `tcgetwinsize`
  for terminal size — all called with `std::io::stdin()`/`stdout()` as the
  `AsFd` source rather than rustix's ownership-taking `take_stdin()`), plus
  a hand-rolled ANSI renderer that diffs at line granularity (whole-line
  string compare against the previous frame; only changed lines get an
  escape-coded redraw — a deliberate coarser cut than full per-cell
  diffing, adequate for a keyboard-driven browser where redraws only
  happen on discrete key presses, not on any animation). Delivered as an
  entity browser: list pane + detail pane, arrow keys or j/k to navigate,
  `/` to enter a query (any `gems-query` SQL-subset string), q or Ctrl+C to
  quit. The state machine (`app::App`) is factored out from all terminal
  I/O and unit-tested directly — same "pure core + thin I/O shell" pattern
  as `RaftCore`/`SwimCore`. Scope for this pass: read-only (no entity
  create/edit — same "needs schema-driven form generation" reasoning as
  the WebUI) and no ABAC subject context (administrative/raw access only,
  same as the CLI's default).
- **WebUI** (`gems-webui`, built): hand-rolled vanilla HTML/CSS/JS (no
  build step, no framework) rather than the Svelte frontend originally
  proposed here — deviation made explicit in `gems-webui/src/main.rs`'s
  module doc: a Svelte build toolchain is real dependency surface for a
  page this size (a two-panel query browser), and the gap in ergonomics
  from hand-written DOM code doesn't show up yet at this UI's scale.
  Served by a small hand-rolled blocking HTTP/1.1 server over `rustix`
  sockets — admin-tool traffic levels don't need an async runtime. API
  surface for this pass is read-only: `/api/types`, `/api/query`,
  `/api/entity`, all `GET`, JSON responses (hand-rolled encoder/decoder).
  Authentication is required by default (§8's `gems-abac::token`): every
  request needs a valid `Authorization: Bearer <token>` header, or the
  server refuses to start; `--insecure` opts back into raw access, for
  local testing only. Entity CRUD/policy admin through the WebUI is a
  later pass (needs the same schema-driven form generation the TUI's
  create/edit scope cut defers).
- **MCP** (`gems-mcp`, built): JSON-RPC 2.0 over stdio (newline-delimited,
  no `Content-Length` framing), exposing `query`, `get_entity`,
  `list_entity_types` — thin adapter over the same engine, subject to the
  same ABAC PEP. Authentication mirrors the WebUI's: an `auth_token`
  tool argument verified against `$GEMS_MCP_SECRET` by default, with the
  same `--insecure` escape hatch. Dispatch logic is split from the stdio
  loop (`protocol.rs`/`tools.rs` vs. a thin `main.rs`) for the same
  testability reason as the TUI's `app.rs` split.

## 10. Crate layout

```
gems/
  crates/
    gems-common/     // TUID, page-size detection, crc32c, error types
    gems-storage/     // rustix file/mmap wrappers, extent format, slab allocator
    gems-index/       // B*-tree engine (primary + secondary B-tree indexes)
    gems-bitmap/       // vendored/trimmed roaring bitmap implementation
    gems-codec/       // GBV binary format encode/decode, JSON encode/decode
    gems-catalog/      // entity kinds: header, EntityAttribute, EntityType,
                        // Subject, Layer, LayerGroup, VariantList, Role,
                        // LinkType, Link — validation against schema
    gems-query/        // SQL-like parser + planner + evaluator
    gems-abac/         // Policy entity kind, PDP, PEP
    gems-cluster/      // shard map, log-shipping replication -> Raft, gossip/SWIM
    gems-cli/          // CLI frontend (bin)
    gems-tui/          // TUI frontend (bin)
    gems-webui/         // HTTP server + API, serves compiled Svelte assets (bin)
    gems-mcp/           // MCP JSON-RPC server (bin)
  webui/                // Svelte source (built separately, output embedded by gems-webui)
  vendor/               // vendored third-party source (crypto primitives, roaring
                        // bitmap reference, anything else borrowed rather than reimplemented),
                        // each with an ATTRIBUTION file naming source + license
```

Dependency direction: `common -> storage -> {index, bitmap} -> codec ->
catalog -> {query, abac} -> cluster -> {cli, tui, webui, mcp}`. Nothing
above `catalog` should need to know about extent/mmap details directly —
that's the point of the layering.

## 11. Suggested build order

1. `gems-common` + `gems-storage`: extent format, slab allocator, file
   header, crash-safety primitives. Test with a fuzzed allocate/free
   workload before anything else touches it.
2. `gems-index`: B*-tree over the allocator, CoW root swap. Test with
   random insert/delete against a reference `BTreeMap` for correctness,
   plus a kill-mid-write crash test (truncate the mmap, reopen, verify the
   last published root is intact).
3. `gems-codec` (GBV) + `gems-catalog` header/aux structs.
4. `gems-bitmap` (roaring, trimmed) + secondary index wiring.
5. `gems-query` + `gems-abac` (the ABAC PEP is much easier to get right
   once the bitmap-based query planner exists to build it on top of).
6. `gems-cli` end-to-end against a single-node engine — this is your
   first real usable milestone.
7. `gems-tui` / `gems-webui` / `gems-mcp` in parallel, all thin over the
   same engine.
8. `gems-cluster`: log-shipping replicas first, Raft second, per §6.
