//! Entity kinds. `Data` entities carry a `schema_ref` pointing at the
//! `EntityType` that governs their body; every other kind is a schema/aux
//! entity with a fixed, code-defined body shape (ARCHITECTURE.md §5.3).

use gems_common::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum EntityKind {
    Data = 0,
    EntityAttribute = 1,
    EntityType = 2,
    Subject = 3,
    Layer = 4,
    LayerGroup = 5,
    VariantList = 6,
    Role = 7,
    LinkType = 8,
    Link = 9,
    Policy = 10,
}

impl EntityKind {
    pub fn from_u8(v: u8) -> Result<Self> {
        use EntityKind::*;
        Ok(match v {
            0 => Data,
            1 => EntityAttribute,
            2 => EntityType,
            3 => Subject,
            4 => Layer,
            5 => LayerGroup,
            6 => VariantList,
            7 => Role,
            8 => LinkType,
            9 => Link,
            10 => Policy,
            _ => {
                return Err(Error::InvalidValue {
                    detail: "unknown entity kind",
                })
            }
        })
    }

    /// Data entities are the only kind whose body shape is
    /// schema-dependent (governed by an `EntityType`); every other kind
    /// has a fixed, code-defined body.
    pub fn is_data(&self) -> bool {
        matches!(self, EntityKind::Data)
    }
}
