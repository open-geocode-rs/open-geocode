//! Enclosing areas: every area of the Pack a record lies in, with the OSM tags
//! that say what kind of area each is.
//!
//! A record's context keeps one area per tier, so a `place=city` contour and an
//! `admin_level=8` district that claim the locality tier show only the winner.
//! The lists belong to context tuples, which [`crate::context`] interns by
//! chain and list together.
//!
//! ```text
//! context/enclosing  tuple_count, then per context tuple, by tuple id:
//!                      len, then len x record_id; smallest area first, a
//!                      derived country last
//!                    area_count, then per area with a tag, by ascending
//!                    record_id: record_id, admin_level, boundary, place
//!                    every number a varint, every tag an optional string
//! ```

use std::collections::BTreeMap;

use serde::Serialize;
use utoipa::ToSchema;

use crate::{
    container::{Bytes, Container, Writer},
    error::{Error, Result},
    pack::RecordId,
    util::codec::{get_opt_string, get_u64, put_opt_str, put_u64},
};

pub(crate) const SECTION: &str = "context/enclosing";
const VERSION: u32 = 1;

/// The `admin_level`, `boundary` and `place` tags of an area, as OSM writes
/// them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, ToSchema)]
pub struct AreaTags {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub admin_level: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boundary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub place: Option<String>,
}

impl AreaTags {
    pub fn from_osm(tags: &BTreeMap<String, String>) -> Self {
        Self {
            admin_level: tags.get("admin_level").cloned(),
            boundary: tags.get("boundary").cloned(),
            place: tags.get("place").cloned(),
        }
    }

    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    pub(crate) fn put(&self, out: &mut Vec<u8>) {
        for tag in [&self.admin_level, &self.boundary, &self.place] {
            put_opt_str(out, tag.as_deref());
        }
    }

    pub(crate) fn get(input: &mut &[u8]) -> Result<Self> {
        Ok(Self {
            admin_level: get_opt_string(input)?,
            boundary: get_opt_string(input)?,
            place: get_opt_string(input)?,
        })
    }
}

pub(crate) fn write(
    pack: &mut Writer,
    lists: &[Vec<RecordId>],
    tags: &BTreeMap<RecordId, AreaTags>,
) -> Result<()> {
    let mut bytes = Vec::new();
    put_u64(&mut bytes, lists.len() as u64);
    for list in lists {
        put_u64(&mut bytes, list.len() as u64);
        list.iter().for_each(|area| put_u64(&mut bytes, *area));
    }
    let tagged = || tags.iter().filter(|(_, tags)| !tags.is_empty());
    put_u64(&mut bytes, tagged().count() as u64);
    for (area, tags) in tagged() {
        put_u64(&mut bytes, *area);
        tags.put(&mut bytes);
    }
    pack.add(SECTION, VERSION, &bytes)
}

#[derive(Debug, Clone)]
pub(crate) struct Reader {
    bytes: Bytes,
    /// Byte offset of each tuple's list; lists vary in length.
    lists: Vec<usize>,
    /// Record id and byte offset of the tags of each tagged area, by record id.
    tags: Vec<(RecordId, usize)>,
}

impl Reader {
    /// `None` for a Pack built before the section existed. Everything that
    /// can be wrong with it is refused here, so a lookup indexes nothing
    /// unchecked.
    pub(crate) fn open(container: &Container, tuples: u64, records: u64) -> Result<Option<Self>> {
        if !container.sections().contains_key(SECTION) {
            return Ok(None);
        }
        let bytes = container.section(SECTION, VERSION)?;
        let corrupt = |detail: String| Error::Inconsistent(format!("{SECTION} {detail}"));
        let mut input: &[u8] = &bytes;
        let offset = |input: &[u8]| bytes.len() - input.len();
        // Every entry takes a byte at least, so a count past the bytes left
        // is refused before anything is allocated.
        let counted = |input: &mut &[u8], what: &str| -> Result<usize> {
            let count = get_u64(input)?;
            usize::try_from(count)
                .ok()
                .filter(|count| *count <= input.len())
                .ok_or_else(|| {
                    corrupt(format!(
                        "says {count} {what}, more than its {} bytes can hold",
                        input.len()
                    ))
                })
        };

        let count = counted(&mut input, "lists")?;
        if count as u64 != tuples {
            return Err(corrupt(format!(
                "holds {count} lists, but the manifest says {tuples} context tuples"
            )));
        }
        let mut lists = Vec::with_capacity(count);
        for _ in 0..count {
            lists.push(offset(input));
            for _ in 0..counted(&mut input, "areas")? {
                let area = get_u64(&mut input)?;
                if area >= records {
                    return Err(corrupt(format!(
                        "lists record {area}, but the Pack has {records} records"
                    )));
                }
            }
        }
        let count = counted(&mut input, "tagged areas")?;
        let mut tags: Vec<(RecordId, usize)> = Vec::with_capacity(count);
        for _ in 0..count {
            let area = get_u64(&mut input)?;
            if area >= records || tags.last().is_some_and(|(last, _)| *last >= area) {
                return Err(corrupt(format!(
                    "tags record {area} out of order or past the Pack's {records} records"
                )));
            }
            tags.push((area, offset(input)));
            AreaTags::get(&mut input)?;
        }
        if !input.is_empty() {
            return Err(corrupt(format!(
                "has {} bytes after its last area",
                input.len()
            )));
        }
        Ok(Some(Self { bytes, lists, tags }))
    }

