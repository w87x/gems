//! `gems`: the CLI frontend from ARCHITECTURE.md §9 — a thin, hand-rolled-
//! arg-parsing wrapper over `gems-engine::Store`. No ABAC enforcement is
//! wired in yet (there's no subject/session concept in a one-shot CLI
//! invocation to enforce against), so this operates with full visibility;
//! that's a v1 scope note, not a design decision to leave permanent.
//!
//! Commands:
//!   gems init <dir>
//!   gems type create <dir> <name>
//!   gems type list <dir>
//!   gems entity create <dir> --type <type-name> --name <name> [key=value ...]
//!   gems entity get <dir> <id-hex>
//!   gems query <dir> "<query string>"

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use gems_catalog::{EntityFlags, EntityHeader, EntityKind, EntityType, EntityTypeKind};
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
     \x20 gems entity get <dir> <id-hex>\n\
     \x20 gems query <dir> \"<query string>\""
        .to_string()
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
    let dir = require_arg(args, 0, "dir")?;
    let id_hex = require_arg(args, 1, "id")?;
    let id = Tuid::from_hex_str(id_hex).ok_or("invalid entity id")?;

    let store = open_store(dir, false)?;
    let Some((header, body)) = store.get(&id).map_err(|e| e.to_string())? else {
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
    let dir = require_arg(args, 0, "dir")?;
    let query_str = require_arg(args, 1, "query")?;

    let store = open_store(dir, false)?;
    let query = gems_query::parse(query_str).map_err(|e| e.to_string())?;
    let ids = store.query(&query).map_err(|e| e.to_string())?;

    for id in ids {
        if let Some((header, _)) = store.get(&id).map_err(|e| e.to_string())? {
            println!("{}  {}", id.to_hex_string(), header.name);
        }
    }
    Ok(())
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
}
