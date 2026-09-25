//! ABAC PDP (policy decision point) and PEP (policy enforcement point),
//! per ARCHITECTURE.md §8. `Policy` itself is a `gems-catalog` entity body
//! (just another entity, per that section's design); this crate is the
//! evaluation logic over it.
//!
//! Deliberately storage-agnostic: `evaluate`/`enforce` take plain slices of
//! `Policy`/entities, not a `gems-engine::Store`. The caller (an engine,
//! CLI, or query executor) is responsible for fetching the candidate
//! entities and the policies that might apply to them; keeping this crate
//! free of a `Store` dependency makes the decision/enforcement logic
//! testable on its own and reusable from anywhere that already has the
//! data in hand.

use gems_catalog::{Effect, EntityHeader, Policy};
use gems_codec::GbvReader;
use gems_common::Tuid;

pub mod token;

/// The acting subject for a decision: their own id, plus every role they
/// hold (roles are resolved by the caller — this crate doesn't know how
/// group/role membership is stored, per ARCHITECTURE.md §5.4's
/// one-direction-of-truth membership model).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubjectContext {
    pub subject_id: Tuid,
    pub roles: Vec<Tuid>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Deny,
    Permit { redact_attributes: Vec<u32> },
}

/// Evaluate every applicable policy for `subject` acting on an entity of
/// `entity_kind`/`schema_ref`, using **deny-overrides**: if any applicable
/// policy denies, the result is `Deny` regardless of any `Permit`.
/// Otherwise, the union of every applicable `Permit` policy's
/// `redact_attributes` is returned. **No applicable policy at all is also
/// `Deny`** — this is a default-deny system (an entity is visible only
/// because some policy explicitly permits it), not default-allow.
pub fn evaluate(
    policies: &[Policy],
    subject: &SubjectContext,
    entity_kind: gems_catalog::EntityKind,
    schema_ref: &Tuid,
) -> Decision {
    let applicable: Vec<&Policy> = policies
        .iter()
        .filter(|p| p.applies(&subject.subject_id, &subject.roles, entity_kind, schema_ref))
        .collect();

    if applicable.iter().any(|p| p.effect == Effect::Deny) {
        return Decision::Deny;
    }
    if applicable.is_empty() {
        return Decision::Deny;
    }

    let mut redact_attributes: Vec<u32> = applicable
        .iter()
        .filter(|p| p.effect == Effect::Permit)
        .flat_map(|p| p.redact_attributes.iter().copied())
        .collect();
    redact_attributes.sort_unstable();
    redact_attributes.dedup();
    Decision::Permit { redact_attributes }
}

