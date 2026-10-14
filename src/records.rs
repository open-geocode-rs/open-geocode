//! Compact record store.
//!
//! Records are written in their final order (the builder sorts them along a
//! Hilbert curve) and packed into blocks of [`BLOCK_RECORDS`]. A block starts
//! with an absolute base coordinate; every record in it stores its display
//! point as a small zigzag delta from that base, then its fields as varints and
//! string-table ids, then any line geometry as a delta chain. Finding record
//! `N` means jumping to block `N / 64` through the block index, then to the
//! record through the block's table of body end offsets.
//!
//! ```text
//! records/index    record_count u64, then (block_count + 1) u64 block offsets
//! records/blocks   blocks: base_lon i32 | base_lat i32 | n x u32 body end | bodies
//! records/strings  segment_count u64 | per segment: first record u64, first string u64
//!                  | string_count u64 | string_count x u64 end offset | UTF-8 bytes
//!
//! body  tag u8        layer (4 bits) | centroid (1) | line geometry (1) | source kind (2)
//!       dlon, dlat    zigzag varints from the block base
//!       context       varint, 0 = none, else (tuple_id + 1) << 1 | ambiguous
//!       source        zigzag OSM object id, or the address count for postcodes;
//!                     an address with the derived kind is an imported row: its
//!                     value is the row number and its dataset name follows the
//!                     address fields
//!       fields        per layer, strings as varint ids local to the record's segment
//!       geometry      line strings only: varint count, then zigzag deltas
//! ```
//!
//! Strings are interned per segment: the build keeps one segment's dictionary
//! in memory, not every distinct string of the input. A segment ends after
//! [`SEGMENT_RECORDS`] records or once its dictionary uses its share of the
//! build budget, whichever comes first. Records are in Hilbert order, so a
//! segment covers one area and most of its strings are its own; only widely
//! shared ones (countries, regions) repeat across segments.

