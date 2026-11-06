//! The source id index: the Pack section that finds the records of one
//! source object, such as `osm:node:1383955044`, without a scan.
//!
//! The section is one byte, the width of an entry, then the record ids that
//! have a source object id, sorted by that id (see
//! [`records::Reader::source_key`]) and then by record id. An entry is as
//! many little-endian bytes as the Pack's last record id needs: 3 for a
//! city, 4 up to 4 billion records. It holds no key: a lookup binary
//! searches it and reads each probed record's own key from the record store,
//! so the id is stored once, in the records. A lookup is about 30 probes on
//! a planet Pack, each one a record header.
//!
//! Built from the sealed Pack (see [`add_to`]), sorted on disk so a planet's
//! keys need not fit in memory.

use std::{
    fs,
    io::{BufWriter, Write},
    sync::Arc,
};

use rayon::prelude::*;

use crate::{
    container::{Bytes, Container},
    error::{Error, Result},
    extsort::{ExternalSorter, Scratch},
    memory::Budget,
    pack::{RecordId, SealedPack},
    record::ObjectType,
    records::{self, SourceKey},
};

const SECTION: &str = "source_ids";
const VERSION: u32 = 1;
const TEMP_FILE: &str = "source-ids.tmp";
/// Most the sort holds in memory before it spills; the build's memory pool
/// caps it lower where it is smaller.
const SORT_BYTES: usize = 512 << 20;
/// Records keyed together on one thread.
const CHUNK_RECORDS: u64 = 1 << 15;
/// Records keyed between two pushes to the sorter, which takes them one
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

/// Bytes an entry needs for any record id below `record_count`.
fn entry_width(record_count: u64) -> usize {
    let bits = u64::BITS - record_count.saturating_sub(1).leading_zeros();
    (bits as usize).div_ceil(8).max(1)
}

/// The index of a Pack, as the section holds it.
#[derive(Debug, Clone)]
pub(crate) struct Index {
    bytes: Bytes,
    width: usize,
}

impl Index {
    pub(crate) fn open(container: &Container, record_count: u64) -> Result<Self> {
        let bytes = container.section(SECTION, VERSION)?;
        let fits = |width: usize| {
            width == entry_width(record_count)
                && (bytes.len() - 1).is_multiple_of(width)
                && ((bytes.len() - 1) / width) as u64 <= record_count
        };
        match bytes.first().map(|&width| usize::from(width)) {
            Some(width) if fits(width) => Ok(Self { bytes, width }),
            _ => Err(Error::Inconsistent(
                "source id index does not fit the records".into(),
            )),
        }
    }

    fn len(&self) -> usize {
        (self.bytes.len() - 1) / self.width
    }

    fn record(&self, at: usize) -> Result<RecordId> {
        let start = at
            .checked_mul(self.width)
            .and_then(|offset| offset.checked_add(1));
        let end = start.and_then(|start| start.checked_add(self.width));
        let bytes = start
            .zip(end)
            .and_then(|(start, end)| self.bytes.get(start..end))
            .ok_or(Error::Decode("source id index is truncated"))?;
        let mut entry = [0; 8];
        entry[..self.width].copy_from_slice(bytes);
        Ok(u64::from_le_bytes(entry))
    }

    /// Every record of the source object, lowest record id first.
    pub(crate) fn find(&self, records: &records::Reader, id: &SourceId) -> Result<Vec<RecordId>> {
        let Some(key) = records.source_key_of(&id.dataset, id.object_type, id.object_id) else {
            return Ok(Vec::new());
        };
        let (mut low, mut high) = (0, self.len());
        while low < high {
            let middle = low + (high - low) / 2;
            if self.key(records, middle)? < key {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        let mut found = Vec::new();
        for at in low..self.len() {
            let record = self.record(at)?;
            if records.source_key(record)? != Some(key) {
                break;
            }
            found.push(record);
        }
        Ok(found)
    }

    fn key(&self, records: &records::Reader, at: usize) -> Result<SourceKey> {
        records.source_key(self.record(at)?)?.ok_or_else(|| {
            Error::Inconsistent("source id index names a record with no source id".into())
        })
    }
}

/// Build the index of the sealed Pack `sealed` and add it to the Pack.
pub(crate) fn add_to(
    sealed: &mut SealedPack,
    scratch: &Arc<Scratch>,
    memory: &Arc<Budget>,
) -> Result<()> {
    // Not `pack::Reader`: opening one needs the section this adds.
    let records = records::Reader::open(&Container::open(sealed.path())?)?;
    let count = records.record_count();
    let mut sorter =
        ExternalSorter::<(u64, u64, u64)>::new(scratch, "source-ids", memory, SORT_BYTES);
    for slab in (0..count).step_by(SLAB_RECORDS as usize) {
        scratch.check_cancelled()?;
        let end = (slab + SLAB_RECORDS).min(count);
        let chunks = (slab..end)
            .step_by(CHUNK_RECORDS as usize)
            .collect::<Vec<_>>()
            .into_par_iter()
            .map(|first| keys(&records, first, (first + CHUNK_RECORDS).min(end)))
            .collect::<Result<Vec<_>>>()?;
        for entry in chunks.into_iter().flatten() {
            sorter.push(entry)?;
        }
    }
    let path = sealed.generation().join(TEMP_FILE);
    let mut out = BufWriter::new(fs::File::create(&path)?);
    let width = entry_width(count);
    out.write_all(&[width as u8])?;
    for entry in sorter.finish()? {
        let (_, _, record) = entry?;
        out.write_all(&record.to_le_bytes()[..width])?;
    }
    out.flush()?;
    drop(out);
    sealed.add_section_files(&[(SECTION, VERSION, path.clone())])?;
    fs::remove_file(path)?;
    Ok(())
}

/// The sort entries of records `first..end`: key, then record id.
fn keys(records: &records::Reader, first: u64, end: u64) -> Result<Vec<(u64, u64, u64)>> {
    let mut entries = Vec::new();
    for record in first..end {
        if let Some((kind, object)) = records.source_key(record)? {
            entries.push((kind, object, record));
        }
    }
    Ok(entries)
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
