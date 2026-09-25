//! Small shared primitives for the length-prefixed positional encoding
//! every schema/aux entity body uses: fixed-width scalars, `Tuid`s, and
//! `u16`-length-prefixed byte/string blobs and `Tuid` lists.

use gems_common::tuid::TUID_LEN;
use gems_common::{Error, Result, Tuid};

pub fn read_u8(buf: &[u8], pos: &mut usize) -> Result<u8> {
    let b = *buf.get(*pos).ok_or(Error::InvalidValue {
        detail: "buffer truncated reading u8",
    })?;
    *pos += 1;
    Ok(b)
}

pub fn read_u16(buf: &[u8], pos: &mut usize) -> Result<u16> {
    let bytes = read_bytes(buf, pos, 2)?;
    Ok(u16::from_le_bytes(bytes.try_into().unwrap()))
}

pub fn read_i64(buf: &[u8], pos: &mut usize) -> Result<i64> {
    let bytes = read_bytes(buf, pos, 8)?;
    Ok(i64::from_le_bytes(bytes.try_into().unwrap()))
}

pub fn read_tuid(buf: &[u8], pos: &mut usize) -> Result<Tuid> {
    let bytes = read_bytes(buf, pos, TUID_LEN)?;
    Ok(Tuid::from_bytes(bytes.try_into().unwrap()))
}

pub fn read_bytes<'a>(buf: &'a [u8], pos: &mut usize, len: usize) -> Result<&'a [u8]> {
    if buf.len() < *pos + len {
        return Err(Error::InvalidValue {
            detail: "buffer truncated",
        });
    }
    let slice = &buf[*pos..*pos + len];
    *pos += len;
    Ok(slice)
}

pub fn write_u16_prefixed(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
    out.extend_from_slice(bytes);
}

pub fn read_u16_prefixed<'a>(buf: &'a [u8], pos: &mut usize) -> Result<&'a [u8]> {
    let len = read_u16(buf, pos)? as usize;
    read_bytes(buf, pos, len)
}

pub fn write_string_prefixed(out: &mut Vec<u8>, s: &str) {
    write_u16_prefixed(out, s.as_bytes());
}

pub fn read_string_prefixed(buf: &[u8], pos: &mut usize) -> Result<String> {
    Ok(String::from_utf8_lossy(read_u16_prefixed(buf, pos)?).into_owned())
}

pub fn write_tuid_list(out: &mut Vec<u8>, list: &[Tuid]) {
    out.extend_from_slice(&(list.len() as u16).to_le_bytes());
    for t in list {
        out.extend_from_slice(t.as_bytes());
    }
}

pub fn read_tuid_list(buf: &[u8], pos: &mut usize) -> Result<Vec<Tuid>> {
    let count = read_u16(buf, pos)? as usize;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        out.push(read_tuid(buf, pos)?);
    }
    Ok(out)
}

pub fn write_optional_bytes(out: &mut Vec<u8>, bytes: &Option<Vec<u8>>) {
    match bytes {
        Some(b) => write_u16_prefixed(out, b),
        None => out.extend_from_slice(&0u16.to_le_bytes()),
    }
}

pub fn read_optional_bytes(buf: &[u8], pos: &mut usize) -> Result<Option<Vec<u8>>> {
    let bytes = read_u16_prefixed(buf, pos)?;
    Ok(if bytes.is_empty() {
        None
    } else {
        Some(bytes.to_vec())
    })
}
