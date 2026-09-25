//! `NotificationHub`: the single entry point most callers actually want —
//! owns both a `SubscriptionEngine` (point-level watches) and a
//! `ViewEngine` (materialized counts), feeds every incoming `LogRecord` to
//! both, and turns a view's count change into the `ViewChanged`
//! notifications any subscriber is waiting on. Still pure/I/O-free; see
//! `tailer.rs` for the shell that actually reads a `ReplicationLog` and
//! drives this.

use gems_cluster::LogRecord;

use crate::view::{ViewEngine, ViewId, ViewPredicate};
use crate::watch::{Notification, SubscriptionEngine, SubscriptionId, Watch};

#[derive(Default)]
pub struct NotificationHub {
    subscriptions: SubscriptionEngine,
    views: ViewEngine,
}

impl NotificationHub {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn subscribe(&mut self, watch: Watch) -> SubscriptionId {
        self.subscriptions.subscribe(watch)
    }

    pub fn unsubscribe(&mut self, id: SubscriptionId) -> bool {
        self.subscriptions.unsubscribe(id)
    }

    pub fn create_view(&mut self, predicate: ViewPredicate) -> ViewId {
        self.views.create_view(predicate)
    }

    pub fn drop_view(&mut self, id: ViewId) -> bool {
        self.views.drop_view(id)
    }

    pub fn view_count(&self, id: ViewId) -> Option<u64> {
        self.views.count(id)
    }

    /// Feed one record through both engines, returning every notification
    /// it produces — point-level ones directly, plus a `ViewCountChanged`
    /// for each `ViewChanged` watch on a view whose count this record
    /// happened to move.
    pub fn process(&mut self, record: &LogRecord) -> Vec<Notification> {
        let mut out = self.subscriptions.process(record);
        for (view_id, new_count) in self.views.process(record) {
            out.extend(self.subscriptions.notify_view_changed(view_id, new_count));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watch::NotificationKind;
    use gems_catalog::{EntityFlags, EntityHeader, EntityKind};
    use gems_common::Tuid;

    fn header(id: Tuid, kind: EntityKind) -> EntityHeader {
        EntityHeader {
            id,
            created_by: [0u8; 16],
            modified_by: [0u8; 16],
            modified_at_ns: 0,
            name: "e".to_string(),
            description: String::new(),
            flags: EntityFlags::NONE,
            entity_kind: kind,
            schema_ref: Tuid::NIL,
            body_offset: 0,
            body_len: 0,
        }
    }

    #[test]
    fn view_changed_watch_fires_via_the_hub_when_a_view_transitions() {
        let mut hub = NotificationHub::new();
        let view = hub.create_view(ViewPredicate::EntityKind(EntityKind::Data));
        let sub = hub.subscribe(Watch::ViewChanged { view_id: view });

        let e1 = Tuid::new([1u8; 16], 1);
        let notifications = hub.process(&LogRecord::Insert {
            header: header(e1, EntityKind::Data),
            body: vec![],
        });

        assert_eq!(
            notifications,
            vec![Notification {
                subscription_id: sub,
                kind: NotificationKind::ViewCountChanged { new_count: 1 }
            }]
        );
        assert_eq!(hub.view_count(view), Some(1));
    }

    #[test]
    fn point_level_and_view_notifications_coexist() {
        let mut hub = NotificationHub::new();
        let e1 = Tuid::new([1u8; 16], 1);
        let deleted_sub = hub.subscribe(Watch::EntityDeleted { id: e1 });
        let view = hub.create_view(ViewPredicate::EntityKind(EntityKind::Data));
        let view_sub = hub.subscribe(Watch::ViewChanged { view_id: view });

        hub.process(&LogRecord::Insert {
            header: header(e1, EntityKind::Data),
            body: vec![],
        });

        let notifications = hub.process(&LogRecord::Delete { id: e1 });
        assert_eq!(notifications.len(), 2);
        assert!(notifications
            .iter()
            .any(|n| n.subscription_id == deleted_sub && n.kind == NotificationKind::Deleted));
        assert!(notifications.iter().any(|n| n.subscription_id == view_sub
            && n.kind == NotificationKind::ViewCountChanged { new_count: 0 }));
    }

    #[test]
    fn view_with_no_matching_watch_produces_no_notification() {
        let mut hub = NotificationHub::new();
        let view = hub.create_view(ViewPredicate::EntityKind(EntityKind::Data));
        let e1 = Tuid::new([1u8; 16], 1);
        let notifications = hub.process(&LogRecord::Insert {
            header: header(e1, EntityKind::Data),
            body: vec![],
        });
        assert!(notifications.is_empty());
        assert_eq!(hub.view_count(view), Some(1));
    }
}
