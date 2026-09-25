//! `LogTailer`: the thin shell that reads a `ReplicationLog` and drives a
//! `NotificationHub` — same role `ReplicaClient` plays for `gems-cluster`'s
//! stage 2, but applying records to a `NotificationHub` instead of a
//! `gems_engine::Store`. Local/same-machine only for this pass: it reads
//! the log file directly rather than over a `ReplicationServer` TCP
//! connection. A remote subscriber (reading a primary's log from another
//! machine) would need a small client analogous to `ReplicaClient` but
//! feeding a `NotificationHub` instead of a `Store` — not implemented
//! here, since nothing about `NotificationHub` requires it: point it at a
//! locally-synced copy of the log (e.g. one a `ReplicaClient` is already
//! maintaining) and it works the same way.

use std::path::{Path, PathBuf};

use gems_cluster::ReplicationLog;
use gems_common::Result;

use crate::hub::NotificationHub;
use crate::watch::Notification;

const OFFSET_FILE_NAME: &str = "subscribe_offset";

pub struct LogTailer {
    log: ReplicationLog,
    offset_path: PathBuf,
    offset: u64,
}

impl LogTailer {
    /// `state_dir` is where this tailer persists its own progress — pass a
    /// directory scoped to this particular subscriber (not the primary's
    /// own store directory) if more than one tailer reads the same log
    /// independently.
    pub fn open(log_path: &Path, state_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(state_dir)?;
        let log = ReplicationLog::open(log_path)?;
        let offset_path = state_dir.join(OFFSET_FILE_NAME);
        let offset = std::fs::read_to_string(&offset_path)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        Ok(LogTailer {
            log,
            offset_path,
            offset,
        })
    }

    fn save_offset(&self) -> Result<()> {
        std::fs::write(&self.offset_path, self.offset.to_string())?;
        Ok(())
    }

    /// Read every record appended since the last call (or since this
    /// tailer's persisted offset, on the first call after opening) and
    /// feed them to `hub`, returning every notification produced, in
    /// order. Non-blocking — returns immediately with whatever is
    /// available; a caller polls this on an interval, or in a loop with
    /// its own sleep, since (like `ReplicationServer`) there's no
    /// OS-specific file-change notification wired in.
    pub fn poll(&mut self, hub: &mut NotificationHub) -> Result<Vec<Notification>> {
        let (records, new_offset) = self.log.read_from(self.offset)?;
        let mut out = Vec::new();
        for record in &records {
            out.extend(hub.process(record));
        }
        if new_offset != self.offset {
            self.offset = new_offset;
            self.save_offset()?;
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::ViewPredicate;
    use crate::watch::{NotificationKind, Watch};
    use gems_catalog::{EntityFlags, EntityHeader, EntityKind};
    use gems_common::Tuid;
    use std::path::PathBuf;

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("gems-subscribe-tailer-test")
            .join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

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
    fn poll_delivers_records_appended_since_last_call() {
        let dir = tmp_dir("poll");
        let log_path = dir.join("primary.gemlog");
        let log = gems_cluster::ReplicationLog::open(&log_path).unwrap();

        let mut tailer = LogTailer::open(&log_path, &dir.join("tailer_state")).unwrap();
        let mut hub = NotificationHub::new();
        let e1 = Tuid::new([1u8; 16], 1);
        let sub = hub.subscribe(Watch::EntityDeleted { id: e1 });

        assert!(tailer.poll(&mut hub).unwrap().is_empty());

        log.append(&gems_cluster::LogRecord::Delete { id: e1 })
            .unwrap();
        let notifications = tailer.poll(&mut hub).unwrap();
        assert_eq!(
            notifications,
            vec![Notification {
                subscription_id: sub,
                kind: NotificationKind::Deleted
            }]
        );

        // A second poll with nothing new appended yields nothing new.
        assert!(tailer.poll(&mut hub).unwrap().is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resumes_from_persisted_offset_across_reopen() {
        let dir = tmp_dir("resume");
        let log_path = dir.join("primary.gemlog");
        let log = gems_cluster::ReplicationLog::open(&log_path).unwrap();
        let state_dir = dir.join("tailer_state");

        let e1 = Tuid::new([1u8; 16], 1);
        let e2 = Tuid::new([2u8; 16], 2);
        log.append(&gems_cluster::LogRecord::Delete { id: e1 })
            .unwrap();

        {
            let mut tailer = LogTailer::open(&log_path, &state_dir).unwrap();
            let mut hub = NotificationHub::new();
            hub.subscribe(Watch::EntityDeleted { id: e1 });
            hub.subscribe(Watch::EntityDeleted { id: e2 });
            let notifications = tailer.poll(&mut hub).unwrap();
            assert_eq!(notifications.len(), 1);
        }

        log.append(&gems_cluster::LogRecord::Delete { id: e2 })
            .unwrap();

        {
            let mut tailer = LogTailer::open(&log_path, &state_dir).unwrap();
            let mut hub = NotificationHub::new();
            let sub = hub.subscribe(Watch::EntityDeleted { id: e2 });
            let notifications = tailer.poll(&mut hub).unwrap();
            assert_eq!(
                notifications,
                vec![Notification {
                    subscription_id: sub,
                    kind: NotificationKind::Deleted
                }],
                "must resume from the persisted offset, not replay e1 or miss e2"
            );
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn poll_drives_view_notifications_too() {
        let dir = tmp_dir("view");
        let log_path = dir.join("primary.gemlog");
        let log = gems_cluster::ReplicationLog::open(&log_path).unwrap();

        let mut tailer = LogTailer::open(&log_path, &dir.join("tailer_state")).unwrap();
        let mut hub = NotificationHub::new();
        let view = hub.create_view(ViewPredicate::EntityKind(EntityKind::Data));
        let sub = hub.subscribe(Watch::ViewChanged { view_id: view });

        let e1 = Tuid::new([1u8; 16], 1);
        log.append(&gems_cluster::LogRecord::Insert {
            header: header(e1, EntityKind::Data),
            body: vec![],
        })
        .unwrap();

        let notifications = tailer.poll(&mut hub).unwrap();
        assert_eq!(
            notifications,
            vec![Notification {
                subscription_id: sub,
                kind: NotificationKind::ViewCountChanged { new_count: 1 }
            }]
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
