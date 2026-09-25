//! Write routes: entities, entity types, subjects, and ABAC policies, plus
//! the first-run `/api/setup` wizard. All `POST`, all reading a JSON body
//! (see `http::Request::json_body`) rather than query parameters, since a
//! write's payload (an entity's fields, a policy's predicates) doesn't fit
//! a URL the way a read's few scalar parameters did.
//!
//! **Every write goes through the Raft cluster's `propose` path
//! (`gems_cluster::shard::ShardedClient`), never a direct
//! `Store::insert`/`delete`.** This server only ever opens a `Store`
//! read-only (see `api.rs`'s module doc) — opening it writable here too
//! would make `gems-webui` a second writer racing the cluster's own Raft
//! nodes for the same store directory's exclusive lock, exactly the hazard
//! OPERATIONS.md's concurrency contract warns never to create. Instead
//! this crate is just another `ShardedClient` caller, the same as
//! `gems-cluster-node propose`/`install`.
//!
//! **Authorization beyond "is this caller signed in"** — who may create a
//! type, mint a token, or edit a policy — isn't part of `gems-abac`'s ABAC
//! model (`evaluate`/`enforce` only ever gate *reads*; see that crate's own
//! module doc). So this module does its own coarse, role-name-based
//! app-layer gating (`require_role`): the `admin` role (see
//! `gems_catalog::bootstrap`) may manage everything; `admin` or `analyst`
//! may write `Data` entities; nothing here is gated finer than that. A
//! real deployment wanting per-type or per-field write policies would need
//! to extend this, not work around it.

use std::time::Duration;

use gems_abac::token::AuthMode;
use gems_abac::SubjectContext;
use gems_catalog::bootstrap::{self, Bootstrap};
use gems_catalog::{
    EntityHeader, EntityKind, EntityType, EntityTypeKind, Policy, Subject, SubjectKind,
    SubjectPredicate, TargetPredicate,
};
use gems_cluster::shard::ShardedClient;
use gems_cluster::LogRecord;
use gems_codec::{GbvBuilder, TypeTag};
use gems_common::{field_id, Tuid};
use gems_engine::Store;
use gems_json::Value;

use crate::api::{authorize, entity_summary, open_store, ApiResult};
use crate::http::Request;

fn json_error(e: String) -> (u16, String) {
    (400, e)
}

fn required_str<'a>(body: &'a Value, key: &str) -> Result<&'a str, (u16, String)> {
    body.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| (400, format!("{key} is required")))
}

fn required_tuid(body: &Value, key: &str) -> Result<Tuid, (u16, String)> {
    let hex = required_str(body, key)?;
    Tuid::from_hex_str(hex).ok_or_else(|| (400, format!("invalid {key}")))
}

fn optional_tuid(body: &Value, key: &str) -> Result<Option<Tuid>, (u16, String)> {
    match body.get(key).and_then(Value::as_str) {
        Some(hex) => Ok(Some(
            Tuid::from_hex_str(hex).ok_or_else(|| (400, format!("invalid {key}")))?,
        )),
        None => Ok(None),
    }
}

fn propose_insert(
    client: &ShardedClient,
    header: EntityHeader,
    body: Vec<u8>,
) -> Result<(), (u16, String)> {
    let id = header.id;
    client
        .propose(&id, LogRecord::Insert { header, body })
        .map(|_| ())
        .map_err(|e| (502, format!("propose failed: {e}")))
}

fn propose_delete(client: &ShardedClient, id: Tuid) -> Result<(), (u16, String)> {
    client
        .propose(&id, LogRecord::Delete { id })
        .map(|_| ())
        .map_err(|e| (502, format!("propose failed: {e}")))
}

/// Looks up a `Role` entity's id by name (`gems_catalog::bootstrap`'s named
/// roles, or any operator-created one) — the only way this module resolves
/// a role name to the `Tuid` a token's `roles` claim actually carries.
fn role_id_by_name(store: &Store, name: &str) -> Result<Option<Tuid>, (u16, String)> {
    for id in store
        .query_by_kind(EntityKind::Role)
        .map_err(|e| (500, e.to_string()))?
    {
        if let Some((header, _)) = store.get(&id).map_err(|e| (500, e.to_string()))? {
            if header.name == name {
                return Ok(Some(id));
            }
        }
    }
    Ok(None)
}

