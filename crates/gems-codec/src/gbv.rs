//! Directory format:
//!
//! ```text
//! u16  field_count
//! [field_count] x { u32 key_id, u8 type_tag, u32 offset, u32 len }   -- sorted by key_id
//! ...raw value bytes...
//! ```
//!
//! `offset` is relative to the start of the value region (immediately
//! after the directory), so a reader only needs the directory plus the
//! backing buffer to slice out any field.

use gems_common::{Error, Result};

const DIRECTORY_ENTRY_LEN: usize = 4 + 1 + 4 + 4; // key_id, type_tag, offset, len

/// Type tags: msgpack's fixed vocabulary plus the domain types requested in
/// ARCHITECTURE.md §4. Composite/geo types store their fields in a fixed
/// binary layout documented alongside each variant; `Array` elements share
/// a single element type and are laid out back-to-back so an indexed
/// element read is also O(1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TypeTag {
    Int64 = 0,
    Float64 = 1,
    Bool = 2,
    Str = 3,
    Bin = 4,
    Point2D = 5,    // 2x f64
    Point3D = 6,    // 3x f64
    Poly2D = 7,     // u32 count, then count x Point2D
    Poly3D = 8,     // u32 count, then count x Point3D
    Geo2D = 9,      // f64 lat, f64 lon
    Geo3D = 10,     // f64 lat, f64 lon, f64 alt
    Ipv4 = 11,      // 4 bytes
    Ipv6 = 12,      // 16 bytes
    Cidr4 = 13,     // 4 bytes network + u8 prefix_len
    Cidr6 = 14,     // 16 bytes network + u8 prefix_len
    Regex = 15,     // pattern string bytes
    Currency = 16,  // i64 minor units + u8 scale
    DateTime = 17,  // i64 ns since epoch
    Duration = 18,  // i64 ns
    Uuid = 19,      // 16 bytes
    EnumRef = 20,   // u32 VariantList entry index
    Array = 21,     // u32 element_count, u8 element_tag, then packed elements
    EntityRef = 22, // 24 bytes: another entity's Tuid
}

impl TypeTag {
    pub fn from_u8(v: u8) -> Result<Self> {
        use TypeTag::*;
        Ok(match v {
            0 => Int64,
            1 => Float64,
            2 => Bool,
            3 => Str,
            4 => Bin,
            5 => Point2D,
            6 => Point3D,
            7 => Poly2D,
            8 => Poly3D,
            9 => Geo2D,
            10 => Geo3D,
            11 => Ipv4,
            12 => Ipv6,
            13 => Cidr4,
            14 => Cidr6,
            15 => Regex,
            16 => Currency,
            17 => DateTime,
            18 => Duration,
            19 => Uuid,
            20 => EnumRef,
            21 => Array,
            22 => EntityRef,
            _ => {
                return Err(Error::InvalidValue {
                    detail: "unknown GBV type tag",
                })
            }
        })
    }
}

struct DirEntry {
    key_id: u32,
    type_tag: u8,
    offset: u32,
    len: u32,
}

/// Builds a GBV buffer from fields added in any order; fields are sorted by
/// `key_id` at `finish()` time so readers can binary-search.
pub struct GbvBuilder {
    entries: Vec<DirEntry>,
    values: Vec<u8>,
}

impl GbvBuilder {
    pub fn new() -> Self {
        GbvBuilder {
            entries: Vec::new(),
            values: Vec::new(),
        }
    }

    pub fn push(&mut self, key_id: u32, type_tag: TypeTag, value: &[u8]) {
        let offset = self.values.len() as u32;
        self.values.extend_from_slice(value);
        self.entries.push(DirEntry {
            key_id,
            type_tag: type_tag as u8,
            offset,
            len: value.len() as u32,
        });
    }

