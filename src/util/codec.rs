//! Byte-level encoding shared by the Pack format and the build scratch files.
//!
//! Integers use LEB128 varints; signed values are zigzag-mapped first so small
//! magnitudes of either sign stay small. Readers advance a `&[u8]` cursor and
//! report truncation as an error instead of panicking, because Pack bytes come
//! from disk.

use crate::{
    error::{Error, Result},
    record::{Entrance, Extent},
};

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

pub(crate) fn put_extent(out: &mut Vec<u8>, extent: Extent) {
    for side in extent {
        put_i64(out, i64::from(side));
    }
}

pub(crate) fn get_extent(input: &mut &[u8]) -> Result<Extent> {
    let mut extent = [0; 4];
    for side in &mut extent {
        *side = i32::try_from(get_i64(input)?)
            .map_err(|_| Error::Decode("an extent's side is out of range"))?;
    }
    Ok(extent)
}

/// A presence byte, then the extent it flags.
pub(crate) fn put_opt_extent(out: &mut Vec<u8>, extent: Option<Extent>) {
    out.push(u8::from(extent.is_some()));
    if let Some(extent) = extent {
        put_extent(out, extent);
    }
}

pub(crate) fn get_opt_extent(input: &mut &[u8]) -> Result<Option<Extent>> {
    match get_u8(input)? {
        0 => Ok(None),
        1 => get_extent(input).map(Some),
        _ => Err(Error::Decode("an extent's presence byte is not 0 or 1")),
    }
}

/// A count, then each entrance as its longitude, its latitude and its kind.
pub(crate) fn put_entrances(out: &mut Vec<u8>, entrances: &[Entrance]) {
    put_u64(out, entrances.len() as u64);
    for entrance in entrances {
        put_i64(out, i64::from(entrance.at[0]));
        put_i64(out, i64::from(entrance.at[1]));
        put_str(out, &entrance.kind);
    }
}

pub(crate) fn get_entrances(input: &mut &[u8]) -> Result<Vec<Entrance>> {
    let count = get_u64(input)?;
    // Each takes at least three bytes, so a corrupt count allocates nothing.
    if count > input.len() as u64 {
        return Err(Error::Decode("entrances are truncated"));
    }
    let side = |input: &mut &[u8]| {
        i32::try_from(get_i64(input)?)
            .map_err(|_| Error::Decode("an entrance's coordinate is out of range"))
    };
    (0..count)
        .map(|_| {
            Ok(Entrance {
                at: [side(input)?, side(input)?],
                kind: get_string(input)?,
            })
        })
        .collect()
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
        .ok_or(Error::Decode("unexpected end of encoded data"))?;
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
    Err(Error::Decode("varint is longer than 64 bits"))
}

pub(crate) fn get_u32(input: &mut &[u8]) -> Result<u32> {
    u32::try_from(get_u64(input)?).map_err(|_| Error::Decode("varint does not fit in u32"))
}

pub(crate) fn get_i64(input: &mut &[u8]) -> Result<i64> {
    Ok(unzigzag(get_u64(input)?))
}

pub(crate) fn get_bytes<'a>(input: &mut &'a [u8]) -> Result<&'a [u8]> {
    let len = usize::try_from(get_u64(input)?)
        .map_err(|_| Error::Decode("length does not fit in usize"))?;
    if input.len() < len {
        return Err(Error::Decode("unexpected end of encoded data"));
    }
    let (bytes, rest) = input.split_at(len);
    *input = rest;
    Ok(bytes)
}

pub(crate) fn get_str<'a>(input: &mut &'a [u8]) -> Result<&'a str> {
    std::str::from_utf8(get_bytes(input)?)
        .map_err(|_| Error::Decode("encoded text is not valid UTF-8"))
}

pub(crate) fn get_string(input: &mut &[u8]) -> Result<String> {
    get_str(input).map(str::to_string)
}

pub(crate) fn get_opt_string(input: &mut &[u8]) -> Result<Option<String>> {
    match get_u8(input)? {
        0 => Ok(None),
        1 => get_string(input).map(Some),
        _ => Err(Error::Decode("invalid option tag")),
    }
}

/// What the value before in a scratch block leaves for the next one to be
/// coded against. A block starts from the default, so blocks decode on
/// their own and in parallel.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Delta {
    int: i64,
    bytes: Vec<u8>,
}

/// An integer as its difference from the one before. Sorted ids and
/// Hilbert keys differ from their neighbours by little, so most take a
/// byte or two instead of up to ten. Wrapping, so a value of any size and
/// either order round-trips.
pub(crate) fn put_delta(out: &mut Vec<u8>, delta: &mut Delta, value: i64) {
    put_i64(out, value.wrapping_sub(delta.int));
    delta.int = value;
}

pub(crate) fn get_delta(input: &mut &[u8], delta: &mut Delta) -> Result<i64> {
    let value = delta.int.wrapping_add(get_i64(input)?);
    delta.int = value;
    Ok(value)
}