/// Requires `subject` to hold at least one of `allowed_roles`, by name.
/// `subject` is `None` only under `AuthMode::Insecure` (see `api.rs`'s
/// `authorize`), which this treats the same way every other route does —
/// unrestricted, explicit-opt-in local testing.
fn require_role(
    subject: &Option<SubjectContext>,
    store: &Store,
    allowed_roles: &[&str],
) -> Result<(), (u16, String)> {
    let Some(subject) = subject else {
        return Ok(());
    };
    for name in allowed_roles {
        if let Some(role_id) = role_id_by_name(store, name)? {
            if subject.roles.contains(&role_id) {
                return Ok(());
            }
        }
    }
    Err((
        403,
        format!("requires one of these roles: {}", allowed_roles.join(", ")),
    ))
}

/// Lists every entity of `kind`, dropping any the caller's own token
/// doesn't have ABAC visibility into (raw, unfiltered access under
/// `AuthMode::Insecure`) — the enforced counterpart to `api::list_types`,
/// used for kinds (`Subject`, `Policy`, `Role`) the seeded policy set
/// deliberately keeps admin-only.
fn list_by_kind_enforced(
    store: &Store,
    subject: &Option<SubjectContext>,
    kind: EntityKind,
) -> Result<Value, (u16, String)> {
    let mut results = Value::array();
    for id in store
        .query_by_kind(kind)
        .map_err(|e| (500, e.to_string()))?
    {
        let found = match subject {
            Some(s) => store
                .get_enforced(&id, s)
                .map_err(|e| (500, e.to_string()))?,
            None => store.get(&id).map_err(|e| (500, e.to_string()))?,
        };
        if let Some((header, _)) = found {
            results.push(entity_summary(&header));
        }
    }
    Ok(results)
}

// ---------------------------------------------------------------------
// Entities
// ---------------------------------------------------------------------

pub fn save_entity(req: &Request, auth: &AuthMode, client: &ShardedClient) -> ApiResult {
    let subject = authorize(req, auth)?;
    let store = open_store(req)?;
    require_role(
        &subject,
        &store,
        &[bootstrap::ADMIN_ROLE_NAME, bootstrap::ANALYST_ROLE_NAME],
    )?;

    let payload = req.json_body().map_err(json_error)?;
    let type_id = required_tuid(&payload, "type")?;
    let name = required_str(&payload, "name")?.to_string();
    let id = optional_tuid(&payload, "id")?.unwrap_or_else(Tuid::generate);

    let mut gbv = GbvBuilder::new();
    if let Some(fields) = payload.get("fields").and_then(Value::entries) {
        for (key, value) in fields {
            let text = value.as_str().unwrap_or_default();
            gbv.push(field_id(key), TypeTag::Str, text.as_bytes());
        }
    }

    let header = EntityHeader::new(id, &name, EntityKind::Data, type_id);
    propose_insert(client, header, gbv.finish())?;

    let mut result = Value::object();
    result.set("id", id.to_hex_string());
    Ok(result)
}

pub fn delete_entity(req: &Request, auth: &AuthMode, client: &ShardedClient) -> ApiResult {
    let subject = authorize(req, auth)?;
    let store = open_store(req)?;
    require_role(
        &subject,
        &store,
        &[bootstrap::ADMIN_ROLE_NAME, bootstrap::ANALYST_ROLE_NAME],
    )?;

    let payload = req.json_body().map_err(json_error)?;
    let id = required_tuid(&payload, "id")?;
    propose_delete(client, id)?;
    Ok(Value::Bool(true))
}

// ---------------------------------------------------------------------
// Entity types
// ---------------------------------------------------------------------

/// Create-only for this pass — attribute editing needs schema-driven form
/// generation from `EntityAttribute` definitions, called out as its own
/// integration project in `main.rs`'s original module doc; editing an
/// existing type's attributes isn't part of this milestone either.
pub fn save_type(req: &Request, auth: &AuthMode, client: &ShardedClient) -> ApiResult {
    let subject = authorize(req, auth)?;
    let store = open_store(req)?;
    require_role(&subject, &store, &[bootstrap::ADMIN_ROLE_NAME])?;

    let payload = req.json_body().map_err(json_error)?;
    let name = required_str(&payload, "name")?.to_string();

    let id = Tuid::generate();
    let entity_type = EntityType {
        kind: EntityTypeKind::Structural,
        attributes: vec![],
        compatible_with: vec![],
        incompatible_with: vec![],
    };
    let header = EntityHeader::new(id, &name, EntityKind::EntityType, Tuid::NIL);
    propose_insert(client, header, entity_type.encode())?;

    let mut result = Value::object();
    result.set("id", id.to_hex_string());
    Ok(result)
}

