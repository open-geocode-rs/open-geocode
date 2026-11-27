use std::{
    fs::File,
    io::{BufWriter, Write},
    path::PathBuf,
    sync::Arc,
};

use crate::error::{Error, Result};
use geojson::{Geometry, GeometryValue};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    api::Address,
    container::{self, Bytes, Container},
    extsort::{Scratch, ScratchFile},
    licences::Licence,
    memory::{Budget, Reservation},
    pack::RecordId,
    parse::lattice::CountryCode,
    record::{
        AccessPoint, AddressComponents, AddressRecord, Brand, Building, Contact,
        DERIVED_FROM_ADDRESS_RECORDS, DerivedSourceProvenance, Entrance, Extent,
        InterpolationAddressComponents, InterpolationRange, InterpolationRecord, Layer,
        LocationPrecision, OSM_DATASET, ObjectType, PlaceRecord, PoiDetails, PoiRecord,
        PostcodeRecord, Prominence, Record, RoadClass, SourceProvenance, StreetRecord,
        point_geometry,
    },
    util::{
        codec::{
            get_i64, get_opt_string, get_str, get_u8, get_u32, get_u64, put_i64, put_opt_str,
            put_str, put_u64, read_u64_le, unzigzag, zigzag,
        },
        geo::{box_around, line_geometry, line_parts, narrowest_box},
    },
};

pub const SECTION_INDEX: &str = "records/index";
pub const SECTION_BLOCKS: &str = "records/blocks";
pub const SECTION_DATASETS: &str = "records/datasets";
pub const SECTION_STRINGS: &str = "records/strings";
pub const RECORDS_VERSION: u32 = 15;
const PREVIOUS_RECORDS_VERSION: u32 = 14;
const OVERTURE_RECORDS_VERSION: u32 = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatalogueId {
    pub record_id: RecordId,
    pub object_type: ObjectType,
    pub object_id: i64,
    pub street: bool,
}

pub const BLOCK_RECORDS: u64 = 64;
const LABEL: &str = "pack-records";
const DICTIONARY_ENTRY_OVERHEAD: usize = 48;
pub const SEGMENT_RECORDS: u64 = 1 << 20;
const DICTIONARY_BUDGET_FRACTION: usize = 8;
const COORDINATE_SCALE: f64 = 10_000_000.0;
const BLOCK_HEADER_BYTES: usize = 18;
const BLOCK_CENTROIDS: u8 = 1;
const BLOCK_WAYS: u8 = 2;
const BLOCK_RELATIONS: u8 = 4;
const BLOCK_DISPUTED: u8 = 8;

