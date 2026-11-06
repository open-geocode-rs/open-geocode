//! The source id index: the Pack sections that find the records of one
//! source object, such as `osm:node:1383955044`, without a scan.
//!
//! `source_ids` is one byte, the width of an entry, then the record ids that
//! have a source object id, sorted by that id (see
//! [`records::Reader::source_key`]) and then by record id. An entry is as
//! many little-endian bytes as the Pack's last record id needs: 3 for a
//! city, 4 up to 4 billion records. It holds no key: a lookup binary
//! searches it and reads each probed record's own key from the record store,
//! so the id is stored once, in the records. A lookup is about 30 probes on
//! a planet Pack, each one a record header.
//!
//! `source_ids/ways` finds a street joined from several OSM ways by any of
//! them. The street record names only its lowest way, and its other ways
//! live in the record (see [`crate::record::StreetRecord::member_ways`]),
//! where a probe cannot take a key for each of them. So this table keeps its
//! keys: the same width byte, then entries of an 8-byte way id (zigzag, the
//! record store's own) and a record id, sorted the same way.
//!
//! Built from the sealed Pack (see [`add_to`]), sorted on disk so a planet's
//! keys need not fit in memory.

use std::{
    fs,
    io::{BufWriter, Write},
    path::Path,
    sync::Arc,
};

use rayon::prelude::*;

use crate::{
    container::{Bytes, Container},
    error::{Error, Result},
    extsort::{ExternalSorter, Scratch},
    memory::Budget,
    pack::{RecordId, SealedPack},
    record::{OSM_DATASET, ObjectType},
    records,
    util::codec::zigzag,
};

const SECTION: &str = "source_ids";
const WAYS_SECTION: &str = "source_ids/ways";
const VERSION: u32 = 1;
const TEMP_FILE: &str = "source-ids.tmp";
const WAYS_TEMP_FILE: &str = "source-id-ways.tmp";
/// Bytes of a way id in `source_ids/ways`.
const WAY_BYTES: usize = 8;
/// Most each sort holds in memory before it spills; the build's memory pool
/// caps it lower where it is smaller.
const SORT_BYTES: usize = 512 << 20;
/// Records keyed together on one thread.
const CHUNK_RECORDS: u64 = 1 << 15;
/// Records keyed between two pushes to the sorters, which take them one
/// thread at a time.
const SLAB_RECORDS: u64 = 1 << 21;

/// A source object's id as a result shows it: `osm:node:1`, or for a row of an
/// imported dataset `openaddresses:nz/countrywide:row:7`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceId {
    dataset: String,
    object_type: ObjectType,
    object_id: i64,
}

impl SourceId {
    /// `None` when `id` is not `dataset:type:number` with a type of `node`,
    /// `way`, `relation` or `row`. The ids of an interpolation range or a
    /// derived record (`derived:osm:postcode:nz:2110`) are no source
    /// object's, so they are not source ids either.
    pub fn parse(id: &str) -> Option<Self> {
        // Reserved for a list of ids, which no dataset name may hold.
        if id.contains(',') {
            return None;
        }
        let mut parts = id.rsplitn(3, ':');
        let object_id = parts.next()?.parse().ok()?;
        let object_type = match parts.next()? {
            "node" => ObjectType::Node,
            "way" => ObjectType::Way,
            "relation" => ObjectType::Relation,
            "row" => ObjectType::Row,
            _ => return None,
        };
        let dataset = parts.next().filter(|dataset| !dataset.is_empty())?;
        Some(Self {
            dataset: dataset.to_string(),
            object_type,
            object_id,
        })
    }
}

/// The id as results write it.
impl std::fmt::Display for SourceId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&crate::labels::record_id(
            &self.dataset,
            self.object_type,
            self.object_id,
        ))
    }
}

/// Bytes an entry needs for any record id below `record_count`.
fn entry_width(record_count: u64) -> usize {
    let bits = u64::BITS - record_count.saturating_sub(1).leading_zeros();
    (bits as usize).div_ceil(8).max(1)
}