pub fn delete_type(req: &Request, auth: &AuthMode, client: &ShardedClient) -> ApiResult {
    let subject = authorize(req, auth)?;
    let store = open_store(req)?;
    require_role(&subject, &store, &[bootstrap::ADMIN_ROLE_NAME])?;

    let payload = req.json_body().map_err(json_error)?;
    let id = required_tuid(&payload, "id")?;
    propose_delete(client, id)?;
    Ok(Value::Bool(true))
}

// ---------------------------------------------------------------------
// Subjects (users) and roles
// ---------------------------------------------------------------------

pub fn list_subjects(req: &Request, auth: &AuthMode) -> ApiResult {
    let subject = authorize(req, auth)?;
    let store = open_store(req)?;
    list_by_kind_enforced(&store, &subject, EntityKind::Subject)
}

pub fn list_roles(req: &Request, auth: &AuthMode) -> ApiResult {
    let subject = authorize(req, auth)?;
    let store = open_store(req)?;
    list_by_kind_enforced(&store, &subject, EntityKind::Role)
}

pub fn save_subject(req: &Request, auth: &AuthMode, client: &ShardedClient) -> ApiResult {
    let subject = authorize(req, auth)?;
    let store = open_store(req)?;
    require_role(&subject, &store, &[bootstrap::ADMIN_ROLE_NAME])?;

    let payload = req.json_body().map_err(json_error)?;
    let name = required_str(&payload, "name")?.to_string();

    let id = Tuid::generate();
    let new_subject = Subject {
        kind: SubjectKind::User,
        credentials: None,
        member_of: vec![],
    };
    let header = EntityHeader::new(id, &name, EntityKind::Subject, Tuid::NIL);
    propose_insert(client, header, new_subject.encode())?;

    let mut result = Value::object();
    result.set("id", id.to_hex_string());
    Ok(result)
}

/// Mints a bearer token for an existing `Subject`, asserting whichever
/// roles the admin caller names. This is the only place a token is ever
/// produced after initial setup — a new user has no way to authenticate
/// until an admin does this and hands them the result. Doesn't write
/// anything; `gems_abac::token::issue` is a pure signing operation.
pub fn issue_token(req: &Request, auth: &AuthMode) -> ApiResult {
    let subject = authorize(req, auth)?;
    let store = open_store(req)?;
    require_role(&subject, &store, &[bootstrap::ADMIN_ROLE_NAME])?;

    let AuthMode::Enforced { secret } = auth else {
        return Err((
            400,
            "cannot issue tokens while running --insecure (there is no secret to sign with)"
                .to_string(),
        ));
    };

    let payload = req.json_body().map_err(json_error)?;
    let target_subject_id = required_tuid(&payload, "subject_id")?;
    let role_names = payload
        .get("role_names")
        .and_then(Value::as_array)
        .ok_or((400, "role_names is required".to_string()))?;

    let mut roles = Vec::new();
    for name in role_names {
        let name = name
            .as_str()
            .ok_or((400, "role_names must be an array of strings".to_string()))?;
        let role_id =
            role_id_by_name(&store, name)?.ok_or_else(|| (400, format!("no such role: {name}")))?;
        roles.push(role_id);
    }

    let token = gems_abac::token::issue(
        secret,
        &SubjectContext {
            subject_id: target_subject_id,
            roles,
        },
        None,
    );
    let mut result = Value::object();
    result.set("token", token);
    Ok(result)
}

// ---------------------------------------------------------------------
// Policies
// ---------------------------------------------------------------------

pub fn list_policies(req: &Request, auth: &AuthMode) -> ApiResult {
    let subject = authorize(req, auth)?;
    let store = open_store(req)?;
    list_by_kind_enforced(&store, &subject, EntityKind::Policy)
}

