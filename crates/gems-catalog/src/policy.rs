//! `Policy`: an ABAC policy entity (ARCHITECTURE.md §8). Reuses the same
//! entity-header/body machinery as every other schema/aux kind — a policy
//! is just another entity, indexed and stored the same way, which is what
//! lets the PDP evaluate "which policies apply" as an ordinary lookup
//! rather than needing a separate policy store.
//!
//! Scope for this pass: `TargetPredicate`/`SubjectPredicate` are simple
//! field-equality matches (entity kind/type; a specific subject or role),
//! not the full query-language fragments ARCHITECTURE.md §8 describes for
//! a target/subject predicate — expressing "every entity in layer group
//! ff" as a target needs the query planner's layer-membership resolution,
//! which doesn't exist yet. `Tuid::NIL` means "unconstrained" for every
//! `Tuid`-valued predicate field, consistent with how `schema_ref` already
//! uses `Tuid::NIL` for "no type" elsewhere in this crate.

use gems_common::{Error, Result, Tuid};

use crate::kind::EntityKind;
use crate::util::{read_tuid, read_u8};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    Permit,
    Deny,
}

/// Which entities a policy applies to. `Tuid::NIL` in `schema_ref` and
/// `None` in `entity_kind` both mean "any".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TargetPredicate {
    pub entity_kind: Option<EntityKind>,
    pub schema_ref: Tuid,
}

impl TargetPredicate {
    pub const ANY: TargetPredicate = TargetPredicate {
        entity_kind: None,
        schema_ref: Tuid::NIL,
    };

    fn matches(&self, entity_kind: EntityKind, schema_ref: &Tuid) -> bool {
        self.entity_kind.is_none_or(|k| k == entity_kind)
            && (self.schema_ref == Tuid::NIL || &self.schema_ref == schema_ref)
    }
}

/// Which subjects a policy applies to. `Tuid::NIL` in either field means
/// "any"; a policy matches if the acting subject's id equals `subject_id`
/// (when set) or the subject holds `role_id` (when set) — either
/// condition matching is enough (an "or", not an "and": a policy scoped by
/// role shouldn't also require a specific subject id).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubjectPredicate {
    pub subject_id: Tuid,
    pub role_id: Tuid,
}

impl SubjectPredicate {
    pub const ANY: SubjectPredicate = SubjectPredicate {
        subject_id: Tuid::NIL,
        role_id: Tuid::NIL,
    };

