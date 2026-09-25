//! `LogRecord`: the unit of replication. Every mutation a `PrimaryStore`
//! makes is appended to its `ReplicationLog` as one of these and streamed
//! to replicas verbatim — this is the "apply record" ARCHITECTURE.md §6
//! says to ship over a plain TCP stream for log-shipping replicas, ahead
//! of attempting full Raft.
//!
//! Replicating at the **logical operation** level (insert this entity,
//! delete that id) rather than at the physical page level is a deliberate
//! choice: the primary index's CoW pages, the ordinal B-trees, and the
//! extent allocator's bitmaps are all internal to `gems-engine::Store` and
//! would be invasive to expose as a diffable byte stream. A replica that
//! replays the same logical operations through its own `Store` ends up
//! with equivalent (though not byte-identical — different page layout,
//! different physical slot placement) state, which is all a read replica
//! needs.
//!
//! Wire format: `u32 payload_len | payload`, where `payload` is
//! `u8 record_type (1=Insert, 2=Delete) | ...`. The same framing is used
//! both on disk (the log file) and on the wire (streamed to a replica).

use gems_catalog::EntityHeader;
use gems_common::tuid::TUID_LEN;
use gems_common::{Error, Result, Tuid};

use crate::frame::check_frame_len;

#[derive(Debug, Clone, PartialEq)]
pub enum LogRecord {
    Insert { header: EntityHeader, body: Vec<u8> },
    Delete { id: Tuid },
}

const INSERT_TAG: u8 = 1;
const DELETE_TAG: u8 = 2;

impl LogRecord {
    pub fn encode(&self) -> Vec<u8> {
        let mut payload = Vec::new();
        match self {
            LogRecord::Insert { header, body } => {
                payload.push(INSERT_TAG);
                let encoded_header = header.encode();
                payload.extend_from_slice(&(encoded_header.len() as u32).to_le_bytes());
                payload.extend_from_slice(&encoded_header);
                payload.extend_from_slice(&(body.len() as u32).to_le_bytes());
                payload.extend_from_slice(body);
            }
            LogRecord::Delete { id } => {
                payload.push(DELETE_TAG);
                payload.extend_from_slice(id.as_bytes());
            }
        }
        let mut framed = Vec::with_capacity(4 + payload.len());
        framed.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        framed.extend_from_slice(&payload);
        framed
    }

    /// Decode one record from the start of `buf`. Returns `None` (not an
    /// error) if `buf` doesn't yet contain a complete record — the normal
    /// case when reading a log tail or a socket that hasn't delivered a
    /// full record yet. Returns `(record, bytes_consumed)` on success,
    /// where `bytes_consumed` includes the length prefix.
    pub fn decode(buf: &[u8]) -> Result<Option<(Self, usize)>> {
        if buf.len() < 4 {
            return Ok(None);
        }
        let payload_len = u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize;
        check_frame_len(
            payload_len,
            "log record payload exceeds the maximum frame size",
        )?;
        let total_len = 4 + payload_len;
        if buf.len() < total_len {
            return Ok(None);
        }
        let payload = &buf[4..total_len];
        if payload.is_empty() {
            return Err(Error::InvalidValue {
                detail: "empty log record payload",
            });
        }

        let record = match payload[0] {
            INSERT_TAG => {
                let mut pos = 1;
                let header_len = read_u32(payload, &mut pos)?;
                let header = EntityHeader::decode(read_slice(payload, &mut pos, header_len)?)?;
                let body_len = read_u32(payload, &mut pos)?;
                let body = read_slice(payload, &mut pos, body_len)?.to_vec();
                LogRecord::Insert { header, body }
            }
            DELETE_TAG => {
                let mut pos = 1;
                let id_bytes = read_slice(payload, &mut pos, TUID_LEN)?;
                LogRecord::Delete {
                    id: Tuid::from_bytes(id_bytes.try_into().unwrap()),
                }
            }
            _ => {
                return Err(Error::InvalidValue {
                    detail: "unknown log record type",
                })
            }
        };
        Ok(Some((record, total_len)))
    }
}