pub fn save_policy(req: &Request, auth: &AuthMode, client: &ShardedClient) -> ApiResult {
    let subject = authorize(req, auth)?;
    let store = open_store(req)?;
    require_role(&subject, &store, &[bootstrap::ADMIN_ROLE_NAME])?;

    let payload = req.json_body().map_err(json_error)?;
    let effect = match required_str(&payload, "effect")? {
        "permit" => gems_catalog::Effect::Permit,
        "deny" => gems_catalog::Effect::Deny,
        other => {
            return Err((
                400,
                format!("effect must be \"permit\" or \"deny\", got {other:?}"),
            ))
        }
    };
    let target_kind = match payload.get("target_kind").and_then(Value::as_str) {
        Some(name) => Some(parse_entity_kind(name)?),
        None => None,
    };
    let target_schema_ref = optional_tuid(&payload, "target_type")?.unwrap_or(Tuid::NIL);
    let role_id = match payload.get("role_name").and_then(Value::as_str) {
        Some(name) => {
            role_id_by_name(&store, name)?.ok_or_else(|| (400, format!("no such role: {name}")))?
        }
        None => Tuid::NIL,
    };
    let subject_id = optional_tuid(&payload, "subject_id")?.unwrap_or(Tuid::NIL);
    let redact_attributes = payload
        .get("redact_fields")
        .and_then(Value::as_array)
        .map(|fields| {
            fields
                .iter()
                .filter_map(Value::as_str)
                .map(field_id)
                .collect()
        })
        .unwrap_or_default();

    let policy = Policy {
        target: TargetPredicate {
            entity_kind: target_kind,
            schema_ref: target_schema_ref,
        },
        subject: SubjectPredicate {
            subject_id,
            role_id,
        },
        effect,
        redact_attributes,
    };

    let id = Tuid::generate();
    let header = EntityHeader::new(id, "policy", EntityKind::Policy, Tuid::NIL);
    propose_insert(client, header, policy.encode())?;

    let mut result = Value::object();
    result.set("id", id.to_hex_string());
    Ok(result)
}

pub fn delete_policy(req: &Request, auth: &AuthMode, client: &ShardedClient) -> ApiResult {
    let subject = authorize(req, auth)?;
    let store = open_store(req)?;
    require_role(&subject, &store, &[bootstrap::ADMIN_ROLE_NAME])?;

    let payload = req.json_body().map_err(json_error)?;
    let id = required_tuid(&payload, "id")?;
    propose_delete(client, id)?;
    Ok(Value::Bool(true))
}

fn parse_entity_kind(s: &str) -> Result<EntityKind, (u16, String)> {
    Ok(match s {
        "Data" => EntityKind::Data,
        "EntityType" => EntityKind::EntityType,
        "Subject" => EntityKind::Subject,
        "Role" => EntityKind::Role,
        "Policy" => EntityKind::Policy,
        other => return Err((400, format!("unknown target_kind: {other:?}"))),
    })
}

// ---------------------------------------------------------------------
// First-run setup
// ---------------------------------------------------------------------

/// Seeds `gems_catalog::bootstrap`'s starter roles/policies/admin subject
/// and returns the new admin's bearer token — the UI's equivalent of
/// `gems-cluster-node install`, callable from a browser with nothing but
/// network access to this server.
///
/// **Not gated by a bearer token** (there is no admin yet to hold one) —
/// instead the caller must know `$GEMS_WEBUI_SECRET` itself, passed as
/// `setup_secret` in the body, proving they already have some other way
/// into the deployment (its environment, its Compose file) rather than
/// being an arbitrary network client. Refuses to run a second time against
/// a store directory that already has a `Role` named `"admin"` — a
/// best-effort check, not a cluster-wide guarantee: `gems_catalog::
/// bootstrap`'s seed entities land in whichever shard their randomly
/// generated ids route to, so a store directory belonging to a
/// *different* shard than the one setup already ran against won't see it.
/// Running setup twice is harmless besides the duplication (ABAC's
/// deny-overrides evaluation doesn't break under redundant `Permit`
/// policies) — this check exists to avoid that duplication in the common
/// case, not to make repeat runs impossible.
pub fn setup(req: &Request, auth: &AuthMode, client: &ShardedClient) -> ApiResult {
    let AuthMode::Enforced { secret } = auth else {
        return Err((
            400,
            "setup is only meaningful when running with a $GEMS_WEBUI_SECRET configured \
             (see --insecure's own docs: that mode never needed a token in the first place)"
                .to_string(),
        ));
    };

    let payload = req.json_body().map_err(json_error)?;
    let setup_secret = required_str(&payload, "setup_secret")?;
    if setup_secret.as_bytes() != secret.as_slice() {
        return Err((403, "incorrect setup secret".to_string()));
    }
    let admin_name = payload
        .get("admin_name")
        .and_then(Value::as_str)
        .unwrap_or("admin")
        .to_string();

    let store = open_store(req)?;
    if role_id_by_name(&store, bootstrap::ADMIN_ROLE_NAME)?.is_some() {
        return Err((
            400,
            "setup already completed (an \"admin\" role already exists in this store)".to_string(),
        ));
    }

    let seed: Bootstrap = bootstrap::bootstrap(&admin_name);
    for entity in seed.entities {
        propose_insert(client, entity.header, entity.body)?;
    }

    let token = gems_abac::token::issue(
        secret,
        &SubjectContext {
            subject_id: seed.admin_subject_id,
            roles: vec![seed.admin_role_id],
        },
        None,
    );

    let mut result = Value::object();
    result.set("admin_token", token);
    Ok(result)
}

