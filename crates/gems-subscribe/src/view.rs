//! `ViewEngine`: materialized `COUNT` views, incrementally maintained from
//! the same `LogRecord` stream `watch.rs` consumes — "count(entities in
//! layer Q) changed" needs actual incremental view maintenance, not just
//! event filtering: a `LogRecord` says an entity was written or deleted,
//! not whether that pushed it in or out of some predicate's matching set,
//! so a view has to evaluate the predicate itself and track membership.
//!
//! **Scope for this pass**, consistent with how other query-adjacent
//! pieces in this workspace (`gems-query`'s planner, `gems-engine`'s
//! secondary indexes) started narrow and expanded: `COUNT` only (no
//! `SUM`/`AVG`/`MIN`/`MAX`), and a single-condition `ViewPredicate` (no
//! compound `AND`/`OR` — `gems-query`'s `Expr` tree would be the natural
//! thing to compile a richer predicate from, but that's a real integration
//! project of its own, not a small addition here). A view keeps the full
//! set of currently-matching entity ids (not just a count) because that's
//! the only correct way to handle an update that moves an entity out of
//! the matching set — you can't tell "no longer matches" from a raw
//! `LogRecord` without knowing it used to match.

use std::collections::{HashMap, HashSet};

use gems_catalog::{EntityHeader, EntityKind};
use gems_cluster::LogRecord;
use gems_codec::GbvReader;
use gems_common::Tuid;

pub type ViewId = u64;

#[derive(Debug, Clone, PartialEq)]
pub enum ViewPredicate {
    EntityKind(EntityKind),
    SchemaRef(Tuid),
    /// A specific GBV attribute equals an exact byte value — e.g. a
    /// `layer_ref` attribute equal to a given `Layer`'s `Tuid` bytes,
    /// which is what "count(entities in layer Q)" compiles to today,
    /// pending real layer-membership resolution in the query planner.
    AttributeEquals {
        attribute_id: u32,
        value: Vec<u8>,
    },
}

impl ViewPredicate {
    fn matches(&self, header: &EntityHeader, body: &[u8]) -> bool {
        match self {
            ViewPredicate::EntityKind(kind) => header.entity_kind == *kind,
            ViewPredicate::SchemaRef(type_id) => header.schema_ref == *type_id,
            ViewPredicate::AttributeEquals {
                attribute_id,
                value,
            } => {
                let Ok(reader) = GbvReader::new(body) else {
                    return false;
                };
                reader
                    .get(*attribute_id)
                    .is_some_and(|(_, v)| v == value.as_slice())
            }
        }
    }
}

struct CountView {
    predicate: ViewPredicate,
    members: HashSet<Tuid>,
}

pub struct ViewEngine {
    views: HashMap<ViewId, CountView>,
    next_id: ViewId,
}

impl Default for ViewEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl ViewEngine {
    pub fn new() -> Self {
        ViewEngine {
            views: HashMap::new(),
            next_id: 0,
        }
    }

    pub fn create_view(&mut self, predicate: ViewPredicate) -> ViewId {
        let id = self.next_id;
        self.next_id += 1;
        self.views.insert(
            id,
            CountView {
                predicate,
                members: HashSet::new(),
            },
        );
        id
    }

    pub fn drop_view(&mut self, id: ViewId) -> bool {
        self.views.remove(&id).is_some()
    }

    pub fn count(&self, id: ViewId) -> Option<u64> {
        self.views.get(&id).map(|v| v.members.len() as u64)
    }