    pub fn finish(mut self) -> Vec<u8> {
        self.entries.sort_by_key(|e| e.key_id);
        let mut out =
            Vec::with_capacity(2 + self.entries.len() * DIRECTORY_ENTRY_LEN + self.values.len());
        out.extend_from_slice(&(self.entries.len() as u16).to_le_bytes());
        for e in &self.entries {
            out.extend_from_slice(&e.key_id.to_le_bytes());
            out.push(e.type_tag);
            out.extend_from_slice(&e.offset.to_le_bytes());
            out.extend_from_slice(&e.len.to_le_bytes());
        }
        out.extend_from_slice(&self.values);
        out
    }
}

impl Default for GbvBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Zero-copy reader over an existing GBV buffer (typically a slice
/// borrowed directly from an mmap'd entity slot).
pub struct GbvReader<'a> {
    buf: &'a [u8],
    field_count: usize,
    values_start: usize,
}

impl<'a> GbvReader<'a> {
    pub fn new(buf: &'a [u8]) -> Result<Self> {
        if buf.len() < 2 {
            return Err(Error::InvalidValue {
                detail: "GBV buffer too short for header",
            });
        }
        let field_count = u16::from_le_bytes([buf[0], buf[1]]) as usize;
        let values_start = 2 + field_count * DIRECTORY_ENTRY_LEN;
        if buf.len() < values_start {
            return Err(Error::InvalidValue {
                detail: "GBV buffer too short for directory",
            });
        }
        Ok(GbvReader {
            buf,
            field_count,
            values_start,
        })
    }

    fn entry_at(&self, i: usize) -> DirEntry {
        let base = 2 + i * DIRECTORY_ENTRY_LEN;
        let key_id = u32::from_le_bytes(self.buf[base..base + 4].try_into().unwrap());
        let type_tag = self.buf[base + 4];
        let offset = u32::from_le_bytes(self.buf[base + 5..base + 9].try_into().unwrap());
        let len = u32::from_le_bytes(self.buf[base + 9..base + 13].try_into().unwrap());
        DirEntry {
            key_id,
            type_tag,
            offset,
            len,
        }
    }

