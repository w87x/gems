//! Unpadded base64url (RFC 4648 §5), the encoding JWT uses for its three
//! dot-separated segments. Hand-rolled alongside `sha256`/`hmac` rather
//! than pulling in a crate for it.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

pub fn encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);

        let n = (b0 as u32) << 16 | (b1 as u32) << 8 | b2 as u32;
        out.push(ALPHABET[(n >> 18 & 0x3f) as usize] as char);
        out.push(ALPHABET[(n >> 12 & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(n >> 6 & 0x3f) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(n & 0x3f) as usize] as char);
        }
    }
    out
}

pub fn decode(input: &str) -> Result<Vec<u8>, &'static str> {
    fn value(c: u8) -> Result<u32, &'static str> {
        match c {
            b'A'..=b'Z' => Ok((c - b'A') as u32),
            b'a'..=b'z' => Ok((c - b'a' + 26) as u32),
            b'0'..=b'9' => Ok((c - b'0' + 52) as u32),
            b'-' => Ok(62),
            b'_' => Ok(63),
            _ => Err("invalid base64url character"),
        }
    }

    let bytes = input.as_bytes();
    if bytes.contains(&b'=') {
        return Err("base64url input must not be padded");
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3 + 3);
    for chunk in bytes.chunks(4) {
        if chunk.len() == 1 {
            return Err("invalid base64url length");
        }
        let v0 = value(chunk[0])?;
        let v1 = value(chunk[1])?;
        let n = v0 << 18 | v1 << 12;
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            let v2 = value(chunk[2])?;
            let n = n | v2 << 6;
            out.push((n >> 8) as u8);
            if chunk.len() > 3 {
                let v3 = value(chunk[3])?;
                let n = n | v3;
                out.push(n as u8);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_arbitrary_bytes() {
        for input in [
            &b""[..],
            b"f",
            b"fo",
            b"foo",
            b"foob",
            b"fooba",
            b"foobar",
            &[0u8, 255, 128, 1, 254],
        ] {
            let encoded = encode(input);
            assert_eq!(decode(&encoded).unwrap(), input);
        }
    }

    #[test]
    fn matches_known_rfc4648_vectors() {
        // RFC 4648 §10, translated to the url-safe alphabet (no +/ or
        // padding differences arise for these particular inputs).
        assert_eq!(encode(b"f"), "Zg");
        assert_eq!(encode(b"fo"), "Zm8");
        assert_eq!(encode(b"foo"), "Zm9v");
        assert_eq!(encode(b"foob"), "Zm9vYg");
        assert_eq!(encode(b"fooba"), "Zm9vYmE");
        assert_eq!(encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn uses_url_safe_characters() {
        // Bytes chosen so the standard base64 alphabet would emit '+'/'/'.
        let encoded = encode(&[0xfb, 0xff, 0xbf]);
        assert!(!encoded.contains('+'));
        assert!(!encoded.contains('/'));
    }

    #[test]
    fn rejects_padding() {
        assert!(decode("Zg==").is_err());
    }

    #[test]
    fn rejects_invalid_characters() {
        assert!(decode("not valid!").is_err());
    }
}
