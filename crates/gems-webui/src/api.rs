//! JSON API routes: `/api/types`, `/api/query`, `/api/entity`. All `GET`
//! with query-string parameters — see `main.rs`'s module doc for why this
//! pass is read-only (writes go through the CLI or MCP for now).
//!
//! **Authentication**: by default (`AuthMode::Enforced`), every request
//! must carry a valid, unexpired `Authorization: Bearer <token>` header —
//! a token `gems-abac::token::issue` produced — and every read goes
//! through `Store::*_enforced` using the `SubjectContext` that token
//! verifies to. This replaced an earlier scheme where a request could
//! just pass `?subject=<hex>` directly: nothing verified that the caller
//! actually *was* that subject, so anyone could read as anyone by editing
//! a query parameter. `AuthMode::Insecure` (the `--insecure` startup flag)
//! restores unauthenticated, unenforced raw access — an explicit opt-in
//! for trusted-network/local-testing use, never the default.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use gems_abac::token::AuthMode;
use gems_abac::SubjectContext;
use gems_catalog::{EntityHeader, EntityKind};
use gems_common::Tuid;
use gems_engine::Store;
use gems_json::Value;

use crate::http::Request;

pub type ApiResult = Result<Value, (u16, String)>;

pub(crate) fn open_store(req: &Request) -> Result<Store, (u16, String)> {
    let dir = req
        .query_param("store_dir")
        .ok_or((400, "store_dir is required".to_string()))?;
    Store::open(Path::new(dir), false).map_err(|e| (400, e.to_string()))
}

/// Authenticates `req` under `auth`, returning the `SubjectContext` to
/// enforce reads under. `Ok(None)` only ever happens in `Insecure` mode —
/// under `Enforced`, a missing/invalid/expired token is always an `Err`
/// (401), never a silent fall-through to raw access.
pub(crate) fn authorize(
    req: &Request,
    auth: &AuthMode,
) -> Result<Option<SubjectContext>, (u16, String)> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    auth.authorize(req.bearer_token(), now)
        .map_err(|e| (401, e.to_string()))
}

pub(crate) fn entity_summary(header: &EntityHeader) -> Value {
    let mut v = Value::object();
    v.set("id", header.id.to_hex_string());
    v.set("name", header.name.clone());
    v.set("description", header.description.clone());
    v.set("kind", format!("{:?}", header.entity_kind));
    v.set("schema_ref", header.schema_ref.to_hex_string());
    v
}

pub fn list_types(req: &Request, auth: &AuthMode) -> ApiResult {
    authorize(req, auth)?;
    let store = open_store(req)?;
    let mut results = Value::array();
    for id in store
        .query_by_kind(EntityKind::EntityType)
        .map_err(|e| (500, e.to_string()))?
    {
        if let Some((header, _)) = store.get(&id).map_err(|e| (500, e.to_string()))? {
            results.push(entity_summary(&header));
        }
    }
    Ok(results)
}

pub fn query(req: &Request, auth: &AuthMode) -> ApiResult {
    let subject = authorize(req, auth)?;
    let store = open_store(req)?;
    let query_str = req
        .query_param("q")
        .ok_or((400, "q (query string) is required".to_string()))?;
    let parsed = gems_query::parse(query_str).map_err(|e| (400, e.to_string()))?;

    let mut results = Value::array();
    match subject {
        Some(subject) => {
            for (header, _) in store
                .query_enforced(&parsed, &subject)
                .map_err(|e| (500, e.to_string()))?
            {
                results.push(entity_summary(&header));
            }
        }
        None => {
            for id in store.query(&parsed).map_err(|e| (500, e.to_string()))? {
                if let Some((header, _)) = store.get(&id).map_err(|e| (500, e.to_string()))? {
                    results.push(entity_summary(&header));
                }
            }
        }
    }
    Ok(results)
}