fn read_u32(buf: &[u8], pos: &mut usize) -> Result<usize> {
    let bytes = read_slice(buf, pos, 4)?;
    Ok(u32::from_le_bytes(bytes.try_into().unwrap()) as usize)
}

fn read_slice<'a>(buf: &'a [u8], pos: &mut usize, len: usize) -> Result<&'a [u8]> {
    if buf.len() < *pos + len {
        return Err(Error::InvalidValue {
            detail: "log record payload truncated",
        });
    }
    let slice = &buf[*pos..*pos + len];
    *pos += len;
    Ok(slice)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gems_catalog::{EntityFlags, EntityKind};

    fn sample_header() -> EntityHeader {
        EntityHeader {
            id: Tuid::new([1u8; 16], 1),
            created_by: [2u8; 16],
            modified_by: [2u8; 16],
            modified_at_ns: 100,
            name: "w1".to_string(),
            description: String::new(),
            flags: EntityFlags::NONE,
            entity_kind: EntityKind::Data,
            schema_ref: Tuid::new([3u8; 16], 3),
            body_offset: 400,
            body_len: 5,
        }
    }

    #[test]
    fn insert_record_roundtrip() {
        let record = LogRecord::Insert {
            header: sample_header(),
            body: b"hello".to_vec(),
        };
        let encoded = record.encode();
        let (decoded, consumed) = LogRecord::decode(&encoded).unwrap().unwrap();
        assert_eq!(consumed, encoded.len());
        assert_eq!(decoded, record);
    }

    #[test]
    fn delete_record_roundtrip() {
        let record = LogRecord::Delete {
            id: Tuid::new([9u8; 16], 9),
        };
        let encoded = record.encode();
        let (decoded, consumed) = LogRecord::decode(&encoded).unwrap().unwrap();
        assert_eq!(consumed, encoded.len());
        assert_eq!(decoded, record);
    }

    #[test]
    fn decode_rejects_a_claimed_length_over_the_frame_cap_without_waiting_for_the_data() {
        // A hostile or corrupted length prefix claiming a huge payload
        // must error out immediately, from just the 4-byte prefix, rather
        // than returning `Ok(None)` and waiting for a reader loop to
        // accumulate that many bytes (the actual memory-exhaustion risk
        // this cap defends against).
        let mut buf = ((crate::frame::MAX_FRAME_LEN as u32) + 1)
            .to_le_bytes()
            .to_vec();
        buf.push(1); // one byte of "payload" — nowhere near the claimed length
        assert!(LogRecord::decode(&buf).is_err());
    }

    #[test]
    fn decode_returns_none_on_incomplete_buffer() {
        let record = LogRecord::Delete {
            id: Tuid::new([1u8; 16], 1),
        };
        let encoded = record.encode();
        assert!(LogRecord::decode(&encoded[..encoded.len() - 1])
            .unwrap()
            .is_none());
        assert!(LogRecord::decode(&[]).unwrap().is_none());
    }

    #[test]
    fn decodes_two_back_to_back_records_from_one_buffer() {
        let a = LogRecord::Delete {
            id: Tuid::new([1u8; 16], 1),
        };
        let b = LogRecord::Delete {
            id: Tuid::new([2u8; 16], 2),
        };
        let mut buf = a.encode();
        buf.extend_from_slice(&b.encode());

        let (decoded_a, consumed_a) = LogRecord::decode(&buf).unwrap().unwrap();
        assert_eq!(decoded_a, a);
        let (decoded_b, consumed_b) = LogRecord::decode(&buf[consumed_a..]).unwrap().unwrap();
        assert_eq!(decoded_b, b);
        assert_eq!(consumed_a + consumed_b, buf.len());
    }
}
