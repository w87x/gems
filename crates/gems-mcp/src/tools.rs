//! The three MCP tools: `query`, `get_entity`, `list_entity_types`. Each
//! takes the JSON-RPC call's `arguments` object plus the server's
//! `AuthMode` and returns a `gems_json::Value` result or a plain-string
//! error — `protocol.rs` is what wraps either into the MCP
//! `content`/`isError` shape.
//!
//! **Authentication**: by default (`AuthMode::Enforced`), every call must
//! carry a valid `auth_token` argument (a token `gems-abac::token::issue`
//! produced), and every read goes through `Store::*_enforced` using the
//! `SubjectContext` that token verifies to. This replaced an earlier
//! scheme where a call could just pass `subject`/`roles` arguments
//! directly — nothing verified the caller actually *was* that subject, so
//! any MCP client could read as anyone by naming a different subject id.
//! `AuthMode::Insecure` restores raw, unauthenticated access — an
//! explicit opt-in, never the default.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use gems_abac::token::AuthMode;
use gems_abac::SubjectContext;
use gems_catalog::{EntityHeader, EntityKind};
use gems_common::Tuid;
use gems_engine::Store;
use gems_json::Value;

type ToolResult = Result<Value, String>;

fn open_store(args: &Value) -> Result<Store, String> {
    let dir = args
        .get("store_dir")
        .and_then(Value::as_str)
        .ok_or("store_dir is required")?;
    Store::open(Path::new(dir), false).map_err(|e| e.to_string())
}

/// Authorizes a tool call under `auth`, using the call's `auth_token`
/// argument (if any) as the bearer token. `Ok(None)` only ever happens in
/// `Insecure` mode — under `Enforced`, a missing/invalid/expired token is
/// always an `Err`, never a silent fall-through to raw access.
fn authorize(args: &Value, auth: &AuthMode) -> Result<Option<SubjectContext>, String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let token = args.get("auth_token").and_then(Value::as_str);
    auth.authorize(token, now).map_err(|e| e.to_string())
}

fn entity_summary(header: &EntityHeader) -> Value {
    let mut v = Value::object();
    v.set("id", header.id.to_hex_string());
    v.set("name", header.name.clone());
    v.set("description", header.description.clone());
    v.set("kind", format!("{:?}", header.entity_kind));
    v.set("schema_ref", header.schema_ref.to_hex_string());
    v
}

pub fn query(args: &Value, auth: &AuthMode) -> ToolResult {
    let subject = authorize(args, auth)?;
    let store = open_store(args)?;
    let query_str = args
        .get("query")
        .and_then(Value::as_str)
        .ok_or("query is required")?;
    let query = gems_query::parse(query_str).map_err(|e| e.to_string())?;

    let mut results = Value::array();
    match subject {
        Some(subject) => {
            for (header, _) in store
                .query_enforced(&query, &subject)
                .map_err(|e| e.to_string())?
            {
                results.push(entity_summary(&header));
            }
        }
        None => {
            for id in store.query(&query).map_err(|e| e.to_string())? {
                if let Some((header, _)) = store.get(&id).map_err(|e| e.to_string())? {
                    results.push(entity_summary(&header));
                }
            }
        }
    }
    Ok(results)
}

pub fn get_entity(args: &Value, auth: &AuthMode) -> ToolResult {
    let subject = authorize(args, auth)?;
    let store = open_store(args)?;
    let id_hex = args
        .get("id")
        .and_then(Value::as_str)
        .ok_or("id is required")?;
    let id = Tuid::from_hex_str(id_hex).ok_or("invalid id: must be 48 hex characters")?;

    let found = match subject {
        Some(subject) => store
            .get_enforced(&id, &subject)
            .map_err(|e| e.to_string())?,
        None => store.get(&id).map_err(|e| e.to_string())?,
    };
    let Some((header, body)) = found else {
        return Err("no such entity (or not visible to this subject)".to_string());
    };

    let mut result = entity_summary(&header);
    if header.entity_kind == EntityKind::Data {
        if let Ok(reader) = gems_codec::GbvReader::new(&body) {
            let mut fields = Value::object();
            for key in reader.key_ids() {
                if let Some((gems_codec::TypeTag::Str, value)) = reader.get(key) {
                    fields.set(
                        &key.to_string(),
                        String::from_utf8_lossy(value).into_owned(),
                    );
                }
            }
            result.set("fields", fields);
        }
    }
    Ok(result)
}

