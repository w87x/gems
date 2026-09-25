//! `SlotPointer`: the address of a stored record, as described in
//! ARCHITECTURE.md §1.3. This is what the primary and secondary indexes
//! store as their "value" half.

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SlotPointer {
    pub file_id: u32,
    pub extent_index: u32,
    pub slot_index: u32,
    pub block_class: u8,
}

impl SlotPointer {
    pub const ENCODED_LEN: usize = 4 + 4 + 4 + 1;

    pub fn encode(&self) -> [u8; Self::ENCODED_LEN] {
        let mut out = [0u8; Self::ENCODED_LEN];
        out[0..4].copy_from_slice(&self.file_id.to_le_bytes());
        out[4..8].copy_from_slice(&self.extent_index.to_le_bytes());
        out[8..12].copy_from_slice(&self.slot_index.to_le_bytes());
        out[12] = self.block_class;
        out
    }

    pub fn decode(bytes: &[u8; Self::ENCODED_LEN]) -> Self {
        SlotPointer {
            file_id: u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
            extent_index: u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            slot_index: u32::from_le_bytes(bytes[8..12].try_into().unwrap()),
            block_class: bytes[12],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let p = SlotPointer {
            file_id: 7,
            extent_index: 42,
            slot_index: 1000,
            block_class: 3,
        };
        assert_eq!(SlotPointer::decode(&p.encode()), p);
    }
}
