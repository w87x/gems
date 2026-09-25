//! The fixed-size header every entity carries, laid out exactly as
//! described in ARCHITECTURE.md §5.1. It is encoded/decoded explicitly
//! (rather than via `#[repr(C)]` + transmute) so the on-disk layout is
//! independent of Rust's struct layout rules and stays portable across
//! platforms.

use gems_common::tuid::TUID_LEN;
use gems_common::{Error, Result, Tuid};

use crate::flags::EntityFlags;
use crate::kind::EntityKind;

pub const MAGIC: u32 = 0x47454d48; // "GEMH"
pub const FORMAT_VERSION: u16 = 1;

pub const NAME_MAX: usize = 63;
pub const DESCRIPTION_MAX: usize = 254;

/// Field layout: `4 (magic) + 2 (version) + 24 (id) + 16 (created_by) + 16
/// (modified_by) + 8 (modified_at) + 1 (name_len) + 63 (name) + 2
/// (desc_len) + 254 (description) + 4 (flags) + 1 (entity_kind) + 24
/// (schema_ref) + 4 (body_offset) + 4 (body_len) + 4 (checksum)`.
pub const ENCODED_LEN: usize =
    4 + 2 + 24 + 16 + 16 + 8 + 1 + NAME_MAX + 2 + DESCRIPTION_MAX + 4 + 1 + 24 + 4 + 4 + 4;

#[derive(Debug, Clone, PartialEq)]
pub struct EntityHeader {
    pub id: Tuid,
    pub created_by: [u8; 16],
    pub modified_by: [u8; 16],
    pub modified_at_ns: i64,
    pub name: String,
    pub description: String,
    pub flags: EntityFlags,
    pub entity_kind: EntityKind,
    /// `EntityType` TUID for `Data` entities; `Tuid::NIL` for schema/aux
    /// entities (ARCHITECTURE.md §5.1).
    pub schema_ref: Tuid,
    pub body_offset: u32,
    pub body_len: u32,
}

impl EntityHeader {
    /// Convenience constructor for callers that don't yet have a real
    /// `created_by`/`modified_by` subject to attribute the write to — the
    /// same "generate a placeholder id" every hand-written header builder
    /// in this workspace (`gems-cli`, `gems-cluster-node`) already did
    /// ad hoc, now shared in one place.
    pub fn new(id: Tuid, name: &str, kind: EntityKind, schema_ref: Tuid) -> Self {
        let placeholder = Tuid::generate().uuid();
        EntityHeader {
            id,
            created_by: placeholder,
            modified_by: placeholder,
            modified_at_ns: id.created_at_ns() as i64,
            name: name.to_string(),
            description: String::new(),
            flags: EntityFlags::NONE,
            entity_kind: kind,
            schema_ref,
            body_offset: 0,
            body_len: 0,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(ENCODED_LEN);
        out.extend_from_slice(&MAGIC.to_le_bytes());
        out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(self.id.as_bytes());
        out.extend_from_slice(&self.created_by);
        out.extend_from_slice(&self.modified_by);
        out.extend_from_slice(&self.modified_at_ns.to_le_bytes());

        let name_bytes = truncate_utf8(&self.name, NAME_MAX);
        out.push(name_bytes.len() as u8);
        out.extend_from_slice(name_bytes);
        out.resize(out.len() + (NAME_MAX - name_bytes.len()), 0);

        let desc_bytes = truncate_utf8(&self.description, DESCRIPTION_MAX);
        out.extend_from_slice(&(desc_bytes.len() as u16).to_le_bytes());
        out.extend_from_slice(desc_bytes);
        out.resize(out.len() + (DESCRIPTION_MAX - desc_bytes.len()), 0);

        out.extend_from_slice(&self.flags.bits().to_le_bytes());
        out.push(self.entity_kind as u8);
        out.extend_from_slice(self.schema_ref.as_bytes());
        out.extend_from_slice(&self.body_offset.to_le_bytes());
        out.extend_from_slice(&self.body_len.to_le_bytes());

        let checksum = gems_common::crc32c::crc32c(&out);
        out.extend_from_slice(&checksum.to_le_bytes());
        out
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        if buf.len() < ENCODED_LEN {
            return Err(Error::CorruptPage {
                detail: "entity header shorter than expected",
            });
        }
        let magic = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        if magic != MAGIC {
            return Err(Error::CorruptPage {
                detail: "entity header magic mismatch",
            });
        }
        let checksum_at = ENCODED_LEN - 4;
        let expected = u32::from_le_bytes(buf[checksum_at..ENCODED_LEN].try_into().unwrap());
        let actual = gems_common::crc32c::crc32c(&buf[0..checksum_at]);
        if expected != actual {
            return Err(Error::CorruptPage {
                detail: "entity header checksum mismatch",
            });
        }

        let mut pos = 6; // magic + version
        let id = Tuid::from_bytes(buf[pos..pos + TUID_LEN].try_into().unwrap());
        pos += TUID_LEN;
        let created_by: [u8; 16] = buf[pos..pos + 16].try_into().unwrap();
        pos += 16;
        let modified_by: [u8; 16] = buf[pos..pos + 16].try_into().unwrap();
        pos += 16;
        let modified_at_ns = i64::from_le_bytes(buf[pos..pos + 8].try_into().unwrap());
        pos += 8;

        let name_len = buf[pos] as usize;
        pos += 1;
        // `name_len` comes straight from an on-disk/on-wire byte, so a
        // corrupted or maliciously crafted header (checksums only catch
        // accidental corruption, not a peer that computes a matching
        // checksum over fabricated bytes) could otherwise claim a length
        // longer than the field's reserved space and pull bytes from the
        // adjacent description field into the decoded name.
        if name_len > NAME_MAX {
            return Err(Error::CorruptPage {
                detail: "entity header name length exceeds the maximum",
            });
        }
        let name = String::from_utf8_lossy(&buf[pos..pos + name_len]).into_owned();
        pos += NAME_MAX;

        let desc_len = u16::from_le_bytes(buf[pos..pos + 2].try_into().unwrap()) as usize;
        pos += 2;
        if desc_len > DESCRIPTION_MAX {
            return Err(Error::CorruptPage {
                detail: "entity header description length exceeds the maximum",
            });
        }
        let description = String::from_utf8_lossy(&buf[pos..pos + desc_len]).into_owned();
        pos += DESCRIPTION_MAX;

        let flags =
            EntityFlags::from_bits(u32::from_le_bytes(buf[pos..pos + 4].try_into().unwrap()));
        pos += 4;
        let entity_kind = EntityKind::from_u8(buf[pos])?;
        pos += 1;
        let schema_ref = Tuid::from_bytes(buf[pos..pos + TUID_LEN].try_into().unwrap());
        pos += TUID_LEN;
        let body_offset = u32::from_le_bytes(buf[pos..pos + 4].try_into().unwrap());
        pos += 4;
        let body_len = u32::from_le_bytes(buf[pos..pos + 4].try_into().unwrap());

        Ok(EntityHeader {
            id,
            created_by,
            modified_by,
            modified_at_ns,
            name,
            description,
            flags,
            entity_kind,
            schema_ref,
            body_offset,
            body_len,
        })
    }
}

/// Truncate a string to at most `max_bytes` UTF-8 bytes without splitting a
/// multi-byte character.
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