const TAG_LAYER_MASK: u8 = 0x0f;
const TAG_CENTROID: u8 = 0x10;
const TAG_LINE: u8 = 0x20;
const TAG_SOURCE_SHIFT: u8 = 6;
const POI_EXTENT: u8 = 1 << 4;
const POI_ACCESS: u8 = 1 << 5;
const POI_CUISINE: u8 = 1 << 6;
const POI_FAX: u8 = 1 << 7;
const ADDRESS_DETAILS: u8 = 1 << 7;
const DETAIL_ACCESS: u8 = 1;
const DETAIL_BUILDING: u8 = 2;
const DETAIL_BUILDING_NAME: u8 = 4;
const DETAIL_BLOCK_NUMBER: u8 = 8;
const NUMBER_TEXT: u64 = 1;
const NUMBER_UNIT: u64 = 2;
const NUMBER_SHIFT: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceKind {
    Node = 0,
    Way = 1,
    Relation = 2,
    Other = 3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextRef {
    pub tuple_id: u32,
    pub ambiguous: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Summary {
    pub id: String,
    pub layer: String,
    pub name: String,
    pub label: String,
    #[serde(skip_serializing_if = "Address::is_empty")]
    pub address: Address,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub road_class: Option<RoadClass>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub brand: Option<Brand>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contact: Option<Contact>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub opening_hours: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub cuisine: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bbox: Option<[f64; 4]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub street_postcode: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub access: Vec<AccessPoint>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub building: Option<Building>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub point: Option<Point>,
    pub source: Source,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
pub struct Point {
    pub lon: f64,
    pub lat: f64,
    pub precision: PointPrecision,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PointPrecision {
    Point,
    Centroid,
    Estimated,
    RepresentativePoint,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct Source {
    pub dataset: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub object_type: Option<ObjectType>,
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
    pub point: Option<Point>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StringSegment {
    first: u64,
    end: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub layer: Layer,
    strings: StringSegment,
    pub lon_e7: i32,
    pub lat_e7: i32,
    pub context: Option<ContextRef>,
    pub disputed: bool,
    centroid: bool,
    line: bool,
    source_kind: SourceKind,
    source_value: u64,
    dataset: Option<u32>,
    gers: Option<u32>,
}

impl Header {
    pub fn lon(&self) -> f64 {
        dequantize(self.lon_e7)
    }

    pub fn lat(&self) -> f64 {
        dequantize(self.lat_e7)
    }

    pub fn has_line(&self) -> bool {
        self.line
    }

    fn point(&self) -> Point {
        let precision = if self.line {
            PointPrecision::RepresentativePoint
        } else if matches!(self.layer, Layer::Address | Layer::Poi) && !self.centroid {
            PointPrecision::Point
        } else {
            PointPrecision::Centroid
        };
        Point {
            lon: self.lon(),
            lat: self.lat(),
            precision,
        }
    }

    pub(crate) fn object(&self) -> Result<(ObjectType, i64)> {
        let object_type = match (self.source_kind, self.dataset) {
            (SourceKind::Node, _) => ObjectType::Node,
            (SourceKind::Way, _) => ObjectType::Way,
            (SourceKind::Relation, _) => ObjectType::Relation,
            (SourceKind::Other, Some(_)) => ObjectType::Row,
            (SourceKind::Other, None) => {
                return Err(Error::Inconsistent(
                    "derived record has no source object".into(),
                ));
            }
        };
        Ok((object_type, unzigzag(self.source_value)))
    }

    fn dataset<'a>(&self, datasets: &'a [String]) -> Result<&'a str> {
        match self.dataset {
            None => Ok(OSM_DATASET),
            Some(index) => datasets
                .get(index as usize)
                .map(String::as_str)
                .ok_or(Error::Decode("record names an unknown dataset")),
        }
    }

    fn record_id(&self, datasets: &[String]) -> Result<String> {
        let (object_type, object_id) = self.object()?;
        Ok(crate::labels::record_id(
            self.dataset(datasets)?,
            object_type,
            object_id,
        ))
    }

    fn source(&self, datasets: &[String]) -> Result<Source> {
        if self.source_kind == SourceKind::Other && self.dataset.is_none() {
            return Ok(Source {
                dataset: "osm".into(),
                object_type: None,
                object_id: None,
                derived_from: Some(DERIVED_FROM_ADDRESS_RECORDS.into()),
                record_count: Some(self.source_value),
            });
        }
        let (object_type, object_id) = self.object()?;
        Ok(Source {
            dataset: self.dataset(datasets)?.to_string(),
            object_type: Some(object_type),
            object_id: Some(object_id),
            derived_from: None,
            record_count: None,
        })
    }

    fn provenance(&self, datasets: &[String]) -> Result<SourceProvenance> {
        let (object_type, object_id) = self.object()?;
        Ok(match self.dataset {
            None => SourceProvenance::osm(object_type, object_id),
            Some(_) => SourceProvenance::imported(self.dataset(datasets)?, object_id),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriterState {
    blocks: (String, u64),
    blocks_len: u64,
    block_offsets: (String, u64),
    record_count: u64,
    segments: Vec<(u64, u64)>,
    string_count: u64,
    strings_len: u64,
    strings: (String, u64),
    string_ends: (String, u64),
    datasets: Vec<String>,
}

#[derive(Debug)]
pub struct Writer {
    blocks: BufWriter<ScratchFile>,
    blocks_len: u64,
    block_offsets: BufWriter<ScratchFile>,
    block: Vec<u8>,
    block_starts: Vec<u32>,
    block_slots: usize,
    continued: u64,
    centroids: u64,
    ways: u64,
    relations: u64,
    disputed: u64,
    block_base: (i32, i32),
    record_count: u64,
    strings: StringTable,
    body: Vec<u8>,
    run: AddressRun,
    datasets: Vec<String>,
}

#[derive(Debug, Default)]
struct AddressRun {
    open: bool,
    street: Option<String>,
    place: Option<String>,
    locality: Option<String>,
    region: Option<String>,
    postcode: Option<String>,
    country: Option<String>,
    areas: Vec<String>,
    block_number_len: Option<std::num::NonZeroU16>,
    street_postcodes: Vec<String>,
    building: Option<Building>,
    dataset: Option<String>,
    context: Option<ContextRef>,
    point: (i32, i32),
    object_id: i64,
    number: u32,
}

impl AddressRun {
    fn continues(&self, record: &AddressRecord, context: Option<ContextRef>) -> bool {
        let address = &record.address;
        self.open
            && self.dataset.as_deref() == imported_dataset(record)
            && self.context == context
            && self.street == address.street
            && self.place == address.place
            && self.locality == address.locality
            && self.region == address.region
            && self.postcode == address.postcode
            && self.country == address.country
            && self.areas == address.areas
            && self.block_number_len == address.block_number_len
            && self.street_postcodes == record.street_postcodes
            && self.building == record.building
            && record.access.is_empty()
    }

    fn start(&mut self, record: &AddressRecord, context: Option<ContextRef>, point: (i32, i32)) {
        let address = &record.address;
        self.open = true;
        self.street.clone_from(&address.street);
        self.place.clone_from(&address.place);
        self.locality.clone_from(&address.locality);
        self.region.clone_from(&address.region);
        self.postcode.clone_from(&address.postcode);
        self.country.clone_from(&address.country);
        self.areas.clone_from(&address.areas);
        self.block_number_len = address.block_number_len;
        self.street_postcodes.clone_from(&record.street_postcodes);
        self.building.clone_from(&record.building);
        match imported_dataset(record) {
            Some(dataset) => {
                let held = self.dataset.get_or_insert_default();
                held.clear();
                held.push_str(dataset);
            }
            None => self.dataset = None,
        }
        self.context = context;
        self.point = point;
        self.object_id = record.source.object_id;
        self.number = plain_number(&address.number).unwrap_or(0);
    }
}

fn imported_dataset(record: &AddressRecord) -> Option<&str> {
    (record.source.object_type == ObjectType::Row).then_some(record.source.dataset.as_str())
}

fn plain_number(number: &str) -> Option<u32> {
    let canonical = !number.is_empty()
        && number.bytes().all(|byte| byte.is_ascii_digit())
        && (number.len() == 1 || !number.starts_with('0'));
    canonical.then(|| number.parse().ok()).flatten()
}

#[derive(Debug)]
struct StringTable {
    ids: FxHashMap<Box<str>, u32>,
    memory: Reservation,
    memory_share: usize,
    segment_records: u64,
    segments: Vec<(u64, u64)>,
    count: u64,
    bytes_len: u64,
    bytes: BufWriter<ScratchFile>,
    ends: BufWriter<ScratchFile>,
}

impl StringTable {
    fn id(&mut self, value: &str) -> Result<u32> {
        if let Some(&id) = self.ids.get(value) {
            return Ok(id);
        }
        let id = u32::try_from(self.ids.len())
            .map_err(|_| Error::Inconsistent("string segment is full".into()))?;
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
        self.ids = FxHashMap::default();
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

impl Writer {
    pub fn create(scratch: &Arc<Scratch>, budget: &Arc<Budget>) -> Result<Self> {
        let create = |name: &str| -> Result<BufWriter<ScratchFile>> {
            Ok(BufWriter::with_capacity(
                1 << 20,
                ScratchFile::create(scratch, LABEL, name)?,
            ))
        };
        Ok(Self {
            blocks: create("records-blocks.bin")?,
            blocks_len: 0,
            block_offsets: create("records-block-offsets.bin")?,
            block: Vec::new(),
            block_starts: Vec::new(),
            block_slots: 0,
            continued: 0,
            centroids: 0,
            ways: 0,
            relations: 0,
            disputed: 0,
            block_base: (0, 0),
            record_count: 0,
            strings: StringTable {
                ids: FxHashMap::default(),
                memory: Reservation::empty(budget),
                memory_share: budget.limit() / DICTIONARY_BUDGET_FRACTION,
                segment_records: SEGMENT_RECORDS,
                segments: Vec::new(),
                count: 0,
                bytes_len: 0,
                bytes: create("records-strings.bin")?,
                ends: create("records-string-ends.bin")?,
            },
            body: Vec::new(),
            run: AddressRun::default(),
            datasets: Vec::new(),
        })
    }

    pub fn record_count(&self) -> u64 {
        self.record_count
    }

    pub fn string_count(&self) -> u64 {
        self.strings.count
    }

    /// The store once every record is written, its files flushed, for a
    /// checkpoint. No record may be written after.
    pub fn state(&mut self) -> Result<WriterState> {
        self.flush_block()?;
        let file = |file: &mut BufWriter<ScratchFile>| -> Result<(String, u64)> {
            file.flush()?;
            Ok((file.get_ref().name(), file.get_ref().bytes()))
        };
        Ok(WriterState {
            blocks: file(&mut self.blocks)?,
            blocks_len: self.blocks_len,
            block_offsets: file(&mut self.block_offsets)?,
            record_count: self.record_count,
            segments: self.strings.segments.clone(),
            string_count: self.strings.count,
            strings_len: self.strings.bytes_len,
            strings: file(&mut self.strings.bytes)?,
            string_ends: file(&mut self.strings.ends)?,
            datasets: self.datasets.clone(),
        })
    }

    pub fn files(&self) -> Vec<PathBuf> {
        [
            &self.blocks,
            &self.block_offsets,
            &self.strings.bytes,
            &self.strings.ends,
        ]
        .into_iter()
        .map(|file| file.get_ref().path().to_path_buf())
        .collect()
    }

    pub fn restore(
        scratch: &Arc<Scratch>,
        budget: &Arc<Budget>,
        state: &WriterState,
    ) -> Result<Self> {
        let reopen = |(name, bytes): &(String, u64)| -> Result<BufWriter<ScratchFile>> {
            Ok(BufWriter::with_capacity(
                1 << 20,
                ScratchFile::reopen(scratch, LABEL, name, *bytes)?,
            ))
        };
        Ok(Self {
            blocks: reopen(&state.blocks)?,
            blocks_len: state.blocks_len,
            block_offsets: reopen(&state.block_offsets)?,
            block: Vec::new(),
            block_starts: Vec::new(),
            block_slots: 0,
            continued: 0,
            centroids: 0,
            ways: 0,
            relations: 0,
            disputed: 0,
            block_base: (0, 0),
            record_count: state.record_count,
            strings: StringTable {
                ids: FxHashMap::default(),
                memory: Reservation::empty(budget),
                memory_share: budget.limit() / DICTIONARY_BUDGET_FRACTION,
                segment_records: SEGMENT_RECORDS,
                segments: state.segments.clone(),
                count: state.string_count,
                bytes_len: state.strings_len,
                bytes: reopen(&state.strings)?,
                ends: reopen(&state.string_ends)?,
            },
            body: Vec::new(),
            run: AddressRun::default(),
            datasets: state.datasets.clone(),
        })
    }

    #[cfg(test)]
    fn with_segment_records(mut self, segment_records: u64) -> Self {
        self.strings.segment_records = segment_records;
        self
    }

    pub fn resident_strings(&self) -> usize {
        self.strings.ids.len()
    }

    pub fn write(&mut self, record: &Record, context: Option<ContextRef>) -> Result<RecordId> {
        let [lon, lat] = record.display_point().ok_or_else(|| {
            Error::BadGeometry(format!(
                "record {} has no finite display point",
                record.id()
            ))
        })?;
        let point = (quantize(lon)?, quantize(lat)?);
        if self.record_count.is_multiple_of(BLOCK_RECORDS) {
            if self.strings.segment_full(self.record_count) {
                self.strings.start_segment(self.record_count);
            }
            self.flush_block()?;
            self.block_base = point;
            self.run.open = false;
        }

        let mut body = std::mem::take(&mut self.body);
        body.clear();
        let slot = 1u64 << self.block_slots;
        if let Record::Address(address) = record
            && address.disputed
        {
            self.disputed |= slot;
        }
        match record {
            Record::Address(address) if self.run.continues(address, context) => {
                self.encode_continuation(&mut body, address, point)?;
                self.continued |= slot;
                if address.location_precision == LocationPrecision::Centroid {
                    self.centroids |= slot;
                }
                match address.source.object_type {
                    ObjectType::Node => {}
                    ObjectType::Way => self.ways |= slot,
                    ObjectType::Relation => self.relations |= slot,
                    ObjectType::Row => {}
                }
            }
            _ => {
                self.block_starts.push(
                    u32::try_from(self.block.len())
                        .map_err(|_| Error::Inconsistent("record block exceeds 4 GiB".into()))?,
                );
                self.encode_body(&mut body, record, point, context)?;
                match record {
                    Record::Address(address) => self.run.start(address, context, point),
                    _ => self.run.open = false,
                }
            }
        }
        self.block.extend_from_slice(&body);
        self.block_slots += 1;
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
            Record::Poi(record) => osm_source(&record.source),
            Record::Postcode(record) => (SourceKind::Other, record.source.record_count),
        };
        let precision = match record {
            Record::Address(address) => Some(address.location_precision),
            Record::Poi(poi) => Some(poi.location_precision),
            _ => None,
        };
        let centroid = precision == Some(LocationPrecision::Centroid);
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
            Record::Address(record) if record.source.object_type == ObjectType::Row => {
                put_u64(out, u64::from(self.dataset_id(&record.source.dataset)?));
            }
            Record::Poi(record) if record.source.object_type == ObjectType::Row => {
                let gers = record
                    .source
                    .gers()
                    .ok_or(Error::Decode("imported POI has no GERS ID"))?;
                put_u64(out, u64::from(self.dataset_id(&record.source.dataset)?));
                put_u64(out, u64::from(self.strings.id(gers)?));
            }
            _ => {}
        }

        match record {
            Record::Address(record) => {
                let present_at = self.put_address(out, &record.address)?;
                self.put_strings(out, &record.street_postcodes)?;
                let name = record
                    .building
                    .as_ref()
                    .and_then(|building| building.name.as_deref());
                let mut details = 0;
                if !record.access.is_empty() {
                    details |= DETAIL_ACCESS;
                }
                if record.building.is_some() {
                    details |= DETAIL_BUILDING;
                }
                if name.is_some() {
                    details |= DETAIL_BUILDING_NAME;
                }
                if record.address.block_number_len.is_some() {
                    details |= DETAIL_BLOCK_NUMBER;
                }
                if details != 0 {
                    out[present_at] |= ADDRESS_DETAILS;
                    out.push(details);
                    if let Some(name) = name {
                        put_u64(out, u64::from(self.strings.id(name)?));
                    }
                    if let Some(length) = record.address.block_number_len {
                        put_u64(out, u64::from(length.get()));
                    }
                    if !record.access.is_empty() {
                        self.put_access(out, point, &record.access)?;
                    }
                }
            }
            Record::Poi(record) => {
                put_u64(out, u64::from(self.strings.id(&record.name)?));
                put_u64(out, u64::from(self.strings.id(&record.category)?));
                self.put_strings(out, &record.alt_names)?;
                out.push(record.prominence.to_byte());
                out.push(record.prominence.importance);
                put_u64(out, record.wikidata.map_or(0, u64::from));
                put_aliases(out, &record.aliases)?;
                match &record.brand {
                    Some(brand) => {
                        put_u64(out, u64::from(self.strings.id(&brand.name)?) + 1);
                        put_u64(out, brand.wikidata.map_or(0, u64::from));
                    }
                    None => put_u64(out, 0),
                }
                let contact = record.contact();
                let details_at = out.len();
                self.put_optional_strings(
                    out,
                    &[
                        contact.and_then(|contact| contact.phone.as_deref()),
                        contact.and_then(|contact| contact.website.as_deref()),
                        contact.and_then(|contact| contact.email.as_deref()),
                        record.opening_hours(),
                    ],
                )?;
                if let Some(extent) = record.extent() {
                    out[details_at] |= POI_EXTENT;
                    for (side, at) in extent.into_iter().zip([point.0, point.1, point.0, point.1]) {
                        put_i64(out, i64::from(side) - i64::from(at));
                    }
                }
                if !record.access().is_empty() {
                    out[details_at] |= POI_ACCESS;
                    self.put_access(out, point, record.access())?;
                }
                if !record.cuisine().is_empty() {
                    out[details_at] |= POI_CUISINE;
                    self.put_strings(out, record.cuisine())?;
                }
                if let Some(fax) = contact.and_then(|contact| contact.fax.as_deref()) {
                    out[details_at] |= POI_FAX;
                    put_u64(out, u64::from(self.strings.id(fax)?));
                }
                match &record.address {
                    Some(address) => {
                        out.push(1);
                        let present_at = self.put_address(out, address)?;
                        if let Some(length) = address.block_number_len {
                            out[present_at] |= ADDRESS_DETAILS;
                            put_u64(out, u64::from(length.get()));
                        }
                    }
                    None => out.push(0),
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
                self.put_strings(out, &record.alt_names)?;
                self.put_optional_strings(out, &[record.locality.as_deref()])?;
                out.push(record.road_class.to_byte());
                put_other_ways(out, record.source.object_id, &record.other_ways)?;
            }
            Record::Postcode(record) => {
                if record.source.derived_from != DERIVED_FROM_ADDRESS_RECORDS {
                    return Err(Error::Inconsistent(format!(
                        "unsupported postcode source {}",
                        record.source.derived_from
                    )));
                }
                put_u64(out, u64::from(self.strings.id(&record.postcode)?));
                put_u64(
                    out,
                    record.country.map_or(0, |country| country.to_u16().into()),
                );
            }
            Record::Place(_, record) => {
                put_u64(out, u64::from(self.strings.id(&record.name)?));
                put_u64(out, u64::from(self.strings.id(&record.place_type)?));
                self.put_strings(out, &record.alt_names)?;
                out.push(record.prominence.to_byte());
                out.push(record.prominence.importance);
                put_u64(out, record.wikidata.map_or(0, u64::from));
            }
        }

        if let Some(parts) = line {
            put_u64(out, parts.len() as u64);
            let mut previous = point;
            for part in parts {
                put_u64(out, part.len() as u64);
                for position in part {
                    put_i64(out, i64::from(position.0) - i64::from(previous.0));
                    put_i64(out, i64::from(position.1) - i64::from(previous.1));
                    previous = position;
                }
            }
        }
        Ok(())
    }

    fn dataset_id(&mut self, dataset: &str) -> Result<u32> {
        let index = match self.datasets.iter().position(|known| known == dataset) {
            Some(index) => index,
            None => {
                self.datasets.push(dataset.to_string());
                self.datasets.len() - 1
            }
        };
        u32::try_from(index).map_err(|_| Error::Inconsistent("too many imported datasets".into()))
    }

    fn encode_continuation(
        &mut self,
        out: &mut Vec<u8>,
        record: &AddressRecord,
        point: (i32, i32),
    ) -> Result<()> {
        let run = &mut self.run;
        put_i64(out, i64::from(point.0) - i64::from(run.point.0));
        put_i64(out, i64::from(point.1) - i64::from(run.point.1));
        put_i64(out, record.source.object_id.wrapping_sub(run.object_id));
        let address = &record.address;
        let unit = if address.unit.is_some() {
            NUMBER_UNIT
        } else {
            0
        };
        let number = match plain_number(&address.number) {
            Some(number) => {
                let delta = zigzag(i64::from(number) - i64::from(run.number));
                run.number = number;
                delta << NUMBER_SHIFT
            }
            None => u64::from(self.strings.id(&address.number)?) << NUMBER_SHIFT | NUMBER_TEXT,
        };
        put_u64(out, number | unit);
        if let Some(unit) = &address.unit {
            put_u64(out, u64::from(self.strings.id(unit)?));
        }
        run.point = point;
        run.object_id = record.source.object_id;
        Ok(())
    }

    fn put_address(&mut self, out: &mut Vec<u8>, address: &AddressComponents) -> Result<usize> {
        put_u64(out, u64::from(self.strings.id(&address.number)?));
        let present_at = out.len();
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
        self.put_strings(out, &address.areas)?;
        Ok(present_at)
    }

    fn put_access(
        &mut self,
        out: &mut Vec<u8>,
        point: (i32, i32),
        access: &[Entrance],
    ) -> Result<()> {
        put_u64(out, access.len() as u64);
        for entrance in access {
            put_i64(out, i64::from(entrance.at[0]) - i64::from(point.0));
            put_i64(out, i64::from(entrance.at[1]) - i64::from(point.1));
            put_u64(out, u64::from(self.strings.id(&entrance.kind)?));
        }
        Ok(())
    }

    fn put_strings(&mut self, out: &mut Vec<u8>, values: &[String]) -> Result<()> {
        put_u64(out, values.len() as u64);
        for value in values {
            put_u64(out, u64::from(self.strings.id(value)?));
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
        if self.block_slots == 0 {
            return Ok(());
        }
        self.block_offsets
            .write_all(&self.blocks_len.to_le_bytes())?;
        let width = if self.block.len() <= usize::from(u16::MAX) {
            2
        } else {
            4
        };
        let bitmaps = [
            (BLOCK_CENTROIDS, self.centroids),
            (BLOCK_WAYS, self.ways),
            (BLOCK_RELATIONS, self.relations),
            (BLOCK_DISPUTED, self.disputed),
        ];
        let flags = bitmaps
            .iter()
            .filter(|(_, bitmap)| *bitmap != 0)
            .fold(0, |flags, (flag, _)| flags | flag);
        let mut header = Vec::with_capacity(BLOCK_HEADER_BYTES + 24);
        header.extend_from_slice(&self.block_base.0.to_le_bytes());
        header.extend_from_slice(&self.block_base.1.to_le_bytes());
        header.push(width as u8);
        header.push(flags);
        header.extend_from_slice(&self.continued.to_le_bytes());
        for (_, bitmap) in bitmaps.iter().filter(|(_, bitmap)| *bitmap != 0) {
            header.extend_from_slice(&bitmap.to_le_bytes());
        }
        for start in &self.block_starts {
            header.extend_from_slice(&start.to_le_bytes()[..width]);
        }
        self.blocks.write_all(&header)?;
        self.blocks.write_all(&self.block)?;
        self.blocks_len += (header.len() + self.block.len()) as u64;
        self.block.clear();
        self.block_starts.clear();
        self.block_slots = 0;
        self.continued = 0;
        self.centroids = 0;
        self.ways = 0;
        self.relations = 0;
        self.disputed = 0;
        Ok(())
    }

    pub fn finish(mut self, pack: &mut container::Writer) -> Result<()> {
        self.flush_block()?;
        self.blocks.flush()?;
        self.block_offsets.flush()?;
        self.strings.bytes.flush()?;
        self.strings.ends.flush()?;

        let version = if self
            .datasets
            .iter()
            .any(|name| name.starts_with(crate::licences::OVERTURE_PREFIX))
        {
            OVERTURE_RECORDS_VERSION
        } else {
            RECORDS_VERSION
        };
        pack.begin(SECTION_INDEX, version)?;
        pack.write_all(&self.record_count.to_le_bytes())?;
        std::io::copy(&mut File::open(self.block_offsets.get_ref().path())?, pack)?;
        pack.write_all(&self.blocks_len.to_le_bytes())?;
        pack.end()?;

        pack.add_file(SECTION_BLOCKS, version, self.blocks.get_ref().path())?;

        let mut datasets = Vec::new();
        put_u64(&mut datasets, self.datasets.len() as u64);
        for dataset in &self.datasets {
            put_str(&mut datasets, dataset);
            match crate::licences::of(dataset) {
                Some(licence) => {
                    datasets.push(1);
                    put_str(&mut datasets, &licence.name);
                    put_opt_str(&mut datasets, licence.url.as_deref());
                    put_opt_str(&mut datasets, licence.attribution.as_deref());
                }
                None => datasets.push(0),
            }
        }
        pack.add(SECTION_DATASETS, version, &datasets)?;

        let strings = &self.strings;
        pack.begin(SECTION_STRINGS, version)?;
        pack.write_all(&(strings.segments.len() as u64).to_le_bytes())?;
        for (first_record, first_string) in &strings.segments {
            pack.write_all(&first_record.to_le_bytes())?;
            pack.write_all(&first_string.to_le_bytes())?;
        }
        pack.write_all(&strings.count.to_le_bytes())?;
        std::io::copy(&mut File::open(strings.ends.get_ref().path())?, pack)?;
        std::io::copy(&mut File::open(strings.bytes.get_ref().path())?, pack)?;
        pack.end()
    }
}

#[derive(Debug, Clone)]
pub struct Reader {
    record_count: u64,
    index: Bytes,
    blocks: Bytes,
    strings: Bytes,
    segment_count: u64,
    string_count: u64,
    ends_start: usize,
    text_start: usize,
    datasets: Arc<Vec<String>>,
    licences: Arc<Vec<Option<Licence>>>,
}

impl Reader {
    pub fn open(container: &Container) -> Result<Self> {
        let version = match container
            .sections()
            .get(SECTION_BLOCKS)
            .map(|section| section.version)
        {
            Some(version @ (PREVIOUS_RECORDS_VERSION | OVERTURE_RECORDS_VERSION)) => version,
            _ => RECORDS_VERSION,
        };
        let index = container.section(SECTION_INDEX, version)?;
        let blocks = container.section(SECTION_BLOCKS, version)?;
        let strings = container.section(SECTION_STRINGS, version)?;
        let datasets = {
            let bytes = container.section(SECTION_DATASETS, version)?;
            let mut input = bytes.as_slice();
            let count = get_u64(&mut input)?;
            if count > input.len() as u64 {
                return Err(Error::Decode("dataset table is truncated"));
            }
            let mut datasets = Vec::new();
            for _ in 0..count {
                let name = get_str(&mut input)?.to_string();
                let licence = match get_u8(&mut input)? {
                    0 => None,
                    _ => Some(Licence {
                        name: get_str(&mut input)?.to_string(),
                        url: get_opt_string(&mut input)?,
                        attribution: get_opt_string(&mut input)?,
                    }),
                };
                datasets.push((name, licence));
            }
            datasets
        };
        let (datasets, licences): (Vec<_>, Vec<_>) = datasets.into_iter().unzip();

        let record_count =
            read_u64_le(&index, 0).ok_or(Error::Decode("records index is truncated"))?;
        let block_count = record_count.div_ceil(BLOCK_RECORDS);
        if index.len() as u64 != 8 + (block_count + 1) * 8 {
            return Err(Error::Inconsistent(
                "records index does not match its record count".into(),
            ));
        }
        if read_u64_le(&index, index.len() - 8) != Some(blocks.len() as u64) {
            return Err(Error::Inconsistent(
                "records index does not match the blocks section".into(),
            ));
        }
        let truncated = || Error::Decode("string table is truncated");
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
                return Err(Error::Inconsistent(
                    "string table segments do not match the records".into(),
                ));
            }
            previous = Some((first_record, first_string));
        }
        if (segment_count == 0) != (record_count == 0) {
            return Err(Error::Inconsistent(
                "string table segments do not match the records".into(),
            ));
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
            return Err(Error::Inconsistent(
                "string table does not match its offsets".into(),
            ));
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
            datasets: Arc::new(datasets),
            licences: Arc::new(licences),
        })
    }

    pub fn record_count(&self) -> u64 {
        self.record_count
    }

    pub fn header(&self, id: RecordId) -> Result<Header> {
        Ok(self.body(id)?.header)
    }

    pub fn record(&self, id: RecordId) -> Result<Record> {
        let entry = self.body(id)?;
        let (header, mut fields) = (entry.header, entry.fields);
        let geometry = |fields: &mut &[u8]| self.geometry(&header, fields);
        let location_precision = if header.centroid {
            LocationPrecision::Centroid
        } else {
            LocationPrecision::Point
        };
        Ok(match header.layer {
            Layer::Address => {
                let (mut address, flagged) = self.entry_address(&entry, &mut fields)?;
                let street_postcodes = self.string_list(header.strings, &mut fields)?;
                let (building, access) =
                    self.entry_details(&entry, flagged, &mut fields, &mut address)?;
                Record::Address(AddressRecord {
                    building,
                    address,
                    street_postcodes,
                    access,
                    geometry: geometry(&mut fields)?,
                    location_precision,
                    source: header.provenance(&self.datasets)?,
                    disputed: header.disputed,
                })
            }
            Layer::Poi => {
                let poi = self.poi_fields(&header, &mut fields)?;
                let mut source = header.provenance(&self.datasets)?;
                if let Some(gers) = header.gers {
                    source.set_gers(self.string(header.strings, gers)?.to_owned());
                }
                Record::Poi(PoiRecord {
                    name: poi.name.to_string(),
                    alt_names: poi.alt_names,
                    category: poi.category.to_string(),
                    address: poi.address,
                    geometry: geometry(&mut fields)?,
                    location_precision,
                    source,
                    prominence: poi.prominence,
                    wikidata: poi.wikidata,
                    brand: poi.brand,
                    details: poi.details,
                    aliases: poi.aliases,
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
                    source: header.provenance(&self.datasets)?,
                })
            }
            Layer::Street => Record::Street(StreetRecord {
                name: self.string_field(header.strings, &mut fields)?.to_string(),
                alt_names: self.string_list(header.strings, &mut fields)?,
                locality: {
                    let [locality] = self.optional_strings(header.strings, &mut fields)?;
                    locality
                },
                road_class: road_class_field(&mut fields)?,
                other_ways: other_ways_field(&mut fields, header.object()?.1)?,
                geometry: geometry(&mut fields)?,
                representative_point: [header.lon(), header.lat()],
                source: header.provenance(&self.datasets)?,
            }),
            Layer::Postcode => Record::Postcode(PostcodeRecord {
                postcode: self.string_field(header.strings, &mut fields)?.to_string(),
                country: country_field(&mut fields)?,
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
                        alt_names: self.string_list(header.strings, &mut fields)?,
                        prominence: Prominence::from_byte(
                            get_u8(&mut fields)?,
                            get_u8(&mut fields)?,
                        ),
                        wikidata: item_field(&mut fields)?,
                        geometry: geometry(&mut fields)?,
                        source: header.provenance(&self.datasets)?,
                    },
                )
            }
        })
    }

    pub fn licence(&self, dataset: &str) -> Option<Licence> {
        if dataset == OSM_DATASET {
            return Some(crate::licences::osm());
        }
        let index = self.datasets.iter().position(|known| known == dataset)?;
        self.licences.get(index).cloned().flatten()
    }

    pub fn summary(&self, id: RecordId) -> Result<Summary> {
        let entry = self.body(id)?;
        let (header, mut fields) = (entry.header, entry.fields);
        let source = header.source(&self.datasets)?;
        let mut category = None;
        let mut road_class = None;
        let mut brand = None;
        let mut contact = None;
        let mut opening_hours = None;
        let mut cuisine = Vec::new();
        let mut extent = None;
        let mut access = Vec::new();
        let mut building = None;
        let mut stated = Address::default();
        let mut street_postcode = None;
        let (record_id, name, label) = match header.layer {
            Layer::Address => {
                let (mut address, flagged) = self.entry_address(&entry, &mut fields)?;
                street_postcode = agreed_postcode(&self.string_list(header.strings, &mut fields)?);
                (building, access) =
                    self.entry_details(&entry, flagged, &mut fields, &mut address)?;
                stated = Address::from(&address);
                (
                    header.record_id(&self.datasets)?,
                    crate::labels::address_name(&address),
                    crate::labels::address_label(&address),
                )
            }
            Layer::Poi => {
                let poi = self.poi_fields(&header, &mut fields)?;
                brand.clone_from(&poi.brand);
                if let Some(details) = poi.details {
                    (contact, opening_hours) = (details.contact, details.opening_hours);
                    cuisine = details.cuisine;
                    extent = details.extent;
                    access = details.access;
                }
                category = Some(poi.category.to_string());
                if let Some(address) = &poi.address {
                    stated = Address::from(address);
                }
                (
                    match header.gers {
                        Some(gers) => {
                            format!("overture:places:{}", self.string(header.strings, gers)?)
                        }
                        None => header.record_id(&self.datasets)?,
                    },
                    poi.name.to_string(),
                    crate::labels::poi_label(poi.name, poi.address.as_ref(), None),
                )
            }
            Layer::Interpolation => {
                let (address, range, anchors) =
                    self.interpolation_fields(header.strings, &mut fields)?;
                let (_, way_id) = header.object()?;
                let name = crate::labels::interpolation_name(&address);
                stated = Address::from(&address);
                (
                    crate::labels::interpolation_record_id(way_id, anchors[0], anchors[1]),
                    name.clone(),
                    crate::labels::interpolation_label(&name, &range, &address),
                )
            }
            Layer::Street => {
                let (object_type, object_id) = header.object()?;
                let name = self.string_field(header.strings, &mut fields)?.to_string();
                self.string_list(header.strings, &mut fields)?;
                let [locality] = self.optional_strings(header.strings, &mut fields)?;
                road_class = Some(road_class_field(&mut fields)?);
                other_ways_field(&mut fields, object_id)?;
                if header.line {
                    let parts = self.line_points(&header, &mut fields)?;
                    extent = narrowest_box(
                        parts
                            .iter()
                            .filter_map(|part| box_around(part.iter().copied())),
                    );
                }
                (
                    crate::labels::osm_record_id(object_type, object_id),
                    name.clone(),
                    crate::labels::street_label(&name, locality.as_deref()),
                )
            }
            Layer::Postcode => {
                let postcode = self.string_field(header.strings, &mut fields)?;
                stated = Address::postcode(postcode);
                (
                    crate::labels::derived_postcode_id(country_field(&mut fields)?, postcode),
                    postcode.to_string(),
                    postcode.to_string(),
                )
            }
            _ => {
                let name = self.string_field(header.strings, &mut fields)?;
                let place_type = self.string_field(header.strings, &mut fields)?;
                let (object_type, object_id) = header.object()?;
                (
                    crate::labels::place_record_id(object_type, object_id, place_type),
                    name.to_string(),
                    name.to_string(),
                )
            }
        };
        Ok(Summary {
            id: record_id,
            layer: header.layer.as_str().to_string(),
            name,
            label,
            address: stated,
            category,
            road_class,
            brand,
            contact,
            opening_hours,
            cuisine,
            bbox: extent.map(bbox),
            timezone: None,
            street_postcode,
            access: access.iter().map(AccessPoint::from).collect(),
            building,
            point: Some(header.point()),
            source,
        })
    }

    pub fn postcode(&self, id: RecordId) -> Result<Option<String>> {
        let entry = self.body(id)?;
        let (header, mut fields) = (entry.header, entry.fields);
        Ok(match header.layer {
            Layer::Address => {
                self.address_components(header.strings, &mut fields)?
                    .postcode
            }
            Layer::Poi => self
                .poi_fields(&header, &mut fields)?
                .address
                .and_then(|address| address.postcode),
            Layer::Interpolation => {
                self.interpolation_fields(header.strings, &mut fields)?
                    .0
                    .postcode
            }
            Layer::Postcode => Some(self.string_field(header.strings, &mut fields)?.to_string()),
            _ => None,
        })
    }

    pub fn prominence(&self, id: RecordId) -> Result<Option<Prominence>> {
        let entry = self.body(id)?;
        let (header, mut fields) = (entry.header, entry.fields);
        match header.layer {
            Layer::Poi => Ok(Some(self.poi_fields(&header, &mut fields)?.prominence)),
            layer if layer.place_layer().is_some() => {
                Ok(Some(self.place_prominence(header.strings, &mut fields)?))
            }
            _ => Ok(None),
        }
    }

    pub fn wikidata(&self, id: RecordId) -> Result<Option<u32>> {
        let entry = self.body(id)?;
        let (header, mut fields) = (entry.header, entry.fields);
        match header.layer {
            Layer::Poi => Ok(self.poi_fields(&header, &mut fields)?.wikidata),
            layer if layer.place_layer().is_some() => {
                self.place_prominence(header.strings, &mut fields)?;
                item_field(&mut fields)
            }
            _ => Ok(None),
        }
    }

    fn place_prominence(&self, strings: StringSegment, fields: &mut &[u8]) -> Result<Prominence> {
        get_u32(fields)?;
        get_u32(fields)?;
        self.string_list(strings, fields)?;
        Ok(Prominence::from_byte(get_u8(fields)?, get_u8(fields)?))
    }

    pub fn poi_category_is(&self, id: RecordId, accept: impl Fn(&str) -> bool) -> Result<bool> {
        let entry = self.body(id)?;
        let (header, mut fields) = (entry.header, entry.fields);
        if header.layer != Layer::Poi {
            return Ok(false);
        }
        self.string_field(header.strings, &mut fields)?;
        Ok(accept(self.string_field(header.strings, &mut fields)?))
    }

    pub fn place_type(&self, id: RecordId) -> Result<Option<String>> {
        let entry = self.body(id)?;
        let (header, mut fields) = (entry.header, entry.fields);
        if header.layer.place_layer().is_none() {
            return Ok(None);
        }
        self.string_field(header.strings, &mut fields)?;
        Ok(Some(
            self.string_field(header.strings, &mut fields)?.to_string(),
        ))
    }

    pub fn postcodes(&self, id: RecordId) -> Result<Vec<String>> {
        let entry = self.body(id)?;
        if entry.header.layer != Layer::Address {
            return Ok(self.postcode(id)?.into_iter().collect());
        }
        let mut fields = entry.fields;
        let address = self.address_components(entry.header.strings, &mut fields)?;
        match address.postcode {
            Some(postcode) => Ok(vec![postcode]),
            None => self.string_list(entry.header.strings, &mut fields),
        }
    }

    pub fn names(&self, id: RecordId) -> Result<Vec<String>> {
        let entry = self.body(id)?;
        let (header, mut fields) = (entry.header, entry.fields);
        let strings = header.strings;
        let (name, alt_names) = match header.layer {
            Layer::Address | Layer::Interpolation | Layer::Postcode => return Ok(Vec::new()),
            Layer::Poi => {
                let poi = self.poi_fields(&header, &mut fields)?;
                (poi.name, poi.alt_names)
            }
            Layer::Street => {
                let name = self.string_field(strings, &mut fields)?;
                (name, self.string_list(strings, &mut fields)?)
            }
            _ => {
                let name = self.string_field(strings, &mut fields)?;
                get_u32(&mut fields)?;
                (name, self.string_list(strings, &mut fields)?)
            }
        };
        Ok(std::iter::once(name.to_string()).chain(alt_names).collect())
    }

    pub fn context_record(&self, id: RecordId) -> Result<Option<ContextRecord>> {
        let header = self.header(id)?;
        if !header.layer.is_context() {
            return Ok(None);
        }
        let summary = self.summary(id)?;
        Ok(Some(ContextRecord {
            id: summary.id,
            layer: summary.layer,
            name: summary.name,
            postcode: (header.layer == Layer::Postcode).then(|| summary.label.clone()),
            label: summary.label,
            point: summary.point,
        }))
    }

    /// Line geometry as 1e-7 degree coordinates, `None` for point records.
    /// The lines of a line record, one per part, `None` for a point record.
    pub fn line(&self, id: RecordId) -> Result<Option<Vec<Vec<[i32; 2]>>>> {
        let entry = self.body(id)?;
        let (header, mut fields) = (entry.header, entry.fields);
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
            .ok_or_else(|| Error::BadGeometry("record JSON must be an object".into()))?;
        object.insert("layer".to_string(), json!(record.layer().as_str()));
        Ok(value)
    }

    fn segment(&self, record_id: RecordId) -> StringSegment {
        let field = |segment: u64, offset: u64| {
            read_u64_le(&self.strings, (8 + segment * 16 + offset) as usize)
                .expect("validated segments")
        };
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
            return Err(Error::IdOutOfRange {
                what: "string",
                id: u64::from(local),
            });
        }
        let end_of = |id: u64| {
            read_u64_le(&self.strings, self.ends_start + id as usize * 8).expect("validated ends")
        };
        let start = if id == 0 { 0 } else { end_of(id - 1) };
        let bytes = self
            .strings
            .get(self.text_start + start as usize..self.text_start + end_of(id) as usize)
            .ok_or(Error::Decode("string offsets are out of range"))?;
        std::str::from_utf8(bytes)
            .map_err(|_| Error::Decode("string table entry is not valid UTF-8"))
    }

    /// Blocks in the store; block `b` holds records `b * 64` up to 64 more.
    pub fn block_count(&self) -> u64 {
        self.record_count.div_ceil(BLOCK_RECORDS)
    }

    fn block(&self, block: u64) -> Result<Block<'_>> {
        let start = read_u64_le(&self.index, (8 + block * 8) as usize).expect("validated index");
        let end = read_u64_le(&self.index, (16 + block * 8) as usize).expect("validated index");
        let bytes = self
            .blocks
            .get(start as usize..end as usize)
            .ok_or(Error::Decode("record block is out of range"))?;
        let truncated = || Error::Decode("record block is truncated");
        let count = (self.record_count - block * BLOCK_RECORDS).min(BLOCK_RECORDS) as usize;
        let header = bytes.get(..BLOCK_HEADER_BYTES).ok_or_else(truncated)?;
        let width = usize::from(header[8]);
        if width != 2 && width != 4 {
            return Err(Error::Decode("record block has an unknown offset width"));
        }
        let flags = header[9];
        let continued = read_u64_le(header, 10).expect("block header");
        let mut at = BLOCK_HEADER_BYTES;
        let mut bitmap = |flag: u8| -> Result<u64> {
            if flags & flag == 0 {
                return Ok(0);
            }
            let value = read_u64_le(bytes, at).ok_or_else(truncated)?;
            at += 8;
            Ok(value)
        };
        let centroids = bitmap(BLOCK_CENTROIDS)?;
        let ways = bitmap(BLOCK_WAYS)?;
        let relations = bitmap(BLOCK_RELATIONS)?;
        let disputed = bitmap(BLOCK_DISPUTED)?;
        let mask = low_slots(count);
        if continued & !mask != 0 || continued & 1 != 0 {
            return Err(Error::Decode("record block marks a slot it does not have"));
        }
        let plain = count - continued.count_ones() as usize;
        let bodies_start = at + plain * width;
        if bytes.len() < bodies_start {
            return Err(truncated());
        }
        Ok(Block {
            first: block * BLOCK_RECORDS,
            base: (
                i32::from_le_bytes(bytes[0..4].try_into().expect("block header")),
                i32::from_le_bytes(bytes[4..8].try_into().expect("block header")),
            ),
            width,
            count,
            continued,
            centroids,
            ways,
            relations,
            disputed,
            starts: &bytes[at..bodies_start],
            bodies: &bytes[bodies_start..],
        })
    }

    pub fn block_ids(&self, block: u64) -> Result<Vec<CatalogueId>> {
        let found = self.block(block)?;
        let strings = self.segment(found.first);
        let mut ids = Vec::new();
        for slot in 0..found.count {
            if found.is_continued(slot) {
                continue;
            }
            let (header, mut fields) = plain_header(found.base, strings, found.plain_body(slot)?)?;
            let street = header.layer == Layer::Street;
            let place = header.layer == Layer::Poi || header.layer.place_layer().is_some();
            if !(place || street)
                || (header.source_kind == SourceKind::Other && header.gers.is_none())
            {
                continue;
            }
            let record_id = found.first + slot as u64;
            let (object_type, object_id) = header.object()?;
            let id = |object_type, object_id| CatalogueId {
                record_id,
                object_type,
                object_id,
                street,
            };
            ids.push(id(object_type, object_id));
            if header.layer == Layer::Poi {
                get_u32(&mut fields)?;
                get_u32(&mut fields)?;
                for _ in 0..get_u64(&mut fields)? {
                    get_u32(&mut fields)?;
                }
                get_u8(&mut fields)?;
                get_u8(&mut fields)?;
                item_field(&mut fields)?;
                ids.extend(
                    aliases_field(&mut fields)?
                        .into_iter()
                        .map(|(kind, alias)| id(kind, alias)),
                );
            } else if street {
                get_u32(&mut fields)?;
                for _ in 0..get_u64(&mut fields)? {
                    get_u32(&mut fields)?;
                }
                self.optional_strings::<1>(header.strings, &mut fields)?;
                road_class_field(&mut fields)?;
                ids.extend(
                    other_ways_field(&mut fields, object_id)?
                        .into_iter()
                        .map(|way| id(ObjectType::Way, way)),
                );
            }
        }
        Ok(ids)
    }

    fn locate(&self, id: RecordId) -> Result<(Block<'_>, usize)> {
        if id >= self.record_count {
            return Err(Error::IdOutOfRange { what: "record", id });
        }
        Ok((
            self.block(id / BLOCK_RECORDS)?,
            (id % BLOCK_RECORDS) as usize,
        ))
    }

    pub fn point(&self, id: RecordId) -> Result<(Layer, f64, f64)> {
        let (block, slot) = self.locate(id)?;
        let (layer, (lon, lat)) = if block.is_continued(slot) {
            let anchor = block.anchor(slot)?;
            let anchor_body = block.plain_body(anchor)?;
            let mut point = plain_point(block.base, anchor_body)?;
            let mut cursor = skip_address_body(anchor_body)?;
            for _ in anchor + 1..=slot {
                let continued = read_continuation(&mut cursor)?;
                point = continued.point(point)?;
            }
            (Layer::Address, point)
        } else {
            let body = block.plain_body(slot)?;
            (
                layer_of(*body.first().ok_or(Error::Decode("record body is empty"))?)?,
                plain_point(block.base, body)?,
            )
        };
        Ok((layer, dequantize(lon), dequantize(lat)))
    }

    pub fn block_points(
        &self,
        block: u64,
        mut visit: impl FnMut(RecordId, Layer, i32, i32, bool) -> Result<()>,
    ) -> Result<()> {
        let block = self.block(block)?;
        let mut previous = block.base;
        let mut cursor: &[u8] = &[];
        for slot in 0..block.count {
            let (layer, point, line) = if block.is_continued(slot) {
                let continued = read_continuation(&mut cursor)?;
                (Layer::Address, continued.point(previous)?, false)
            } else {
                let body = block.plain_body(slot)?;
                let tag = *body.first().ok_or(Error::Decode("record body is empty"))?;
                let layer = layer_of(tag)?;
                if layer == Layer::Address && block.is_continued(slot + 1) {
                    cursor = skip_address_body(body)?;
                }
                (layer, plain_point(block.base, body)?, tag & TAG_LINE != 0)
            };
            previous = point;
            visit(block.first + slot as u64, layer, point.0, point.1, line)?;
        }
        Ok(())
    }

    pub fn address_numbers(
        &self,
        first: RecordId,
        count: u64,
        mut visit: impl FnMut(RecordId, &str, Option<&str>) -> Result<()>,
    ) -> Result<()> {
        let end = first.saturating_add(count).min(self.record_count);
        let mut id = first;
        let mut number = String::new();
        while id < end {
            let (block, start) = self.locate(id)?;
            let strings = self.segment(id);
            let stop = (end - block.first).min(block.count as u64) as usize;
            let begin = if block.is_continued(start) {
                block.anchor(start)?
            } else {
                start
            };
            let mut plain = 0;
            let mut cursor: &[u8] = &[];
            for slot in begin..stop {
                let unit;
                if block.is_continued(slot) {
                    let continued = read_continuation(&mut cursor)?;
                    unit = continued.unit;
                    number.clear();
                    match continued.number(plain)? {
                        ContinuedNumber::Text(id) => number.push_str(self.string(strings, id)?),
                        ContinuedNumber::Plain(value) => {
                            plain = value;
                            number.push_str(&value.to_string());
                        }
                    }
                } else {
                    let body = block.plain_body(slot)?;
                    let (header, mut fields) = plain_header(block.base, strings, body)?;
                    if header.layer != Layer::Address {
                        continue;
                    }
                    let text = self.string_field(strings, &mut fields)?;
                    plain = plain_number(text).unwrap_or(0);
                    number.clear();
                    number.push_str(text);
                    unit = unit_id(&mut fields)?;
                    if block.is_continued(slot + 1) {
                        cursor = skip_address_body(body)?;
                    }
                }
                if slot >= start {
                    let unit = unit.map(|id| self.string(strings, id)).transpose()?;
                    visit(block.first + slot as u64, &number, unit)?;
                }
            }
            id = block.first + stop as u64;
        }
        Ok(())
    }

    fn body(&self, id: RecordId) -> Result<Entry<'_>> {
        let (block, slot) = self.locate(id)?;
        let strings = self.segment(id);
        let disputed = block.disputed & 1 << slot != 0;
        if !block.is_continued(slot) {
            let (mut header, fields) = plain_header(block.base, strings, block.plain_body(slot)?)?;
            header.disputed = disputed;
            return Ok(Entry {
                header,
                fields,
                continued: None,
            });
        }

        let anchor = block.anchor(slot)?;
        let anchor_body = block.plain_body(anchor)?;
        let (anchor_header, fields) = plain_header(block.base, strings, anchor_body)?;
        if anchor_header.layer != Layer::Address {
            return Err(Error::Decode(
                "continued address follows a record of another layer",
            ));
        }
        let mut point = (anchor_header.lon_e7, anchor_header.lat_e7);
        let mut object_id = unzigzag(anchor_header.source_value);
        let mut plain = {
            let mut number = fields;
            plain_number(self.string_field(strings, &mut number)?).unwrap_or(0)
        };
        let mut cursor = skip_address_body(anchor_body)?;
        let mut last = None;
        for _ in anchor + 1..=slot {
            let continued = read_continuation(&mut cursor)?;
            point = continued.point(point)?;
            object_id = object_id.wrapping_add(continued.object_delta);
            let number = continued.number(plain)?;
            if let ContinuedNumber::Plain(value) = number {
                plain = value;
            }
            last = Some((number, continued.unit));
        }
        let (number, unit) = last.expect("a continued record follows its anchor");
        Ok(Entry {
            header: Header {
                layer: Layer::Address,
                strings,
                lon_e7: point.0,
                lat_e7: point.1,
                context: anchor_header.context,
                disputed,
                centroid: block.centroids & 1 << slot != 0,
                line: false,
                source_kind: match anchor_header.dataset {
                    Some(_) => SourceKind::Other,
                    None => block.source_kind(slot),
                },
                source_value: zigzag(object_id),
                dataset: anchor_header.dataset,
                gers: None,
            },
            fields,
            continued: Some(Continued { number, unit }),
        })
    }

    fn string_field(&self, strings: StringSegment, fields: &mut &[u8]) -> Result<&str> {
        self.string(strings, get_u32(fields)?)
    }

    fn string_list(&self, strings: StringSegment, fields: &mut &[u8]) -> Result<Vec<String>> {
        let count = get_u64(fields)?;
        if count > fields.len() as u64 {
            return Err(Error::Decode("string list is truncated"));
        }
        (0..count)
            .map(|_| Ok(self.string_field(strings, fields)?.to_string()))
            .collect()
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

    fn entry_address(
        &self,
        entry: &Entry<'_>,
        fields: &mut &[u8],
    ) -> Result<(AddressComponents, bool)> {
        let strings = entry.header.strings;
        let (mut address, flagged) = self.address_fields(strings, fields)?;
        if let Some(continued) = &entry.continued {
            address.number = match continued.number {
                ContinuedNumber::Plain(number) => number.to_string(),
                ContinuedNumber::Text(id) => self.string(strings, id)?.to_string(),
            };
            address.unit = continued
                .unit
                .map(|id| self.string(strings, id).map(str::to_string))
                .transpose()?;
        }
        Ok((address, flagged))
    }

    fn entry_details(
        &self,
        entry: &Entry<'_>,
        flagged: bool,
        fields: &mut &[u8],
        address: &mut AddressComponents,
    ) -> Result<(Option<Building>, Vec<Entrance>)> {
        if !flagged {
            return Ok((None, Vec::new()));
        }
        let (building, has_access, block_number_len) =
            self.details_field(entry.header.strings, fields)?;
        address.block_number_len = block_number_len;
        let access = if has_access && entry.continued.is_none() {
            self.access_field(&entry.header, fields)?
        } else {
            Vec::new()
        };
        Ok((building, access))
    }

    fn details_field(
        &self,
        strings: StringSegment,
        fields: &mut &[u8],
    ) -> Result<(Option<Building>, bool, Option<std::num::NonZeroU16>)> {
        let details = get_u8(fields)?;
        let building = if details & DETAIL_BUILDING == 0 {
            None
        } else {
            Some(Building {
                name: if details & DETAIL_BUILDING_NAME == 0 {
                    None
                } else {
                    Some(self.string_field(strings, fields)?.to_string())
                },
            })
        };
        let block = (details & DETAIL_BLOCK_NUMBER != 0)
            .then(|| block_number_len(fields))
            .transpose()?
            .flatten();
        Ok((building, details & DETAIL_ACCESS != 0, block))
    }

    fn access_field(&self, header: &Header, fields: &mut &[u8]) -> Result<Vec<Entrance>> {
        let count = get_u64(fields)?;
        if count > fields.len() as u64 {
            return Err(Error::Decode("entrances are truncated"));
        }
        (0..count)
            .map(|_| {
                Ok(Entrance {
                    at: [
                        add_delta(header.lon_e7, get_i64(fields)?)?,
                        add_delta(header.lat_e7, get_i64(fields)?)?,
                    ],
                    kind: self.string_field(header.strings, fields)?.to_string(),
                })
            })
            .collect()
    }

    fn address_components(
        &self,
        strings: StringSegment,
        fields: &mut &[u8],
    ) -> Result<AddressComponents> {
        Ok(self.address_fields(strings, fields)?.0)
    }

    fn address_fields(
        &self,
        strings: StringSegment,
        fields: &mut &[u8],
    ) -> Result<(AddressComponents, bool)> {
        let number = self.string_field(strings, fields)?.to_string();
        let flagged = fields
            .first()
            .is_some_and(|present| present & ADDRESS_DETAILS != 0);
        let [street, place, unit, locality, region, postcode, country] =
            self.optional_strings(strings, fields)?;
        let areas = self.string_list(strings, fields)?;
        Ok((
            AddressComponents {
                block_number_len: None,
                areas,
                number,
                street,
                place,
                unit,
                locality,
                region,
                postcode,
                country,
            },
            flagged,
        ))
    }

    fn poi_fields(&self, header: &Header, fields: &mut &[u8]) -> Result<PoiFields<'_>> {
        let (strings, point) = (header.strings, [header.lon_e7, header.lat_e7]);
        let name = self.string_field(strings, fields)?;
        let category = self.string_field(strings, fields)?;
        let alt_names = self.string_list(strings, fields)?;
        let prominence = Prominence::from_byte(get_u8(fields)?, get_u8(fields)?);
        let wikidata = item_field(fields)?;
        let aliases = aliases_field(fields)?;
        let brand = match get_u64(fields)? {
            0 => None,
            id => Some(Brand {
                name: self
                    .string(
                        strings,
                        u32::try_from(id - 1)
                            .map_err(|_| Error::Decode("brand string id is out of range"))?,
                    )?
                    .to_string(),
                wikidata: item_field(fields)?,
            }),
        };
        let flags = fields.first().copied().unwrap_or_default();
        let [phone, website, email, opening_hours] = self.optional_strings(strings, fields)?;
        let extent = if flags & POI_EXTENT != 0 {
            let mut sides = [0; 4];
            for (side, at) in sides
                .iter_mut()
                .zip([point[0], point[1], point[0], point[1]])
            {
                *side = add_delta(at, get_i64(fields)?)?;
            }
            Some(sides)
        } else {
            None
        };
        let access = if flags & POI_ACCESS != 0 {
            self.access_field(header, fields)?
        } else {
            Vec::new()
        };
        let cuisine = if flags & POI_CUISINE != 0 {
            self.string_list(strings, fields)?
        } else {
            Vec::new()
        };
        let fax = (flags & POI_FAX != 0)
            .then(|| self.string_field(strings, fields).map(str::to_string))
            .transpose()?;
        let address = match get_u8(fields)? {
            0 => None,
            _ => {
                let (mut address, flagged) = self.address_fields(strings, fields)?;
                if flagged {
                    address.block_number_len = block_number_len(fields)?;
                }
                Some(address)
            }
        };
        Ok(PoiFields {
            name,
            category,
            alt_names,
            prominence,
            wikidata,
            aliases,
            brand,
            details: PoiDetails {
                contact: Contact::with_fax(phone, website, email, fax),
                opening_hours,
                extent,
                access,
                cuisine,
            }
            .kept(),
            address,
        })
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

    fn skip_fields(&self, header: &Header, fields: &mut &[u8]) -> Result<()> {
        match header.layer {
            Layer::Address => {
                let (_, flagged) = self.address_fields(header.strings, fields)?;
                self.string_list(header.strings, fields)?;
                if flagged && self.details_field(header.strings, fields)?.1 {
                    self.access_field(header, fields)?;
                }
            }
            Layer::Poi => {
                self.poi_fields(header, fields)?;
            }
            Layer::Interpolation => {
                self.interpolation_fields(header.strings, fields)?;
            }
            Layer::Street => {
                get_u32(fields)?;
                self.string_list(header.strings, fields)?;
                self.optional_strings::<1>(header.strings, fields)?;
                road_class_field(fields)?;
                other_ways_field(fields, header.object()?.1)?;
            }
            Layer::Postcode => {
                get_u32(fields)?;
                country_field(fields)?;
            }
            _ => {
                self.place_prominence(header.strings, fields)?;
                item_field(fields)?;
            }
        }
        Ok(())
    }

    fn geometry(&self, header: &Header, fields: &mut &[u8]) -> Result<Geometry> {
        if !header.line {
            return Ok(point_geometry(header.lon(), header.lat()));
        }
        let parts = self
            .line_points(header, fields)?
            .into_iter()
            .map(|part| {
                part.into_iter()
                    .map(|[lon, lat]| vec![dequantize(lon), dequantize(lat)].into())
                    .collect()
            })
            .collect();
        Ok(line_geometry(parts))
    }

    fn line_points(&self, header: &Header, fields: &mut &[u8]) -> Result<Vec<Vec<[i32; 2]>>> {
        let part_count = usize::try_from(get_u64(fields)?)?;
        if part_count == 0 || part_count > fields.len() {
            return Err(Error::BadGeometry(format!(
                "stored line geometry has an invalid part count {part_count}"
            )));
        }
        let mut parts = Vec::with_capacity(part_count);
        let mut previous = [header.lon_e7, header.lat_e7];
        for _ in 0..part_count {
            let count = usize::try_from(get_u64(fields)?)?;
            if count < 2 || count > fields.len() {
                return Err(Error::BadGeometry(format!(
                    "stored line geometry has an invalid point count {count}"
                )));
            }
            let mut points = Vec::with_capacity(count);
            for _ in 0..count {
                let lon = add_delta(previous[0], get_i64(fields)?)?;
                let lat = add_delta(previous[1], get_i64(fields)?)?;
                previous = [lon, lat];
                points.push(previous);
            }
            parts.push(points);
        }
        Ok(parts)
    }
}

struct PoiFields<'a> {
    name: &'a str,
    category: &'a str,
    alt_names: Vec<String>,
    prominence: Prominence,
    wikidata: Option<u32>,
    aliases: Vec<(ObjectType, i64)>,
    brand: Option<Brand>,
    details: Option<Box<PoiDetails>>,
    address: Option<AddressComponents>,
}

struct Block<'a> {
    first: RecordId,
    base: (i32, i32),
    width: usize,
    count: usize,
    continued: u64,
    centroids: u64,
    ways: u64,
    relations: u64,
    disputed: u64,
    starts: &'a [u8],
    bodies: &'a [u8],
}

impl<'a> Block<'a> {
    fn is_continued(&self, slot: usize) -> bool {
        slot < self.count && self.continued & 1 << slot != 0
    }

    fn plain_body(&self, slot: usize) -> Result<&'a [u8]> {
        let rank = slot - (self.continued & low_slots(slot)).count_ones() as usize;
        let at = rank * self.width;
        let mut bytes = [0; 4];
        bytes[..self.width].copy_from_slice(&self.starts[at..at + self.width]);
        self.bodies
            .get(u32::from_le_bytes(bytes) as usize..)
            .filter(|body| !body.is_empty())
            .ok_or(Error::Decode("record body is out of range"))
    }

    fn anchor(&self, slot: usize) -> Result<usize> {
        let plain = !self.continued & low_slots(slot);
        if plain == 0 {
            return Err(Error::Decode(
                "continued address has no anchor in its block",
            ));
        }
        Ok(63 - plain.leading_zeros() as usize)
    }

    fn source_kind(&self, slot: usize) -> SourceKind {
        let bit = 1 << slot;
        if self.ways & bit != 0 {
            SourceKind::Way
        } else if self.relations & bit != 0 {
            SourceKind::Relation
        } else {
            SourceKind::Node
        }
    }
}

fn low_slots(slot: usize) -> u64 {
    if slot >= 64 {
        u64::MAX
    } else {
        (1 << slot) - 1
    }
}

struct ContinuedFields {
    d_lon: i64,
    d_lat: i64,
    object_delta: i64,
    code: u64,
    unit: Option<u32>,
}

impl ContinuedFields {
    fn point(&self, previous: (i32, i32)) -> Result<(i32, i32)> {
        Ok((
            add_delta(previous.0, self.d_lon)?,
            add_delta(previous.1, self.d_lat)?,
        ))
    }

    fn number(&self, plain: u32) -> Result<ContinuedNumber> {
        let value = self.code >> NUMBER_SHIFT;
        Ok(if self.code & NUMBER_TEXT != 0 {
            ContinuedNumber::Text(
                u32::try_from(value).map_err(|_| Error::Decode("house number id out of range"))?,
            )
        } else {
            ContinuedNumber::Plain(
                u32::try_from(i64::from(plain) + unzigzag(value))
                    .map_err(|_| Error::Decode("house number out of range"))?,
            )
        })
    }
}

fn read_continuation(cursor: &mut &[u8]) -> Result<ContinuedFields> {
    let d_lon = get_i64(cursor)?;
    let d_lat = get_i64(cursor)?;
    let object_delta = get_i64(cursor)?;
    let code = get_u64(cursor)?;
    let unit = (code & NUMBER_UNIT != 0)
        .then(|| get_u32(cursor))
        .transpose()?;
    Ok(ContinuedFields {
        d_lon,
        d_lat,
        object_delta,
        code,
        unit,
    })
}

fn unit_id(fields: &mut &[u8]) -> Result<Option<u32>> {
    let present = get_u8(fields)?;
    for bit in 0..2 {
        if present & (1 << bit) != 0 {
            get_u32(fields)?;
        }
    }
    (present & (1 << 2) != 0)
        .then(|| get_u32(fields))
        .transpose()
}

fn block_number_len(fields: &mut &[u8]) -> Result<Option<std::num::NonZeroU16>> {
    Ok(std::num::NonZeroU16::new(
        u16::try_from(get_u64(fields)?)
            .map_err(|_| Error::Decode("invalid block number length"))?,
    ))
}

fn skip_address_body(mut body: &[u8]) -> Result<&[u8]> {
    let tag = get_u8(&mut body)?;
    let imported = source_kind_of(tag) == SourceKind::Other;
    for _ in 0..5 + usize::from(imported) {
        get_u64(&mut body)?;
    }
    let present = get_u8(&mut body)?;
    for _ in 0..(present & !ADDRESS_DETAILS).count_ones() {
        get_u64(&mut body)?;
    }
    for _ in 0..2 {
        let strings = get_u64(&mut body)?;
        if strings > body.len() as u64 {
            return Err(Error::Decode("string list is truncated"));
        }
        for _ in 0..strings {
            get_u64(&mut body)?;
        }
    }
    let details = if present & ADDRESS_DETAILS != 0 {
        get_u8(&mut body)?
    } else {
        0
    };
    if details & DETAIL_BUILDING_NAME != 0 {
        get_u64(&mut body)?;
    }
    if details & DETAIL_BLOCK_NUMBER != 0 {
        get_u64(&mut body)?;
    }
    if details & DETAIL_ACCESS != 0 {
        let entrances = get_u64(&mut body)?;
        if entrances > body.len() as u64 {
            return Err(Error::Decode("entrances are truncated"));
        }
        for _ in 0..entrances * 3 {
            get_u64(&mut body)?;
        }
    }
    Ok(body)
}

const ALIAS_KIND_BITS: u32 = 2;

pub(crate) fn put_aliases(out: &mut Vec<u8>, aliases: &[(ObjectType, i64)]) -> Result<()> {
    put_u64(out, aliases.len() as u64);
    for (object_type, object_id) in aliases {
        let kind = match object_type {
            ObjectType::Node => 0,
            ObjectType::Way => 1,
            ObjectType::Relation => 2,
            ObjectType::Row => {
                return Err(Error::Inconsistent("an imported row is no alias".into()));
            }
        };
        let number = u64::try_from(*object_id)
            .ok()
            .filter(|number| number >> (u64::BITS - ALIAS_KIND_BITS) == 0)
            .ok_or_else(|| Error::Inconsistent(format!("alias {object_id} is out of range")))?;
        put_u64(out, number << ALIAS_KIND_BITS | kind);
    }
    Ok(())
}

pub(crate) fn aliases_field(fields: &mut &[u8]) -> Result<Vec<(ObjectType, i64)>> {
    let count = get_u64(fields)?;
    if count > fields.len() as u64 {
        return Err(Error::Decode("alias list is truncated"));
    }
    (0..count)
        .map(|_| {
            let value = get_u64(fields)?;
            let object_type = match value & ((1 << ALIAS_KIND_BITS) - 1) {
                0 => ObjectType::Node,
                1 => ObjectType::Way,
                2 => ObjectType::Relation,
                _ => return Err(Error::Decode("an alias names no kind of object")),
            };
            let number = i64::try_from(value >> ALIAS_KIND_BITS)
                .map_err(|_| Error::Decode("alias is out of range"))?;
            Ok((object_type, number))
        })
        .collect()
}

pub(crate) fn put_other_ways(out: &mut Vec<u8>, own: i64, ways: &[i64]) -> Result<()> {
    put_u64(out, ways.len() as u64);
    let mut before = own;
    for way in ways {
        let delta = way
            .checked_sub(before)
            .and_then(|delta| u64::try_from(delta).ok())
            .filter(|delta| *delta > 0)
            .ok_or_else(|| {
                Error::Inconsistent(format!(
                    "a street's way {way} is not above the way before it, {before}"
                ))
            })?;
        put_u64(out, delta);
        before = *way;
    }
    Ok(())
}

pub(crate) fn other_ways_field(fields: &mut &[u8], own: i64) -> Result<Vec<i64>> {
    let count = get_u64(fields)?;
    if count > fields.len() as u64 {
        return Err(Error::Decode("a street's way list is truncated"));
    }
    let mut before = own;
    (0..count)
        .map(|_| {
            before = i64::try_from(get_u64(fields)?)
                .ok()
                .and_then(|delta| before.checked_add(delta))
                .ok_or(Error::Decode("a street's way is out of range"))?;
            Ok(before)
        })
        .collect()
}

fn item_field(fields: &mut &[u8]) -> Result<Option<u32>> {
    match get_u64(fields)? {
        0 => Ok(None),
        item => u32::try_from(item)
            .map(Some)
            .map_err(|_| Error::Decode("Wikidata item is out of range")),
    }
}

fn road_class_field(fields: &mut &[u8]) -> Result<RoadClass> {
    RoadClass::from_byte(get_u8(fields)?).ok_or(Error::Decode("street has an unknown road class"))
}

fn country_field(fields: &mut &[u8]) -> Result<Option<CountryCode>> {
    match get_u64(fields)? {
        0 => Ok(None),
        code => u16::try_from(code)
            .ok()
            .and_then(CountryCode::from_u16)
            .map(Some)
            .ok_or(Error::Decode("postcode country is not a country code")),
    }
}

struct Entry<'a> {
    header: Header,
    fields: &'a [u8],
    continued: Option<Continued>,
}