/// The PEP: apply `evaluate` to each `(header, body)` pair, dropping
/// denied entities and redacting fields per any `Permit` obligations.
/// This is what turns a query's raw candidate list into the partial,
/// access-controlled result ARCHITECTURE.md §8 describes.
pub fn enforce(
    policies: &[Policy],
    subject: &SubjectContext,
    entities: impl IntoIterator<Item = (EntityHeader, Vec<u8>)>,
) -> Vec<(EntityHeader, Vec<u8>)> {
    entities
        .into_iter()
        .filter_map(|(header, body)| {
            match evaluate(policies, subject, header.entity_kind, &header.schema_ref) {
                Decision::Deny => None,
                Decision::Permit { redact_attributes } if redact_attributes.is_empty() => {
                    Some((header, body))
                }
                Decision::Permit { redact_attributes } => {
                    let redacted = GbvReader::new(&body)
                        .map(|r| r.redact(&redact_attributes))
                        .unwrap_or(body);
                    Some((header, redacted))
                }
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gems_catalog::{EntityFlags, EntityKind, SubjectPredicate, TargetPredicate};
    use gems_codec::{GbvBuilder, TypeTag};

    fn header(kind: EntityKind, schema_ref: Tuid) -> EntityHeader {
        EntityHeader {
            id: Tuid::new([1u8; 16], 1),
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

    fn permit_all() -> Policy {
        Policy {
            target: TargetPredicate::ANY,
            subject: SubjectPredicate::ANY,
            effect: Effect::Permit,
            redact_attributes: vec![],
        }
    }

    fn anonymous() -> SubjectContext {
        SubjectContext {
            subject_id: Tuid::new([99u8; 16], 99),
            roles: vec![],
        }
    }

    #[test]
    fn no_policies_means_default_deny() {
        let decision = evaluate(&[], &anonymous(), EntityKind::Data, &Tuid::NIL);
        assert_eq!(decision, Decision::Deny);
    }

    #[test]
    fn a_matching_permit_allows() {
        let policies = vec![permit_all()];
        let decision = evaluate(&policies, &anonymous(), EntityKind::Data, &Tuid::NIL);
        assert_eq!(
            decision,
            Decision::Permit {
                redact_attributes: vec![]
            }
        );
    }

    #[test]
    fn deny_overrides_a_matching_permit() {
        let policies = vec![
            permit_all(),
            Policy {
                target: TargetPredicate::ANY,
                subject: SubjectPredicate::ANY,
                effect: Effect::Deny,
                redact_attributes: vec![],
            },
        ];
        let decision = evaluate(&policies, &anonymous(), EntityKind::Data, &Tuid::NIL);
        assert_eq!(decision, Decision::Deny);
    }

    #[test]
    fn redact_obligations_from_permit_policies_are_unioned() {
        let policies = vec![
            Policy {
                target: TargetPredicate::ANY,
                subject: SubjectPredicate::ANY,
                effect: Effect::Permit,
                redact_attributes: vec![1, 2],
            },
            Policy {
                target: TargetPredicate::ANY,
                subject: SubjectPredicate::ANY,
                effect: Effect::Permit,
                redact_attributes: vec![2, 3],
            },
        ];
        let decision = evaluate(&policies, &anonymous(), EntityKind::Data, &Tuid::NIL);
        assert_eq!(
            decision,
            Decision::Permit {
                redact_attributes: vec![1, 2, 3]
            }
        );
    }

    #[test]
    fn target_predicate_scopes_a_policy_to_matching_entities_only() {
        let type_a = Tuid::new([1u8; 16], 1);
        let type_b = Tuid::new([2u8; 16], 2);
        let policies = vec![Policy {
            target: TargetPredicate {
                entity_kind: Some(EntityKind::Data),
                schema_ref: type_a,
            },
            subject: SubjectPredicate::ANY,
            effect: Effect::Permit,
            redact_attributes: vec![],
        }];

        assert_eq!(
            evaluate(&policies, &anonymous(), EntityKind::Data, &type_a),
            Decision::Permit {
                redact_attributes: vec![]
            }
        );
        assert_eq!(
            evaluate(&policies, &anonymous(), EntityKind::Data, &type_b),
            Decision::Deny,
            "policy scoped to type_a must not permit type_b"
        );
    }

    #[test]
    fn enforce_drops_denied_and_redacts_permitted() {
        let type_id = Tuid::new([1u8; 16], 1);
        let mut body = GbvBuilder::new();
        body.push(1, TypeTag::Str, b"visible");
        body.push(2, TypeTag::Str, b"salary-secret");
        let body = body.finish();

        let visible_header = header(EntityKind::Data, type_id);
        let denied_header = header(EntityKind::Subject, Tuid::NIL); // no policy targets Subject

        let policies = vec![Policy {
            target: TargetPredicate {
                entity_kind: Some(EntityKind::Data),
                schema_ref: type_id,
            },
            subject: SubjectPredicate::ANY,
            effect: Effect::Permit,
            redact_attributes: vec![2],
        }];

        let result = enforce(
            &policies,
            &anonymous(),
            vec![
                (visible_header.clone(), body.clone()),
                (denied_header, body.clone()),
            ],
        );

        assert_eq!(result.len(), 1, "the Subject-kind entity must be dropped");
        let (got_header, got_body) = &result[0];
        assert_eq!(got_header.id, visible_header.id);

        let reader = GbvReader::new(got_body).unwrap();
        assert_eq!(reader.get(1).unwrap().1, b"visible");
        assert!(reader.get(2).is_none(), "field 2 must be redacted");
    }
}
