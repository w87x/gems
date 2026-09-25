//! Seed data for a brand-new store: the three built-in `Role` tags this
//! workspace's admin tooling assumes exist (`admin`/`analyst`/`consumer`,
//! see gems-webui's workspace split), a starter policy set that makes the
//! system usable the moment it's seeded, and the first `Subject` (a human
//! operator) to hold the `admin` role.
//!
//! Deliberately just data: this module builds `(EntityHeader, body)` pairs
//! and returns the generated ids, but never touches a `Store` or a
//! `gems-cluster` client itself — every caller (an installer CLI
//! subcommand, a webui first-run endpoint) already has its own way of
//! getting a write applied (direct `Store::insert` for a brand-new
//! single-process store, or `gems-cluster`'s Raft `propose` path for a
//! running cluster), and this module shouldn't need to know which.
//!
//! **Default policy set** (deliberately small — an operator can add finer-
//! grained policies afterward through the same admin UI/CLI that reads
//! this module's role ids back out by name):
//! - `admin` role: `Permit` on every entity kind, unredacted — the
//!   operator who ran setup can see everything, including other subjects
//!   and policies, to administer the system.
//! - Any authenticated subject: `Permit` on `Data` and `EntityType`
//!   entities — every signed-in user can browse the data model and query
//!   data; only `admin` can see `Subject`/`Policy`/`Role` entities
//!   themselves. Distinguishing `analyst` (can also write data) from
//!   `consumer` (read-only) is enforced at the application layer (the
//!   webui's own route guards), not by ABAC — see `gems-abac`'s own scope
//!   note that write-path authorization isn't part of the engine yet.

use gems_common::Tuid;

use crate::header::EntityHeader;
use crate::kind::EntityKind;
use crate::layer::Role;
use crate::policy::{Effect, Policy, SubjectPredicate, TargetPredicate};
use crate::subject::{Subject, SubjectKind};

/// Name every seeded `Role` is given, so callers can look its id back up
/// later (`Store::query_by_kind(EntityKind::Role)` + match on `header.name`)
/// without this module needing to expose the ids through any other channel.
pub const ADMIN_ROLE_NAME: &str = "admin";
pub const ANALYST_ROLE_NAME: &str = "analyst";
pub const CONSUMER_ROLE_NAME: &str = "consumer";

/// One `(header, body)` pair ready to write via whatever path the caller
/// uses (direct `Store::insert` or a `gems-cluster` `LogRecord::Insert`).
pub struct SeedEntity {
    pub header: EntityHeader,
    pub body: Vec<u8>,
}

pub struct Bootstrap {
    pub admin_role_id: Tuid,
    pub analyst_role_id: Tuid,
    pub consumer_role_id: Tuid,
    pub admin_subject_id: Tuid,
    /// Every entity to write, in an order safe to apply sequentially (the
    /// roles first, then the policies that reference their ids, then the
    /// admin subject) — though since every id is pre-generated here rather
    /// than assigned on insert, applying them in any order is equally
    /// correct; the order is just easier to read in a log.
    pub entities: Vec<SeedEntity>,
}

/// Builds the seed data described in this module's doc, naming the admin
/// subject `admin_name` (e.g. an operator's chosen username).
pub fn bootstrap(admin_name: &str) -> Bootstrap {
    let admin_role_id = Tuid::generate();
    let analyst_role_id = Tuid::generate();
    let consumer_role_id = Tuid::generate();
    let admin_subject_id = Tuid::generate();

    let mut entities = Vec::new();

    for (id, name) in [
        (admin_role_id, ADMIN_ROLE_NAME),
        (analyst_role_id, ANALYST_ROLE_NAME),
        (consumer_role_id, CONSUMER_ROLE_NAME),
    ] {
        entities.push(SeedEntity {
            header: EntityHeader::new(id, name, EntityKind::Role, Tuid::NIL),
            body: Role.encode(),
        });
    }

    let admin_sees_everything = Policy {
        target: TargetPredicate::ANY,
        subject: SubjectPredicate {
            subject_id: Tuid::NIL,
            role_id: admin_role_id,
        },
        effect: Effect::Permit,
        redact_attributes: vec![],
    };
    let anyone_reads_data = Policy {
        target: TargetPredicate {
            entity_kind: Some(EntityKind::Data),
            schema_ref: Tuid::NIL,
        },
        subject: SubjectPredicate::ANY,
        effect: Effect::Permit,
        redact_attributes: vec![],
    };
    let anyone_reads_types = Policy {
        target: TargetPredicate {
            entity_kind: Some(EntityKind::EntityType),
            schema_ref: Tuid::NIL,
        },
        subject: SubjectPredicate::ANY,
        effect: Effect::Permit,
        redact_attributes: vec![],
    };
    for policy in [admin_sees_everything, anyone_reads_data, anyone_reads_types] {
        let id = Tuid::generate();
        entities.push(SeedEntity {
            header: EntityHeader::new(id, "policy", EntityKind::Policy, Tuid::NIL),
            body: policy.encode(),
        });
    }

    entities.push(SeedEntity {
        header: EntityHeader::new(admin_subject_id, admin_name, EntityKind::Subject, Tuid::NIL),
        body: Subject {
            kind: SubjectKind::User,
            credentials: None,
            member_of: vec![],
        }
        .encode(),
    });

    Bootstrap {
        admin_role_id,
        analyst_role_id,
        consumer_role_id,
        admin_subject_id,
        entities,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeds_three_roles_three_policies_and_one_admin_subject() {
        let b = bootstrap("admin");
        assert_eq!(b.entities.len(), 7);
        let roles = b
            .entities
            .iter()
            .filter(|e| e.header.entity_kind == EntityKind::Role)
            .count();
        let policies = b
            .entities
            .iter()
            .filter(|e| e.header.entity_kind == EntityKind::Policy)
            .count();
        let subjects = b
            .entities
            .iter()
            .filter(|e| e.header.entity_kind == EntityKind::Subject)
            .count();
        assert_eq!((roles, policies, subjects), (3, 3, 1));
    }

    #[test]
    fn role_ids_are_distinct_and_match_the_seeded_role_entities() {
        let b = bootstrap("admin");
        assert_ne!(b.admin_role_id, b.analyst_role_id);
        assert_ne!(b.admin_role_id, b.consumer_role_id);
        assert_ne!(b.analyst_role_id, b.consumer_role_id);

        let role_ids: Vec<Tuid> = b
            .entities
            .iter()
            .filter(|e| e.header.entity_kind == EntityKind::Role)
            .map(|e| e.header.id)
            .collect();
        assert!(role_ids.contains(&b.admin_role_id));
        assert!(role_ids.contains(&b.analyst_role_id));
        assert!(role_ids.contains(&b.consumer_role_id));
    }

    #[test]
    fn admin_subject_is_named_as_requested() {
        let b = bootstrap("root");
        let subject = b
            .entities
            .iter()
            .find(|e| e.header.id == b.admin_subject_id)
            .unwrap();
        assert_eq!(subject.header.name, "root");
    }
}