struct Continued {
    number: ContinuedNumber,
    unit: Option<u32>,
}

#[derive(Clone, Copy)]
enum ContinuedNumber {
    Plain(u32),
    Text(u32),
}

fn layer_of(tag: u8) -> Result<Layer> {
    Layer::ALL
        .get(usize::from(tag & TAG_LAYER_MASK))
        .copied()
        .ok_or(Error::Decode("record has an unknown layer code"))
}

fn source_kind_of(tag: u8) -> SourceKind {
    match tag >> TAG_SOURCE_SHIFT {
        0 => SourceKind::Node,
        1 => SourceKind::Way,
        2 => SourceKind::Relation,
        _ => SourceKind::Other,
    }
}

fn plain_point(base: (i32, i32), mut body: &[u8]) -> Result<(i32, i32)> {
    get_u8(&mut body)?;
    Ok((
        add_delta(base.0, get_i64(&mut body)?)?,
        add_delta(base.1, get_i64(&mut body)?)?,
    ))
}

fn plain_header(
    base: (i32, i32),
    strings: StringSegment,
    mut body: &[u8],
) -> Result<(Header, &[u8])> {
    let tag = get_u8(&mut body)?;
    let layer = layer_of(tag)?;
    let lon_e7 = add_delta(base.0, get_i64(&mut body)?)?;
    let lat_e7 = add_delta(base.1, get_i64(&mut body)?)?;
    let context = match get_u64(&mut body)? {
        0 => None,
        value => Some(ContextRef {
            tuple_id: u32::try_from((value >> 1) - 1)
                .map_err(|_| Error::Decode("context id out of range"))?,
            ambiguous: value & 1 == 1,
        }),
    };
    let source_value = get_u64(&mut body)?;
    let source_kind = source_kind_of(tag);
    let imported = source_kind == SourceKind::Other;
    let dataset = (imported && matches!(layer, Layer::Address | Layer::Poi))
        .then(|| get_u32(&mut body))
        .transpose()?;
    let gers = (imported && layer == Layer::Poi)
        .then(|| get_u32(&mut body))
        .transpose()?;
    Ok((
        Header {
            layer,
            strings,
            lon_e7,
            lat_e7,
            context,
            disputed: false,
            centroid: tag & TAG_CENTROID != 0,
            line: tag & TAG_LINE != 0,
            source_kind,
            source_value,
            dataset,
            gers,
        },
        body,
    ))
}

