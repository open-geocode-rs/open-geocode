//! Byte-level encoding shared by the Pack format and the build scratch files.
//!
//! Integers use LEB128 varints; signed values are zigzag-mapped first so small
//! magnitudes of either sign stay small. Readers advance a `&[u8]` cursor and
//! report truncation as an error instead of panicking, because Pack bytes come
//! from disk.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};

pub(crate) fn put_u64(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

pub(crate) fn put_i64(out: &mut Vec<u8>, value: i64) {
    put_u64(out, zigzag(value));
}

pub(crate) fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    put_u64(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

pub(crate) fn put_str(out: &mut Vec<u8>, value: &str) {
    put_bytes(out, value.as_bytes());
}

pub(crate) fn put_opt_str(out: &mut Vec<u8>, value: Option<&str>) {
    match value {
        Some(value) => {
            out.push(1);
            put_str(out, value);
        }
        None => out.push(0),
    }
}

pub(crate) fn put_tags(out: &mut Vec<u8>, tags: &BTreeMap<String, String>) {
    put_u64(out, tags.len() as u64);
    for (key, value) in tags {
        put_str(out, key);
        put_str(out, value);
    }
}

pub(crate) const fn zigzag(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)) as u64
}

pub(crate) const fn unzigzag(value: u64) -> i64 {
    ((value >> 1) as i64) ^ -((value & 1) as i64)
}

pub(crate) fn get_u8(input: &mut &[u8]) -> Result<u8> {
    let (&byte, rest) = input
        .split_first()
        .context("unexpected end of encoded data")?;
    *input = rest;
    Ok(byte)
}

pub(crate) fn get_u64(input: &mut &[u8]) -> Result<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = get_u8(input)?;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    bail!("varint is longer than 64 bits")
}

pub(crate) fn get_u32(input: &mut &[u8]) -> Result<u32> {
    u32::try_from(get_u64(input)?).context("varint does not fit in u32")
}

pub(crate) fn get_i64(input: &mut &[u8]) -> Result<i64> {
    Ok(unzigzag(get_u64(input)?))
}

pub(crate) fn get_i32(input: &mut &[u8]) -> Result<i32> {
    i32::try_from(get_i64(input)?).context("varint does not fit in i32")
}

pub(crate) fn get_bytes<'a>(input: &mut &'a [u8]) -> Result<&'a [u8]> {
    let len = usize::try_from(get_u64(input)?).context("length does not fit in usize")?;
    if input.len() < len {
        bail!("unexpected end of encoded data");
    }
    let (bytes, rest) = input.split_at(len);
    *input = rest;
    Ok(bytes)
}

pub(crate) fn get_str<'a>(input: &mut &'a [u8]) -> Result<&'a str> {
    std::str::from_utf8(get_bytes(input)?).context("encoded text is not valid UTF-8")
}

pub(crate) fn get_string(input: &mut &[u8]) -> Result<String> {
    get_str(input).map(str::to_string)
}

pub(crate) fn get_opt_string(input: &mut &[u8]) -> Result<Option<String>> {
    match get_u8(input)? {
        0 => Ok(None),
        1 => get_string(input).map(Some),
        other => bail!("invalid option tag {other}"),
    }
}

pub(crate) fn get_tags(input: &mut &[u8]) -> Result<BTreeMap<String, String>> {
    let count = get_u64(input)?;
    let mut tags = BTreeMap::new();
    for _ in 0..count {
        let key = get_string(input)?;
        let value = get_string(input)?;
        tags.insert(key, value);
    }
    Ok(tags)
}

pub(crate) fn read_u64_le(bytes: &[u8], offset: usize) -> Option<u64> {
    let array: [u8; 8] = bytes.get(offset..offset.checked_add(8)?)?.try_into().ok()?;
    Some(u64::from_le_bytes(array))
}

pub(crate) fn read_u32_le(bytes: &[u8], offset: usize) -> Option<u32> {
    let array: [u8; 4] = bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?;
    Some(u32::from_le_bytes(array))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_varints_and_zigzag() {
        let mut out = Vec::new();
        let unsigned = [
            0,
            1,
            127,
            128,
            16_383,
            16_384,
            u64::from(u32::MAX),
            u64::MAX,
        ];
        let signed = [0, -1, 1, -64, 64, i64::from(i32::MIN), i64::MAX, i64::MIN];
        for value in unsigned {
            put_u64(&mut out, value);
        }
        for value in signed {
            put_i64(&mut out, value);
        }
        put_str(&mut out, "King Street");
        put_opt_str(&mut out, None);
        put_opt_str(&mut out, Some("M5V"));

        let mut input = out.as_slice();
        for value in unsigned {
            assert_eq!(get_u64(&mut input).unwrap(), value);
        }
        for value in signed {
            assert_eq!(get_i64(&mut input).unwrap(), value);
        }
        assert_eq!(get_str(&mut input).unwrap(), "King Street");
        assert_eq!(get_opt_string(&mut input).unwrap(), None);
        assert_eq!(get_opt_string(&mut input).unwrap().as_deref(), Some("M5V"));
        assert!(input.is_empty());
        assert!(get_u8(&mut input).is_err());
    }

    #[test]
    fn small_magnitudes_encode_in_one_byte() {
        for value in [-64, -1, 0, 1, 63] {
            let mut out = Vec::new();
            put_i64(&mut out, value);
            assert_eq!(out.len(), 1, "value {value}");
        }
    }

    #[test]
    fn rejects_truncated_strings() {
        let mut out = Vec::new();
        put_str(&mut out, "abcdef");
        out.truncate(3);
        assert!(get_str(&mut out.as_slice()).is_err());
    }
}
