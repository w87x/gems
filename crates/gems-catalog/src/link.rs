//! `LinkType` / `Link`: a lightweight property-graph edge, modeled after
//! RDF's subject–predicate–object triple (`LinkType` is the predicate) —
//! ARCHITECTURE.md §5.4. Both `source` and `target` are TUID lists (a
//! single `Link` record can connect several entities on each side); the
//! query/index layer indexes every `Link` in both directions via roaring
//! bitmaps so graph traversal is a bitmap lookup rather than a scan.

use gems_common::{Error, Result, Tuid};

use crate::util::{read_tuid, read_tuid_list, read_u8, write_tuid_list};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkType {
    pub directional: bool,
    pub temporal_allowed: bool,
}

impl LinkType {
    pub fn encode(&self) -> Vec<u8> {
        vec![self.directional as u8, self.temporal_allowed as u8]
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        if buf.len() < 2 {
            return Err(Error::InvalidValue {
                detail: "LinkType buffer too short",
            });
        }
        Ok(LinkType {
            directional: buf[0] != 0,
            temporal_allowed: buf[1] != 0,
        })
    }
}

/// `Temporal` links carry a validity window (`from`/`to`); `Constant`
/// links hold regardless of time and leave both unset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkTemporality {
    Constant,
    Temporal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub source: Vec<Tuid>,
    pub target: Vec<Tuid>,
    pub link_type: Tuid,
    pub temporality: LinkTemporality,
    /// Nanoseconds since epoch; meaningful only when `temporality` is
    /// `Temporal`.
    pub from: Option<i64>,
    pub to: Option<i64>,
}

impl Link {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        write_tuid_list(&mut out, &self.source);
        write_tuid_list(&mut out, &self.target);
        out.extend_from_slice(self.link_type.as_bytes());
        out.push(match self.temporality {
            LinkTemporality::Constant => 0,
            LinkTemporality::Temporal => 1,
        });
        write_optional_i64(&mut out, self.from);
        write_optional_i64(&mut out, self.to);
        out
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut pos = 0;
        let source = read_tuid_list(buf, &mut pos)?;
        let target = read_tuid_list(buf, &mut pos)?;
        let link_type = read_tuid(buf, &mut pos)?;
        let temporality = match read_u8(buf, &mut pos)? {
            0 => LinkTemporality::Constant,
            1 => LinkTemporality::Temporal,
            _ => {
                return Err(Error::InvalidValue {
                    detail: "unknown link temporality",
                })
            }
        };
        let from = read_optional_i64(buf, &mut pos)?;
        let to = read_optional_i64(buf, &mut pos)?;
        Ok(Link {
            source,
            target,
            link_type,
            temporality,
            from,
            to,
        })
    }
}

fn write_optional_i64(out: &mut Vec<u8>, v: Option<i64>) {
    match v {
        Some(v) => {
            out.push(1);
            out.extend_from_slice(&v.to_le_bytes());
        }
        None => {
            out.push(0);
            out.extend_from_slice(&0i64.to_le_bytes());
        }
    }
}

fn read_optional_i64(buf: &[u8], pos: &mut usize) -> Result<Option<i64>> {
    let present = read_u8(buf, pos)? != 0;
    let value = crate::util::read_i64(buf, pos)?;
    Ok(present.then_some(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_type_roundtrip() {
        let lt = LinkType {
            directional: true,
            temporal_allowed: true,
        };
        assert_eq!(LinkType::decode(&lt.encode()).unwrap(), lt);
    }

    #[test]
    fn link_roundtrip_temporal() {
        let link = Link {
            source: vec![Tuid::new([1u8; 16], 1)],
            target: vec![Tuid::new([2u8; 16], 2), Tuid::new([3u8; 16], 3)],
            link_type: Tuid::new([4u8; 16], 4),
            temporality: LinkTemporality::Temporal,
            from: Some(1000),
            to: Some(2000),
        };
        assert_eq!(Link::decode(&link.encode()).unwrap(), link);
    }

    #[test]
    fn link_roundtrip_constant_no_window() {
        let link = Link {
            source: vec![Tuid::new([5u8; 16], 5)],
            target: vec![Tuid::new([6u8; 16], 6)],
            link_type: Tuid::new([7u8; 16], 7),
            temporality: LinkTemporality::Constant,
            from: None,
            to: None,
        };
        assert_eq!(Link::decode(&link.encode()).unwrap(), link);
    }
}