fn osm_source(source: &SourceProvenance) -> (SourceKind, u64) {
    let kind = match source.object_type {
        ObjectType::Node => SourceKind::Node,
        ObjectType::Way => SourceKind::Way,
        ObjectType::Relation => SourceKind::Relation,
        ObjectType::Row => SourceKind::Other,
    };
    (kind, zigzag(source.object_id))
}

fn layer_code(layer: Layer) -> u8 {
    Layer::ALL
        .iter()
        .position(|candidate| *candidate == layer)
        .expect("every layer has a code") as u8
}

type LineVertices = Vec<Vec<(i32, i32)>>;

fn line_coordinates(geometry: &Geometry) -> Result<Option<LineVertices>> {
    if let GeometryValue::Point { .. } = &geometry.value {
        return Ok(None);
    }
    let Some(parts) = line_parts(geometry) else {
        return Err(Error::BadGeometry(format!(
            "unsupported stored record geometry: {}",
            geometry.value.type_name()
        )));
    };
    if parts.is_empty() {
        return Err(Error::BadGeometry("line geometry has no parts".into()));
    }
    parts
        .into_iter()
        .map(|part| {
            if part.len() < 2 {
                return Err(Error::BadGeometry(
                    "line geometry needs at least two positions".into(),
                ));
            }
            part.iter()
                .map(|position| {
                    let [lon, lat, ..] = position.as_slice() else {
                        return Err(Error::BadGeometry(
                            "line position is missing lon/lat".into(),
                        ));
                    };
                    Ok((quantize(*lon)?, quantize(*lat)?))
                })
                .collect::<Result<Vec<_>>>()
        })
        .collect::<Result<Vec<_>>>()
        .map(Some)
}