pub fn get_entity(req: &Request, auth: &AuthMode) -> ApiResult {
    let subject = authorize(req, auth)?;
    let store = open_store(req)?;
    let id_hex = req
        .query_param("id")
        .ok_or((400, "id is required".to_string()))?;
    let id = Tuid::from_hex_str(id_hex).ok_or((400, "invalid id".to_string()))?;

    let found = match subject {
        Some(subject) => store
            .get_enforced(&id, &subject)
            .map_err(|e| (500, e.to_string()))?,
        None => store.get(&id).map_err(|e| (500, e.to_string()))?,
    };
    let Some((header, body)) = found else {
        return Err((404, "no such entity".to_string()));
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

/// Turn an `ApiResult` into a JSON body: `{"ok": true, "data": ...}` or
/// `{"ok": false, "error": "..."}`, always with a `200` transport status
/// so the frontend's `fetch` calls don't need to special-case HTTP-level
/// errors separately from application-level ones — the `ok` field is
/// the single thing callers branch on.
pub fn to_response_body(result: ApiResult) -> String {
    let mut body = Value::object();
    match result {
        Ok(data) => {
            body.set("ok", true);
            body.set("data", data);
        }
        Err((_, message)) => {
            body.set("ok", false);
            body.set("error", message);
        }
    }
    body.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gems_catalog::{EntityFlags, EntityType, EntityTypeKind};
    use gems_codec::{GbvBuilder, TypeTag};
    use std::path::PathBuf;

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("gems-webui-api-test")
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

    fn seed(dir: &Path) -> (Tuid, Tuid) {
        let mut store = Store::create(dir).unwrap();
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

    fn req(pairs: &[(&str, &str)]) -> Request {
        req_with_headers(pairs, &[])
    }

    fn req_with_headers(pairs: &[(&str, &str)], headers: &[(&str, &str)]) -> Request {
        Request {
            method: "GET".to_string(),
            path: "/api/test".to_string(),
            query: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body: Vec::new(),
        }
    }

    fn bearer_req(pairs: &[(&str, &str)], token: &str) -> Request {
        req_with_headers(pairs, &[("Authorization", &format!("Bearer {token}"))])
    }

    #[test]
    fn list_types_returns_the_seeded_type() {
        let dir = tmp_dir("list_types");
        seed(&dir);
        let result = list_types(
            &req(&[("store_dir", dir.to_str().unwrap())]),
            &AuthMode::Insecure,
        )
        .unwrap();
        assert_eq!(result.as_array().unwrap().len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn query_finds_the_seeded_entity() {
        let dir = tmp_dir("query");
        let (_, entity_id) = seed(&dir);
        let result = query(
            &req(&[
                ("store_dir", dir.to_str().unwrap()),
                ("q", "SELECT * FROM entities WHERE type IN (widget)"),
            ]),
            &AuthMode::Insecure,
        )
        .unwrap();
        let matches = result.as_array().unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(
            matches[0].get("id").unwrap().as_str(),
            Some(entity_id.to_hex_string().as_str())
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn get_entity_includes_fields() {
        let dir = tmp_dir("get_entity");
        let (_, entity_id) = seed(&dir);
        let result = get_entity(
            &req(&[
                ("store_dir", dir.to_str().unwrap()),
                ("id", &entity_id.to_hex_string()),
            ]),
            &AuthMode::Insecure,
        )
        .unwrap();
        assert_eq!(
            result.get("fields").unwrap().get("1").unwrap().as_str(),
            Some("active")
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_store_dir_is_a_400() {
        let err = list_types(&req(&[]), &AuthMode::Insecure).unwrap_err();
        assert_eq!(err.0, 400);
    }

    #[test]
    fn missing_entity_is_a_404() {
        let dir = tmp_dir("missing_entity");
        seed(&dir);
        let err = get_entity(
            &req(&[
                ("store_dir", dir.to_str().unwrap()),
                ("id", &Tuid::generate().to_hex_string()),
            ]),
            &AuthMode::Insecure,
        )
        .unwrap_err();
        assert_eq!(err.0, 404);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn enforced_mode_without_a_token_is_a_401() {
        let dir = tmp_dir("enforced_no_token");
        seed(&dir);
        let auth = AuthMode::Enforced {
            secret: b"test-secret".to_vec(),
        };
        let err = list_types(&req(&[("store_dir", dir.to_str().unwrap())]), &auth).unwrap_err();
        assert_eq!(err.0, 401);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn enforced_mode_rejects_a_token_signed_with_a_different_secret() {
        let dir = tmp_dir("enforced_wrong_secret");
        seed(&dir);
        let subject = SubjectContext {
            subject_id: Tuid::generate(),
            roles: vec![],
        };
        let token = gems_abac::token::issue(b"attacker-secret", &subject, None);
        let auth = AuthMode::Enforced {
            secret: b"real-secret".to_vec(),
        };
        let req = bearer_req(&[("store_dir", dir.to_str().unwrap())], &token);
        let err = list_types(&req, &auth).unwrap_err();
        assert_eq!(err.0, 401);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn enforced_mode_with_a_valid_token_enforces_abac_rather_than_raw_access() {
        // No policies exist for this subject, so an enforced query must
        // see nothing even though the entity exists — proving this path
        // actually goes through query_enforced, not the raw store.
        let dir = tmp_dir("enforced_sees_nothing_without_policy");
        seed(&dir);
        let secret = b"test-secret".to_vec();
        let subject = SubjectContext {
            subject_id: Tuid::generate(),
            roles: vec![],
        };
        let token = gems_abac::token::issue(&secret, &subject, None);
        let auth = AuthMode::Enforced { secret };
        let req = bearer_req(
            &[
                ("store_dir", dir.to_str().unwrap()),
                ("q", "SELECT * FROM entities WHERE type IN (widget)"),
            ],
            &token,
        );
        let result = query(&req, &auth).unwrap();
        assert_eq!(result.as_array().unwrap().len(), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn to_response_body_wraps_ok_and_error_shapes() {
        let ok_body = to_response_body(Ok(Value::Number(1.0)));
        let parsed = gems_json::parse(&ok_body).unwrap();
        assert_eq!(parsed.get("ok"), Some(&Value::Bool(true)));

        let err_body = to_response_body(Err((404, "nope".to_string())));
        let parsed = gems_json::parse(&err_body).unwrap();
        assert_eq!(parsed.get("ok"), Some(&Value::Bool(false)));
        assert_eq!(parsed.get("error").unwrap().as_str(), Some("nope"));
    }
}
