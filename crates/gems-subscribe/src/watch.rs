//! `SubscriptionEngine`: point-level change notifications — "entity Z
//! deleted" and "entity X's attribute Y changed" — driven by the same
//! `LogRecord` stream `gems-cluster`'s replication already produces. A
//! subscriber is architecturally a replica that doesn't write to a local
//! `Store`: it evaluates a predicate against each record instead of
//! applying it.
//!
//! Pure and I/O-free, same pattern as `gems_cluster::raft`/`gossip`:
//! `process()` takes a `&LogRecord` and returns the notifications it
//! produces, with no idea where records come from (a `LogTailer` in
//! `tailer.rs` is the thin shell that reads a `ReplicationLog` and drives
//! this).
//!
//! **`AttributeChanged` needs a "before" to compare against, which a
//! single `LogRecord::Insert` doesn't carry** (it's the new state, not a
//! diff) — so this engine keeps its own small cache of last-observed
//! values, but only for `(entity, attribute)` pairs someone actually
//! subscribed to, not a full shadow copy of every entity. The first
//! sighting of a watched attribute seeds the cache without firing a
//! notification (there's no prior value to have "changed" from); this is
//! a deliberate choice, not an oversight — a subscription created after
//! the entity already exists shouldn't fire once just for existing.

use std::collections::HashMap;

use gems_cluster::LogRecord;
use gems_codec::GbvReader;
use gems_common::Tuid;