pub(crate) fn quantize(value: f64) -> Result<i32> {
    if !value.is_finite() {
        return Err(Error::CoordinateNotFinite);
    }
    let scaled = (value * COORDINATE_SCALE).round();
    if scaled < i32::MIN as f64 || scaled > i32::MAX as f64 {
        return Err(Error::CoordinateOutOfRange { value });
    }
    Ok(scaled as i32)
}

pub(crate) fn dequantize(value: i32) -> f64 {
    value as f64 / COORDINATE_SCALE
}

pub(crate) fn bbox(extent: Extent) -> [f64; 4] {
    extent.map(dequantize)
}

fn add_delta(base: i32, delta: i64) -> Result<i32> {
    i32::try_from(i64::from(base) + delta)
        .map_err(|_| Error::Decode("coordinate delta is out of range"))
}

fn agreed_postcode(postcodes: &[String]) -> Option<String> {
    let first = postcodes.first()?;
    let key = crate::text_index::compact_postcode(first)?;
    postcodes
        .iter()
        .all(|postcode| crate::text_index::compact_postcode(postcode).as_ref() == Some(&key))
        .then(|| first.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{record::PlaceLayer, test_support::TempDir};

    fn line(points: &[[f64; 2]]) -> Geometry {
        Geometry::new(GeometryValue::LineString {
            coordinates: points.iter().map(|point| point.to_vec().into()).collect(),
        })
    }

    fn sample_records() -> Vec<Record> {
        let mut records = vec![
            Record::Address(AddressRecord {
                building: None,
                access: Vec::new(),
                disputed: false,
                street_postcodes: Vec::new(),
                address: AddressComponents {
                    block_number_len: None,
                    areas: vec!["Old Toronto".into(), "Liberty Village".into()],
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
                source: SourceProvenance::osm(ObjectType::Node, 1),
            }),
            Record::Street(StreetRecord {
                other_ways: vec![5, 1_400_000_000],
                road_class: RoadClass::Residential,
                alt_names: vec!["Rue King".into(), "キングストリート".into()],
                locality: None,
                name: "King Street".into(),
                geometry: line(&[[-79.0, 43.0], [-79.001, 43.001], [-79.0015, 43.0]]),
                representative_point: [-79.0, 43.0],
                source: SourceProvenance::osm(ObjectType::Way, 2),
            }),
            Record::Postcode(PostcodeRecord {
                postcode: "M5V 1A1".into(),
                country: None,
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
                source: SourceProvenance::osm(ObjectType::Way, 5),
            }),
            Record::Poi(PoiRecord {
                alt_names: vec!["Tim's".into()],
                name: "Tim Hortons".into(),
                category: "amenity:cafe".into(),
                address: Some(AddressComponents {
                    block_number_len: None,
                    areas: Vec::new(),
                    number: "123".into(),
                    street: Some("King Street West".into()),
                    place: None,
                    unit: None,
                    locality: None,
                    region: None,
                    postcode: Some("M5V 1A1".into()),
                    country: None,
                }),
                geometry: point_geometry(-79.0, 43.0),
                location_precision: LocationPrecision::Centroid,
                source: SourceProvenance::osm(ObjectType::Way, 6),
                prominence: Prominence {
                    linked: true,
                    languages: 12,
                    importance: 0,
                },
                wikidata: None,
                brand: None,
                details: None,
                aliases: Vec::new(),
            }),
            Record::Poi(PoiRecord {
                alt_names: Vec::new(),
                name: "Riverdale Farm".into(),
                category: "tourism:attraction".into(),
                address: None,
                geometry: point_geometry(-79.0, 43.0),
                location_precision: LocationPrecision::Point,
                source: SourceProvenance::osm(ObjectType::Node, 7),
                prominence: Default::default(),
                wikidata: None,
                brand: None,
                details: None,
                aliases: Vec::new(),
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
                    alt_names: vec!["Lieu exemple".into()],
                    name: "Example place".into(),
                    place_type: "city".into(),
                    geometry: point_geometry(-79.0, 43.0),
                    source: SourceProvenance::osm(ObjectType::Relation, 3),
                    prominence: Default::default(),
                    wikidata: None,
                },
            ));
        }
        records.push(Record::Place(
            PlaceLayer::Country,
            PlaceRecord {
                alt_names: Vec::new(),
                name: "Canada".into(),
                place_type: "derived_country:CA".into(),
                geometry: point_geometry(-79.0, 43.0),
                source: SourceProvenance::osm(ObjectType::Relation, 4),
                prominence: Prominence {
                    importance: 180,
                    linked: true,
                    languages: 120,
                },
                wikidata: Some(16),
            },
        ));
        records
    }

    fn write_store(records: &[Record], contexts: &[Option<ContextRef>]) -> Reader {
        let root = TempDir::new("records");
        let scratch = Scratch::create(root.join("scratch")).expect("scratch");
        let mut writer = Writer::create(&scratch, &Budget::unlimited()).expect("writer");
        for (index, record) in records.iter().enumerate() {
            let id = writer
                .write(record, contexts.get(index).copied().flatten())
                .expect("write");
            assert_eq!(id, index as u64);
        }
        let path = root.join("pack.ogp");
        let mut pack = container::Writer::create(&path).expect("pack");
        writer.finish(&mut pack).expect("finish");
        pack.finish().expect("pack finish");
        Reader::open(&Container::open(&path).expect("open")).expect("reader")
    }

    fn detailed_poi(id: i64, details: Option<Box<PoiDetails>>) -> Record {
        Record::Poi(PoiRecord {
            alt_names: Vec::new(),
            name: "Queen Bakery".into(),
            category: "shop:bakery".into(),
            address: None,
            geometry: point_geometry(-79.0, 43.0),
            location_precision: LocationPrecision::Point,
            source: SourceProvenance::osm(ObjectType::Node, id),
            prominence: Default::default(),
            wikidata: None,
            brand: None,
            details,
            aliases: Vec::new(),
        })
    }

    #[test]
    fn a_poi_and_a_street_answer_with_the_box_around_them() {
        let text = |value: &str| Some(value.to_string());
        let hall = [-790_002_000, 429_999_000, -789_998_500, 430_001_250];
        let street = |id: i64, parts: Vec<Vec<[f64; 2]>>| {
            Record::Street(StreetRecord {
                name: "Quay Street".into(),
                road_class: RoadClass::Residential,
                alt_names: Vec::new(),
                locality: None,
                representative_point: parts[0][0],
                geometry: line_geometry(
                    parts
                        .into_iter()
                        .map(|part| {
                            part.into_iter()
                                .map(|[lon, lat]| vec![lon, lat].into())
                                .collect()
                        })
                        .collect(),
                ),
                source: SourceProvenance::osm(ObjectType::Way, id),
                other_ways: Vec::new(),
            })
        };
        let records = [
            detailed_poi(
                0,
                PoiDetails {
                    extent: Some(hall),
                    ..PoiDetails::default()
                }
                .kept(),
            ),
            detailed_poi(
                1,
                PoiDetails {
                    access: Vec::new(),
                    contact: None,
                    opening_hours: text("24/7"),
                    extent: Some(hall),
                    cuisine: Vec::new(),
                }
                .kept(),
            ),
            detailed_poi(2, PoiDetails::stated(None, text("24/7"))),
            detailed_poi(3, None),
            street(
                4,
                vec![vec![[-79.2, 43.1], [-79.19, 43.12], [-79.21, 43.11]]],
            ),
            street(
                5,
                vec![
                    vec![[179.9, -16.5], [180.0, -16.4]],
                    vec![[-180.0, -16.4], [-179.95, -16.3]],
                ],
            ),
        ];
        let reader = write_store(&records, &[]);
        for (id, record) in records.iter().enumerate() {
            assert_eq!(&reader.record(id as u64).expect("record"), record, "{id}");
        }
        let boxes = (0..records.len() as u64)
            .map(|id| reader.summary(id).expect("summary").bbox)
            .collect::<Vec<_>>();
        assert_eq!(
            boxes,
            [
                Some([-79.0002, 42.9999, -78.99985, 43.000125]),
                Some([-79.0002, 42.9999, -78.99985, 43.000125]),
                None,
                None,
                Some([-79.21, 43.1, -79.19, 43.12]),
                Some([179.9, -16.5, -179.95, -16.3]),
            ]
        );
        assert_eq!(
            reader.summary(1).expect("summary").opening_hours,
            text("24/7")
        );
        let bare = serde_json::to_value(reader.summary(3).expect("summary")).expect("value");
        assert_eq!(bare.get("bbox"), None);
    }

    #[test]
    fn an_address_without_a_postcode_has_the_one_its_street_agrees_on() {
        let house = |id: i64, postcodes: &[&str], access: Vec<Entrance>| {
            Record::Address(AddressRecord {
                building: None,
                access,
                disputed: false,
                street_postcodes: postcodes.iter().map(|code| code.to_string()).collect(),
                address: AddressComponents {
                    block_number_len: None,
                    areas: Vec::new(),
                    number: (id * 2).to_string(),
                    street: Some(format!("Street {}", postcodes.len())),
                    place: None,
                    unit: None,
                    locality: Some("Toronto".into()),
                    region: None,
                    postcode: None,
                    country: None,
                },
                geometry: point_geometry(-79.0 + id as f64 * 1e-4, 43.0),
                location_precision: LocationPrecision::Point,
                source: SourceProvenance::osm(ObjectType::Way, id),
            })
        };
        let door = vec![Entrance {
            at: [-790_000_500, 430_000_000],
            kind: "main".to_string(),
        }];
        let records = [
            house(1, &["M5V 1A1", "m5v1a1"], Vec::new()),
            house(2, &["M5V 1A1", "m5v1a1"], door),
            house(3, &["M5V 1A1", "M5V 2B2", "M5V 3C3"], Vec::new()),
            house(4, &[], Vec::new()),
        ];
        let reader = write_store(&records, &[]);
        let agreed = |id: u64| reader.summary(id).expect("summary").street_postcode;
        assert_eq!(agreed(0).as_deref(), Some("M5V 1A1"));
        assert_eq!(agreed(1).as_deref(), Some("M5V 1A1"));
        assert_eq!(agreed(2), None, "the street's addresses disagree");
        assert_eq!(agreed(3), None, "the street's addresses state none");
    }

    #[test]
    fn an_address_round_trips_the_building_it_is_drawn_as() {
        let house = |id: i64, number: &str, building: Option<Building>, access: Vec<Entrance>| {
            Record::Address(AddressRecord {
                access,
                building,
                disputed: false,
                street_postcodes: Vec::new(),
                address: AddressComponents {
                    block_number_len: None,
                    areas: Vec::new(),
                    number: number.into(),
                    street: Some("King Street".into()),
                    place: None,
                    unit: None,
                    locality: Some("Toronto".into()),
                    region: None,
                    postcode: None,
                    country: None,
                },
                geometry: point_geometry(-79.0 + id as f64 * 1e-4, 43.0),
                location_precision: LocationPrecision::Centroid,
                source: SourceProvenance::osm(ObjectType::Way, id),
            })
        };
        let unnamed = || Some(Building::default());
        let named = |name: &str| {
            Some(Building {
                name: Some(name.into()),
            })
        };
        let door = vec![Entrance {
            at: [-789_996_000, 430_000_000],
            kind: "main".to_string(),
        }];
        let records = [
            house(1, "2", None, Vec::new()),
            house(2, "4", unnamed(), Vec::new()),
            house(3, "6", unnamed(), Vec::new()),
            house(4, "8", named("Building 41"), Vec::new()),
            house(5, "10", None, Vec::new()),
            house(6, "12", named("Hall"), door),
            house(7, "14", named("Hall"), Vec::new()),
        ];
        let reader = write_store(&records, &[]);
        for (id, record) in records.iter().enumerate() {
            assert_eq!(&reader.record(id as u64).expect("record"), record, "{id}");
            let Record::Address(address) = record else {
                unreachable!("an address");
            };
            assert_eq!(
                reader.summary(id as u64).expect("summary").building,
                address.building,
                "{id}"
            );
        }
    }

    #[test]
    fn a_poi_and_an_address_round_trip_their_entrances() {
        let entrance = |lon: i32, lat: i32, kind: &str| Entrance {
            at: [lon, lat],
            kind: kind.to_string(),
        };
        let doors = vec![
            entrance(-790_000_500, 430_000_000, "main"),
            entrance(-789_999_000, 430_001_250, "service"),
        ];
        let house = |id: i64, number: &str, access: Vec<Entrance>| {
            Record::Address(AddressRecord {
                building: None,
                access,
                disputed: false,
                street_postcodes: vec!["M5V 1A1".into()],
                address: AddressComponents {
                    block_number_len: None,
                    areas: Vec::new(),
                    number: number.into(),
                    street: Some("King Street".into()),
                    place: None,
                    unit: None,
                    locality: Some("Toronto".into()),
                    region: None,
                    postcode: None,
                    country: None,
                },
                geometry: point_geometry(-79.0 + id as f64 * 1e-4, 43.0),
                location_precision: LocationPrecision::Point,
                source: SourceProvenance::osm(ObjectType::Way, id),
            })
        };
        let records = [
            house(1, "4", Vec::new()),
            house(2, "6", doors.clone()),
            house(3, "8", Vec::new()),
            house(4, "10", Vec::new()),
            house(5, "12", vec![entrance(-789_995_000, 430_000_000, "yes")]),
            detailed_poi(
                6,
                PoiDetails {
                    access: doors,
                    ..PoiDetails::default()
                }
                .kept(),
            ),
            detailed_poi(
                7,
                PoiDetails {
                    contact: None,
                    opening_hours: Some("24/7".into()),
                    extent: Some([-790_002_000, 429_999_000, -789_998_500, 430_001_250]),
                    access: vec![entrance(-790_002_000, 430_000_000, "staircase")],
                    cuisine: vec!["pizza".into(), "italian".into()],
                }
                .kept(),
            ),
            detailed_poi(8, None),
        ];
        let reader = write_store(&records, &[]);
        for (id, record) in records.iter().enumerate() {
            assert_eq!(&reader.record(id as u64).expect("record"), record, "{id}");
        }
        let point = |lon: f64, lat: f64, kind: &str| AccessPoint {
            lon,
            lat,
            kind: kind.to_string(),
        };
        let both = vec![
            point(-79.00005, 43.0, "main"),
            point(-78.9999, 43.000125, "service"),
        ];
        let access = (0..records.len() as u64)
            .map(|id| reader.summary(id).expect("summary").access)
            .collect::<Vec<_>>();
        assert_eq!(
            access,
            [
                Vec::new(),
                both.clone(),
                Vec::new(),
                Vec::new(),
                vec![point(-78.9995, 43.0, "yes")],
                both,
                vec![point(-79.0002, 43.0, "staircase")],
                Vec::new(),
            ]
        );
        assert_eq!(
            (1..5)
                .map(|id| reader.summary(id).expect("summary").name)
                .collect::<Vec<_>>(),
            [
                "6 King Street",
                "8 King Street",
                "10 King Street",
                "12 King Street"
            ]
        );
        assert_eq!(
            reader.record_json(1).expect("json")["access"],
            json!([
                {"lon": -79.00005, "lat": 43.0, "kind": "main"},
                {"lon": -78.9999, "lat": 43.000125, "kind": "service"},
            ])
        );
        assert_eq!(
            reader.record_json(6).expect("json")["access"],
            json!([{"lon": -79.0002, "lat": 43.0, "kind": "staircase"}])
        );
        for bare in [0, 2, 7] {
            assert_eq!(
                reader.record_json(bare).expect("json").get("access"),
                None,
                "{bare}"
            );
            let summary = serde_json::to_value(reader.summary(bare).expect("summary"));
            assert_eq!(summary.expect("value").get("access"), None, "{bare}");
        }
    }

    #[test]
    fn a_poi_round_trips_the_contact_and_hours_it_states() {
        let text = |value: &str| Some(value.to_string());
        let full = Contact {
            fax: None,
            phone: text("+1 416-555-0111"),
            website: text("https://queenbakery.example/"),
            email: text("hello@queenbakery.example"),
        };
        let hours = "Mo-Fr 07:00-18:00; Sa 08:00-14:00";
        let stated = [
            (Some(full.clone()), text(hours)),
            (Some(full), None),
            (None, text(hours)),
            (Contact::stated(text("+1 416-555-0111"), None, None), None),
            (
                Contact::stated(None, text("queenbakery.example"), None),
                None,
            ),
            (
                Contact::stated(None, None, text("hello@queenbakery.example")),
                text("24/7"),
            ),
            (None, None),
        ];
        let records = stated
            .iter()
            .enumerate()
            .map(|(id, (contact, hours))| {
                detailed_poi(
                    id as i64,
                    PoiDetails::stated(contact.clone(), hours.clone()),
                )
            })
            .collect::<Vec<_>>();
        let reader = write_store(&records, &[]);
        for (id, (record, (contact, hours))) in records.iter().zip(&stated).enumerate() {
            let id = id as u64;
            assert_eq!(&reader.record(id).expect("record"), record);
            let summary = reader.summary(id).expect("summary");
            assert_eq!((&summary.contact, &summary.opening_hours), (contact, hours));
        }
        assert_eq!(
            reader.record_json(0).expect("json")["contact"],
            json!({
                "phone": "+1 416-555-0111",
                "website": "https://queenbakery.example/",
                "email": "hello@queenbakery.example",
            })
        );
        assert_eq!(reader.record_json(0).expect("json")["opening_hours"], hours);
        let bare = reader.record_json(6).expect("json");
        assert_eq!(bare.get("contact"), None);
        assert_eq!(bare.get("opening_hours"), None);
        assert_eq!(
            serde_json::to_value(reader.summary(3).expect("summary")).expect("value")["contact"],
            json!({"phone": "+1 416-555-0111"})
        );
        let bare = serde_json::to_value(reader.summary(6).expect("summary")).expect("value");
        assert_eq!(bare.get("contact"), None);
        assert_eq!(bare.get("opening_hours"), None);
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
        let canada = records.len() as u64 - 1;
        assert_eq!(
            reader.prominence(canada).expect("prominence"),
            Some(Prominence {
                importance: 180,
                linked: true,
                languages: 120,
            })
        );
        assert_eq!(reader.record_json(canada).expect("json")["wikidata"], "Q16");
        assert_eq!(
            reader.record_json(3).expect("interpolation")["anchor_ids"],
            json!(["osm:node:10", "osm:node:20"])
        );
        let poi = reader.summary(4).expect("POI summary");
        assert_eq!(poi.layer, "poi");
        assert_eq!(poi.category.as_deref(), Some("amenity:cafe"));
        assert_eq!(poi.label, "Tim Hortons, 123 King Street West, M5V 1A1");
        assert_eq!(
            poi.point.expect("point").precision,
            PointPrecision::Centroid
        );
        assert_eq!(
            reader
                .summary(5)
                .expect("POI summary")
                .point
                .expect("point")
                .precision,
            PointPrecision::Point
        );
        assert_eq!(
            reader.postcode(4).expect("postcode").as_deref(),
            Some("M5V 1A1")
        );
        assert_eq!(reader.postcode(5).expect("postcode"), None);
        assert_eq!(
            reader.names(0).expect("address names"),
            Vec::<String>::new()
        );
        assert_eq!(
            reader.names(1).expect("street names"),
            vec!["King Street", "Rue King", "キングストリート"]
        );
        assert_eq!(
            reader.names(4).expect("POI names"),
            vec!["Tim Hortons", "Tim's"]
        );
        for (id, record) in records.iter().enumerate() {
            assert_eq!(
                reader.names(id as u64).expect("names"),
                record.names(),
                "record {id}"
            );
        }
        assert_eq!(reader.line(0).expect("address line"), None);
        assert_eq!(
            reader.line(1).expect("street line"),
            Some(vec![vec![
                [-790_000_000, 430_000_000],
                [-790_010_000, 430_010_000],
                [-790_015_000, 430_000_000]
            ]])
        );
    }

    #[test]
    fn string_dictionary_is_bounded_by_the_segment() {
        let root = TempDir::new("segments");
        let scratch = Scratch::create(root.join("scratch")).expect("scratch");
        let budget = Budget::unlimited();
        let mut writer = Writer::create(&scratch, &budget)
            .expect("writer")
            .with_segment_records(100);
        let base = sample_records().remove(0);
        let mut records = Vec::new();
        let mut most_resident = 0;
        for index in 0..1_050 {
            let mut record = base.clone();
            if let Record::Address(address) = &mut record {
                address.address.street = Some(format!("Street {index}"));
                address.address.number = (index % 7).to_string();
            }
            writer.write(&record, None).expect("write");
            most_resident = most_resident.max(writer.resident_strings());
            assert!(budget.used() >= writer.resident_strings() * DICTIONARY_ENTRY_OVERHEAD);
            records.push(record);
        }
        assert!(
            most_resident <= 100_u64.next_multiple_of(BLOCK_RECORDS) as usize + 14,
            "dictionary held {most_resident} strings"
        );
        assert!(budget.peak() < 200 * (DICTIONARY_ENTRY_OVERHEAD + 16));
        let path = root.join("pack.ogp");
        let mut pack = container::Writer::create(&path).expect("pack");
        writer.finish(&mut pack).expect("finish");
        pack.finish().expect("pack finish");
        let reader = Reader::open(&Container::open(&path).expect("open")).expect("reader");
        for (id, record) in records.iter().enumerate() {
            assert_eq!(&reader.record(id as u64).expect("record"), record);
        }
    }

    #[test]
    fn string_dictionary_stays_within_its_share_of_a_small_budget() {
        let root = TempDir::new("dictionary");
        let scratch = Scratch::create(root.join("scratch")).expect("scratch");
        let budget = Budget::new(1 << 20);
        let mut writer = Writer::create(&scratch, &budget).expect("writer");
        let base = sample_records().remove(0);
        for index in 0..5_000 {
            let mut record = base.clone();
            if let Record::Address(address) = &mut record {
                address.address.street = Some(format!("Unique Street Number {index}"));
            }
            writer.write(&record, None).expect("write");
        }
        let share = budget.limit() / DICTIONARY_BUDGET_FRACTION;
        let slack = BLOCK_RECORDS as usize * (64 + DICTIONARY_ENTRY_OVERHEAD);
        assert!(
            budget.peak() <= share + slack,
            "dictionary peaked at {} of {share}",
            budget.peak()
        );
        assert!(writer.strings.segments.len() > 1);
        let path = root.join("pack.ogp");
        let mut pack = container::Writer::create(&path).expect("pack");
        writer.finish(&mut pack).expect("finish");
        pack.finish().expect("pack finish");
        let reader = Reader::open(&Container::open(&path).expect("open")).expect("reader");
        for id in [0, 2_499, 4_999] {
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
    fn continues_a_street_run_and_reads_every_address_back() {
        let anchor = sample_records().remove(0);
        let street = sample_records().remove(1);
        let mut records = Vec::new();
        let mut contexts = Vec::new();
        let numbers = [
            "1",
            "3",
            "3A",
            "007",
            "0",
            "12;14",
            "4294967295",
            "5",
            "2",
            "100",
            "99",
        ];
        for index in 0..BLOCK_RECORDS * 2 + 9 {
            let mut record = anchor.clone();
            if let Record::Address(address) = &mut record {
                let index_i64 = index as i64;
                address.address.number = numbers[index as usize % numbers.len()].to_string();
                address.address.unit = (index % 5 == 0).then(|| format!("Unit {index}"));
                address.geometry =
                    point_geometry(-79.0 + index as f64 * 0.0001, 43.0 - index as f64 * 0.00007);
                address.location_precision = if index % 3 == 0 {
                    LocationPrecision::Centroid
                } else {
                    LocationPrecision::Point
                };
                address.source = SourceProvenance::osm(
                    if index % 4 == 0 {
                        ObjectType::Way
                    } else {
                        ObjectType::Node
                    },
                    1_000_000 - index_i64 * 37,
                );
                address.disputed = index % 7 == 3;
                if index == 70 {
                    address.address.postcode = Some("M5V 9Z9".into());
                }
            }
            if index == 20 {
                records.push(street.clone());
                contexts.push(None);
            }
            let context = (index != 30).then_some(ContextRef {
                tuple_id: 4,
                ambiguous: false,
            });
            records.push(record);
            contexts.push(context);
        }
        let reader = write_store(&records, &contexts);
        let mut scanned = Vec::new();
        for block in 0..reader.block_count() {
            reader
                .block_points(block, |id, layer, lon, lat, line| {
                    scanned.push((id, layer, lon, lat, line));
                    Ok(())
                })
                .expect("block points");
        }
        assert_eq!(scanned.len(), records.len());
        for (index, record) in records.iter().enumerate() {
            let id = index as u64;
            assert_eq!(&reader.record(id).expect("record"), record, "record {id}");
            assert_eq!(
                reader.header(id).expect("header").context,
                contexts[index],
                "record {id}"
            );
            let [lon, lat] = record.display_point().expect("point");
            let (layer, point_lon, point_lat) = reader.point(id).expect("point");
            assert_eq!(
                (
                    layer,
                    quantize(point_lon).unwrap(),
                    quantize(point_lat).unwrap()
                ),
                (
                    record.layer(),
                    quantize(lon).unwrap(),
                    quantize(lat).unwrap()
                )
            );
            assert_eq!(
                (
                    scanned[index].0,
                    scanned[index].1,
                    scanned[index].2,
                    scanned[index].3
                ),
                (
                    id,
                    record.layer(),
                    quantize(lon).unwrap(),
                    quantize(lat).unwrap()
                )
            );
            let summary = reader.summary(id).expect("summary");
            assert_eq!(summary.id, reader.record_json(id).expect("json")["id"]);
        }
        assert!(reader.record(records.len() as u64).is_err());
    }

    #[test]
    fn keeps_imported_rows_apart_from_osm_runs() {
        let osm = sample_records().remove(0);
        let mut records = Vec::new();
        for (index, dataset) in [
            None,
            Some("openaddresses:nz/countrywide"),
            Some("openaddresses:nz/countrywide"),
            Some("openaddresses:au/countrywide"),
            None,
            Some("openaddresses:au/countrywide"),
        ]
        .into_iter()
        .enumerate()
        {
            let mut record = osm.clone();
            if let Record::Address(address) = &mut record {
                address.address.number = (index * 2).to_string();
                address.location_precision = LocationPrecision::Point;
                if let Some(dataset) = dataset {
                    address.source = SourceProvenance::imported(dataset, 1_000 + index as i64);
                }
            }
            records.push(record);
        }
        let reader = write_store(&records, &[]);
        for (index, record) in records.iter().enumerate() {
            let id = index as u64;
            assert_eq!(&reader.record(id).expect("record"), record, "record {id}");
            let summary = reader.summary(id).expect("summary");
            assert_eq!(summary.id, reader.record_json(id).expect("json")["id"]);
        }
        assert_eq!(
            reader.summary(2).expect("summary").id,
            "openaddresses:nz/countrywide:row:1002"
        );
        assert_eq!(
            reader.summary(2).expect("summary").source.dataset,
            "openaddresses:nz/countrywide"
        );
    }
}