    /// Apply one record to every view, returning the ids of views whose
    /// count actually changed (and their new count) — the hand-off point
    /// for a `Watch::ViewChanged`-style subscription.
    pub fn process(&mut self, record: &LogRecord) -> Vec<(ViewId, u64)> {
        let mut changed = Vec::new();
        match record {
            LogRecord::Delete { id } => {
                for (&view_id, view) in self.views.iter_mut() {
                    if view.members.remove(id) {
                        changed.push((view_id, view.members.len() as u64));
                    }
                }
            }
            LogRecord::Insert { header, body } => {
                for (&view_id, view) in self.views.iter_mut() {
                    let matches = view.predicate.matches(header, body);
                    let was_member = view.members.contains(&header.id);
                    if matches && !was_member {
                        view.members.insert(header.id);
                        changed.push((view_id, view.members.len() as u64));
                    } else if !matches && was_member {
                        view.members.remove(&header.id);
                        changed.push((view_id, view.members.len() as u64));
                    }
                    // matches && was_member, or !matches && !was_member:
                    // no membership transition, so no change to report,
                    // even though the entity's other fields may have.
                }
            }
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gems_catalog::EntityFlags;
    use gems_codec::{GbvBuilder, TypeTag};

    fn header_of_kind(id: Tuid, kind: EntityKind, schema_ref: Tuid) -> EntityHeader {
        EntityHeader {
            id,
            created_by: [0u8; 16],
            modified_by: [0u8; 16],
            modified_at_ns: 0,
            name: "e".to_string(),
            description: String::new(),
            flags: EntityFlags::NONE,
            entity_kind: kind,
            schema_ref,
            body_offset: 0,
            body_len: 0,
        }
    }

    fn layer_body(layer_attr_id: u32, layer: Tuid) -> Vec<u8> {
        let mut b = GbvBuilder::new();
        b.push(layer_attr_id, TypeTag::EntityRef, layer.as_bytes());
        b.finish()
    }

    #[test]
    fn count_increases_when_entities_start_matching() {
        let mut views = ViewEngine::new();
        let layer_q = Tuid::new([9u8; 16], 9);
        let view = views.create_view(ViewPredicate::AttributeEquals {
            attribute_id: 1,
            value: layer_q.as_bytes().to_vec(),
        });
        assert_eq!(views.count(view), Some(0));

        let e1 = Tuid::new([1u8; 16], 1);
        let changed = views.process(&LogRecord::Insert {
            header: header_of_kind(e1, EntityKind::Data, Tuid::NIL),
            body: layer_body(1, layer_q),
        });
        assert_eq!(changed, vec![(view, 1)]);
        assert_eq!(views.count(view), Some(1));
    }

    #[test]
    fn count_decreases_when_an_entity_moves_out_of_the_predicate() {
        let mut views = ViewEngine::new();
        let layer_q = Tuid::new([9u8; 16], 9);
        let layer_r = Tuid::new([8u8; 16], 8);
        let view = views.create_view(ViewPredicate::AttributeEquals {
            attribute_id: 1,
            value: layer_q.as_bytes().to_vec(),
        });

        let e1 = Tuid::new([1u8; 16], 1);
        views.process(&LogRecord::Insert {
            header: header_of_kind(e1, EntityKind::Data, Tuid::NIL),
            body: layer_body(1, layer_q),
        });
        assert_eq!(views.count(view), Some(1));

        // e1 moves to a different layer: must leave the view's matching set.
        let changed = views.process(&LogRecord::Insert {
            header: header_of_kind(e1, EntityKind::Data, Tuid::NIL),
            body: layer_body(1, layer_r),
        });
        assert_eq!(changed, vec![(view, 0)]);
        assert_eq!(views.count(view), Some(0));
    }

    #[test]
    fn count_decreases_on_delete() {
        let mut views = ViewEngine::new();
        let view = views.create_view(ViewPredicate::EntityKind(EntityKind::Data));
        let e1 = Tuid::new([1u8; 16], 1);
        views.process(&LogRecord::Insert {
            header: header_of_kind(e1, EntityKind::Data, Tuid::NIL),
            body: vec![],
        });
        assert_eq!(views.count(view), Some(1));

        let changed = views.process(&LogRecord::Delete { id: e1 });
        assert_eq!(changed, vec![(view, 0)]);
    }

    #[test]
    fn unrelated_updates_do_not_report_a_change() {
        let mut views = ViewEngine::new();
        let view = views.create_view(ViewPredicate::EntityKind(EntityKind::Data));
        let e1 = Tuid::new([1u8; 16], 1);
        views.process(&LogRecord::Insert {
            header: header_of_kind(e1, EntityKind::Data, Tuid::NIL),
            body: vec![],
        });

        // Re-inserting the same entity, still matching the same predicate:
        // no membership transition, so no reported change.
        let changed = views.process(&LogRecord::Insert {
            header: header_of_kind(e1, EntityKind::Data, Tuid::new([2u8; 16], 2)),
            body: vec![],
        });
        assert!(changed.is_empty());
        assert_eq!(views.count(view), Some(1));
    }

    #[test]
    fn multiple_views_are_tracked_independently() {
        let mut views = ViewEngine::new();
        let data_view = views.create_view(ViewPredicate::EntityKind(EntityKind::Data));
        let attr_view = views.create_view(ViewPredicate::EntityKind(EntityKind::EntityAttribute));

        let e1 = Tuid::new([1u8; 16], 1);
        let changed = views.process(&LogRecord::Insert {
            header: header_of_kind(e1, EntityKind::Data, Tuid::NIL),
            body: vec![],
        });
        assert_eq!(changed, vec![(data_view, 1)]);
        assert_eq!(views.count(attr_view), Some(0));
    }

    #[test]
    fn dropped_view_is_no_longer_tracked() {
        let mut views = ViewEngine::new();
        let view = views.create_view(ViewPredicate::EntityKind(EntityKind::Data));
        assert!(views.drop_view(view));
        assert_eq!(views.count(view), None);
    }
}