pub type SubscriptionId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Watch {
    EntityDeleted {
        id: Tuid,
    },
    AttributeChanged {
        id: Tuid,
        attribute_id: u32,
    },
    /// Fires whenever the entity is written at all — the cheapest watch,
    /// no diffing, no cache entry. "Written," not "content changed": a
    /// caller that calls `Store::insert` with unchanged data still counts,
    /// since this engine has no cheaper way to know the caller's intent
    /// wasn't a real update.
    AnyChange {
        id: Tuid,
    },
    /// Fires when a `view::ViewEngine` view's count changes. Not handled
    /// by `SubscriptionEngine::process` itself (it only sees `LogRecord`s,
    /// not view state) — `NotificationHub` in `hub.rs` is what actually
    /// drives this, by feeding a `ViewEngine`'s output back in as the
    /// trigger.
    ViewChanged {
        view_id: u64,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum NotificationKind {
    Deleted,
    Touched,
    FieldChanged {
        attribute_id: u32,
        new_value: Vec<u8>,
    },
    ViewCountChanged {
        new_count: u64,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Notification {
    pub subscription_id: SubscriptionId,
    pub kind: NotificationKind,
}

#[derive(Default)]
pub struct SubscriptionEngine {
    watches: HashMap<SubscriptionId, Watch>,
    next_id: SubscriptionId,
    field_cache: HashMap<(Tuid, u32), Vec<u8>>,
}

impl SubscriptionEngine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn subscribe(&mut self, watch: Watch) -> SubscriptionId {
        let id = self.next_id;
        self.next_id += 1;
        self.watches.insert(id, watch);
        id
    }

    /// Stop watching. Any cached field value for this watch is left in
    /// place (harmless — it just means a later re-subscription to the
    /// same `(entity, attribute)` won't fire on its first sighting either,
    /// which is the same "first sighting seeds, doesn't fire" rule
    /// applied consistently).
    pub fn unsubscribe(&mut self, id: SubscriptionId) -> bool {
        self.watches.remove(&id).is_some()
    }

    /// Emit `ViewCountChanged` notifications for every `ViewChanged`
    /// watch on `view_id`. Called by `NotificationHub`, not derived from a
    /// `LogRecord` directly — a `ViewEngine` is what actually decides a
    /// view's count changed.
    pub fn notify_view_changed(&self, view_id: u64, new_count: u64) -> Vec<Notification> {
        self.watches
            .iter()
            .filter_map(|(&sub_id, watch)| match watch {
                Watch::ViewChanged { view_id: watched } if *watched == view_id => {
                    Some(Notification {
                        subscription_id: sub_id,
                        kind: NotificationKind::ViewCountChanged { new_count },
                    })
                }
                _ => None,
            })
            .collect()
    }

    pub fn process(&mut self, record: &LogRecord) -> Vec<Notification> {
        match record {
            LogRecord::Delete { id } => self.process_delete(*id),
            LogRecord::Insert { header, body } => self.process_insert(header.id, body),
        }
    }

    fn process_delete(&mut self, deleted_id: Tuid) -> Vec<Notification> {
        self.watches
            .iter()
            .filter_map(|(&sub_id, watch)| match watch {
                Watch::EntityDeleted { id } | Watch::AnyChange { id } if *id == deleted_id => {
                    Some(Notification {
                        subscription_id: sub_id,
                        kind: NotificationKind::Deleted,
                    })
                }
                _ => None,
            })
            .collect()
    }

    fn process_insert(&mut self, entity_id: Tuid, body: &[u8]) -> Vec<Notification> {
        let reader = GbvReader::new(body).ok();
        let mut out = Vec::new();

        // Collect first so we can mutate `field_cache` while iterating
        // without borrowing `self.watches` and `self.field_cache`
        // simultaneously.
        let watches: Vec<(SubscriptionId, Watch)> =
            self.watches.iter().map(|(&id, &w)| (id, w)).collect();

        for (sub_id, watch) in watches {
            match watch {
                Watch::AnyChange { id } if id == entity_id => out.push(Notification {
                    subscription_id: sub_id,
                    kind: NotificationKind::Touched,
                }),
                Watch::AttributeChanged { id, attribute_id } if id == entity_id => {
                    let Some(reader) = &reader else { continue };
                    let Some((_, new_value)) = reader.get(attribute_id) else {
                        continue;
                    };
                    let key = (id, attribute_id);
                    let changed = match self.field_cache.get(&key) {
                        Some(old) => old.as_slice() != new_value,
                        None => false, // first sighting: seed only
                    };
                    self.field_cache.insert(key, new_value.to_vec());
                    if changed {
                        out.push(Notification {
                            subscription_id: sub_id,
                            kind: NotificationKind::FieldChanged {
                                attribute_id,
                                new_value: new_value.to_vec(),
                            },
                        });
                    }
                }
                _ => {}
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gems_catalog::{EntityFlags, EntityHeader, EntityKind};
    use gems_codec::{GbvBuilder, TypeTag};

    fn header(id: Tuid) -> EntityHeader {
        EntityHeader {
            id,
            created_by: [0u8; 16],
            modified_by: [0u8; 16],
            modified_at_ns: 0,
            name: "e".to_string(),
            description: String::new(),
            flags: EntityFlags::NONE,
            entity_kind: EntityKind::Data,
            schema_ref: Tuid::NIL,
            body_offset: 0,
            body_len: 0,
        }
    }

    fn body_with_field(attribute_id: u32, value: &[u8]) -> Vec<u8> {
        let mut b = GbvBuilder::new();
        b.push(attribute_id, TypeTag::Str, value);
        b.finish()
    }

    #[test]
    fn entity_deleted_fires_only_for_the_matching_id() {
        let mut engine = SubscriptionEngine::new();
        let watched = Tuid::new([1u8; 16], 1);
        let other = Tuid::new([2u8; 16], 2);
        let sub = engine.subscribe(Watch::EntityDeleted { id: watched });

        assert!(engine.process(&LogRecord::Delete { id: other }).is_empty());

        let notifications = engine.process(&LogRecord::Delete { id: watched });
        assert_eq!(
            notifications,
            vec![Notification {
                subscription_id: sub,
                kind: NotificationKind::Deleted
            }]
        );
    }

    #[test]
    fn any_change_fires_on_insert_and_on_delete() {
        let mut engine = SubscriptionEngine::new();
        let id = Tuid::new([1u8; 16], 1);
        let sub = engine.subscribe(Watch::AnyChange { id });

        let notifications = engine.process(&LogRecord::Insert {
            header: header(id),
            body: body_with_field(1, b"x"),
        });
        assert_eq!(notifications[0].kind, NotificationKind::Touched);

        let notifications = engine.process(&LogRecord::Delete { id });
        assert_eq!(notifications[0].kind, NotificationKind::Deleted);
        assert_eq!(notifications[0].subscription_id, sub);
    }

    #[test]
    fn attribute_changed_does_not_fire_on_first_sighting() {
        let mut engine = SubscriptionEngine::new();
        let id = Tuid::new([1u8; 16], 1);
        engine.subscribe(Watch::AttributeChanged {
            id,
            attribute_id: 7,
        });

        let notifications = engine.process(&LogRecord::Insert {
            header: header(id),
            body: body_with_field(7, b"initial"),
        });
        assert!(
            notifications.is_empty(),
            "first sighting should only seed the cache"
        );
    }

    #[test]
    fn attribute_changed_fires_only_when_the_value_actually_differs() {
        let mut engine = SubscriptionEngine::new();
        let id = Tuid::new([1u8; 16], 1);
        let sub = engine.subscribe(Watch::AttributeChanged {
            id,
            attribute_id: 7,
        });

        engine.process(&LogRecord::Insert {
            header: header(id),
            body: body_with_field(7, b"initial"),
        });

        // Same value again: no notification.
        let notifications = engine.process(&LogRecord::Insert {
            header: header(id),
            body: body_with_field(7, b"initial"),
        });
        assert!(notifications.is_empty());

        // Different value: fires.
        let notifications = engine.process(&LogRecord::Insert {
            header: header(id),
            body: body_with_field(7, b"changed"),
        });
        assert_eq!(
            notifications,
            vec![Notification {
                subscription_id: sub,
                kind: NotificationKind::FieldChanged {
                    attribute_id: 7,
                    new_value: b"changed".to_vec()
                }
            }]
        );
    }

    #[test]
    fn attribute_changed_ignores_unrelated_fields_and_entities() {
        let mut engine = SubscriptionEngine::new();
        let watched = Tuid::new([1u8; 16], 1);
        let other = Tuid::new([2u8; 16], 2);
        engine.subscribe(Watch::AttributeChanged {
            id: watched,
            attribute_id: 7,
        });

        engine.process(&LogRecord::Insert {
            header: header(watched),
            body: body_with_field(7, b"v1"),
        });

        // A different entity changing an attribute with the same id: no effect.
        let notifications = engine.process(&LogRecord::Insert {
            header: header(other),
            body: body_with_field(7, b"v1-for-other"),
        });
        assert!(notifications.is_empty());

        // The watched entity changing a *different* attribute: no effect.
        let notifications = engine.process(&LogRecord::Insert {
            header: header(watched),
            body: body_with_field(8, b"unrelated"),
        });
        assert!(notifications.is_empty());
    }

    #[test]
    fn view_changed_notifies_only_matching_watches() {
        let mut engine = SubscriptionEngine::new();
        let sub_a = engine.subscribe(Watch::ViewChanged { view_id: 1 });
        let _sub_b = engine.subscribe(Watch::ViewChanged { view_id: 2 });

        let notifications = engine.notify_view_changed(1, 5);
        assert_eq!(
            notifications,
            vec![Notification {
                subscription_id: sub_a,
                kind: NotificationKind::ViewCountChanged { new_count: 5 }
            }]
        );
    }

    #[test]
    fn unsubscribe_stops_future_notifications() {
        let mut engine = SubscriptionEngine::new();
        let id = Tuid::new([1u8; 16], 1);
        let sub = engine.subscribe(Watch::EntityDeleted { id });
        assert!(engine.unsubscribe(sub));
        assert!(!engine.unsubscribe(sub), "already removed");

        let notifications = engine.process(&LogRecord::Delete { id });
        assert!(notifications.is_empty());
    }
}