/// Builds the `ShardedClient` every write route submits proposals through,
/// from the same `$GEMS_SHARD_MAP`/`$GEMS_CLUSTER_SECRET` environment
/// variables `gems-cluster-node propose`/`install` read — `gems-webui` is
/// just another client of the cluster it's serving a UI for.
pub fn build_sharded_client_from_env() -> Result<ShardedClient, String> {
    // Reachable addresses can take a little while after `docker compose
    // up` starts every service roughly in parallel, same reasoning as
    // `gems-cluster-node`'s own retry loop.
    const RESOLVE_DEADLINE: Duration = Duration::from_secs(30);
    const PROPOSE_TIMEOUT: Duration = Duration::from_secs(5);

    let raw = std::env::var("GEMS_SHARD_MAP").map_err(|_| "$GEMS_SHARD_MAP must be set")?;
    let root = gems_json::parse(&raw).map_err(|e| format!("GEMS_SHARD_MAP: {e}"))?;
    let shards = root
        .get("shards")
        .and_then(Value::as_array)
        .ok_or("GEMS_SHARD_MAP: missing top-level \"shards\" array")?;

    let mut map = gems_cluster::shard::ShardMap::new();
    let num_shards = shards.len() as u32;
    for entry in shards {
        let shard_id = entry
            .get("shard")
            .and_then(Value::as_u64)
            .ok_or("GEMS_SHARD_MAP: a shard entry is missing an integer \"shard\" id")?
            as u32;
        let members = entry
            .get("members")
            .and_then(Value::as_array)
            .ok_or("GEMS_SHARD_MAP: a shard entry is missing a \"members\" array")?;
        for member in members {
            let node = member
                .get("node")
                .and_then(Value::as_u64)
                .ok_or("GEMS_SHARD_MAP: a member is missing an integer \"node\" id")?
                as u32;
            let client_addr = member
                .get("client")
                .and_then(Value::as_str)
                .ok_or("GEMS_SHARD_MAP: a member is missing its \"client\" address")?;
            let addr = resolve_with_retry(client_addr, RESOLVE_DEADLINE)?;
            map.add_member(shard_id, node, addr);
        }
    }
    if num_shards == 0 {
        return Err("GEMS_SHARD_MAP has no shards".to_string());
    }

    Ok(ShardedClient::new(
        gems_cluster::shard::ShardRouter::new(num_shards),
        map,
        PROPOSE_TIMEOUT,
    ))
}