/// A section of fixed-size entries behind a width byte.
#[derive(Debug, Clone)]
struct Table {
    bytes: Bytes,
    /// Bytes of a record id.
    width: usize,
    /// Bytes of an entry.
    stride: usize,
}

impl Table {
    /// The section `name`, whose entries are `key_bytes` of key, then a record
    /// id.
    fn open(
        container: &Container,
        name: &str,
        key_bytes: usize,
        record_count: u64,
    ) -> Result<Self> {
        let bytes = container.section(name, VERSION)?;
        let width = entry_width(record_count);
        let stride = key_bytes + width;
        let body = bytes.len().saturating_sub(1);
        if bytes.first() != Some(&(width as u8))
            || !body.is_multiple_of(stride)
            || (body / stride) as u64 > record_count
        {
            return Err(Error::Inconsistent(format!(
                "{name} does not fit the records"
            )));
        }
        Ok(Self {
            bytes,
            width,
            stride,
        })
    }

    fn len(&self) -> usize {
        (self.bytes.len() - 1) / self.stride
    }

    /// The `width` bytes at `offset` of entry `at`, little-endian.
    fn field(&self, at: usize, offset: usize, width: usize) -> Result<u64> {
        let start = at
            .checked_mul(self.stride)
            .and_then(|start| start.checked_add(1 + offset));
        let end = start.and_then(|start| start.checked_add(width));
        let bytes = start
            .zip(end)
            .and_then(|(start, end)| self.bytes.get(start..end))
            .ok_or(Error::Decode("source id index is truncated"))?;
        let mut field = [0; 8];
        field[..width].copy_from_slice(bytes);
        Ok(u64::from_le_bytes(field))
    }

