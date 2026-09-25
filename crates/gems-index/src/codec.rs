//! `FixedCodec` implementations for the concrete key/value types the
//! primary index uses today: `Tuid` keys, `SlotPointer` values. Secondary
//! indexes over other fixed-width field types (§3 of ARCHITECTURE.md) plug
//! into the same `BTree<K, V>` by implementing this trait.

use crate::node::FixedCodec;
use gems_common::Tuid;
use gems_storage::SlotPointer;

impl FixedCodec for Tuid {
    const LEN: usize = gems_common::tuid::TUID_LEN;

    fn encode_into(&self, out: &mut [u8]) {
        out.copy_from_slice(self.as_bytes());
    }

    fn decode_from(bytes: &[u8]) -> Self {
        let arr: [u8; gems_common::tuid::TUID_LEN] = bytes.try_into().unwrap();
        Tuid::from_bytes(arr)
    }
}

impl FixedCodec for SlotPointer {
    const LEN: usize = SlotPointer::ENCODED_LEN;

    fn encode_into(&self, out: &mut [u8]) {
        out.copy_from_slice(&self.encode());
    }

    fn decode_from(bytes: &[u8]) -> Self {
        let arr: [u8; SlotPointer::ENCODED_LEN] = bytes.try_into().unwrap();
        SlotPointer::decode(&arr)
    }
}
