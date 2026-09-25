//! `gems`: the CLI frontend from ARCHITECTURE.md §9 — a thin, hand-rolled-
//! arg-parsing wrapper over `gems-engine::Store`.
//!
//! `entity get`/`query` default to raw (unenforced) reads — this is an
//! administrative tool operating directly on the store's files, not a
//! multi-tenant client, so bypassing ABAC by default is the same call a
//! DBA console makes. Passing `--as <subject-hex>[,<role-hex>,...]`
//! switches to `Store::get_enforced`/`query_enforced` so policies can
//! actually be exercised and tested from the command line.
//!
//! Commands:
//!   gems init <dir>
//!   gems type create <dir> <name>
//!   gems type list <dir>
//!   gems entity create <dir> --type <type-name> --name <name> [key=value ...]
//!   gems entity get <dir> <id-hex> [--as <subject-hex>[,<role-hex>,...]]
//!   gems policy create <dir> --effect <permit|deny>
//!       [--target-kind <kind>] [--target-type <type-name>]
//!       [--subject <id-hex>] [--role <id-hex>] [--redact <field-name> ...]
//!   gems query <dir> "<query string>" [--as <subject-hex>[,<role-hex>,...]]

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use gems_abac::SubjectContext;
use gems_catalog::{
    Effect, EntityFlags, EntityHeader, EntityKind, EntityType, EntityTypeKind, Policy,
    SubjectPredicate, TargetPredicate,
};
use gems_codec::{GbvBuilder, GbvReader, TypeTag};
use gems_common::Tuid;
use gems_engine::Store;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("init") => cmd_init(&args[1..]),
        Some("type") => match args.get(1).map(String::as_str) {
            Some("create") => cmd_type_create(&args[2..]),
            Some("list") => cmd_type_list(&args[2..]),
            _ => Err(usage()),
        },
        Some("entity") => match args.get(1).map(String::as_str) {
            Some("create") => cmd_entity_create(&args[2..]),
            Some("get") => cmd_entity_get(&args[2..]),
            _ => Err(usage()),
        },
        Some("policy") => match args.get(1).map(String::as_str) {
            Some("create") => cmd_policy_create(&args[2..]),
            _ => Err(usage()),
        },
        Some("query") => cmd_query(&args[1..]),
        _ => Err(usage()),
    }
}

fn usage() -> String {
    "usage:\n\
     \x20 gems init <dir>\n\
     \x20 gems type create <dir> <name>\n\
     \x20 gems type list <dir>\n\
     \x20 gems entity create <dir> --type <type-name> --name <name> [key=value ...]\n\
     \x20 gems entity get <dir> <id-hex> [--as <subject-hex>[,<role-hex>,...]]\n\
     \x20 gems policy create <dir> --effect <permit|deny>\n\
     \x20     [--target-kind <kind>] [--target-type <type-name>]\n\
     \x20     [--subject <id-hex>] [--role <id-hex>] [--redact <field-name> ...]\n\
     \x20 gems query <dir> \"<query string>\" [--as <subject-hex>[,<role-hex>,...]]"
        .to_string()
}

/// Parses a trailing `--as <subject-hex>[,<role-hex>,...]` flag out of
/// `args`, returning the remaining args and the parsed `SubjectContext` (if
/// present). Kept separate from each command's own flag parsing since both
/// `entity get` and `query` accept it identically.
fn extract_as_flag(args: &[String]) -> Result<(Vec<String>, Option<SubjectContext>), String> {
    let mut remaining = Vec::with_capacity(args.len());
    let mut subject = None;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--as" {
            let value = args.get(i + 1).ok_or("--as needs a value")?;
            let mut parts = value.split(',');
            let subject_hex = parts.next().ok_or("--as needs a subject id")?;
            let subject_id = Tuid::from_hex_str(subject_hex).ok_or("invalid subject id")?;
            let mut roles = Vec::new();
            for role_hex in parts {
                roles.push(Tuid::from_hex_str(role_hex).ok_or("invalid role id")?);
            }
            subject = Some(SubjectContext { subject_id, roles });
            i += 2;
        } else {
            remaining.push(args[i].clone());
            i += 1;
        }
    }
    Ok((remaining, subject))
}