/// Bytes front coded against the ones before: the length of the prefix
/// both share, then the rest. Sort keys that start with a cell, a street or
/// a name share most of their bytes with their neighbours in a sorted run.
pub(crate) fn put_front(out: &mut Vec<u8>, delta: &mut Delta, bytes: &[u8]) {
    let shared = delta
        .bytes
        .iter()
        .zip(bytes)
        .take_while(|(left, right)| left == right)
        .count();
    put_u64(out, shared as u64);
    put_bytes(out, &bytes[shared..]);
    delta.bytes.truncate(shared);
    delta.bytes.extend_from_slice(&bytes[shared..]);
}

/// The bytes [`put_front`] wrote, borrowed from `delta` until the next call.
pub(crate) fn get_front<'a>(input: &mut &[u8], delta: &'a mut Delta) -> Result<&'a [u8]> {
    let shared = usize::try_from(get_u64(input)?)
        .map_err(|_| Error::Decode("length does not fit in usize"))?;
    if shared > delta.bytes.len() {
        return Err(Error::Decode(
            "front coded prefix is longer than the key before",
        ));
    }
    let rest = get_bytes(input)?;
    delta.bytes.truncate(shared);
    delta.bytes.extend_from_slice(rest);
    Ok(&delta.bytes)
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
    fn an_extent_round_trips_and_a_bad_one_is_refused() {
        let crossing = [1_770_000_000, -190_000_000, -1_785_000_000, i32::MAX];
        let mut out = Vec::new();
        put_opt_extent(&mut out, None);
        put_opt_extent(&mut out, Some(crossing));
        let mut input = out.as_slice();
        assert_eq!(get_opt_extent(&mut input).unwrap(), None);
        assert_eq!(get_opt_extent(&mut input).unwrap(), Some(crossing));
        assert!(input.is_empty());
        assert!(matches!(
            get_opt_extent(&mut &out[1..out.len() - 1]),
            Err(Error::Decode("unexpected end of encoded data"))
        ));
        assert!(matches!(
            get_opt_extent(&mut [2u8].as_slice()),
            Err(Error::Decode("an extent's presence byte is not 0 or 1"))
        ));
        let mut wide = vec![1];
        put_i64(&mut wide, i64::from(i32::MAX) + 1);
        assert!(matches!(
            get_opt_extent(&mut wide.as_slice()),
            Err(Error::Decode("an extent's side is out of range"))
        ));
    }

    #[test]
    fn entrances_round_trip_and_bad_ones_are_refused() {
        let entrances = vec![
            Entrance {
                at: [74_263_000, 437_396_000],
                kind: "main".to_string(),
            },
            Entrance {
                at: [-1_799_999_999, -899_999_999],
                kind: "staircase".to_string(),
            },
        ];
        let mut out = Vec::new();
        put_entrances(&mut out, &[]);
        put_entrances(&mut out, &entrances);
        let mut input = out.as_slice();
        assert_eq!(get_entrances(&mut input).unwrap(), Vec::new());
        assert_eq!(get_entrances(&mut input).unwrap(), entrances);
        assert!(input.is_empty());
        assert!(matches!(
            get_entrances(&mut &out[1..out.len() - 1]),
            Err(Error::Decode("unexpected end of encoded data"))
        ));
        assert!(matches!(
            get_entrances(&mut [200u8, 1].as_slice()),
            Err(Error::Decode("entrances are truncated"))
        ));
        let mut wide = vec![1];
        put_i64(&mut wide, i64::from(i32::MIN) - 1);
        wide.extend_from_slice(&[0, 0, 0]);
        assert!(matches!(
            get_entrances(&mut wide.as_slice()),
            Err(Error::Decode("an entrance's coordinate is out of range"))
        ));
    }

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
    fn delta_and_front_coding_round_trip_in_any_order() {
        let values = [5, 7, 7, 1_000_000, -3, i64::MAX, i64::MIN, 0];
        let keys: [&[u8]; 6] = [b"king st", b"king street", b"king", b"", b"queen", b"queen"];
        let mut out = Vec::new();
        let mut delta = Delta::default();
        for (value, key) in values.iter().zip(keys.iter().cycle()) {
            put_delta(&mut out, &mut delta, *value);
            put_front(&mut out, &mut delta, key);
        }
        let mut input = out.as_slice();
        let mut delta = Delta::default();
        for (value, key) in values.iter().zip(keys.iter().cycle()) {
            assert_eq!(get_delta(&mut input, &mut delta).unwrap(), *value);
            assert_eq!(get_front(&mut input, &mut delta).unwrap(), *key);
        }
        assert!(input.is_empty());
    }

    #[test]
    fn rejects_a_front_prefix_longer_than_the_key_before() {
        let mut out = Vec::new();
        put_u64(&mut out, 4);
        put_bytes(&mut out, b"x");
        assert!(get_front(&mut out.as_slice(), &mut Delta::default()).is_err());
    }

    #[test]
    fn rejects_truncated_strings() {
        let mut out = Vec::new();
        put_str(&mut out, "abcdef");
        out.truncate(3);
        assert!(get_str(&mut out.as_slice()).is_err());
    }
}
