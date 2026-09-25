//! `Layer` / `LayerGroup`: organizational containment for entities
//! (ARCHITECTURE.md §5.4). A `Layer` optionally carries a stored query
//! string, making it a "dynamic layer" whose membership is a materialized,
//! invalidate-on-write view rather than a fixed list — the query/planner
//! layer owns actually running and caching that query; this crate only
//! stores the string.

use gems_common::{Error, Result, Tuid};

use crate::util::{read_string_prefixed, read_tuid, read_u8, write_string_prefixed};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layer {
    pub group_ref: Tuid, // Tuid::NIL if this layer belongs to no LayerGroup
    /// `Some(query)` makes this a dynamic layer (ARCHITECTURE.md §5.4).
    pub dynamic_query: Option<String>,
}

impl Layer {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(self.group_ref.as_bytes());
        match &self.dynamic_query {
            Some(q) => {
                out.push(1);
                write_string_prefixed(&mut out, q);
            }
            None => out.push(0),
        }
        out
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut pos = 0;
        let group_ref = read_tuid(buf, &mut pos)?;
        let has_query = read_u8(buf, &mut pos)? != 0;
        let dynamic_query = if has_query {
            Some(read_string_prefixed(buf, &mut pos)?)
        } else {
            None
        };
        Ok(Layer {
            group_ref,
            dynamic_query,
        })
    }
}

/// `LayerGroup` is pure organization — its name/description already live
/// in the common entity header (ARCHITECTURE.md §5.1), so its body carries
/// no additional fields today. Kept as a distinct type (rather than reusing
/// `Layer`'s body) so the two can diverge independently later — e.g. if
/// layer groups grow their own nesting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LayerGroup;

impl LayerGroup {
    pub fn encode(&self) -> Vec<u8> {
        Vec::new()
    }

    pub fn decode(_buf: &[u8]) -> Result<Self> {
        Ok(LayerGroup)
    }
}

/// `VariantList`: an ordered enum of allowed values, referenced by an
/// `EntityAttribute` whose prototype is `EnumRef` (ARCHITECTURE.md §5.4).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VariantList {
    pub values: Vec<String>,
}

impl VariantList {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(self.values.len() as u16).to_le_bytes());
        for v in &self.values {
            write_string_prefixed(&mut out, v);
        }
        out
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut pos = 0;
        let count = crate::util::read_u16(buf, &mut pos)? as usize;
        let mut values = Vec::with_capacity(count);
        for _ in 0..count {
            values.push(read_string_prefixed(buf, &mut pos)?);
        }
        Ok(VariantList { values })
    }
}

/// `Role`: a pure tag entity. Attach to a `Subject` via a `Multiple,
/// EntityRef` attribute pointing at `Role` TUIDs (ARCHITECTURE.md §5.4);
/// "who has role X" is served the same way as group membership — a
/// derived roaring-bitmap index, not a field on `Role` itself. Its body
/// carries nothing beyond the common header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Role;

impl Role {
    pub fn encode(&self) -> Vec<u8> {
        Vec::new()
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        if !buf.is_empty() {
            return Err(Error::InvalidValue {
                detail: "Role body must be empty",
            });
        }
        Ok(Role)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layer_roundtrip_static() {
        let l = Layer {
            group_ref: Tuid::new([1u8; 16], 1),
            dynamic_query: None,
        };
        assert_eq!(Layer::decode(&l.encode()).unwrap(), l);
    }

    #[test]
    fn layer_roundtrip_dynamic() {
        let l = Layer {
            group_ref: Tuid::NIL,
            dynamic_query: Some("SELECT * FROM entities WHERE type IN (foo)".to_string()),
        };
        assert_eq!(Layer::decode(&l.encode()).unwrap(), l);
    }

    #[test]
    fn layer_group_roundtrip() {
        assert_eq!(
            LayerGroup::decode(&LayerGroup.encode()).unwrap(),
            LayerGroup
        );
    }

    #[test]
    fn variant_list_roundtrip() {
        let vl = VariantList {
            values: vec!["red".to_string(), "green".to_string(), "blue".to_string()],
        };
        assert_eq!(VariantList::decode(&vl.encode()).unwrap(), vl);
    }

    #[test]
    fn role_roundtrip_and_rejects_nonempty() {
        assert_eq!(Role::decode(&Role.encode()).unwrap(), Role);
        assert!(Role::decode(&[1]).is_err());
    }
}