fn cmd_init(args: &[String]) -> Result<(), String> {
    let dir = require_arg(args, 0, "dir")?;
    Store::create(Path::new(dir)).map_err(|e| e.to_string())?;
    println!("initialized store at {dir}");
    Ok(())
}

fn cmd_type_create(args: &[String]) -> Result<(), String> {
    let dir = require_arg(args, 0, "dir")?;
    let name = require_arg(args, 1, "name")?;

    let mut store = open_store(dir, true)?;

    let entity_type = EntityType {
        kind: EntityTypeKind::Structural,
        attributes: vec![],
        compatible_with: vec![],
        incompatible_with: vec![],
    };
    let id = Tuid::generate();
    let header = new_header(id, name, EntityKind::EntityType, Tuid::NIL);
    store
        .insert(header, &entity_type.encode())
        .map_err(|e| e.to_string())?;

    println!("{}", id.to_hex_string());
    Ok(())
}

fn cmd_type_list(args: &[String]) -> Result<(), String> {
    let dir = require_arg(args, 0, "dir")?;
    let store = open_store(dir, false)?;

    let ids = store
        .query_by_kind(EntityKind::EntityType)
        .map_err(|e| e.to_string())?;
    for id in ids {
        if let Some((header, _)) = store.get(&id).map_err(|e| e.to_string())? {
            println!("{}  {}", id.to_hex_string(), header.name);
        }
    }
    Ok(())
}

type EntityCreateArgs = (String, String, Vec<(String, String)>);

/// Parses `--type <name> --name <name> key=value key2=value2 ...` into
/// (type name, entity name, [(key, value)]).
fn parse_entity_create_args(args: &[String]) -> Result<EntityCreateArgs, String> {
    let mut type_name = None;
    let mut entity_name = None;
    let mut fields = Vec::new();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--type" => {
                type_name = Some(args.get(i + 1).ok_or("--type needs a value")?.clone());
                i += 2;
            }
            "--name" => {
                entity_name = Some(args.get(i + 1).ok_or("--name needs a value")?.clone());
                i += 2;
            }
            other => {
                let (key, value) = other
                    .split_once('=')
                    .ok_or_else(|| format!("expected key=value, found '{other}'"))?;
                fields.push((key.to_string(), value.to_string()));
                i += 1;
            }
        }
    }

    Ok((
        type_name.ok_or("--type is required")?,
        entity_name.ok_or("--name is required")?,
        fields,
    ))
}

fn cmd_entity_create(args: &[String]) -> Result<(), String> {
    let dir = args.first().ok_or("missing <dir>")?;
    let (type_name, entity_name, fields) = parse_entity_create_args(&args[1..])?;

    let mut store = open_store(dir, true)?;
    let type_id = store
        .find_entity_type_by_name(&type_name)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no such entity type: {type_name}"))?;

    // Field ids are the field name's bytes hashed into a u32 for this v1
    // CLI — real attribute-id assignment goes through EntityAttribute
    // definitions once the schema-driven write path exists; this is
    // enough to write/read fields back out consistently for now.
    let mut body = GbvBuilder::new();
    for (key, value) in &fields {
        body.push(field_id(key), TypeTag::Str, value.as_bytes());
    }
    let body = body.finish();

    let id = Tuid::generate();
    let header = new_header(id, &entity_name, EntityKind::Data, type_id);
    store.insert(header, &body).map_err(|e| e.to_string())?;

    println!("{}", id.to_hex_string());
    Ok(())
}