use std::{
    collections::HashMap,
    fs::File,
    io::{BufWriter, Write},
    path::PathBuf,
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use geojson::{Geometry, GeometryValue};
use serde::Serialize;
use serde_json::{Value, json};

use crate::{
    container::{Bytes, Container, ContainerWriter},
    extsort::Scratch,
    memory::{MemoryBudget, Reservation},
    pack::RecordId,
    record::{
        AddressComponents, AddressRecord, DERIVED_FROM_ADDRESS_RECORDS, DerivedSourceProvenance,
        InterpolationAddressComponents, InterpolationRange, InterpolationRecord, Layer,
        LocationPrecision, OsmObjectType, PlaceRecord, PostcodeRecord, Record, SourceProvenance,
        StreetRecord, point_geometry,
    },
    util::codec::{get_i64, get_u8, get_u32, get_u64, put_i64, put_u64, read_u32_le, read_u64_le},
};

pub const SECTION_INDEX: &str = "records/index";
pub const SECTION_BLOCKS: &str = "records/blocks";
pub const SECTION_STRINGS: &str = "records/strings";
pub const RECORDS_VERSION: u32 = 4;
/// Version of a store holding imported address rows. Older binaries only read
/// `RECORDS_VERSION`, so they refuse such a Pack when they open it instead of
/// failing on the first imported row a query reaches.
pub const RECORDS_VERSION_IMPORTED: u32 = 5;

pub const BLOCK_RECORDS: u64 = 64;
/// Approximate bytes a dictionary entry costs beyond its text: the boxed
/// key, the id and the hash table slot.
const DICTIONARY_ENTRY_OVERHEAD: usize = 48;
/// Records per string segment.
pub const SEGMENT_RECORDS: u64 = 1 << 20;
/// Share of the build budget the string dictionary may use.
const DICTIONARY_BUDGET_FRACTION: usize = 8;
const COORDINATE_SCALE: f64 = 10_000_000.0;
const BLOCK_HEADER_BYTES: usize = 8;

const TAG_LAYER_MASK: u8 = 0x0f;
const TAG_CENTROID: u8 = 0x10;
const TAG_LINE: u8 = 0x20;
const TAG_SOURCE_SHIFT: u8 = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceKind {
    Node = 0,
    Way = 1,
    Relation = 2,
    Derived = 3,
}

/// Admin context reference stored in a record body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextRef {
    pub tuple_id: u32,
    pub ambiguous: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RecordSummary {
    pub id: String,
    pub layer: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub point: Option<RecordPoint>,
    pub source: RecordSource,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
pub struct RecordPoint {
    pub lon: f64,
    pub lat: f64,
    pub precision: RecordPointPrecision,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecordPointPrecision {
    Point,
    Centroid,
    RepresentativePoint,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RecordSource {
    pub dataset: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub object_type: Option<OsmObjectType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub object_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub derived_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record_count: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContextRecord {
    pub id: String,
    pub layer: String,
    pub label: String,
    pub name: String,
    pub postcode: Option<String>,
    pub point: Option<RecordPoint>,
}

/// Global string ids `first..end` belong to one segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StringSegment {
    first: u64,
    end: u64,
}

/// The fixed part of a record: enough for spatial filtering without touching
/// strings or geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordHeader {
    pub layer: Layer,
    strings: StringSegment,
    pub lon_e7: i32,
    pub lat_e7: i32,
    pub context: Option<ContextRef>,
    centroid: bool,
    line: bool,
    source_kind: SourceKind,
    source_value: u64,
}

impl RecordHeader {
    pub fn lon(&self) -> f64 {
        dequantize(self.lon_e7)
    }

    pub fn lat(&self) -> f64 {
        dequantize(self.lat_e7)
    }

    pub fn has_line(&self) -> bool {
        self.line
    }

    fn point(&self) -> RecordPoint {
        let precision = if self.line {
            RecordPointPrecision::RepresentativePoint
        } else if self.layer == Layer::Address && !self.centroid {
            RecordPointPrecision::Point
        } else {
            RecordPointPrecision::Centroid
        };
        RecordPoint {
            lon: self.lon(),
            lat: self.lat(),
            precision,
        }
    }

    /// An address row imported from a file, not an OSM object. Derived is
    /// otherwise postcode-only, so older packs never hold this combination.
    fn imported(&self) -> bool {
        self.source_kind == SourceKind::Derived && self.layer == Layer::Address
    }

    fn osm_source(&self) -> Result<(OsmObjectType, i64)> {
        let object_type = match self.source_kind {
            SourceKind::Node => OsmObjectType::Node,
            SourceKind::Way => OsmObjectType::Way,
            SourceKind::Relation => OsmObjectType::Relation,
            SourceKind::Derived => bail!("derived record has no OSM source"),
        };
        Ok((object_type, crate::util::codec::unzigzag(self.source_value)))
    }

    fn source(&self) -> Result<RecordSource> {
        if self.imported() {
            // The dataset name follows the address fields; the caller fills it in.
            return Ok(RecordSource {
                dataset: String::new(),
                object_type: Some(OsmObjectType::Row),
                object_id: Some(crate::util::codec::unzigzag(self.source_value)),
                derived_from: None,
                record_count: None,
            });
        }
        if self.source_kind == SourceKind::Derived {
            return Ok(RecordSource {
                dataset: "osm".into(),
                object_type: None,
                object_id: None,
                derived_from: Some(DERIVED_FROM_ADDRESS_RECORDS.into()),
                record_count: Some(self.source_value),
            });
        }
        let (object_type, object_id) = self.osm_source()?;
        Ok(RecordSource {
            dataset: "osm".into(),
            object_type: Some(object_type),
            object_id: Some(object_id),
            derived_from: None,
            record_count: None,
        })
    }

    fn provenance(&self, dataset: Option<&str>) -> Result<SourceProvenance> {
        if let Some(dataset) = dataset {
            return Ok(SourceProvenance::row(
                dataset,
                crate::util::codec::unzigzag(self.source_value),
            ));
        }
        let (object_type, object_id) = self.osm_source()?;
        Ok(SourceProvenance::osm(object_type, object_id))
    }
}

pub struct RecordsWriter {
    blocks: BufWriter<File>,
    blocks_path: PathBuf,
    blocks_len: u64,
    /// Start of every block, streamed to scratch.
    block_offsets: BufWriter<File>,
    block_offsets_path: PathBuf,
    /// Bodies of the open block and where each one ends.
    block: Vec<u8>,
    block_ends: Vec<u32>,
    block_base: (i32, i32),
    record_count: u64,
    /// Whether any record is an imported address row.
    imported: bool,
    strings: StringTable,
    body: Vec<u8>,
}

/// Strings of the current segment. Bytes and end offsets stream to scratch
/// files; only the current segment's dictionary is held in memory.
struct StringTable {
    ids: HashMap<Box<str>, u32>,
    /// The dictionary's memory, held against the build budget. It cannot
    /// spill, but it is bounded by one segment and released at the next.
    memory: Reservation,
    /// Dictionary bytes after which the next record starts a new segment.
    memory_share: usize,
    segment_records: u64,
    /// First record and first string of every segment.
    segments: Vec<(u64, u64)>,
    count: u64,
    bytes_len: u64,
    bytes: BufWriter<File>,
    bytes_path: PathBuf,
    ends: BufWriter<File>,
    ends_path: PathBuf,
}

impl StringTable {
    /// Segment-local id of `value`.
    fn id(&mut self, value: &str) -> Result<u32> {
        if let Some(&id) = self.ids.get(value) {
            return Ok(id);
        }
        let id = u32::try_from(self.ids.len()).context("string segment is full")?;
        self.bytes.write_all(value.as_bytes())?;
        self.bytes_len += value.len() as u64;
        self.ends.write_all(&self.bytes_len.to_le_bytes())?;
        self.count += 1;
        self.ids.insert(value.into(), id);
        let bytes = value.len() + DICTIONARY_ENTRY_OVERHEAD;
        if !self.memory.try_grow(bytes) {
            self.memory.force_grow(bytes);
        }
        Ok(id)
    }

    fn start_segment(&mut self, first_record: u64) {
        self.ids = HashMap::new();
        self.memory.release_all();
        self.segments.push((first_record, self.count));
    }

    fn segment_full(&self, record_id: u64) -> bool {
        let Some((first_record, _)) = self.segments.last() else {
            return true;
        };
        record_id - first_record >= self.segment_records || self.memory.bytes() >= self.memory_share
    }
}

impl RecordsWriter {
    pub fn create(scratch: &Arc<Scratch>, budget: &Arc<MemoryBudget>) -> Result<Self> {
        let blocks_path = scratch.path().join("records-blocks.bin");
        let block_offsets_path = scratch.path().join("records-block-offsets.bin");
        let bytes_path = scratch.path().join("records-strings.bin");
        let ends_path = scratch.path().join("records-string-ends.bin");
        let create = |path: &PathBuf| -> Result<BufWriter<File>> {
            Ok(BufWriter::with_capacity(
                1 << 20,
                File::create(path)
                    .with_context(|| format!("failed to create {}", path.display()))?,
            ))
        };
        Ok(Self {
            blocks: create(&blocks_path)?,
            blocks_path,
            blocks_len: 0,
            block_offsets: create(&block_offsets_path)?,
            block_offsets_path,
            block: Vec::new(),
            block_ends: Vec::new(),
            block_base: (0, 0),
            record_count: 0,
            imported: false,
            strings: StringTable {
                ids: HashMap::new(),
                memory: Reservation::empty(budget),
                memory_share: budget.limit() / DICTIONARY_BUDGET_FRACTION,
                segment_records: SEGMENT_RECORDS,
                segments: Vec::new(),
                count: 0,
                bytes_len: 0,
                bytes: create(&bytes_path)?,
                bytes_path,
                ends: create(&ends_path)?,
                ends_path,
            },
            body: Vec::new(),
        })
    }

    pub fn record_count(&self) -> u64 {
        self.record_count
    }

    pub fn string_count(&self) -> u64 {
        self.strings.count
    }

    #[cfg(test)]
    fn with_segment_records(mut self, segment_records: u64) -> Self {
        // Applies from the next segment; the writer has not started one yet.
        self.strings.segment_records = segment_records;
        self
    }

    /// Strings in the current segment's in-memory dictionary.
    pub fn resident_strings(&self) -> usize {
        self.strings.ids.len()
    }

    pub fn write(&mut self, record: &Record, context: Option<ContextRef>) -> Result<RecordId> {
        let [lon, lat] = record
            .display_point()
            .with_context(|| format!("record {} has no finite display point", record.id()))?;
        let point = (quantize(lon)?, quantize(lat)?);
        if self.strings.segment_full(self.record_count) {
            self.strings.start_segment(self.record_count);
        }
        if self.record_count % BLOCK_RECORDS == 0 {
            self.flush_block()?;
            self.block_base = point;
        }

        let mut body = std::mem::take(&mut self.body);
        body.clear();
        self.encode_body(&mut body, record, point, context)?;
        self.block.extend_from_slice(&body);
        self.block_ends
            .push(u32::try_from(self.block.len()).context("record block exceeds 4 GiB")?);
        self.body = body;

        let id = self.record_count;
        self.record_count += 1;
        Ok(id)
    }

    fn encode_body(
        &mut self,
        out: &mut Vec<u8>,
        record: &Record,
        point: (i32, i32),
        context: Option<ContextRef>,
    ) -> Result<()> {
        let line = line_coordinates(record.geometry())?;
        let (source_kind, source_value) = match record {
            Record::Address(record) => osm_source(&record.source),
            Record::Interpolation(record) => osm_source(&record.source),
            Record::Street(record) => osm_source(&record.source),
            Record::Place(_, record) => osm_source(&record.source),
            Record::Postcode(record) => (SourceKind::Derived, record.source.record_count),
        };
        let centroid = matches!(record, Record::Address(address) if address.location_precision == LocationPrecision::Centroid);
        let mut tag = layer_code(record.layer()) | (source_kind as u8) << TAG_SOURCE_SHIFT;
        if centroid {
            tag |= TAG_CENTROID;
        }
        if line.is_some() {
            tag |= TAG_LINE;
        }
        out.push(tag);
        put_i64(out, i64::from(point.0) - i64::from(self.block_base.0));
        put_i64(out, i64::from(point.1) - i64::from(self.block_base.1));
        put_u64(
            out,
            context.map_or(0, |context| {
                (u64::from(context.tuple_id) + 1) << 1 | u64::from(context.ambiguous)
            }),
        );
        put_u64(out, source_value);

        match record {
            Record::Address(record) => {
                let address = &record.address;
                put_u64(out, u64::from(self.strings.id(&address.number)?));
                self.put_optional_strings(
                    out,
                    &[
                        address.street.as_deref(),
                        address.place.as_deref(),
                        address.unit.as_deref(),
                        address.locality.as_deref(),
                        address.region.as_deref(),
                        address.postcode.as_deref(),
                        address.country.as_deref(),
                    ],
                )?;
                if record.source.object_type == OsmObjectType::Row {
                    self.imported = true;
                    put_u64(out, u64::from(self.strings.id(&record.source.dataset)?));
                }
            }
            Record::Interpolation(record) => {
                let address = &record.address;
                self.put_optional_strings(
                    out,
                    &[
                        address.street.as_deref(),
                        address.place.as_deref(),
                        address.locality.as_deref(),
                        address.region.as_deref(),
                        address.postcode.as_deref(),
                        address.country.as_deref(),
                    ],
                )?;
                let range = &record.interpolation;
                put_u64(out, u64::from(self.strings.id(&range.kind)?));
                put_u64(out, u64::from(range.start));
                put_u64(out, u64::from(range.end.wrapping_sub(range.start)));
                put_u64(out, u64::from(range.step));
                put_i64(out, record.anchor_node_ids[0]);
                put_i64(out, record.anchor_node_ids[1]);
            }
            Record::Street(record) => {
                put_u64(out, u64::from(self.strings.id(&record.name)?));
            }
            Record::Postcode(record) => {
                if record.source.derived_from != DERIVED_FROM_ADDRESS_RECORDS {
                    bail!("unsupported postcode source {}", record.source.derived_from);
                }
                put_u64(out, u64::from(self.strings.id(&record.postcode)?));
            }
            Record::Place(_, record) => {
                put_u64(out, u64::from(self.strings.id(&record.name)?));
                put_u64(out, u64::from(self.strings.id(&record.place_type)?));
            }
        }

        if let Some(line) = line {
            put_u64(out, line.len() as u64);
            let mut previous = point;
            for position in line {
                put_i64(out, i64::from(position.0) - i64::from(previous.0));
                put_i64(out, i64::from(position.1) - i64::from(previous.1));
                previous = position;
            }
        }
        Ok(())
    }

    fn put_optional_strings(&mut self, out: &mut Vec<u8>, values: &[Option<&str>]) -> Result<()> {
        let mut present = 0u8;
        for (bit, value) in values.iter().enumerate() {
            if value.is_some() {
                present |= 1 << bit;
            }
        }
        out.push(present);
        for value in values.iter().flatten() {
            put_u64(out, u64::from(self.strings.id(value)?));
        }
        Ok(())
    }

    fn flush_block(&mut self) -> Result<()> {
        if self.block_ends.is_empty() {
            return Ok(());
        }
        self.block_offsets
            .write_all(&self.blocks_len.to_le_bytes())?;
        self.blocks.write_all(&self.block_base.0.to_le_bytes())?;
        self.blocks.write_all(&self.block_base.1.to_le_bytes())?;
        for end in &self.block_ends {
            self.blocks.write_all(&end.to_le_bytes())?;
        }
        self.blocks.write_all(&self.block)?;
        self.blocks_len +=
            (BLOCK_HEADER_BYTES + self.block_ends.len() * 4 + self.block.len()) as u64;
        self.block.clear();
        self.block_ends.clear();
        Ok(())
    }

    /// Copy the store into the Pack.
    pub fn finish(mut self, pack: &mut ContainerWriter) -> Result<()> {
        self.flush_block()?;
        self.blocks.flush()?;
        self.block_offsets.flush()?;
        self.strings.bytes.flush()?;
        self.strings.ends.flush()?;

        let version = if self.imported {
            RECORDS_VERSION_IMPORTED
        } else {
            RECORDS_VERSION
        };
        pack.begin(SECTION_INDEX, version)?;
        pack.write_all(&self.record_count.to_le_bytes())?;
        std::io::copy(&mut File::open(&self.block_offsets_path)?, pack)?;
        pack.write_all(&self.blocks_len.to_le_bytes())?;
        pack.end()?;

        pack.add_file(SECTION_BLOCKS, version, &self.blocks_path)?;

        let strings = &self.strings;
        pack.begin(SECTION_STRINGS, version)?;
        pack.write_all(&(strings.segments.len() as u64).to_le_bytes())?;
        for (first_record, first_string) in &strings.segments {
            pack.write_all(&first_record.to_le_bytes())?;
            pack.write_all(&first_string.to_le_bytes())?;
        }
        pack.write_all(&strings.count.to_le_bytes())?;
        std::io::copy(&mut File::open(&strings.ends_path)?, pack)?;
        std::io::copy(&mut File::open(&strings.bytes_path)?, pack)?;
        pack.end()
    }
}

#[derive(Clone)]
pub struct RecordsReader {
    record_count: u64,
    index: Bytes,
    blocks: Bytes,
    strings: Bytes,
    segment_count: u64,
    string_count: u64,
    /// Byte offsets within `strings` of the end-offset table and the text.
    ends_start: usize,
    text_start: usize,
}

impl RecordsReader {
    pub fn open(container: &Container) -> Result<Self> {
        let version = match container.sections().get(SECTION_INDEX) {
            Some(info) if info.version == RECORDS_VERSION_IMPORTED => RECORDS_VERSION_IMPORTED,
            _ => RECORDS_VERSION,
        };
        let index = container.section(SECTION_INDEX, version)?;
        let blocks = container.section(SECTION_BLOCKS, version)?;
        let strings = container.section(SECTION_STRINGS, version)?;

        let record_count = read_u64_le(&index, 0).context("records index is truncated")?;
        let block_count = record_count.div_ceil(BLOCK_RECORDS);
        if index.len() as u64 != 8 + (block_count + 1) * 8 {
            bail!("records index does not match its record count");
        }
        if read_u64_le(&index, index.len() - 8) != Some(blocks.len() as u64) {
            bail!("records index does not match the blocks section");
        }
        let truncated = || anyhow::anyhow!("string table is truncated");
        let segment_count = read_u64_le(&strings, 0).ok_or_else(truncated)?;
        let count_at = usize::try_from(8 + segment_count * 16)?;
        let mut previous: Option<(u64, u64)> = None;
        for segment in 0..segment_count {
            let at = (8 + segment * 16) as usize;
            let first_record = read_u64_le(&strings, at).ok_or_else(truncated)?;
            let first_string = read_u64_le(&strings, at + 8).ok_or_else(truncated)?;
            let ordered = match previous {
                None => first_record == 0,
                Some((record, string)) => first_record > record && first_string >= string,
            };
            if !ordered || first_record >= record_count {
                bail!("string table segments do not match the records");
            }
            previous = Some((first_record, first_string));
        }
        if (segment_count == 0) != (record_count == 0) {
            bail!("string table segments do not match the records");
        }
        let string_count = read_u64_le(&strings, count_at).ok_or_else(truncated)?;
        let ends_start = count_at + 8;
        let text_start = ends_start + usize::try_from(string_count * 8)?;
        let text_len = if string_count == 0 {
            0
        } else {
            read_u64_le(&strings, text_start - 8).ok_or_else(truncated)?
        };
        if strings.len() as u64 != text_start as u64 + text_len {
            bail!("string table does not match its offsets");
        }
        Ok(Self {
            record_count,
            index,
            blocks,
            strings,
            segment_count,
            string_count,
            ends_start,
            text_start,
        })
    }

    pub fn record_count(&self) -> u64 {
        self.record_count
    }

    pub fn header(&self, id: RecordId) -> Result<RecordHeader> {
        Ok(self.body(id)?.0)
    }

    pub fn record(&self, id: RecordId) -> Result<Record> {
        let (header, mut fields) = self.body(id)?;
        let geometry = |fields: &mut &[u8]| self.geometry(&header, fields);
        Ok(match header.layer {
            Layer::Address => {
                let address = self.address_components(header.strings, &mut fields)?;
                let dataset = self.imported_dataset(&header, &mut fields)?;
                Record::Address(AddressRecord {
                    address,
                    geometry: geometry(&mut fields)?,
                    location_precision: if header.centroid {
                        LocationPrecision::Centroid
                    } else {
                        LocationPrecision::Point
                    },
                    source: header.provenance(dataset)?,
                })
            }
            Layer::Interpolation => {
                let (address, interpolation, anchor_node_ids) =
                    self.interpolation_fields(header.strings, &mut fields)?;
                Record::Interpolation(InterpolationRecord {
                    address,
                    interpolation,
                    anchor_node_ids,
                    geometry: geometry(&mut fields)?,
                    representative_point: [header.lon(), header.lat()],
                    source: header.provenance(None)?,
                })
            }
            Layer::Street => Record::Street(StreetRecord {
                name: self.string_field(header.strings, &mut fields)?.to_string(),
                geometry: geometry(&mut fields)?,
                representative_point: [header.lon(), header.lat()],
                source: header.provenance(None)?,
            }),
            Layer::Postcode => Record::Postcode(PostcodeRecord {
                postcode: self.string_field(header.strings, &mut fields)?.to_string(),
                geometry: geometry(&mut fields)?,
                source: DerivedSourceProvenance::osm_address_records(header.source_value),
            }),
            layer => {
                let place_layer = layer.place_layer().expect("remaining layers are places");
                Record::Place(
                    place_layer,
                    PlaceRecord {
                        name: self.string_field(header.strings, &mut fields)?.to_string(),
                        place_type: self.string_field(header.strings, &mut fields)?.to_string(),
                        geometry: geometry(&mut fields)?,
                        source: header.provenance(None)?,
                    },
                )
            }
        })
    }

    /// Id, label, layer, point and source without decoding geometry.
    pub fn summary(&self, id: RecordId) -> Result<RecordSummary> {
        let (header, mut fields) = self.body(id)?;
        let mut source = header.source()?;
        let (record_id, label) = match header.layer {
            Layer::Address => {
                let address = self.address_components(header.strings, &mut fields)?;
                let id = match self.imported_dataset(&header, &mut fields)? {
                    Some(dataset) => {
                        let provenance = header.provenance(Some(dataset))?;
                        source.dataset = provenance.dataset.clone();
                        crate::labels::source_record_id(&provenance)
                    }
                    None => {
                        let (object_type, object_id) = header.osm_source()?;
                        crate::labels::osm_record_id(object_type, object_id)
                    }
                };
                (id, crate::labels::address_label(&address))
            }
            Layer::Interpolation => {
                let (address, range, anchors) =
                    self.interpolation_fields(header.strings, &mut fields)?;
                let (_, way_id) = header.osm_source()?;
                (
                    crate::labels::interpolation_record_id(way_id, anchors[0], anchors[1]),
                    crate::labels::interpolation_label(
                        &crate::labels::interpolation_name(&address),
                        &range,
                        &address,
                    ),
                )
            }
            Layer::Street => {
                let (object_type, object_id) = header.osm_source()?;
                (
                    crate::labels::osm_record_id(object_type, object_id),
                    self.string_field(header.strings, &mut fields)?.to_string(),
                )
            }
            Layer::Postcode => {
                let postcode = self.string_field(header.strings, &mut fields)?;
                (
                    crate::labels::derived_postcode_id(postcode),
                    postcode.to_string(),
                )
            }
            _ => {
                let name = self.string_field(header.strings, &mut fields)?;
                let place_type = self.string_field(header.strings, &mut fields)?;
                let (object_type, object_id) = header.osm_source()?;
                (
                    crate::labels::place_record_id(object_type, object_id, place_type),
                    name.to_string(),
                )
            }
        };
        Ok(RecordSummary {
            id: record_id,
            layer: header.layer.as_str().to_string(),
            label,
            point: Some(header.point()),
            source,
        })
    }

    /// The postcode an address, interpolation or postcode record states,
    /// without decoding its geometry.
    pub fn postcode(&self, id: RecordId) -> Result<Option<String>> {
        let (header, mut fields) = self.body(id)?;
        Ok(match header.layer {
            Layer::Address => {
                self.address_components(header.strings, &mut fields)?
                    .postcode
            }
            Layer::Interpolation => {
                self.interpolation_fields(header.strings, &mut fields)?
                    .0
                    .postcode
            }
            Layer::Postcode => Some(self.string_field(header.strings, &mut fields)?.to_string()),
            _ => None,
        })
    }

    /// Context (place or postcode) record view, `None` for other layers.
    pub fn context_record(&self, id: RecordId) -> Result<Option<ContextRecord>> {
        let header = self.header(id)?;
        if !header.layer.is_context() {
            return Ok(None);
        }
        let summary = self.summary(id)?;
        Ok(Some(ContextRecord {
            id: summary.id,
            layer: summary.layer,
            name: summary.label.clone(),
            postcode: (header.layer == Layer::Postcode).then(|| summary.label.clone()),
            label: summary.label,
            point: summary.point,
        }))
    }

    /// Line geometry as 1e-7 degree coordinates, `None` for point records.
    pub fn line(&self, id: RecordId) -> Result<Option<Vec<[i32; 2]>>> {
        let (header, mut fields) = self.body(id)?;
        if !header.line {
            return Ok(None);
        }
        self.skip_fields(&header, &mut fields)?;
        self.line_points(&header, &mut fields).map(Some)
    }

    pub fn record_json(&self, id: RecordId) -> Result<Value> {
        let record = self.record(id)?;
        let mut value = serde_json::to_value(&record)?;
        let object = value
            .as_object_mut()
            .context("record JSON must be an object")?;
        object.insert("layer".to_string(), json!(record.layer().as_str()));
        Ok(value)
    }

    fn segment(&self, record_id: RecordId) -> StringSegment {
        let field = |segment: u64, offset: u64| {
            read_u64_le(&self.strings, (8 + segment * 16 + offset) as usize)
                .expect("validated segments")
        };
        // Last segment whose first record is at or before this one.
        let (mut low, mut high) = (0, self.segment_count);
        while low < high {
            let mid = low + (high - low) / 2;
            if field(mid, 0) <= record_id {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        let segment = low.saturating_sub(1);
        StringSegment {
            first: field(segment, 8),
            end: if segment + 1 < self.segment_count {
                field(segment + 1, 8)
            } else {
                self.string_count
            },
        }
    }

    fn string(&self, segment: StringSegment, local: u32) -> Result<&str> {
        let id = segment.first + u64::from(local);
        if id >= segment.end {
            bail!("string id {local} is out of range for its segment");
        }
        let end_of = |id: u64| {
            read_u64_le(&self.strings, self.ends_start + id as usize * 8).expect("validated ends")
        };
        let start = if id == 0 { 0 } else { end_of(id - 1) };
        let bytes = self
            .strings
            .get(self.text_start + start as usize..self.text_start + end_of(id) as usize)
            .context("string offsets are out of range")?;
        std::str::from_utf8(bytes).context("string table entry is not valid UTF-8")
    }

    /// The block base point and the body bytes of a record.
    fn locate(&self, id: RecordId) -> Result<(i32, i32, &[u8])> {
        if id >= self.record_count {
            bail!("record {id} is out of range");
        }
        let block = id / BLOCK_RECORDS;
        let start = read_u64_le(&self.index, (8 + block * 8) as usize).expect("validated index");
        let end = read_u64_le(&self.index, (16 + block * 8) as usize).expect("validated index");
        let block_bytes = self
            .blocks
            .get(start as usize..end as usize)
            .context("record block is out of range")?;
        let records_in_block =
            (self.record_count - block * BLOCK_RECORDS).min(BLOCK_RECORDS) as usize;
        let bodies_start = BLOCK_HEADER_BYTES + records_in_block * 4;
        if block_bytes.len() < bodies_start {
            bail!("record block {block} is truncated");
        }
        let base_lon = i32::from_le_bytes(block_bytes[0..4].try_into().expect("block header"));
        let base_lat = i32::from_le_bytes(block_bytes[4..8].try_into().expect("block header"));
        let slot = (id % BLOCK_RECORDS) as usize;
        let body_end = |slot: usize| {
            read_u32_le(block_bytes, BLOCK_HEADER_BYTES + slot * 4).expect("validated header")
                as usize
        };
        let body_start = if slot == 0 { 0 } else { body_end(slot - 1) };
        let body = block_bytes
            .get(bodies_start + body_start..bodies_start + body_end(slot))
            .context("record body is out of range")?;
        Ok((base_lon, base_lat, body))
    }

    /// Layer and display point only: the hot path of spatial queries.
    pub fn point(&self, id: RecordId) -> Result<(Layer, f64, f64)> {
        let (base_lon, base_lat, mut body) = self.locate(id)?;
        let tag = get_u8(&mut body)?;
        let layer = *Layer::ALL
            .get(usize::from(tag & TAG_LAYER_MASK))
            .with_context(|| format!("record {id} has an unknown layer code"))?;
        let lon = add_delta(base_lon, get_i64(&mut body)?)?;
        let lat = add_delta(base_lat, get_i64(&mut body)?)?;
        Ok((layer, dequantize(lon), dequantize(lat)))
    }

    fn body(&self, id: RecordId) -> Result<(RecordHeader, &[u8])> {
        let (base_lon, base_lat, mut body) = self.locate(id)?;
        let tag = get_u8(&mut body)?;
        let layer = *Layer::ALL
            .get(usize::from(tag & TAG_LAYER_MASK))
            .with_context(|| format!("record {id} has an unknown layer code"))?;
        let source_kind = match tag >> TAG_SOURCE_SHIFT {
            0 => SourceKind::Node,
            1 => SourceKind::Way,
            2 => SourceKind::Relation,
            _ => SourceKind::Derived,
        };
        let lon_e7 = add_delta(base_lon, get_i64(&mut body)?)?;
        let lat_e7 = add_delta(base_lat, get_i64(&mut body)?)?;
        let context = match get_u64(&mut body)? {
            0 => None,
            value => Some(ContextRef {
                tuple_id: u32::try_from((value >> 1) - 1).context("context id out of range")?,
                ambiguous: value & 1 == 1,
            }),
        };
        let source_value = get_u64(&mut body)?;
        Ok((
            RecordHeader {
                layer,
                strings: self.segment(id),
                lon_e7,
                lat_e7,
                context,
                centroid: tag & TAG_CENTROID != 0,
                line: tag & TAG_LINE != 0,
                source_kind,
                source_value,
            },
            body,
        ))
    }

    fn string_field(&self, strings: StringSegment, fields: &mut &[u8]) -> Result<&str> {
        self.string(strings, get_u32(fields)?)
    }

    fn optional_strings<const N: usize>(
        &self,
        strings: StringSegment,
        fields: &mut &[u8],
    ) -> Result<[Option<String>; N]> {
        let present = get_u8(fields)?;
        let mut values: [Option<String>; N] = std::array::from_fn(|_| None);
        for (bit, value) in values.iter_mut().enumerate() {
            if present & (1 << bit) != 0 {
                *value = Some(self.string_field(strings, fields)?.to_string());
            }
        }
        Ok(values)
    }

    fn address_components(
        &self,
        strings: StringSegment,
        fields: &mut &[u8],
    ) -> Result<AddressComponents> {
        let number = self.string_field(strings, fields)?.to_string();
        let [street, place, unit, locality, region, postcode, country] =
            self.optional_strings(strings, fields)?;
        Ok(AddressComponents {
            number,
            street,
            place,
            unit,
            locality,
            region,
            postcode,
            country,
        })
    }

    /// The dataset name stored after the fields of an imported address row.
    fn imported_dataset(&self, header: &RecordHeader, fields: &mut &[u8]) -> Result<Option<&str>> {
        if !header.imported() {
            return Ok(None);
        }
        self.string_field(header.strings, fields).map(Some)
    }

    fn interpolation_fields(
        &self,
        strings: StringSegment,
        fields: &mut &[u8],
    ) -> Result<(InterpolationAddressComponents, InterpolationRange, [i64; 2])> {
        let [street, place, locality, region, postcode, country] =
            self.optional_strings(strings, fields)?;
        let kind = self.string_field(strings, fields)?.to_string();
        let start = get_u32(fields)?;
        let end = start.wrapping_add(get_u32(fields)?);
        let step = get_u32(fields)?;
        let anchors = [get_i64(fields)?, get_i64(fields)?];
        Ok((
            InterpolationAddressComponents {
                street,
                place,
                locality,
                region,
                postcode,
                country,
            },
            InterpolationRange {
                kind,
                start,
                end,
                step,
            },
            anchors,
        ))
    }

    fn skip_fields(&self, header: &RecordHeader, fields: &mut &[u8]) -> Result<()> {
        match header.layer {
            Layer::Address => {
                self.address_components(header.strings, fields)?;
                self.imported_dataset(header, fields)?;
            }
            Layer::Interpolation => {
                self.interpolation_fields(header.strings, fields)?;
            }
            Layer::Street | Layer::Postcode => {
                get_u32(fields)?;
            }
            _ => {
                get_u32(fields)?;
                get_u32(fields)?;
            }
        }
        Ok(())
    }

    fn geometry(&self, header: &RecordHeader, fields: &mut &[u8]) -> Result<Geometry> {
        if !header.line {
            return Ok(point_geometry(header.lon(), header.lat()));
        }
        let coordinates = self
            .line_points(header, fields)?
            .into_iter()
            .map(|[lon, lat]| vec![dequantize(lon), dequantize(lat)].into())
            .collect();
        Ok(Geometry::new(GeometryValue::LineString { coordinates }))
    }

    fn line_points(&self, header: &RecordHeader, fields: &mut &[u8]) -> Result<Vec<[i32; 2]>> {
        let count = usize::try_from(get_u64(fields)?)?;
        if count < 2 || count > fields.len() {
            bail!("stored line geometry has an invalid point count {count}");
        }
        let mut points = Vec::with_capacity(count);
        let mut previous = [header.lon_e7, header.lat_e7];
        for _ in 0..count {
            let lon = add_delta(previous[0], get_i64(fields)?)?;
            let lat = add_delta(previous[1], get_i64(fields)?)?;
            previous = [lon, lat];
            points.push(previous);
        }
        Ok(points)
    }
}

fn osm_source(source: &SourceProvenance) -> (SourceKind, u64) {
    let kind = match source.object_type {
        OsmObjectType::Node => SourceKind::Node,
        OsmObjectType::Way => SourceKind::Way,
        OsmObjectType::Relation => SourceKind::Relation,
        OsmObjectType::Row => SourceKind::Derived,
    };
    (kind, crate::util::codec::zigzag(source.object_id))
}

fn layer_code(layer: Layer) -> u8 {
    Layer::ALL
        .iter()
        .position(|candidate| *candidate == layer)
        .expect("every layer has a code") as u8
}

/// Quantized vertices of a line geometry, `None` for points.
fn line_coordinates(geometry: &Geometry) -> Result<Option<Vec<(i32, i32)>>> {
    match &geometry.value {
        GeometryValue::Point { .. } => Ok(None),
        GeometryValue::LineString { coordinates } => {
            if coordinates.len() < 2 {
                bail!("line geometry needs at least two positions");
            }
            coordinates
                .iter()
                .map(|position| {
                    let [lon, lat, ..] = position.as_slice() else {
                        bail!("line position is missing lon/lat");
                    };
                    Ok((quantize(*lon)?, quantize(*lat)?))
                })
                .collect::<Result<Vec<_>>>()
                .map(Some)
        }
        other => bail!("unsupported stored record geometry: {}", other.type_name()),
    }
}

pub(crate) fn quantize(value: f64) -> Result<i32> {
    if !value.is_finite() {
        bail!("coordinate must be finite");
    }
    let scaled = (value * COORDINATE_SCALE).round();
    if scaled < i32::MIN as f64 || scaled > i32::MAX as f64 {
        bail!("coordinate {value} is out of range");
    }
    Ok(scaled as i32)
}

pub(crate) fn dequantize(value: i32) -> f64 {
    value as f64 / COORDINATE_SCALE
}

fn add_delta(base: i32, delta: i64) -> Result<i32> {
    i32::try_from(i64::from(base) + delta).context("coordinate delta is out of range")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::PlaceLayer;

    fn line(points: &[[f64; 2]]) -> Geometry {
        Geometry::new(GeometryValue::LineString {
            coordinates: points.iter().map(|point| point.to_vec().into()).collect(),
        })
    }

    fn sample_records() -> Vec<Record> {
        let mut records = vec![
            Record::Address(AddressRecord {
                address: AddressComponents {
                    number: "10".into(),
                    street: None,
                    place: Some("Market Square".into()),
                    unit: Some("2".into()),
                    locality: Some("Toronto".into()),
                    region: Some("ON".into()),
                    postcode: Some("M5V 1A1".into()),
                    country: Some("CA".into()),
                },
                geometry: point_geometry(-79.0, 43.0),
                location_precision: LocationPrecision::Centroid,
                source: SourceProvenance::osm(OsmObjectType::Node, 1),
            }),
            Record::Street(StreetRecord {
                name: "King Street".into(),
                geometry: line(&[[-79.0, 43.0], [-79.001, 43.001], [-79.0015, 43.0]]),
                representative_point: [-79.0, 43.0],
                source: SourceProvenance::osm(OsmObjectType::Way, 2),
            }),
            Record::Postcode(PostcodeRecord {
                postcode: "M5V 1A1".into(),
                geometry: point_geometry(-79.0, 43.0),
                source: DerivedSourceProvenance::osm_address_records(12),
            }),
            Record::Interpolation(InterpolationRecord {
                address: InterpolationAddressComponents {
                    street: Some("King Street".into()),
                    place: None,
                    locality: Some("Toronto".into()),
                    region: None,
                    postcode: Some("M5V".into()),
                    country: Some("CA".into()),
                },
                interpolation: InterpolationRange {
                    kind: "even".into(),
                    start: 10,
                    end: 20,
                    step: 2,
                },
                anchor_node_ids: [10, 20],
                geometry: line(&[[-79.0, 43.0], [-79.001, 43.001]]),
                representative_point: [-79.0, 43.0],
                source: SourceProvenance::osm(OsmObjectType::Way, 5),
            }),
        ];
        for layer in [
            PlaceLayer::Country,
            PlaceLayer::Region,
            PlaceLayer::District,
            PlaceLayer::Place,
            PlaceLayer::Locality,
            PlaceLayer::Neighbourhood,
        ] {
            records.push(Record::Place(
                layer,
                PlaceRecord {
                    name: "Example place".into(),
                    place_type: "city".into(),
                    geometry: point_geometry(-79.0, 43.0),
                    source: SourceProvenance::osm(OsmObjectType::Relation, 3),
                },
            ));
        }
        records.push(Record::Place(
            PlaceLayer::Country,
            PlaceRecord {
                name: "Canada".into(),
                place_type: "derived_country:CA".into(),
                geometry: point_geometry(-79.0, 43.0),
                source: SourceProvenance::osm(OsmObjectType::Relation, 4),
            },
        ));
        records
    }

    fn write_store(records: &[Record], contexts: &[Option<ContextRef>]) -> RecordsReader {
        RecordsReader::open(&Container::open(write_container(records, contexts)).expect("open"))
            .expect("reader")
    }

    fn write_container(records: &[Record], contexts: &[Option<ContextRef>]) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("open-geocode-records-{}", uuid::Uuid::new_v4()));
        let scratch = Scratch::create(root.join("scratch")).expect("scratch");
        let mut writer =
            RecordsWriter::create(&scratch, &MemoryBudget::unlimited()).expect("writer");
        for (index, record) in records.iter().enumerate() {
            let id = writer
                .write(record, contexts.get(index).copied().flatten())
                .expect("write");
            assert_eq!(id, index as u64);
        }
        let path = root.join("pack.ogp");
        let mut pack = ContainerWriter::create(&path).expect("pack");
        writer.finish(&mut pack).expect("finish");
        pack.finish().expect("pack finish");
        path
    }

    #[test]
    fn imported_rows_round_trip_and_bump_the_section_version() {
        let mut records = sample_records();
        let version = |path: &std::path::Path| {
            Container::open(path).expect("open").sections()[SECTION_INDEX].version
        };
        assert_eq!(
            version(&write_container(&records, &[])),
            RECORDS_VERSION,
            "a store without imported rows stays readable by older binaries"
        );

        let row = Record::Address(AddressRecord {
            address: AddressComponents {
                number: "7".into(),
                street: Some("BARRINGER STREET".into()),
                place: None,
                unit: None,
                locality: None,
                region: None,
                postcode: None,
                country: None,
            },
            geometry: point_geometry(172.6, -43.5),
            location_precision: LocationPrecision::Point,
            source: SourceProvenance::row("linz", 42),
        });
        records.push(row.clone());
        let path = write_container(&records, &[]);
        assert_eq!(version(&path), RECORDS_VERSION_IMPORTED);
        let reader = RecordsReader::open(&Container::open(&path).expect("open")).expect("reader");
        let id = records.len() as u64 - 1;
        assert_eq!(reader.record(id).expect("record"), row);
        assert_eq!(reader.summary(id).expect("summary").id, "linz:42");
    }

    #[test]
    fn round_trips_every_layer_and_matches_summaries() {
        let records = sample_records();
        let reader = write_store(&records, &[]);
        assert_eq!(reader.record_count(), records.len() as u64);
        for (id, record) in records.iter().enumerate() {
            let id = id as u64;
            assert_eq!(&reader.record(id).expect("record"), record);
            let summary = reader.summary(id).expect("summary");
            let full = reader.record_json(id).expect("json");
            assert_eq!(summary.id, full["id"]);
            assert_eq!(summary.label, full["label"]);
            assert_eq!(summary.layer, full["layer"]);
            assert_eq!(
                serde_json::to_value(&summary.source).expect("source"),
                full["source"]
            );
            let point = summary.point.expect("point");
            assert_eq!((point.lon, point.lat), (-79.0, 43.0));
        }
        assert_eq!(
            reader.record_json(3).expect("interpolation")["anchor_ids"],
            json!(["osm:node:10", "osm:node:20"])
        );
        assert_eq!(reader.line(0).expect("address line"), None);
        assert_eq!(
            reader.line(1).expect("street line"),
            Some(vec![
                [-790_000_000, 430_000_000],
                [-790_010_000, 430_010_000],
                [-790_015_000, 430_000_000]
            ])
        );
    }

    #[test]
    fn string_dictionary_is_bounded_by_the_segment() {
        let root =
            std::env::temp_dir().join(format!("open-geocode-segments-{}", uuid::Uuid::new_v4()));
        let scratch = Scratch::create(root.join("scratch")).expect("scratch");
        let budget = MemoryBudget::unlimited();
        let mut writer = RecordsWriter::create(&scratch, &budget)
            .expect("writer")
            .with_segment_records(100);
        let base = sample_records().remove(0);
        let mut records = Vec::new();
        let mut most_resident = 0;
        for index in 0..1_050 {
            let mut record = base.clone();
            if let Record::Address(address) = &mut record {
                // Every record has its own street; locality is shared by all.
                address.address.street = Some(format!("Street {index}"));
                address.address.number = (index % 7).to_string();
            }
            writer.write(&record, None).expect("write");
            most_resident = most_resident.max(writer.resident_strings());
            assert!(budget.used() >= writer.resident_strings() * DICTIONARY_ENTRY_OVERHEAD);
            records.push(record);
        }
        assert!(
            most_resident <= 100 + 12,
            "dictionary held {most_resident} strings"
        );
        assert!(budget.peak() < 200 * (DICTIONARY_ENTRY_OVERHEAD + 16));
        let path = root.join("pack.ogp");
        let mut pack = ContainerWriter::create(&path).expect("pack");
        writer.finish(&mut pack).expect("finish");
        pack.finish().expect("pack finish");
        let reader = RecordsReader::open(&Container::open(&path).expect("open")).expect("reader");
        for (id, record) in records.iter().enumerate() {
            assert_eq!(&reader.record(id as u64).expect("record"), record);
        }
    }

    #[test]
    fn string_dictionary_stays_within_its_share_of_a_small_budget() {
        let root =
            std::env::temp_dir().join(format!("open-geocode-dictionary-{}", uuid::Uuid::new_v4()));
        let scratch = Scratch::create(root.join("scratch")).expect("scratch");
        let budget = MemoryBudget::new(64 << 20);
        let mut writer = RecordsWriter::create(&scratch, &budget).expect("writer");
        let base = sample_records().remove(0);
        // Every record brings a new street name: the worst case.
        for index in 0..200_000 {
            let mut record = base.clone();
            if let Record::Address(address) = &mut record {
                address.address.street = Some(format!("Unique Street Number {index}"));
            }
            writer.write(&record, None).expect("write");
        }
        // One entry can land after the share is reached; the next record
        // starts a new segment.
        let share = budget.limit() / DICTIONARY_BUDGET_FRACTION;
        let slack = 8 * (64 + DICTIONARY_ENTRY_OVERHEAD);
        assert!(
            budget.peak() <= share + slack,
            "dictionary peaked at {} of {share}",
            budget.peak()
        );
        assert!(writer.strings.segments.len() > 1);
        let path = root.join("pack.ogp");
        let mut pack = ContainerWriter::create(&path).expect("pack");
        writer.finish(&mut pack).expect("finish");
        pack.finish().expect("pack finish");
        let reader = RecordsReader::open(&Container::open(&path).expect("open")).expect("reader");
        for id in [0, 99_999, 199_999] {
            let Record::Address(address) = reader.record(id).expect("record") else {
                panic!("address");
            };
            assert_eq!(
                address.address.street.as_deref(),
                Some(format!("Unique Street Number {id}").as_str())
            );
        }
    }

    #[test]
    fn spans_blocks_and_keeps_contexts() {
        let base = sample_records().remove(0);
        let mut records = Vec::new();
        let mut contexts = Vec::new();
        for index in 0..(BLOCK_RECORDS * 3 + 5) {
            let mut record = base.clone();
            if let Record::Address(address) = &mut record {
                address.address.number = index.to_string();
                address.geometry = point_geometry(-79.0 + index as f64 * 0.001, 43.0);
                address.source.object_id = index as i64;
            }
            records.push(record);
            contexts.push((index % 3 != 0).then_some(ContextRef {
                tuple_id: index as u32,
                ambiguous: index % 2 == 0,
            }));
        }
        let reader = write_store(&records, &contexts);
        for (index, record) in records.iter().enumerate() {
            assert_eq!(&reader.record(index as u64).expect("record"), record);
            assert_eq!(
                reader.header(index as u64).expect("header").context,
                contexts[index]
            );
        }
        assert!(reader.record(records.len() as u64).is_err());
    }

    #[test]
    fn line_geometry_follows_the_summary_fields() {
        let mut records = sample_records();
        records.truncate(2);
        let reader = write_store(&records, &[]);
        let (header, fields) = reader.body(1).expect("body");
        let mut cursor = fields;
        reader.skip_fields(&header, &mut cursor).expect("fields");
        let geometry_offset = fields.len() - cursor.len();

        // A damaged point count breaks full decoding but not the summary, which
        // stops before the geometry bytes.
        let mut damaged = fields.to_vec();
        damaged[geometry_offset] = 1;
        let mut cursor = damaged.as_slice();
        assert_eq!(
            reader
                .string_field(header.strings, &mut cursor)
                .expect("name"),
            "King Street"
        );
        assert!(reader.geometry(&header, &mut cursor).is_err());
    }
}
