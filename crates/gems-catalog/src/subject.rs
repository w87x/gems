//! `Subject`: user/group/organization/service (ARCHITECTURE.md §5.4).
//!
//! Membership is one-directional by design: a `Subject` records
//! `member_of` (the groups/orgs *it* belongs to); "who belongs to group
//! X" is a derived roaring-bitmap secondary index (§3), not a physically
//! stored reverse edge — the same LDAP-`memberOf`-overlay problem
//! `ARCHITECTURE.md` calls out, avoided by only ever writing one
//! direction of truth.
//!
//! Credential storage here is deliberately just opaque, bounded byte
//! blobs with no hashing/verification logic. ARCHITECTURE.md §0 calls out
//! password hashing (Argon2id) and TOTP (HMAC-SHA) as the one place
//! worth vendoring a well-known reference implementation instead of
//! hand-rolling — that vendoring is a separate, focused piece of work and
//! doesn't belong bundled into the entity-model crate.

use gems_common::{Error, Result, Tuid};

use crate::util::{
    read_optional_bytes, read_tuid_list, read_u8, write_optional_bytes, write_tuid_list,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubjectKind {
    BuiltinSystem,
    BuiltinAnonymous,
    BuiltinEveryone,
    BuiltinNobody,
    BuiltinAuthorized,
    User,
    Group,
    Organization,
    Service,
}

impl SubjectKind {
    fn to_u8(self) -> u8 {
        match self {
            SubjectKind::BuiltinSystem => 0,
            SubjectKind::BuiltinAnonymous => 1,
            SubjectKind::BuiltinEveryone => 2,
            SubjectKind::BuiltinNobody => 3,
            SubjectKind::BuiltinAuthorized => 4,
            SubjectKind::User => 5,
            SubjectKind::Group => 6,
            SubjectKind::Organization => 7,
            SubjectKind::Service => 8,
        }
    }

    fn from_u8(v: u8) -> Result<Self> {
        Ok(match v {
            0 => SubjectKind::BuiltinSystem,
            1 => SubjectKind::BuiltinAnonymous,
            2 => SubjectKind::BuiltinEveryone,
            3 => SubjectKind::BuiltinNobody,
            4 => SubjectKind::BuiltinAuthorized,
            5 => SubjectKind::User,
            6 => SubjectKind::Group,
            7 => SubjectKind::Organization,
            8 => SubjectKind::Service,
            _ => {
                return Err(Error::InvalidValue {
                    detail: "unknown SubjectKind",
                })
            }
        })
    }

    /// Only `User`/`Service` subjects carry `Credentials`.
    pub fn accepts_credentials(self) -> bool {
        matches!(self, SubjectKind::User | SubjectKind::Service)
    }
}

/// Opaque credential material. See the module doc: no hashing/verification
/// logic lives here, just bounded storage for whatever a vendored crypto
/// implementation produces.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Credentials {
    pub password_hash: Option<Vec<u8>>,
    pub totp_secret: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Subject {
    pub kind: SubjectKind,
    pub credentials: Option<Credentials>,
    pub member_of: Vec<Tuid>,
}

impl Subject {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(self.kind.to_u8());
        match &self.credentials {
            Some(c) => {
                out.push(1);
                write_optional_bytes(&mut out, &c.password_hash);
                write_optional_bytes(&mut out, &c.totp_secret);
            }
            None => out.push(0),
        }
        write_tuid_list(&mut out, &self.member_of);
        out
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut pos = 0;
        let kind = SubjectKind::from_u8(read_u8(buf, &mut pos)?)?;
        let has_credentials = read_u8(buf, &mut pos)? != 0;
        let credentials = if has_credentials {
            let password_hash = read_optional_bytes(buf, &mut pos)?;
            let totp_secret = read_optional_bytes(buf, &mut pos)?;
            Some(Credentials {
                password_hash,
                totp_secret,
            })
        } else {
            None
        };
        let member_of = read_tuid_list(buf, &mut pos)?;
        Ok(Subject {
            kind,
            credentials,
            member_of,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_user_with_credentials() {
        let s = Subject {
            kind: SubjectKind::User,
            credentials: Some(Credentials {
                password_hash: Some(vec![0xaa; 32]),
                totp_secret: Some(vec![0xbb; 20]),
            }),
            member_of: vec![Tuid::new([1u8; 16], 1), Tuid::new([2u8; 16], 2)],
        };
        assert_eq!(Subject::decode(&s.encode()).unwrap(), s);
    }

    #[test]
    fn roundtrip_group_without_credentials() {
        let s = Subject {
            kind: SubjectKind::Group,
            credentials: None,
            member_of: vec![Tuid::new([3u8; 16], 3)],
        };
        assert_eq!(Subject::decode(&s.encode()).unwrap(), s);
    }

    #[test]
    fn builtin_kinds_reject_credentials() {
        assert!(!SubjectKind::BuiltinAnonymous.accepts_credentials());
        assert!(SubjectKind::User.accepts_credentials());
        assert!(SubjectKind::Service.accepts_credentials());
    }
}