fn cmd_entity_get(args: &[String]) -> Result<(), String> {
    let (args, subject) = extract_as_flag(args)?;
    let dir = require_arg(&args, 0, "dir")?;
    let id_hex = require_arg(&args, 1, "id")?;
    let id = Tuid::from_hex_str(id_hex).ok_or("invalid entity id")?;

    let store = open_store(dir, false)?;
    let result = match &subject {
        Some(subject) => store.get_enforced(&id, subject),
        None => store.get(&id),
    };
    let Some((header, body)) = result.map_err(|e| e.to_string())? else {
        return Err("no such entity".to_string());
    };

    println!("id:          {}", header.id.to_hex_string());
    println!("name:        {}", header.name);
    println!("description: {}", header.description);
    println!("kind:        {:?}", header.entity_kind);
    println!("schema_ref:  {}", header.schema_ref.to_hex_string());
    if header.entity_kind == EntityKind::Data {
        if let Ok(reader) = GbvReader::new(&body) {
            for key in reader.key_ids() {
                if let Some((TypeTag::Str, value)) = reader.get(key) {
                    println!("  field[{key}] = {}", String::from_utf8_lossy(value));
                }
            }
        }
    }
    Ok(())
}

fn cmd_query(args: &[String]) -> Result<(), String> {
    let (args, subject) = extract_as_flag(args)?;
    let dir = require_arg(&args, 0, "dir")?;
    let query_str = require_arg(&args, 1, "query")?;

    let store = open_store(dir, false)?;
    let query = gems_query::parse(query_str).map_err(|e| e.to_string())?;

    match subject {
        Some(subject) => {
            for (header, _) in store
                .query_enforced(&query, &subject)
                .map_err(|e| e.to_string())?
            {
                println!("{}  {}", header.id.to_hex_string(), header.name);
            }
        }
        None => {
            for id in store.query(&query).map_err(|e| e.to_string())? {
                if let Some((header, _)) = store.get(&id).map_err(|e| e.to_string())? {
                    println!("{}  {}", id.to_hex_string(), header.name);
                }
            }
        }
    }
    Ok(())
}

/// Parses `--effect <permit|deny> [--target-kind <kind>] [--target-type
/// <name>] [--subject <id-hex>] [--role <id-hex>] [--redact <field> ...]`.
fn parse_policy_create_args(args: &[String]) -> Result<PolicyCreateArgs, String> {
    let mut effect = None;
    let mut target_kind = None;
    let mut target_type_name = None;
    let mut subject_id = Tuid::NIL;
    let mut role_id = Tuid::NIL;
    let mut redact = Vec::new();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--effect" => {
                i += 1;
                effect = Some(match args.get(i).map(String::as_str) {
                    Some("permit") => Effect::Permit,
                    Some("deny") => Effect::Deny,
                    _ => return Err("--effect must be `permit` or `deny`".to_string()),
                });
                i += 1;
            }
            "--target-kind" => {
                i += 1;
                let kind_str = args.get(i).ok_or("--target-kind needs a value")?;
                target_kind = Some(parse_entity_kind(kind_str)?);
                i += 1;
            }
            "--target-type" => {
                i += 1;
                target_type_name = Some(args.get(i).ok_or("--target-type needs a value")?.clone());
                i += 1;
            }
            "--subject" => {
                i += 1;
                let hex = args.get(i).ok_or("--subject needs a value")?;
                subject_id = Tuid::from_hex_str(hex).ok_or("invalid --subject id")?;
                i += 1;
            }
            "--role" => {
                i += 1;
                let hex = args.get(i).ok_or("--role needs a value")?;
                role_id = Tuid::from_hex_str(hex).ok_or("invalid --role id")?;
                i += 1;
            }
            "--redact" => {
                i += 1;
                redact.push(field_id(args.get(i).ok_or("--redact needs a value")?));
                i += 1;
            }
            other => return Err(format!("unrecognized flag '{other}'")),
        }
    }

    Ok((
        effect.ok_or("--effect is required")?,
        target_kind,
        target_type_name,
        subject_id,
        role_id,
        redact,
    ))
}