    /// The first entry for which `before` is false, as the entries are
    /// sorted by what it tests.
    fn partition_point(&self, mut before: impl FnMut(usize) -> Result<bool>) -> Result<usize> {
        let (mut low, mut high) = (0, self.len());
        while low < high {
            let middle = low + (high - low) / 2;
            if before(middle)? {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        Ok(low)
    }
}

/// The index of a Pack, as its sections hold it.
#[derive(Debug, Clone)]
pub(crate) struct Index {
    ids: Table,
    ways: Table,
}

impl Index {
    pub(crate) fn open(container: &Container, record_count: u64) -> Result<Self> {
        Ok(Self {
            ids: Table::open(container, SECTION, 0, record_count)?,
            ways: Table::open(container, WAYS_SECTION, WAY_BYTES, record_count)?,
        })
    }

    /// Every record of the source object, lowest record id first: the
    /// records that show its id, and the street an OSM way is one of the ways
    /// of.
    pub(crate) fn find(&self, records: &records::Reader, id: &SourceId) -> Result<Vec<RecordId>> {
        let mut found = self.find_ids(records, id)?;
        if id.dataset == OSM_DATASET && id.object_type == ObjectType::Way {
            found.extend(self.find_ways(zigzag(id.object_id))?);
            found.sort_unstable();
            found.dedup();
        }
        Ok(found)
    }

    fn find_ids(&self, records: &records::Reader, id: &SourceId) -> Result<Vec<RecordId>> {
        let Some(key) = records.source_key_of(&id.dataset, id.object_type, id.object_id) else {
            return Ok(Vec::new());
        };
        let record = |at| self.ids.field(at, 0, self.ids.width);
        let low = self.ids.partition_point(|at| {
            Ok(records.source_key(record(at)?)?.ok_or_else(|| {
                Error::Inconsistent("source id index names a record with no source id".into())
            })? < key)
        })?;
        let mut found = Vec::new();
        for at in low..self.ids.len() {
            let record = record(at)?;
            if records.source_key(record)? != Some(key) {
                break;
            }
            found.push(record);
        }
        Ok(found)
    }

    fn find_ways(&self, way: u64) -> Result<Vec<RecordId>> {
        let key = |at| self.ways.field(at, 0, WAY_BYTES);
        let low = self.ways.partition_point(|at| Ok(key(at)? < way))?;
        let mut found = Vec::new();
        for at in low..self.ways.len() {
            if key(at)? != way {
                break;
            }
            found.push(self.ways.field(at, WAY_BYTES, self.ways.width)?);
        }
        Ok(found)
    }
}

/// Build the index of the sealed Pack `sealed` and add it to the Pack.
pub(crate) fn add_to(
    sealed: &mut SealedPack,
    scratch: &Arc<Scratch>,
    memory: &Arc<Budget>,
) -> Result<()> {
    // Not `pack::Reader`: opening one needs the sections this adds.
    let records = records::Reader::open(&Container::open(sealed.path())?)?;
    let count = records.record_count();
    let mut ids = ExternalSorter::<(u64, u64, u64)>::new(scratch, "source-ids", memory, SORT_BYTES);
    let mut ways = ExternalSorter::<(u64, u64)>::new(scratch, "source-id-ways", memory, SORT_BYTES);
    for slab in (0..count).step_by(SLAB_RECORDS as usize) {
        scratch.check_cancelled()?;
        let end = (slab + SLAB_RECORDS).min(count);
        let chunks = (slab..end)
            .step_by(CHUNK_RECORDS as usize)
            .collect::<Vec<_>>()
            .into_par_iter()
            .map(|first| keys(&records, first, (first + CHUNK_RECORDS).min(end)))
            .collect::<Result<Vec<_>>>()?;
        for (keyed, member_ways) in chunks {
            for entry in keyed {
                ids.push(entry)?;
            }
            for entry in member_ways {
                ways.push(entry)?;
            }
        }
    }
    let width = entry_width(count);
    let ids_path = sealed.generation().join(TEMP_FILE);
    write_table(
        &ids_path,
        width,
        ids.finish()?.map(|entry| {
            let (_, _, record) = entry?;
            Ok(record.to_le_bytes()[..width].to_vec())
        }),
    )?;
    let ways_path = sealed.generation().join(WAYS_TEMP_FILE);
    write_table(
        &ways_path,
        width,
        ways.finish()?.map(|entry| {
            let (way, record) = entry?;
            let mut bytes = way.to_le_bytes().to_vec();
            bytes.extend_from_slice(&record.to_le_bytes()[..width]);
            Ok(bytes)
        }),
    )?;
    sealed.add_section_files(&[
        (SECTION, VERSION, ids_path.clone()),
        (WAYS_SECTION, VERSION, ways_path.clone()),
    ])?;
    fs::remove_file(ids_path)?;
    fs::remove_file(ways_path)?;
    Ok(())
}

fn write_table(
    path: &Path,
    width: usize,
    entries: impl Iterator<Item = Result<Vec<u8>>>,
) -> Result<()> {
    let mut out = BufWriter::new(fs::File::create(path)?);
    out.write_all(&[width as u8])?;
    for entry in entries {
        out.write_all(&entry?)?;
    }
    out.flush()?;
    Ok(())
}
/// Sort entries: (key, record id) with a source key, and (way, record id).
type Keyed = (Vec<(u64, u64, u64)>, Vec<(u64, u64)>);

/// What records `first..end` add to the sorts: for each record with a source
/// key, the key then the record id; for each way a street joins, the way
/// then the record id.
fn keys(records: &records::Reader, first: u64, end: u64) -> Result<Keyed> {
    let (mut keyed, mut ways) = (Vec::new(), Vec::new());
    for record in first..end {
        let (key, member_ways) = records.source_keys(record)?;
        if let Some((kind, object)) = key {
            keyed.push((kind, object, record));
        }
        ways.extend(member_ways.into_iter().map(|way| (zigzag(way), record)));
    }
    Ok((keyed, ways))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_entry_is_as_wide_as_the_last_record_id_needs() {
        for (count, width) in [
            (0, 1),
            (1, 1),
            (256, 1),
            (257, 2),
            (1 << 16, 2),
            ((1 << 16) + 1, 3),
            (1 << 24, 3),
            ((1 << 24) + 1, 4),
            (1 << 32, 4),
            ((1 << 32) + 1, 5),
            (u64::MAX, 8),
        ] {
            assert_eq!(entry_width(count), width, "{count} records");
        }
    }
}
