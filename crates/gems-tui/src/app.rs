//! `App`: the TUI's state machine, deliberately separated from all
//! terminal I/O (raw mode, ANSI rendering, reading escape sequences —
//! those live in `term.rs`/`screen.rs`/`main.rs`) so it's testable the
//! same way `gems-query`'s parser or `gems-cluster`'s `RaftCore` are: feed
//! it inputs, assert on the resulting state, no terminal required.
//!
//! **Scope for this pass**: browse and query, no ABAC subject context (the
//! CLI's `--as` and the WebUI's `subject` parameter both make that opt-in;
//! a TUI session would want the same, but wiring a subject-entry mode into
//! the key-handling state machine below is a distinct feature, not a small
//! addition — administrative/raw access only for now, same as the CLI's
//! default). No entity creation/editing either — same "needs schema-driven
//! form generation" reasoning as the WebUI's read-only scope.

use std::path::{Path, PathBuf};

use gems_catalog::EntityKind;
use gems_codec::{GbvReader, TypeTag};
use gems_common::Tuid;
use gems_engine::Store;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Enter,
    Escape,
    Backspace,
    Quit,
    EnterQueryMode,
    Char(char),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Browse,
    QueryInput,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EntitySummary {
    pub id: Tuid,
    pub name: String,
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct DetailView {
    pub header_lines: Vec<(String, String)>,
    pub fields: Vec<(String, String)>,
}

pub struct App {
    store: Store,
    pub store_dir: PathBuf,
    pub mode: Mode,
    pub query_input: String,
    pub last_query: String,
    pub entities: Vec<EntitySummary>,
    pub selected: usize,
    pub error: Option<String>,
    pub detail: Option<DetailView>,
}

const DEFAULT_QUERY: &str = "SELECT * FROM entities";

impl App {
    pub fn open(store_dir: &Path) -> gems_common::Result<Self> {
        let store = Store::open(store_dir, false)?;
        let mut app = App {
            store,
            store_dir: store_dir.to_path_buf(),
            mode: Mode::Browse,
            query_input: String::new(),
            last_query: DEFAULT_QUERY.to_string(),
            entities: Vec::new(),
            selected: 0,
            error: None,
            detail: None,
        };
        app.run_query(DEFAULT_QUERY);
        Ok(app)
    }

    pub fn run_query(&mut self, query_str: &str) {
        self.last_query = query_str.to_string();
        self.error = None;
        self.entities.clear();
        self.selected = 0;
        self.detail = None;

        let parsed = match gems_query::parse(query_str) {
            Ok(q) => q,
            Err(e) => {
                self.error = Some(e.to_string());
                return;
            }
        };
        let ids = match self.store.query(&parsed) {
            Ok(ids) => ids,
            Err(e) => {
                self.error = Some(e.to_string());
                return;
            }
        };
        for id in ids {
            if let Ok(Some((header, _))) = self.store.get(&id) {
                self.entities.push(EntitySummary {
                    id: header.id,
                    name: header.name,
                    kind: format!("{:?}", header.entity_kind),
                });
            }
        }
        self.load_detail();
    }

    pub fn move_selection(&mut self, delta: i32) {
        if self.entities.is_empty() {
            return;
        }
        let len = self.entities.len() as i32;
        let mut next = self.selected as i32 + delta;
        next = next.clamp(0, len - 1);
        self.selected = next as usize;
        self.load_detail();
    }

    pub fn load_detail(&mut self) {
        self.detail = None;
        let Some(summary) = self.entities.get(self.selected) else {
            return;
        };
        let Ok(Some((header, body))) = self.store.get(&summary.id) else {
            return;
        };

        let mut view = DetailView::default();
        view.header_lines
            .push(("id".to_string(), header.id.to_hex_string()));
        view.header_lines
            .push(("name".to_string(), header.name.clone()));
        view.header_lines
            .push(("description".to_string(), header.description.clone()));
        view.header_lines
            .push(("kind".to_string(), format!("{:?}", header.entity_kind)));
        view.header_lines
            .push(("schema_ref".to_string(), header.schema_ref.to_hex_string()));

        if header.entity_kind == EntityKind::Data {
            if let Ok(reader) = GbvReader::new(&body) {
                for key in reader.key_ids() {
                    if let Some((TypeTag::Str, value)) = reader.get(key) {
                        view.fields
                            .push((key.to_string(), String::from_utf8_lossy(value).into_owned()));
                    }
                }
            }
        }
        self.detail = Some(view);
    }

    /// Handle one input event. Returns `true` if the application should
    /// quit.
    pub fn handle_key(&mut self, key: Key) -> bool {
        match self.mode {
            Mode::Browse => match key {
                Key::Quit => return true,
                Key::Up => self.move_selection(-1),
                Key::Down => self.move_selection(1),
                Key::EnterQueryMode => {
                    self.mode = Mode::QueryInput;
                    self.query_input.clear();
                }
                _ => {}
            },
            Mode::QueryInput => match key {
                Key::Enter => {
                    let query = self.query_input.clone();
                    self.mode = Mode::Browse;
                    self.run_query(&query);
                }
                Key::Escape => {
                    self.mode = Mode::Browse;
                }
                Key::Backspace => {
                    self.query_input.pop();
                }
                Key::Char(c) => self.query_input.push(c),
                _ => {}
            },
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gems_catalog::{EntityFlags, EntityHeader, EntityType, EntityTypeKind};
    use gems_codec::GbvBuilder;
    use std::path::PathBuf;

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("gems-tui-app-test")
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

    fn seed(dir: &Path) -> (Tuid, Vec<Tuid>) {
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

        let mut ids = Vec::new();
        for n in 0..3u8 {
            let id = Tuid::generate();
            let mut body = GbvBuilder::new();
            body.push(1, TypeTag::Str, format!("v{n}").as_bytes());
            store
                .insert(
                    header(id, &format!("w{n}"), EntityKind::Data, type_id),
                    &body.finish(),
                )
                .unwrap();
            ids.push(id);
        }
        (type_id, ids)
    }

    #[test]
    fn open_runs_the_default_query_and_loads_detail() {
        let dir = tmp_dir("open");
        seed(&dir);
        let app = App::open(&dir).unwrap();
        // 1 EntityType ("widget") + 3 Data entities seeded below.
        assert_eq!(app.entities.len(), 4);
        assert!(app.detail.is_some());
        assert!(app.error.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn arrow_keys_move_selection_and_clamp_at_the_edges() {
        let dir = tmp_dir("navigate");
        seed(&dir);
        let mut app = App::open(&dir).unwrap();

        assert_eq!(app.selected, 0);
        app.handle_key(Key::Up); // clamps at 0
        assert_eq!(app.selected, 0);

        app.handle_key(Key::Down);
        assert_eq!(app.selected, 1);
        app.handle_key(Key::Down);
        app.handle_key(Key::Down);
        app.handle_key(Key::Down); // one past the end (4 entities: widget type + w0..w2)
        assert_eq!(app.selected, 3, "must clamp at the last entity");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn quit_key_returns_true() {
        let dir = tmp_dir("quit");
        seed(&dir);
        let mut app = App::open(&dir).unwrap();
        assert!(app.handle_key(Key::Quit));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn query_mode_builds_up_input_and_runs_on_enter() {
        let dir = tmp_dir("query_mode");
        seed(&dir);
        let mut app = App::open(&dir).unwrap();

        app.handle_key(Key::EnterQueryMode);
        assert_eq!(app.mode, Mode::QueryInput);
        for c in "SELECT * FROM entities WHERE type IN (widget) LIMIT 1".chars() {
            app.handle_key(Key::Char(c));
        }
        app.handle_key(Key::Enter);

        assert_eq!(app.mode, Mode::Browse);
        assert_eq!(app.entities.len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn escape_in_query_mode_discards_input_without_running_it() {
        let dir = tmp_dir("query_escape");
        seed(&dir);
        let mut app = App::open(&dir).unwrap();
        let original_count = app.entities.len();

        app.handle_key(Key::EnterQueryMode);
        app.handle_key(Key::Char('x'));
        app.handle_key(Key::Escape);

        assert_eq!(app.mode, Mode::Browse);
        assert_eq!(
            app.entities.len(),
            original_count,
            "query must not have run"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn backspace_removes_the_last_character() {
        let dir = tmp_dir("backspace");
        seed(&dir);
        let mut app = App::open(&dir).unwrap();
        app.handle_key(Key::EnterQueryMode);
        app.handle_key(Key::Char('a'));
        app.handle_key(Key::Char('b'));
        app.handle_key(Key::Backspace);
        assert_eq!(app.query_input, "a");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn invalid_query_sets_an_error_and_clears_the_list() {
        let dir = tmp_dir("bad_query");
        seed(&dir);
        let mut app = App::open(&dir).unwrap();
        app.run_query("NOT VALID SQL AT ALL");
        assert!(app.error.is_some());
        assert!(app.entities.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }
}