type PolicyCreateArgs = (
    Effect,
    Option<EntityKind>,
    Option<String>,
    Tuid,
    Tuid,
    Vec<u32>,
);

fn cmd_policy_create(args: &[String]) -> Result<(), String> {
    let dir = args.first().ok_or("missing <dir>")?;
    let (effect, target_kind, target_type_name, subject_id, role_id, redact_attributes) =
        parse_policy_create_args(&args[1..])?;

    let mut store = open_store(dir, true)?;
    let target_schema_ref = match target_type_name {
        Some(name) => store
            .find_entity_type_by_name(&name)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no such entity type: {name}"))?,
        None => Tuid::NIL,
    };

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
    let header = new_header(id, "policy", EntityKind::Policy, Tuid::NIL);
    store
        .insert(header, &policy.encode())
        .map_err(|e| e.to_string())?;

    println!("{}", id.to_hex_string());
    Ok(())
}

fn parse_entity_kind(s: &str) -> Result<EntityKind, String> {
    Ok(match s.to_ascii_lowercase().as_str() {
        "data" => EntityKind::Data,
        "entityattribute" => EntityKind::EntityAttribute,
        "entitytype" => EntityKind::EntityType,
        "subject" => EntityKind::Subject,
        "layer" => EntityKind::Layer,
        "layergroup" => EntityKind::LayerGroup,
        "variantlist" => EntityKind::VariantList,
        "role" => EntityKind::Role,
        "linktype" => EntityKind::LinkType,
        "link" => EntityKind::Link,
        "policy" => EntityKind::Policy,
        other => return Err(format!("unknown entity kind: {other}")),
    })
}

fn open_store(dir: &str, writable: bool) -> Result<Store, String> {
    Store::open(&PathBuf::from(dir), writable).map_err(|e| e.to_string())
}

fn require_arg<'a>(args: &'a [String], index: usize, name: &str) -> Result<&'a str, String> {
    args.get(index)
        .map(String::as_str)
        .ok_or_else(|| format!("missing <{name}>"))
}

fn new_header(id: Tuid, name: &str, kind: EntityKind, schema_ref: Tuid) -> EntityHeader {
    let system = Tuid::generate().uuid(); // placeholder "created_by" until Subject/session plumbing exists
    EntityHeader {
        id,
        created_by: system,
        modified_by: system,
        modified_at_ns: id.created_at_ns() as i64,
        name: name.to_string(),
        description: String::new(),
        flags: EntityFlags::NONE,
        entity_kind: kind,
        schema_ref,
        body_offset: 0,
        body_len: 0,
    }
}

