//! FNV-1a, folding a human-readable field name into the `u32` attribute id
//! used as a GBV key (`gems-codec::GbvBuilder`/`GbvReader`). A real
//! schema-driven write path assigns these from `EntityAttribute`
//! definitions instead; this is a placeholder shared by every caller that
//! still lets a person type field names by hand (`gems-cli`, `gems-webui`).

pub fn field_id(name: &str) -> u32 {
    let mut hash: u32 = 0x811c9dc5;
    for b in name.as_bytes() {
        hash ^= *b as u32;
        hash = hash.wrapping_mul(0x01000193);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_id_is_stable_and_distinguishes_names() {
        assert_eq!(field_id("status"), field_id("status"));
        assert_ne!(field_id("status"), field_id("qty"));
    }
}
