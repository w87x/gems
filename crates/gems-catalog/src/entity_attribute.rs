//! `EntityAttribute`: an attribute definition (ARCHITECTURE.md §5.3) — the
//! prototype (value type), cardinality/nullability, which secondary index
//! structure (if any) it gets, and its validation/formatting rules. This
//! is a schema/aux entity body, so unlike `Data` entity bodies (GBV maps,
//! §4) it has a fixed, code-defined positional layout.

use gems_codec::TypeTag;
use gems_common::tuid::TUID_LEN;
use gems_common::{Error, Result, Tuid};

pub const FORMAT_TEMPLATE_MAX: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cardinality {
    Single,
    Multiple,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Indexed {
    None,
    Bitmap,
    BTree,
}

/// Validation constraints for an attribute's raw (canonical) value.
/// `min`/`max` apply to numeric prototypes; `regex_ref`/`enum_ref` point at
/// a `Regex`-prototype attribute's stored pattern or a `VariantList` entity
/// respectively (`Tuid::NIL` = not set); `precision` is a decimal scale
/// (e.g. cents for `Currency`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ValidationRule {
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub regex_ref: Tuid,
    pub enum_ref: Tuid,
    pub precision: u8,
}

impl ValidationRule {
    pub const NONE: ValidationRule = ValidationRule {
        min: None,
        max: None,
        regex_ref: Tuid::NIL,
        enum_ref: Tuid::NIL,
        precision: 0,
    };
}

/// A display template applied to the raw canonical value at render time —
/// e.g. `"${1}"` for a `Currency` attribute stored as integer minor units.
/// Purely presentational; never affects what's stored or compared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatRule {
    pub template: String,
}

impl FormatRule {
    pub const NONE: FormatRule = FormatRule {
        template: String::new(),
    };
}

#[derive(Debug, Clone, PartialEq)]
pub struct EntityAttribute {
    pub prototype: TypeTag,
    pub cardinality: Cardinality,
    pub nullable: bool,
    pub indexed: Indexed,
    pub validation: ValidationRule,
    pub format: FormatRule,
}

const FIXED_LEN: usize = 1 // prototype
    + 1 // cardinality
    + 1 // nullable
    + 1 // indexed
    + 1 + 8 // has_min, min
    + 1 + 8 // has_max, max
    + TUID_LEN // regex_ref
    + TUID_LEN // enum_ref
    + 1 // precision
    + 1; // format_len

impl EntityAttribute {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(FIXED_LEN + FORMAT_TEMPLATE_MAX);
        out.push(self.prototype as u8);
        out.push(match self.cardinality {
            Cardinality::Single => 0,
            Cardinality::Multiple => 1,
        });
        out.push(self.nullable as u8);
        out.push(match self.indexed {
            Indexed::None => 0,
            Indexed::Bitmap => 1,
            Indexed::BTree => 2,
        });
        match self.validation.min {
            Some(v) => {
                out.push(1);
                out.extend_from_slice(&v.to_le_bytes());
            }
            None => {
                out.push(0);
                out.extend_from_slice(&0f64.to_le_bytes());
            }
        }
        match self.validation.max {
            Some(v) => {
                out.push(1);
                out.extend_from_slice(&v.to_le_bytes());
            }
            None => {
                out.push(0);
                out.extend_from_slice(&0f64.to_le_bytes());
            }
        }
        out.extend_from_slice(self.validation.regex_ref.as_bytes());
        out.extend_from_slice(self.validation.enum_ref.as_bytes());
        out.push(self.validation.precision);

        let template_bytes = truncate_utf8(&self.format.template, FORMAT_TEMPLATE_MAX);
        out.push(template_bytes.len() as u8);
        out.extend_from_slice(template_bytes);
        out
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        if buf.len() < FIXED_LEN {
            return Err(Error::InvalidValue {
                detail: "EntityAttribute buffer too short",
            });
        }
        let mut pos = 0;
        let prototype = TypeTag::from_u8(buf[pos])?;
        pos += 1;
        let cardinality = match buf[pos] {
            0 => Cardinality::Single,
            1 => Cardinality::Multiple,
            _ => {
                return Err(Error::InvalidValue {
                    detail: "unknown cardinality",
                })
            }
        };
        pos += 1;
        let nullable = buf[pos] != 0;
        pos += 1;
        let indexed = match buf[pos] {
            0 => Indexed::None,
            1 => Indexed::Bitmap,
            2 => Indexed::BTree,
            _ => {
                return Err(Error::InvalidValue {
                    detail: "unknown indexed mode",
                })
            }
        };
        pos += 1;

        let has_min = buf[pos] != 0;
        pos += 1;
        let min_raw = f64::from_le_bytes(buf[pos..pos + 8].try_into().unwrap());
        pos += 8;
        let min = has_min.then_some(min_raw);

        let has_max = buf[pos] != 0;
        pos += 1;
        let max_raw = f64::from_le_bytes(buf[pos..pos + 8].try_into().unwrap());
        pos += 8;
        let max = has_max.then_some(max_raw);

        let regex_ref = Tuid::from_bytes(buf[pos..pos + TUID_LEN].try_into().unwrap());
        pos += TUID_LEN;
        let enum_ref = Tuid::from_bytes(buf[pos..pos + TUID_LEN].try_into().unwrap());
        pos += TUID_LEN;
        let precision = buf[pos];
        pos += 1;

        let template_len = buf[pos] as usize;
        pos += 1;
        if buf.len() < pos + template_len {
            return Err(Error::InvalidValue {
                detail: "EntityAttribute format template truncated",
            });
        }
        let template = String::from_utf8_lossy(&buf[pos..pos + template_len]).into_owned();

        Ok(EntityAttribute {
            prototype,
            cardinality,
            nullable,
            indexed,
            validation: ValidationRule {
                min,
                max,
                regex_ref,
                enum_ref,
                precision,
            },
            format: FormatRule { template },
        })
    }
}

fn truncate_utf8(s: &str, max_bytes: usize) -> &[u8] {
    if s.len() <= max_bytes {
        return s.as_bytes();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s.as_bytes()[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_with_all_fields_set() {
        let attr = EntityAttribute {
            prototype: TypeTag::Currency,
            cardinality: Cardinality::Single,
            nullable: false,
            indexed: Indexed::BTree,
            validation: ValidationRule {
                min: Some(0.0),
                max: Some(1_000_000.0),
                regex_ref: Tuid::NIL,
                enum_ref: Tuid::new([3u8; 16], 7),
                precision: 2,
            },
            format: FormatRule {
                template: "${1}".to_string(),
            },
        };
        let encoded = attr.encode();
        let decoded = EntityAttribute::decode(&encoded).unwrap();
        assert_eq!(decoded, attr);
    }

    #[test]
    fn roundtrip_with_no_optional_fields() {
        let attr = EntityAttribute {
            prototype: TypeTag::Str,
            cardinality: Cardinality::Multiple,
            nullable: true,
            indexed: Indexed::None,
            validation: ValidationRule::NONE,
            format: FormatRule::NONE,
        };
        let decoded = EntityAttribute::decode(&attr.encode()).unwrap();
        assert_eq!(decoded, attr);
    }
}