    fn matches(&self, acting_subject: &Tuid, subject_roles: &[Tuid]) -> bool {
        if self.subject_id == Tuid::NIL && self.role_id == Tuid::NIL {
            return true;
        }
        (self.subject_id != Tuid::NIL && &self.subject_id == acting_subject)
            || (self.role_id != Tuid::NIL && subject_roles.contains(&self.role_id))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    pub target: TargetPredicate,
    pub subject: SubjectPredicate,
    pub effect: Effect,
    /// Attribute ids to redact from the result body. Only meaningful when
    /// `effect` is `Permit` — a `Deny` drops the whole entity, so there's
    /// nothing left to redact (ARCHITECTURE.md §8).
    pub redact_attributes: Vec<u32>,
}

impl Policy {
    /// Whether this policy applies to `(acting_subject, subject_roles)`
    /// acting on an entity of `entity_kind`/`schema_ref`.
    pub fn applies(
        &self,
        acting_subject: &Tuid,
        subject_roles: &[Tuid],
        entity_kind: EntityKind,
        schema_ref: &Tuid,
    ) -> bool {
        self.target.matches(entity_kind, schema_ref)
            && self.subject.matches(acting_subject, subject_roles)
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self.target.entity_kind {
            Some(k) => {
                out.push(1);
                out.push(k as u8);
            }
            None => {
                out.push(0);
                out.push(0);
            }
        }
        out.extend_from_slice(self.target.schema_ref.as_bytes());
        out.extend_from_slice(self.subject.subject_id.as_bytes());
        out.extend_from_slice(self.subject.role_id.as_bytes());
        out.push(match self.effect {
            Effect::Permit => 0,
            Effect::Deny => 1,
        });
        out.extend_from_slice(&(self.redact_attributes.len() as u16).to_le_bytes());
        for id in &self.redact_attributes {
            out.extend_from_slice(&id.to_le_bytes());
        }
        out
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut pos = 0;
        let has_kind = read_u8(buf, &mut pos)? != 0;
        let kind_byte = read_u8(buf, &mut pos)?;
        let entity_kind = if has_kind {
            Some(EntityKind::from_u8(kind_byte)?)
        } else {
            None
        };
        let schema_ref = read_tuid(buf, &mut pos)?;
        let subject_id = read_tuid(buf, &mut pos)?;
        let role_id = read_tuid(buf, &mut pos)?;
        let effect = match read_u8(buf, &mut pos)? {
            0 => Effect::Permit,
            1 => Effect::Deny,
            _ => {
                return Err(Error::InvalidValue {
                    detail: "unknown policy effect",
                })
            }
        };
        let redact_count = crate::util::read_u16(buf, &mut pos)? as usize;
        let mut redact_attributes = Vec::with_capacity(redact_count);
        for _ in 0..redact_count {
            let bytes = crate::util::read_bytes(buf, &mut pos, 4)?;
            redact_attributes.push(u32::from_le_bytes(bytes.try_into().unwrap()));
        }

        Ok(Policy {
            target: TargetPredicate {
                entity_kind,
                schema_ref,
            },
            subject: SubjectPredicate {
                subject_id,
                role_id,
            },
            effect,
            redact_attributes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_full_policy() {
        let p = Policy {
            target: TargetPredicate {
                entity_kind: Some(EntityKind::Data),
                schema_ref: Tuid::new([1u8; 16], 1),
            },
            subject: SubjectPredicate {
                subject_id: Tuid::NIL,
                role_id: Tuid::new([2u8; 16], 2),
            },
            effect: Effect::Permit,
            redact_attributes: vec![7, 9, 42],
        };
        assert_eq!(Policy::decode(&p.encode()).unwrap(), p);
    }

    #[test]
    fn roundtrip_unconstrained_policy() {
        let p = Policy {
            target: TargetPredicate::ANY,
            subject: SubjectPredicate::ANY,
            effect: Effect::Deny,
            redact_attributes: vec![],
        };
        assert_eq!(Policy::decode(&p.encode()).unwrap(), p);
    }

    #[test]
    fn target_matches_kind_and_type() {
        let t = TargetPredicate {
            entity_kind: Some(EntityKind::Data),
            schema_ref: Tuid::new([1u8; 16], 1),
        };
        assert!(t.matches(EntityKind::Data, &Tuid::new([1u8; 16], 1)));
        assert!(!t.matches(EntityKind::Data, &Tuid::new([2u8; 16], 2)));
        assert!(!t.matches(EntityKind::Subject, &Tuid::new([1u8; 16], 1)));
        assert!(TargetPredicate::ANY.matches(EntityKind::Role, &Tuid::NIL));
    }

    #[test]
    fn subject_matches_by_id_or_role() {
        let alice = Tuid::new([1u8; 16], 1);
        let bob = Tuid::new([2u8; 16], 2);
        let admin_role = Tuid::new([3u8; 16], 3);

        let by_id = SubjectPredicate {
            subject_id: alice,
            role_id: Tuid::NIL,
        };
        assert!(by_id.matches(&alice, &[]));
        assert!(!by_id.matches(&bob, &[admin_role]));

        let by_role = SubjectPredicate {
            subject_id: Tuid::NIL,
            role_id: admin_role,
        };
        assert!(by_role.matches(&bob, &[admin_role]));
        assert!(!by_role.matches(&bob, &[]));

        assert!(SubjectPredicate::ANY.matches(&bob, &[]));
    }
}