pub fn list_entity_types(args: &Value, auth: &AuthMode) -> ToolResult {
    authorize(args, auth)?;
    let store = open_store(args)?;
    let mut results = Value::array();
    for id in store
        .query_by_kind(EntityKind::EntityType)
        .map_err(|e| e.to_string())?
    {
        if let Some((header, _)) = store.get(&id).map_err(|e| e.to_string())? {
            results.push(entity_summary(&header));
        }
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gems_catalog::{EntityFlags, EntityType, EntityTypeKind};
    use gems_codec::{GbvBuilder, TypeTag};
    use gems_engine::Store as EngineStore;
    use std::path::PathBuf;

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("gems-mcp-tools-test")
            .join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn header(id: Tuid, name: &str, kind: EntityKind, schema_ref: Tuid) -> EntityHeader {
        EntityHeader {
            id,
            created_by: [0u8; 16],
            modified_by: [0u8; 16],
            modified_at_ns: 0,
            name: name.to_string(),
            description: String::new(),
            flags: EntityFlags::NONE,
            entity_kind: kind,
            schema_ref,
            body_offset: 0,
            body_len: 0,
        }
    }

    fn seed_store(dir: &Path) -> (Tuid, Tuid) {
        let mut store = EngineStore::create(dir).unwrap();
        let type_id = Tuid::generate();
        store
            .insert(
                header(type_id, "widget", EntityKind::EntityType, Tuid::NIL),
                &EntityType {
                    kind: EntityTypeKind::Structural,
                    attributes: vec![],
                    compatible_with: vec![],
                    incompatible_with: vec![],
                }
                .encode(),
            )
            .unwrap();

        let entity_id = Tuid::generate();
        let mut body = GbvBuilder::new();
        body.push(1, TypeTag::Str, b"active");
        store
            .insert(
                header(entity_id, "w1", EntityKind::Data, type_id),
                &body.finish(),
            )
            .unwrap();
        (type_id, entity_id)
    }

    fn args_with(pairs: &[(&str, &str)]) -> Value {
        let mut v = Value::object();
        for (k, val) in pairs {
            v.set(k, *val);
        }
        v
    }

    #[test]
    fn list_entity_types_returns_the_seeded_type() {
        let dir = tmp_dir("list_types");
        seed_store(&dir);
        let args = args_with(&[("store_dir", dir.to_str().unwrap())]);
        let result = list_entity_types(&args, &AuthMode::Insecure).unwrap();
        let types = result.as_array().unwrap();
        assert_eq!(types.len(), 1);
        assert_eq!(types[0].get("name").unwrap().as_str(), Some("widget"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn get_entity_returns_fields_for_a_data_entity() {
        let dir = tmp_dir("get_entity");
        let (_, entity_id) = seed_store(&dir);
        let args = args_with(&[
            ("store_dir", dir.to_str().unwrap()),
            ("id", &entity_id.to_hex_string()),
        ]);
        let result = get_entity(&args, &AuthMode::Insecure).unwrap();
        assert_eq!(result.get("name").unwrap().as_str(), Some("w1"));
        assert_eq!(
            result.get("fields").unwrap().get("1").unwrap().as_str(),
            Some("active")
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn get_entity_with_invalid_id_is_an_error() {
        let dir = tmp_dir("get_entity_bad_id");
        seed_store(&dir);
        let args = args_with(&[("store_dir", dir.to_str().unwrap()), ("id", "not-hex")]);
        assert!(get_entity(&args, &AuthMode::Insecure).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn query_finds_the_seeded_data_entity_by_type() {
        let dir = tmp_dir("query_tool");
        let (_, entity_id) = seed_store(&dir);
        let mut args = Value::object();
        args.set("store_dir", dir.to_str().unwrap());
        args.set("query", "SELECT * FROM entities WHERE type IN (widget)");
        let result = query(&args, &AuthMode::Insecure).unwrap();
        let matches = result.as_array().unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(
            matches[0].get("id").unwrap().as_str(),
            Some(entity_id.to_hex_string().as_str())
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn query_with_missing_store_dir_is_an_error() {
        let args = Value::object();
        assert!(query(&args, &AuthMode::Insecure).is_err());
    }

    #[test]
    fn enforced_mode_without_a_token_is_an_error() {
        let dir = tmp_dir("query_no_token");
        seed_store(&dir);
        let mut args = Value::object();
        args.set("store_dir", dir.to_str().unwrap());
        args.set("query", "SELECT * FROM entities WHERE type IN (widget)");
        let auth = AuthMode::Enforced {
            secret: b"secret".to_vec(),
        };
        assert!(query(&args, &auth).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn enforced_query_with_a_valid_token_sees_nothing_without_a_permit_policy() {
        let dir = tmp_dir("query_enforced");
        seed_store(&dir);
        let secret = b"secret".to_vec();
        let subject = SubjectContext {
            subject_id: Tuid::generate(),
            roles: vec![],
        };
        let token = gems_abac::token::issue(&secret, &subject, None);
        let mut args = Value::object();
        args.set("store_dir", dir.to_str().unwrap());
        args.set("query", "SELECT * FROM entities WHERE type IN (widget)");
        args.set("auth_token", token);
        let auth = AuthMode::Enforced { secret };
        let result = query(&args, &auth).unwrap();
        assert!(result.as_array().unwrap().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_token_signed_with_a_different_secret_is_rejected() {
        let dir = tmp_dir("query_wrong_secret");
        seed_store(&dir);
        let subject = SubjectContext {
            subject_id: Tuid::generate(),
            roles: vec![],
        };
        let token = gems_abac::token::issue(b"attacker-secret", &subject, None);
        let mut args = Value::object();
        args.set("store_dir", dir.to_str().unwrap());
        args.set("query", "SELECT * FROM entities WHERE type IN (widget)");
        args.set("auth_token", token);
        let auth = AuthMode::Enforced {
            secret: b"real-secret".to_vec(),
        };
        assert!(query(&args, &auth).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
