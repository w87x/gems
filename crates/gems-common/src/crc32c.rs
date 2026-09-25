//! CRC32C (Castagnoli) checksum, used to detect torn/corrupt writes on
//! extent headers, index pages, and entity slots. This is an integrity
//! check, not a cryptographic primitive, so a plain from-spec
//! implementation (no vendoring needed) is the right call — the exception
//! called out in ARCHITECTURE.md §0 is specifically about crypto (password
//! hashing, TOTP), not checksums.

const POLY: u32 = 0x82f63b78; // reversed Castagnoli polynomial

fn table() -> &'static [u32; 256] {
    use std::sync::OnceLock;
    static TABLE: OnceLock<[u32; 256]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = [0u32; 256];
        let mut i = 0u32;
        while (i as usize) < 256 {
            let mut c = i;
            let mut k = 0;
            while k < 8 {
                c = if c & 1 != 0 { POLY ^ (c >> 1) } else { c >> 1 };
                k += 1;
            }
            table[i as usize] = c;
            i += 1;
        }
        table
    })
}

pub fn crc32c(data: &[u8]) -> u32 {
    let table = table();
    let mut crc = !0u32;
    for &byte in data {
        crc = table[((crc ^ byte as u32) & 0xff) as usize] ^ (crc >> 8);
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vector() {
        // Standard CRC32C check value for the ASCII string "123456789".
        assert_eq!(crc32c(b"123456789"), 0xE3069283);
    }

    #[test]
    fn empty_input() {
        assert_eq!(crc32c(b""), 0);
    }
}