    fn sample() -> EntityHeader {
        EntityHeader {
            id: Tuid::new([7u8; 16], 123),
            created_by: [1u8; 16],
            modified_by: [2u8; 16],
            modified_at_ns: 456,
            name: "widgets".to_string(),
            description: "a bunch of widgets".to_string(),
            flags: EntityFlags::DISABLED,
            entity_kind: EntityKind::Data,
            schema_ref: Tuid::new([9u8; 16], 1),
            body_offset: 512,
            body_len: 128,
        }
    }

    #[test]
    fn roundtrip() {
        let h = sample();
        let encoded = h.encode();
        assert_eq!(encoded.len(), ENCODED_LEN);
        let decoded = EntityHeader::decode(&encoded).unwrap();
        assert_eq!(decoded.id, h.id);
        assert_eq!(decoded.name, h.name);
        assert_eq!(decoded.description, h.description);
        assert_eq!(decoded.flags, h.flags);
        assert_eq!(decoded.schema_ref, h.schema_ref);
        assert_eq!(decoded.body_offset, h.body_offset);
        assert_eq!(decoded.body_len, h.body_len);
    }

    #[test]
    fn detects_corruption() {
        let mut encoded = sample().encode();
        encoded[10] ^= 0xff;
        assert!(EntityHeader::decode(&encoded).is_err());
    }

    #[test]
    fn truncates_long_name_at_char_boundary() {
        let mut h = sample();
        h.name = "a".repeat(100);
        let encoded = h.encode();
        let decoded = EntityHeader::decode(&encoded).unwrap();
        assert_eq!(decoded.name.len(), NAME_MAX);
    }

    #[test]
    fn rejects_a_name_length_byte_claiming_more_than_the_reserved_field() {
        // A crafted header (checksum recomputed to match, as a hostile
        // peer able to construct arbitrary bytes would do) claiming a
        // name_len bigger than NAME_MAX must be rejected rather than
        // silently reading bytes from the adjacent description field.
        let mut encoded = sample().encode();
        let name_len_pos = 6 + TUID_LEN + 16 + 16 + 8; // matches decode()'s `pos` there
        encoded[name_len_pos] = 200; // > NAME_MAX (63)
        let checksum_at = ENCODED_LEN - 4;
        let checksum = gems_common::crc32c::crc32c(&encoded[0..checksum_at]);
        encoded[checksum_at..ENCODED_LEN].copy_from_slice(&checksum.to_le_bytes());
        assert!(EntityHeader::decode(&encoded).is_err());
    }

    #[test]
    fn rejects_a_description_length_claiming_more_than_the_reserved_field() {
        let mut encoded = sample().encode();
        let desc_len_pos = 6 + TUID_LEN + 16 + 16 + 8 + 1 + NAME_MAX;
        encoded[desc_len_pos..desc_len_pos + 2].copy_from_slice(&(60000u16).to_le_bytes()); // > DESCRIPTION_MAX (254)
        let checksum_at = ENCODED_LEN - 4;
        let checksum = gems_common::crc32c::crc32c(&encoded[0..checksum_at]);
        encoded[checksum_at..ENCODED_LEN].copy_from_slice(&checksum.to_le_bytes());
        assert!(EntityHeader::decode(&encoded).is_err());
    }
}