fn resolve_with_retry(host_port: &str, deadline: Duration) -> Result<std::net::SocketAddr, String> {
    use std::net::ToSocketAddrs;
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

#[cfg(test)]
mod tests {
    use super::*;
    use gems_cluster::raft_net::{self, RaftTiming};
    use gems_cluster::shard::{ShardMap, ShardRouter};
    use std::net::{SocketAddr, TcpListener};
    use std::path::PathBuf;

    fn free_port() -> u16 {
        TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("gems-webui-write-api-test")
            .join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// Spawns a single-node Raft "cluster" (one shard, one node — it
    /// self-elects leader immediately with no peers to wait on) and
    /// returns a `ShardedClient` pointed at it plus the read-only `Store`
    /// directory it writes into, so a test can propose through the client
    /// and then verify the result by opening the same store directly.
    fn single_node_cluster(name: &str) -> (ShardedClient, PathBuf, raft_net::RaftNodeHandle) {
        let dir = tmp_dir(name);
        let peer_port = free_port();
        let client_port = free_port();
        let handle = raft_net::spawn(
            1,
            &format!("127.0.0.1:{peer_port}"),
            &format!("127.0.0.1:{client_port}"),
            std::collections::HashMap::new(),
            &dir,
            RaftTiming {
                tick_interval: Duration::from_millis(20),
                heartbeat_interval_ticks: 3,
                election_timeout_ticks_range: (4, 6),
            },
            std::sync::Arc::new(b"test-secret".to_vec()),
        )
        .unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if let Ok((gems_cluster::raft::Role::Leader, _)) = handle.status() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        let addr: SocketAddr = format!("127.0.0.1:{client_port}").parse().unwrap();
        let mut map = ShardMap::new();
        map.add_member(0, 1, addr);
        let client = ShardedClient::new(ShardRouter::new(1), map, Duration::from_secs(2));
        (client, dir, handle)
    }

    fn req_with_body(headers: &[(&str, &str)], query: &[(&str, &str)], body: Value) -> Request {
        Request {
            method: "POST".to_string(),
            path: "/api/test".to_string(),
            query: query
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body: body.to_string().into_bytes(),
        }
    }

    fn store_dir_query(dir: &std::path::Path) -> Vec<(&str, &str)> {
        vec![("store_dir", dir.to_str().unwrap())]
    }

    /// Polls a fresh read-only `Store::open` until `id` shows up, since a
    /// successful `propose` only proves leader-acceptance, not that the
    /// entry has been applied to the store yet.
    fn poll_until_visible(dir: &std::path::Path, id: &Tuid) -> Option<(EntityHeader, Vec<u8>)> {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            let store = Store::open(dir, false).unwrap();
            if let Some(found) = store.get(id).unwrap() {
                return Some(found);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        None
    }

    #[test]
    fn save_entity_then_delete_entity_round_trip_through_a_real_cluster() {
        let (client, dir, handle) = single_node_cluster("save_delete_entity");

        // Seed a type directly (not through the write API — types aren't
        // this test's subject) so save_entity has a valid `type` to point
        // at.
        let type_id = Tuid::generate();
        client
            .propose(
                &type_id,
                LogRecord::Insert {
                    header: EntityHeader::new(type_id, "widget", EntityKind::EntityType, Tuid::NIL),
                    body: EntityType {
                        kind: EntityTypeKind::Structural,
                        attributes: vec![],
                        compatible_with: vec![],
                        incompatible_with: vec![],
                    }
                    .encode(),
                },
            )
            .unwrap();

        let mut body = Value::object();
        body.set("type", type_id.to_hex_string());
        body.set("name", "w1");
        let mut fields = Value::object();
        fields.set("status", "active");
        body.set("fields", fields);

        let req = req_with_body(&[], &store_dir_query(&dir), body);
        let result = save_entity(&req, &AuthMode::Insecure, &client).unwrap();
        let id = Tuid::from_hex_str(result.get("id").unwrap().as_str().unwrap()).unwrap();

        // `propose`'s returned index is only leader-acceptance, not proof
        // the entry has been applied to the store yet (see node.rs's own
        // note on this) — poll briefly rather than assuming it's already
        // visible the instant `propose` returns.
        let (header, body) = poll_until_visible(&dir, &id)
            .expect("entity should become visible in the store shortly after a successful propose");
        assert_eq!(header.name, "w1");
        let reader = gems_codec::GbvReader::new(&body).unwrap();
        assert_eq!(reader.get(field_id("status")).unwrap().1, b"active");

        let mut delete_body = Value::object();
        delete_body.set("id", id.to_hex_string());
        let req = req_with_body(&[], &store_dir_query(&dir), delete_body);
        delete_entity(&req, &AuthMode::Insecure, &client).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let store = Store::open(&dir, false).unwrap();
            if store.get(&id).unwrap().is_none() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "delete never became visible"
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        handle.shutdown();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn setup_seeds_roles_and_issues_an_admin_token_that_verifies() {
        let (client, dir, handle) = single_node_cluster("setup");
        let secret = b"webui-secret".to_vec();
        let auth = AuthMode::Enforced {
            secret: secret.clone(),
        };

        let mut body = Value::object();
        body.set("setup_secret", "webui-secret");
        body.set("admin_name", "root");
        let req = req_with_body(&[], &store_dir_query(&dir), body);
        let result = setup(&req, &auth, &client).unwrap();
        let token = result.get("admin_token").unwrap().as_str().unwrap();

        let verified = gems_abac::token::verify(&secret, token, 0).unwrap();
        let store = Store::open(&dir, false).unwrap();
        let admin_role = role_id_by_name(&store, bootstrap::ADMIN_ROLE_NAME)
            .unwrap()
            .unwrap();
        assert!(verified.roles.contains(&admin_role));

        // Running it again against the same store must be refused rather
        // than seeding a second admin role.
        let mut body = Value::object();
        body.set("setup_secret", "webui-secret");
        let req = req_with_body(&[], &store_dir_query(&dir), body);
        assert!(setup(&req, &auth, &client).is_err());

        handle.shutdown();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn setup_rejects_the_wrong_setup_secret() {
        let (client, dir, handle) = single_node_cluster("setup_wrong_secret");
        let auth = AuthMode::Enforced {
            secret: b"real-secret".to_vec(),
        };
        let mut body = Value::object();
        body.set("setup_secret", "wrong-secret");
        let req = req_with_body(&[], &store_dir_query(&dir), body);
        let err = setup(&req, &auth, &client).unwrap_err();
        assert_eq!(err.0, 403);

        handle.shutdown();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn non_admin_subject_cannot_save_a_type() {
        let (client, dir, handle) = single_node_cluster("non_admin_type");
        let secret = b"webui-secret".to_vec();
        let auth = AuthMode::Enforced {
            secret: secret.clone(),
        };

        // Seed via setup so an "admin" role exists to be denied against.
        let mut setup_body = Value::object();
        setup_body.set("setup_secret", "webui-secret");
        let req = req_with_body(&[], &store_dir_query(&dir), setup_body);
        setup(&req, &auth, &client).unwrap();

        let plain_subject = SubjectContext {
            subject_id: Tuid::generate(),
            roles: vec![],
        };
        let token = gems_abac::token::issue(&secret, &plain_subject, None);

        let mut body = Value::object();
        body.set("name", "widget");
        let req = req_with_body(
            &[("Authorization", &format!("Bearer {token}"))],
            &store_dir_query(&dir),
            body,
        );
        let err = save_type(&req, &auth, &client).unwrap_err();
        assert_eq!(err.0, 403);

        handle.shutdown();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn issue_token_mints_a_token_carrying_the_requested_roles() {
        let (client, dir, handle) = single_node_cluster("issue_token");
        let secret = b"webui-secret".to_vec();
        let auth = AuthMode::Enforced {
            secret: secret.clone(),
        };

        let mut setup_body = Value::object();
        setup_body.set("setup_secret", "webui-secret");
        setup_body.set("admin_name", "root");
        let req = req_with_body(&[], &store_dir_query(&dir), setup_body);
        let setup_result = setup(&req, &auth, &client).unwrap();
        let admin_token = setup_result.get("admin_token").unwrap().as_str().unwrap();

        let mut new_subject_body = Value::object();
        new_subject_body.set("name", "alice");
        let req = req_with_body(
            &[("Authorization", &format!("Bearer {admin_token}"))],
            &store_dir_query(&dir),
            new_subject_body,
        );
        let created = save_subject(&req, &auth, &client).unwrap();
        let alice_id = created.get("id").unwrap().as_str().unwrap();

        let mut issue_body = Value::object();
        issue_body.set("subject_id", alice_id);
        issue_body.set("role_names", vec![bootstrap::ANALYST_ROLE_NAME]);
        let req = req_with_body(
            &[("Authorization", &format!("Bearer {admin_token}"))],
            &store_dir_query(&dir),
            issue_body,
        );
        let issued = issue_token(&req, &auth).unwrap();
        let alice_token = issued.get("token").unwrap().as_str().unwrap();

        let verified = gems_abac::token::verify(&secret, alice_token, 0).unwrap();
        let store = Store::open(&dir, false).unwrap();
        let analyst_role = role_id_by_name(&store, bootstrap::ANALYST_ROLE_NAME)
            .unwrap()
            .unwrap();
        assert_eq!(verified.roles, vec![analyst_role]);

        handle.shutdown();
        std::fs::remove_dir_all(&dir).ok();
    }
}