    /// Binary search the directory for `key_id` and return `(type_tag,
    /// value_bytes)` without copying or touching any other field.
    pub fn get(&self, key_id: u32) -> Option<(TypeTag, &'a [u8])> {
        let mut lo = 0usize;
        let mut hi = self.field_count;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let e = self.entry_at(mid);
            match e.key_id.cmp(&key_id) {
                std::cmp::Ordering::Equal => {
                    let tag = TypeTag::from_u8(e.type_tag).ok()?;
                    let bytes = self.field_bytes(&e)?;
                    return Some((tag, bytes));
                }
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
            }
        }
        None
    }

    /// Slices out a field's value bytes, checked against the buffer's
    /// actual length. `offset`/`len` come straight from the on-disk/
    /// on-wire directory — a corrupted buffer, or an entity body a
    /// malicious Raft leader/replication primary crafted, can claim any
    /// `u32` there. Without this check, `get`/`redact` would slice
    /// `self.buf[start..end]` directly and panic (index out of bounds) the
    /// instant a value's claimed range ran past the buffer, crashing
    /// whatever process just read an ordinary entity.
    fn field_bytes(&self, entry: &DirEntry) -> Option<&'a [u8]> {
        let start = self.values_start.checked_add(entry.offset as usize)?;
        let end = start.checked_add(entry.len as usize)?;
        if end > self.buf.len() {
            return None;
        }
        Some(&self.buf[start..end])
    }

    pub fn field_count(&self) -> usize {
        self.field_count
    }

    pub fn key_ids(&self) -> impl Iterator<Item = u32> + '_ {
        (0..self.field_count).map(move |i| self.entry_at(i).key_id)
    }

    /// Rebuild this buffer with every field in `exclude` removed — the
    /// ABAC PEP's field-level redaction obligation (ARCHITECTURE.md §8):
    /// a `Permit` decision can still hide specific attributes from the
    /// result rather than denying the whole entity. Fields not in
    /// `exclude` keep their original bytes untouched.
    pub fn redact(&self, exclude: &[u32]) -> Vec<u8> {
        let mut builder = GbvBuilder::new();
        for i in 0..self.field_count {
            let entry = self.entry_at(i);
            if exclude.contains(&entry.key_id) {
                continue;
            }
            if let (Ok(tag), Some(bytes)) =
                (TypeTag::from_u8(entry.type_tag), self.field_bytes(&entry))
            {
                builder.push(entry.key_id, tag, bytes);
            }
        }
        builder.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_returns_none_instead_of_panicking_on_an_out_of_range_directory_entry() {
        // Build one legitimate field, then corrupt its directory entry's
        // offset/len to claim bytes far past the end of the buffer — the
        // shape a corrupted on-disk value or a maliciously crafted
        // replicated entity body could take. Before the fix this indexed
        // `self.buf[start..end]` directly and panicked.
        let mut b = GbvBuilder::new();
        b.push(1, TypeTag::Str, b"hi");
        let mut buf = b.finish();

        // Directory entry layout: u32 key_id, u8 type_tag, u32 offset, u32 len.
        let offset_pos = 2 + 4 + 1; // after field_count header + key_id + type_tag
        buf[offset_pos..offset_pos + 4].copy_from_slice(&0u32.to_le_bytes());
        buf[offset_pos + 4..offset_pos + 8].copy_from_slice(&u32::MAX.to_le_bytes());

        let r = GbvReader::new(&buf).unwrap();
        assert_eq!(r.get(1), None);
        // redact() must also not panic on the same corrupted entry.
        assert!(GbvReader::new(&r.redact(&[])).is_ok());
    }

    #[test]
    fn roundtrip_multiple_fields() {
        let mut b = GbvBuilder::new();
        b.push(5, TypeTag::Str, b"hello");
        b.push(1, TypeTag::Int64, &42i64.to_le_bytes());
        b.push(3, TypeTag::Bool, &[1]);
        let buf = b.finish();

        let r = GbvReader::new(&buf).unwrap();
        assert_eq!(r.field_count(), 3);

        let (tag, val) = r.get(1).unwrap();
        assert_eq!(tag, TypeTag::Int64);
        assert_eq!(i64::from_le_bytes(val.try_into().unwrap()), 42);

        let (tag, val) = r.get(5).unwrap();
        assert_eq!(tag, TypeTag::Str);
        assert_eq!(val, b"hello");

        assert!(r.get(999).is_none());
    }

    #[test]
    fn directory_is_sorted_regardless_of_insertion_order() {
        let mut b = GbvBuilder::new();
        for key in [9, 2, 7, 1] {
            b.push(key, TypeTag::Bool, &[1]);
        }
        let buf = b.finish();
        let r = GbvReader::new(&buf).unwrap();
        let keys: Vec<u32> = r.key_ids().collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted);
    }

    #[test]
    fn rejects_truncated_buffer() {
        assert!(GbvReader::new(&[1]).is_err());
    }

    #[test]
    fn redact_removes_only_the_named_fields() {
        let mut b = GbvBuilder::new();
        b.push(1, TypeTag::Str, b"visible");
        b.push(2, TypeTag::Str, b"secret");
        b.push(3, TypeTag::Int64, &7i64.to_le_bytes());
        let buf = b.finish();

        let r = GbvReader::new(&buf).unwrap();
        let redacted = r.redact(&[2]);
        let r2 = GbvReader::new(&redacted).unwrap();

        assert_eq!(r2.field_count(), 2);
        assert_eq!(r2.get(1).unwrap().1, b"visible");
        assert!(r2.get(2).is_none());
        assert_eq!(
            i64::from_le_bytes(r2.get(3).unwrap().1.try_into().unwrap()),
            7
        );
    }

    #[test]
    fn redact_with_no_matches_keeps_everything() {
        let mut b = GbvBuilder::new();
        b.push(1, TypeTag::Bool, &[1]);
        let buf = b.finish();
        let r = GbvReader::new(&buf).unwrap();
        let redacted = r.redact(&[999]);
        assert_eq!(GbvReader::new(&redacted).unwrap().field_count(), 1);
    }
}
