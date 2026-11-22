use crate::{
    container::{Bytes, Container},
    error::{Error, Result},
    pack::RecordId,
};

pub(crate) const SECTION: &str = "tiger/address_ranges";
pub(crate) const VERSION: u32 = 1;
pub(crate) const DATASET: &str = "tiger:2026:addrfeat";
const INDEX_WIDTH: usize = 12;
const RANGE_WIDTH: usize = 22;
const FRACTION_SCALE: f32 = 16_777_215.0;
const ZIP_MASK: u32 = (1 << 17) - 1;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Range {
    pub street: RecordId,
    pub from: u32,
    pub to: u32,
    pub start: f32,
    pub end: f32,
    pub zip: Option<u32>,
    pub line: u32,
    pub step: u8,
    pub side: u8,
}

impl Range {
    pub(crate) fn fraction(self, number: u32) -> Option<f64> {
        let (low, high) = self.bounds();
        if !(low..=high).contains(&number)
            || !number
                .abs_diff(self.from)
                .is_multiple_of(u32::from(self.step))
        {
            return None;
        }
        let along = if self.from == self.to {
            0.5
        } else {
            f64::from(number.abs_diff(self.from)) / f64::from(self.from.abs_diff(self.to))
        };
        Some(f64::from(self.start) + along * f64::from(self.end - self.start))
    }

    pub(crate) fn number_at(self, fraction: f64) -> Option<u32> {
        let (start, end) = (f64::from(self.start), f64::from(self.end));
        if !(start.min(end)..=start.max(end)).contains(&fraction) || start == end {
            return None;
        }
        let step = u32::from(self.step);
        let span = self.from.abs_diff(self.to);
        let steps = (span as f64 * (fraction - start) / (end - start) / f64::from(step)).round();
        let offset = (steps as u32 * step).min(span);
        Some(if self.from <= self.to {
            self.from + offset
        } else {
            self.from - offset
        })
    }

    pub(crate) fn side_key(self, side: u8) -> u64 {
        u64::from(self.line) * 2 + u64::from(side == b'R')
    }

    pub(crate) fn id(self, number: u32) -> String {
        format!(
            "derived:tiger:{}:{}:{number}",
            self.line,
            char::from(self.side)
        )
    }

    pub(crate) fn parity(self) -> char {
        match (self.step, self.from % 2) {
            (2, 0) => 'E',
            (2, _) => 'O',
            _ => 'B',
        }
    }

    pub(crate) fn bounds(self) -> (u32, u32) {
        (self.from.min(self.to), self.from.max(self.to))
    }

    pub(crate) fn postcode(self) -> Option<String> {
        self.zip.map(|zip| format!("{zip:05}"))
    }

    fn write(self, bytes: &mut Vec<u8>) {
        bytes.extend(self.from.to_le_bytes());
        bytes.extend(self.to.to_le_bytes());
        for fraction in [self.start, self.end] {
            let packed = (fraction * FRACTION_SCALE).round() as u32;
            bytes.extend(&packed.to_le_bytes()[..3]);
        }
        let flags = u32::from(self.step == 2) << 17 | u32::from(self.side == b'R') << 18;
        bytes.extend((self.zip.unwrap_or(ZIP_MASK) | flags).to_le_bytes());
        bytes.extend(self.line.to_le_bytes());
    }

    fn read(street: RecordId, bytes: &[u8]) -> Self {
        let int = |at| u32::from_le_bytes(bytes[at..at + 4].try_into().expect("u32"));
        let fraction = |at| (int(at) & 0xff_ffff) as f32 / FRACTION_SCALE;
        let code = int(14);
        let zip = code & ZIP_MASK;
        Self {
            street,
            from: int(0),
            to: int(4),
            start: fraction(8),
            end: fraction(11),
            zip: (zip != ZIP_MASK).then_some(zip),
            line: int(18),
            step: if code & (1 << 17) != 0 { 2 } else { 1 },
            side: if code & (1 << 18) != 0 { b'R' } else { b'L' },
        }
    }
}