    pub(crate) fn areas(&self, tuple_id: u32) -> Result<Vec<RecordId>> {
        let Some(&at) = self.lists.get(tuple_id as usize) else {
            return Err(Error::IdOutOfRange {
                what: "context tuple",
                id: u64::from(tuple_id),
            });
        };
        let mut input = &self.bytes[at..];
        let len = get_u64(&mut input)?;
        (0..len).map(|_| get_u64(&mut input)).collect()
    }

    pub(crate) fn tags(&self, area: RecordId) -> Result<Option<AreaTags>> {
        let Ok(found) = self.tags.binary_search_by_key(&area, |(id, _)| *id) else {
            return Ok(None);
        };
        AreaTags::get(&mut &self.bytes[self.tags[found].1..]).map(Some)
    }
}

#[cfg(test)]
impl AreaTags {
    pub(crate) fn of(place: Option<&str>, boundary: Option<&str>, level: Option<&str>) -> Self {
        let text = |value: Option<&str>| value.map(str::to_string);
        Self {
            admin_level: text(level),
            boundary: text(boundary),
            place: text(place),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    /// A Pack file holding what `add` puts in it, opened as a Pack opens it.
    fn open(
        add: impl FnOnce(&mut Writer) -> Result<()>,
        tuples: u64,
        records: u64,
    ) -> Result<Option<Reader>> {
        let dir = TempDir::new("enclosing");
        let path = dir.join("pack");
        let mut pack = Writer::create(&path).expect("pack");
        add(&mut pack)?;
        pack.finish().expect("finish");
        Reader::open(&Container::open(&path).expect("open"), tuples, records)
    }

    #[test]
    fn lists_and_tags_round_trip_as_written() {
        let tags = BTreeMap::from([
            (
                1,
                AreaTags::of(Some("city"), Some("administrative"), Some("8")),
            ),
            (2, AreaTags::of(Some(" City "), Some(""), None)),
            (3, AreaTags::default()),
        ]);
        let reader = open(|pack| write(pack, &[vec![7, 2, 9], vec![]], &tags), 2, 10);
        let reader = reader.unwrap().expect("section");

        assert_eq!(reader.areas(0).unwrap(), [7, 2, 9]);
        assert!(reader.areas(1).unwrap().is_empty());
        let out_of_range = reader.areas(2).unwrap_err();
        assert_eq!(
            out_of_range.to_string(),
            "context tuple id 2 is out of range"
        );
        let found = [1, 2, 3].map(|area| reader.tags(area).unwrap());
        assert_eq!(found, [tags.get(&1).cloned(), tags.get(&2).cloned(), None]);
    }

    #[test]
    fn a_damaged_section_is_refused_with_what_is_wrong() {
        // Two lists, (1, 2) and (3), then area 3 with admin_level "8".
        let good = [2, 2, 1, 2, 1, 3, 1, 3, 1, 1, b'8', 0, 0];
        let padded = [&good[..], &[0]].concat();
        let cases: [(&[u8], [u64; 2], &str); 5] = [
            (
                &good,
                [3, 4],
                "holds 2 lists, but the manifest says 3 context tuples",
            ),
            (&good, [2, 3], "lists record 3, but the Pack has 3 records"),
            (
                &[1, 100, 1],
                [1, 4],
                "says 100 areas, more than its 1 bytes can hold",
            ),
            (
                &[0, 2, 3, 0, 0, 0, 2, 0, 0, 0],
                [0, 4],
                "tags record 2 out of order or past the Pack's 4 records",
            ),
            (&padded, [2, 4], "has 1 bytes after its last area"),
        ];
        let refusal = |bytes: &[u8], [tuples, records]: [u64; 2]| {
            open(|pack| pack.add(SECTION, VERSION, bytes), tuples, records)
                .expect_err("refused")
                .to_string()
        };
        for (bytes, sizes, what) in cases {
            let expected = format!("Pack is inconsistent: context/enclosing {what}");
            assert_eq!(refusal(bytes, sizes), expected, "{bytes:?}");
        }
        assert_eq!(
            refusal(&good[..12], [2, 4]),
            "Pack data is malformed: unexpected end of encoded data"
        );
        assert!(open(|_| Ok(()), 2, 4).unwrap().is_none());
    }
}
