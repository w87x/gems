//! `EntityType`: modeled on LDAP's `objectClass` (ARCHITECTURE.md §5.3) —
//! exactly one `Structural` type per data entity, plus any number of
//! compatible `Auxiliary` types, with an explicit exclusion list for
//! combinations that don't make sense together.

use gems_common::tuid::TUID_LEN;
use gems_common::{Error, Result, Tuid};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntityTypeKind {
    Structural,
    Auxiliary,
}

/// One attribute this type contributes, with whether it's required and an
/// optional default value. `default` is a raw encoded value (e.g. a GBV
/// single-field payload) whose shape is defined by the referenced
/// `EntityAttribute`'s prototype — this crate doesn't interpret it.
#[derive(Debug, Clone, PartialEq)]
pub struct AttributeRef {
    pub attribute_id: Tuid,
    pub required: bool,
    pub default: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EntityType {
    pub kind: EntityTypeKind,
    pub attributes: Vec<AttributeRef>,
    pub compatible_with: Vec<Tuid>,
    /// Explicit exclusions; these win over `compatible_with` when both
    /// somehow name the same type (ARCHITECTURE.md §5.3).
    pub incompatible_with: Vec<Tuid>,
}

impl EntityType {
    pub fn is_compatible_with(&self, other: &Tuid) -> bool {
        !self.incompatible_with.contains(other) && self.compatible_with.contains(other)
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(match self.kind {
            EntityTypeKind::Structural => 0,
            EntityTypeKind::Auxiliary => 1,
        });

        out.extend_from_slice(&(self.attributes.len() as u16).to_le_bytes());
        for a in &self.attributes {
            out.extend_from_slice(a.attribute_id.as_bytes());
            out.push(a.required as u8);
            match &a.default {
                Some(bytes) => {
                    out.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
                    out.extend_from_slice(bytes);
                }
                None => out.extend_from_slice(&0u16.to_le_bytes()),
            }
        }

        write_tuid_list(&mut out, &self.compatible_with);
        write_tuid_list(&mut out, &self.incompatible_with);
        out
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut pos = 0;
        let kind = match read_u8(buf, &mut pos)? {
            0 => EntityTypeKind::Structural,
            1 => EntityTypeKind::Auxiliary,
            _ => {
                return Err(Error::InvalidValue {
                    detail: "unknown EntityType kind",
                })
            }
        };

        let attr_count = read_u16(buf, &mut pos)? as usize;
        let mut attributes = Vec::with_capacity(attr_count);
        for _ in 0..attr_count {
            let attribute_id = read_tuid(buf, &mut pos)?;
            let required = read_u8(buf, &mut pos)? != 0;
            let default_len = read_u16(buf, &mut pos)? as usize;
            let default = if default_len == 0 {
                None
            } else {
                Some(read_bytes(buf, &mut pos, default_len)?.to_vec())
            };
            attributes.push(AttributeRef {
                attribute_id,
                required,
                default,
            });
        }

        let compatible_with = read_tuid_list(buf, &mut pos)?;
        let incompatible_with = read_tuid_list(buf, &mut pos)?;

        Ok(EntityType {
            kind,
            attributes,
            compatible_with,
            incompatible_with,
        })
    }
}

fn write_tuid_list(out: &mut Vec<u8>, list: &[Tuid]) {
    out.extend_from_slice(&(list.len() as u16).to_le_bytes());
    for t in list {
        out.extend_from_slice(t.as_bytes());
    }
}

fn read_tuid_list(buf: &[u8], pos: &mut usize) -> Result<Vec<Tuid>> {
    let count = read_u16(buf, pos)? as usize;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        out.push(read_tuid(buf, pos)?);
    }
    Ok(out)
}

fn read_u8(buf: &[u8], pos: &mut usize) -> Result<u8> {
    let b = *buf.get(*pos).ok_or(Error::InvalidValue {
        detail: "EntityType buffer truncated",
    })?;
    *pos += 1;
    Ok(b)
}

fn read_u16(buf: &[u8], pos: &mut usize) -> Result<u16> {
    let bytes = read_bytes(buf, pos, 2)?;
    Ok(u16::from_le_bytes(bytes.try_into().unwrap()))
}

fn read_tuid(buf: &[u8], pos: &mut usize) -> Result<Tuid> {
    let bytes = read_bytes(buf, pos, TUID_LEN)?;
    Ok(Tuid::from_bytes(bytes.try_into().unwrap()))
}

fn read_bytes<'a>(buf: &'a [u8], pos: &mut usize, len: usize) -> Result<&'a [u8]> {
    if buf.len() < *pos + len {
        return Err(Error::InvalidValue {
            detail: "EntityType buffer truncated",
        });
    }
    let slice = &buf[*pos..*pos + len];
    *pos += len;
    Ok(slice)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_with_attributes_and_lists() {
        let et = EntityType {
            kind: EntityTypeKind::Structural,
            attributes: vec![
                AttributeRef {
                    attribute_id: Tuid::new([1u8; 16], 1),
                    required: true,
                    default: None,
                },
                AttributeRef {
                    attribute_id: Tuid::new([2u8; 16], 2),
                    required: false,
                    default: Some(vec![1, 2, 3, 4]),
                },
            ],
            compatible_with: vec![Tuid::new([3u8; 16], 3)],
            incompatible_with: vec![Tuid::new([4u8; 16], 4), Tuid::new([5u8; 16], 5)],
        };
        let decoded = EntityType::decode(&et.encode()).unwrap();
        assert_eq!(decoded, et);
    }

    #[test]
    fn roundtrip_empty() {
        let et = EntityType {
            kind: EntityTypeKind::Auxiliary,
            attributes: vec![],
            compatible_with: vec![],
            incompatible_with: vec![],
        };
        let decoded = EntityType::decode(&et.encode()).unwrap();
        assert_eq!(decoded, et);
    }

    #[test]
    fn incompatible_wins_over_compatible() {
        let clashing = Tuid::new([9u8; 16], 9);
        let et = EntityType {
            kind: EntityTypeKind::Structural,
            attributes: vec![],
            compatible_with: vec![clashing],
            incompatible_with: vec![clashing],
        };
        assert!(!et.is_compatible_with(&clashing));
    }

    #[test]
    fn rejects_truncated_buffer() {
        assert!(EntityType::decode(&[0, 5]).is_err());
    }
}