// Separate streets from ranges so a road's id is written once. Three bytes
// give sub-metre precision even along a street longer than a state.
pub(crate) fn encode(mut ranges: Vec<Range>) -> Result<Vec<u8>> {
    if ranges.is_empty() {
        return Ok(Vec::new());
    }
    ranges.sort_unstable_by_key(|range| {
        (
            range.street,
            range.line,
            range.side,
            range.from,
            range.to,
            range.zip,
            range.start.to_bits(),
            range.end.to_bits(),
        )
    });
    ranges.dedup();
    let mut bytes = vec![0; 8];
    let mut last = None;
    for (offset, range) in ranges.iter().enumerate() {
        if last != Some(range.street) {
            last = Some(range.street);
            let offset = u32::try_from(offset)
                .map_err(|_| Error::Inconsistent("too many TIGER address ranges".into()))?;
            bytes.extend(range.street.to_le_bytes());
            bytes.extend(offset.to_le_bytes());
        }
    }
    let index_end = bytes.len() as u64;
    bytes[..8].copy_from_slice(&index_end.to_le_bytes());
    bytes.reserve(ranges.len() * RANGE_WIDTH);
    for range in ranges {
        range.write(&mut bytes);
    }
    Ok(bytes)
}

#[derive(Default)]
pub(crate) struct Ranges {
    index: Option<Bytes>,
    rows: Option<Bytes>,
}

impl Ranges {
    pub(crate) fn is_empty(&self) -> bool {
        self.rows.is_none()
    }

    pub(crate) fn open(container: &Container) -> Result<Self> {
        if !container.sections().contains_key(SECTION) {
            return Ok(Self::default());
        }
        let bytes = container.section(SECTION, VERSION)?;
        let malformed = || Error::Decode("TIGER address ranges are truncated");
        let end = crate::util::codec::read_u64_le(&bytes, 0)
            .and_then(|end| usize::try_from(end).ok())
            .filter(|&end| end >= 8 && end <= bytes.len() && (end - 8).is_multiple_of(INDEX_WIDTH))
            .filter(|&end| (bytes.len() - end).is_multiple_of(RANGE_WIDTH))
            .ok_or_else(malformed)?;
        Ok(Self {
            index: Some(bytes.slice(8..end)),
            rows: Some(bytes.slice(end..bytes.len())),
        })
    }

    pub(crate) fn for_street(&self, street: RecordId) -> impl Iterator<Item = Range> + '_ {
        self.street_bytes(street)
            .chunks_exact(RANGE_WIDTH)
            .map(move |bytes| Range::read(street, bytes))
    }

    fn street_bytes(&self, street: RecordId) -> &[u8] {
        let index = self.index.as_deref().unwrap_or_default();
        let rows = self.rows.as_deref().unwrap_or_default();
        let at = |row| &index[row * INDEX_WIDTH..][..INDEX_WIDTH];
        let id = |row| u64::from_le_bytes(at(row)[..8].try_into().expect("TIGER street id"));
        let offset = |row| {
            u32::from_le_bytes(at(row)[8..].try_into().expect("TIGER range offset")) as usize
                * RANGE_WIDTH
        };
        let count = index.len() / INDEX_WIDTH;
        let mut low = 0;
        let mut high = count;
        while low < high {
            let mid = low + (high - low) / 2;
            match id(mid).cmp(&street) {
                std::cmp::Ordering::Less => low = mid + 1,
                std::cmp::Ordering::Greater => high = mid,
                std::cmp::Ordering::Equal => {
                    let end = if mid + 1 < count {
                        offset(mid + 1)
                    } else {
                        rows.len()
                    };
                    return rows.get(offset(mid)..end).unwrap_or_default();
                }
            }
        }
        &[]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_descending_odd_range_keeps_its_side_zip_and_line_direction() {
        let range = Range {
            street: 18,
            from: 151,
            to: 101,
            start: 0.85,
            end: 0.25,
            zip: Some(2809),
            line: 2481480,
            step: 2,
            side: b'R',
        };
        let mut bytes = Vec::new();
        range.write(&mut bytes);
        let decoded = Range::read(range.street, &bytes);
        assert_eq!(decoded.postcode().as_deref(), Some("02809"));
        assert_eq!(decoded.side, b'R');
        assert_eq!(decoded.line, 2481480);
        assert_eq!(decoded.fraction(130), None);
        assert_eq!(decoded.fraction(153), None);
        assert!((decoded.fraction(125).unwrap() - 0.538).abs() < 0.000001);
    }
}