/// FNV-1a, folding a field name into the u32 attribute id used as a GBV
/// key. A real schema-driven write path assigns these from
/// `EntityAttribute` definitions instead; this is a CLI-only placeholder
/// for typing simple `key=value` pairs by hand.
fn field_id(name: &str) -> u32 {
    let mut hash: u32 = 0x811c9dc5;
    for b in name.as_bytes() {
        hash ^= *b as u32;
        hash = hash.wrapping_mul(0x01000193);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("gems-cli-test")
            .join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn parses_entity_create_args() {
        let args: Vec<String> = ["--type", "widget", "--name", "w1", "status=active", "qty=5"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (type_name, entity_name, fields) = parse_entity_create_args(&args).unwrap();
        assert_eq!(type_name, "widget");
        assert_eq!(entity_name, "w1");
        assert_eq!(
            fields,
            vec![
                ("status".to_string(), "active".to_string()),
                ("qty".to_string(), "5".to_string()),
            ]
        );
    }

    #[test]
    fn rejects_missing_type_flag() {
        let args: Vec<String> = ["--name", "w1"].iter().map(|s| s.to_string()).collect();
        assert!(parse_entity_create_args(&args).is_err());
    }

    #[test]
    fn field_id_is_stable_and_distinguishes_names() {
        assert_eq!(field_id("status"), field_id("status"));
        assert_ne!(field_id("status"), field_id("qty"));
    }

    #[test]
    fn end_to_end_init_type_entity_query() {
        let dir = tmp_dir("e2e");
        let dir_str = dir.to_str().unwrap().to_string();

        cmd_init(std::slice::from_ref(&dir_str)).unwrap();
        cmd_type_create(&[dir_str.clone(), "widget".to_string()]).unwrap();

        cmd_entity_create(&[
            dir_str.clone(),
            "--type".to_string(),
            "widget".to_string(),
            "--name".to_string(),
            "w1".to_string(),
            "status=active".to_string(),
        ])
        .unwrap();

        // Re-derive the id by querying, since cmd_entity_create only prints it.
        let store = open_store(&dir_str, false).unwrap();
        let type_id = store.find_entity_type_by_name("widget").unwrap().unwrap();
        let ids = store.query_by_schema_ref(&type_id).unwrap();
        assert_eq!(ids.len(), 1);
        let (header, body) = store.get(&ids[0]).unwrap().unwrap();
        assert_eq!(header.name, "w1");
        let reader = GbvReader::new(&body).unwrap();
        assert_eq!(reader.get(field_id("status")).unwrap().1, b"active");

        let query = gems_query::parse("SELECT * FROM entities WHERE type IN (widget)").unwrap();
        let queried = store.query(&query).unwrap();
        assert_eq!(queried, vec![ids[0]]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn abac_end_to_end_default_deny_then_permit_via_policy() {
        let dir = tmp_dir("abac_e2e");
        let dir_str = dir.to_str().unwrap().to_string();
        let anon_hex = Tuid::NIL.to_hex_string();

        cmd_init(std::slice::from_ref(&dir_str)).unwrap();
        cmd_type_create(&[dir_str.clone(), "widget".to_string()]).unwrap();
        cmd_entity_create(&[
            dir_str.clone(),
            "--type".to_string(),
            "widget".to_string(),
            "--name".to_string(),
            "w1".to_string(),
        ])
        .unwrap();

        let store = open_store(&dir_str, false).unwrap();
        let type_id = store.find_entity_type_by_name("widget").unwrap().unwrap();
        let entity_id = store.query_by_schema_ref(&type_id).unwrap()[0];
        drop(store);

        // Raw (unenforced) read always works.
        cmd_entity_get(&[dir_str.clone(), entity_id.to_hex_string()]).unwrap();

        // Enforced read with no policies at all: default deny.
        let denied = cmd_entity_get(&[
            dir_str.clone(),
            entity_id.to_hex_string(),
            "--as".to_string(),
            anon_hex.clone(),
        ]);
        assert!(
            denied.is_err(),
            "no policy exists yet, so this must be denied"
        );

        // A permit-all-of-type-widget policy makes it visible.
        cmd_policy_create(&[
            dir_str.clone(),
            "--effect".to_string(),
            "permit".to_string(),
            "--target-kind".to_string(),
            "data".to_string(),
            "--target-type".to_string(),
            "widget".to_string(),
        ])
        .unwrap();

        cmd_entity_get(&[
            dir_str.clone(),
            entity_id.to_hex_string(),
            "--as".to_string(),
            anon_hex,
        ])
        .unwrap();

        // Enforced query also reflects the policy.
        let store = open_store(&dir_str, false).unwrap();
        let query = gems_query::parse("SELECT * FROM entities WHERE type IN (widget)").unwrap();
        let subject = SubjectContext {
            subject_id: Tuid::NIL,
            roles: vec![],
        };
        let results = store.query_enforced(&query, &subject).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0.id, entity_id);

        std::fs::remove_dir_all(&dir).ok();
    }
}