pub(crate) const BLOCKS_SECTION: &str = "tiger/blocks2020";
pub(crate) const BLOCKS_DATASET: &str = "tiger:2026:tabblock20";
pub(crate) const HISTORICAL_SECTION: &str = "tiger/blocks2010";
pub(crate) const SIDE_BLOCKS_SECTION: &str = "tiger/side_blocks";
pub(crate) const SIDE_ZIPS_SECTION: &str = "tiger/side_zips";
pub(crate) const ZCTA_SECTION: &str = "tiger/zcta";
const BLOCK_STRIDE: usize = 64;
const BLOCK_INDEX_WIDTH: usize = 16;

pub(crate) fn encode_unambiguous(mut entries: Vec<(RecordId, u64)>) -> Vec<u8> {
    entries.sort_unstable();
    entries.dedup();
    let agreed = entries
        .chunk_by(|left, right| left.0 == right.0)
        .filter_map(|chunk| {
            if let [entry] = chunk {
                Some(*entry)
            } else {
                None
            }
        })
        .collect();
    encode_blocks(agreed)
}

pub(crate) fn encode_blocks(mut entries: Vec<(RecordId, u64)>) -> Vec<u8> {
    if entries.is_empty() {
        return Vec::new();
    }
    entries.sort_unstable();
    entries.dedup_by_key(|entry| entry.0);
    let mut index = Vec::new();
    let mut body = Vec::new();
    for chunk in entries.chunks(BLOCK_STRIDE) {
        let first = chunk[0].0;
        index.extend(first.to_le_bytes());
        index.extend((body.len() as u64).to_le_bytes());
        let (mut previous_id, mut previous_code) = (first, 0);
        for &(id, code) in chunk {
            crate::util::codec::put_u64(&mut body, id - previous_id);
            crate::util::codec::put_i64(&mut body, code as i64 - previous_code);
            previous_id = id;
            previous_code = code as i64;
        }
    }
    let mut bytes = Vec::with_capacity(8 + index.len() + body.len());
    bytes.extend(((8 + index.len()) as u64).to_le_bytes());
    bytes.extend(index);
    bytes.extend(body);
    bytes
}

#[derive(Default)]
pub(crate) struct Blocks {
    index: Option<Bytes>,
    body: Option<Bytes>,
}

impl Blocks {
    pub(crate) fn open(container: &Container, section: &str) -> Result<Self> {
        if !container.sections().contains_key(section) {
            return Ok(Self::default());
        }
        let bytes = container.section(section, VERSION)?;
        let end = crate::util::codec::read_u64_le(&bytes, 0)
            .and_then(|end| usize::try_from(end).ok())
            .filter(|&end| {
                end >= 8 && end <= bytes.len() && (end - 8).is_multiple_of(BLOCK_INDEX_WIDTH)
            })
            .ok_or(Error::Decode("TIGER block codes are truncated"))?;
        Ok(Self {
            index: Some(bytes.slice(8..end)),
            body: Some(bytes.slice(end..bytes.len())),
        })
    }

    pub(crate) fn code(&self, record_id: RecordId) -> Option<String> {
        self.value(record_id).map(|code| format!("{code:015}"))
    }

    pub(crate) fn postcode(&self, record_id: RecordId) -> Option<String> {
        self.value(record_id).map(|code| format!("{code:05}"))
    }

    pub(crate) fn value(&self, record_id: RecordId) -> Option<i64> {
        use crate::util::codec::{get_i64, get_u64, read_u64_le};
        let index = self.index.as_deref()?;
        let body = self.body.as_deref()?;
        let mut low = 0;
        let mut high = index.len() / BLOCK_INDEX_WIDTH;
        while low < high {
            let mid = low + (high - low) / 2;
            if read_u64_le(index, mid * BLOCK_INDEX_WIDTH)? <= record_id {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        let row = low.checked_sub(1)? * BLOCK_INDEX_WIDTH;
        let mut id = read_u64_le(index, row)?;
        let start = usize::try_from(read_u64_le(index, row + 8)?).ok()?;
        let end = read_u64_le(index, row + BLOCK_INDEX_WIDTH + 8)
            .and_then(|end| usize::try_from(end).ok())
            .unwrap_or(body.len());
        let mut bytes = body.get(start..end)?;
        let mut code = 0i64;
        while !bytes.is_empty() {
            id = id.checked_add(get_u64(&mut bytes).ok()?)?;
            code = code.checked_add(get_i64(&mut bytes).ok()?)?;
            if id == record_id {
                return Some(code);
            }
            if id > record_id {
                break;
            }
        }
        None
    }
}
