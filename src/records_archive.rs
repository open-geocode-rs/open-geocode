use std::{
    collections::HashMap,
    fs::{self, File},
    io::{BufWriter, Seek, SeekFrom, Write},
    path::Path,
};

use anyhow::{Context, Result, anyhow, bail};
use geojson::{Geometry, GeometryValue};
use serde::Serialize;
use serde_json::{Value, json};

use crate::{
    pack::RecordId,
    record::{
        AddressComponents, AddressRecord, DerivedSourceProvenance, InterpolationAddressComponents,
        InterpolationRange, InterpolationRecord, LocationPrecision, OsmObjectType, PlaceLayer,
        PlaceRecord, PostcodeRecord, SourceProvenance, StreetRecord, point_geometry,
    },
    records_store::{
        self as store, AddressEntry, EntryKind, GeometryType, InterpolationEntry,
        LocationPrecisionCode, PlaceEntry, PostcodeEntry, RecordsStore, SourceObject, StreetEntry,
        pack_directory_entry,
    },
    util::geo::point_lon_lat,
};

const COORDINATE_SCALE: f64 = 10_000_000.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Span {
    start: u64,
    len: u32,
}

impl Span {
    const EMPTY: Self = Self { start: 0, len: 0 };
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

/// Common geometry/representative-point fields shared by every entry.
struct GeometryFields {
    display_lon: i32,
    display_lat: i32,
    geometry_type: u8,
    geometry_start: u64,
    geometry_len: u32,
}

pub struct RecordsArchiveWriter {
    directory: BufWriter<File>,
    blob: BufWriter<File>,
    strings: BufWriter<File>,
    geometries: BufWriter<File>,
    record_count: u64,
    blob_len: u64,
    strings_len: u64,
    geometries_len: u64,
    intern: HashMap<String, Span>,
}

pub struct RecordsArchiveReader {
    store: RecordsStore,
}

impl RecordsArchiveWriter {
    pub fn create(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        fs::create_dir_all(path).with_context(|| format!("failed to create {}", path.display()))?;

        let directory = create_with_header(
            &path.join(store::DIRECTORY_FILE),
            store::DIRECTORY_MAGIC,
            store::DIRECTORY_HEADER_BYTES - 8,
        )?;
        let blob = create_with_header(
            &path.join(store::BLOB_FILE),
            store::BLOB_MAGIC,
            store::ARENA_HEADER_BYTES - 8,
        )?;
        let strings = create_with_header(
            &path.join(store::STRINGS_FILE),
            store::STRINGS_MAGIC,
            store::ARENA_HEADER_BYTES - 8,
        )?;
        let geometries = create_with_header(
            &path.join(store::GEOMETRIES_FILE),
            store::GEOMETRIES_MAGIC,
            store::ARENA_HEADER_BYTES - 8,
        )?;

        Ok(Self {
            directory,
            blob,
            strings,
            geometries,
            record_count: 0,
            blob_len: 0,
            strings_len: 0,
            geometries_len: 0,
            intern: HashMap::new(),
        })
    }

    pub fn write_address(&mut self, record: &AddressRecord) -> Result<RecordId> {
        let address = &record.address;
        let number = self.push_text(&address.number)?;
        let street = self.opt_interned(address.street.as_deref())?;
        let place = self.opt_interned(address.place.as_deref())?;
        let unit = self.opt_text(address.unit.as_deref())?;
        let locality = self.opt_interned(address.locality.as_deref())?;
        let region = self.opt_interned(address.region.as_deref())?;
        let postcode = self.opt_interned(address.postcode.as_deref())?;
        let country = self.opt_interned(address.country.as_deref())?;
        let display_point =
            point_lon_lat(&record.geometry).context("expected finite point geometry")?;
        let geometry = self.encode_geometry(&record.geometry, display_point)?;

        let entry = AddressEntry {
            source_object_id: record.source.object_id,
            display_lon: geometry.display_lon,
            display_lat: geometry.display_lat,
            geometry_start: geometry.geometry_start,
            number_start: number.start,
            street_start: street.start,
            place_start: place.start,
            unit_start: unit.start,
            locality_start: locality.start,
            region_start: region.start,
            postcode_start: postcode.start,
            country_start: country.start,
            geometry_len: geometry.geometry_len,
            number_len: len16(number.len, "number")?,
            street_len: len16(street.len, "street")?,
            place_len: len16(place.len, "place")?,
            unit_len: len16(unit.len, "unit")?,
            locality_len: len16(locality.len, "locality")?,
            region_len: len16(region.len, "region")?,
            postcode_len: len16(postcode.len, "postcode")?,
            country_len: len16(country.len, "country")?,
            source_object: source_object_code(record.source.object_type) as u8,
            geometry_type: geometry.geometry_type,
            location_precision: location_precision_code(record.location_precision) as u8,
            _pad: [0; 1],
        };
        self.push_entry(EntryKind::Address, &entry)
    }

    pub fn write_place(&mut self, record: &PlaceRecord, layer: PlaceLayer) -> Result<RecordId> {
        let name = self.push_text_interned(&record.name)?;
        let place_type = self.push_text_interned(&record.place_type)?;
        let display_point =
            point_lon_lat(&record.geometry).context("expected finite point geometry")?;
        let geometry = self.encode_geometry(&record.geometry, display_point)?;

        let entry = PlaceEntry {
            source_object_id: record.source.object_id,
            display_lon: geometry.display_lon,
            display_lat: geometry.display_lat,
            geometry_start: geometry.geometry_start,
            name_start: name.start,
            place_type_start: place_type.start,
            geometry_len: geometry.geometry_len,
            name_len: len16(name.len, "name")?,
            place_type_len: len16(place_type.len, "place_type")?,
            source_object: source_object_code(record.source.object_type) as u8,
            geometry_type: geometry.geometry_type,
            location_precision: LocationPrecisionCode::Centroid as u8,
            place_layer: place_layer_code(layer),
            _pad: [0; 4],
        };
        self.push_entry(EntryKind::Place, &entry)
    }

    pub fn write_postcode(&mut self, record: &PostcodeRecord) -> Result<RecordId> {
        let postcode = self.push_text_interned(&record.postcode)?;
        let derived_from = self.push_text_interned(&record.source.derived_from)?;
        let display_point =
            point_lon_lat(&record.geometry).context("expected finite point geometry")?;
        let geometry = self.encode_geometry(&record.geometry, display_point)?;

        let entry = PostcodeEntry {
            derived_record_count: record.source.record_count,
            display_lon: geometry.display_lon,
            display_lat: geometry.display_lat,
            geometry_start: geometry.geometry_start,
            postcode_start: postcode.start,
            derived_from_start: derived_from.start,
            geometry_len: geometry.geometry_len,
            postcode_len: len16(postcode.len, "postcode")?,
            derived_from_len: len16(derived_from.len, "derived_from")?,
            geometry_type: geometry.geometry_type,
            location_precision: LocationPrecisionCode::Centroid as u8,
            _pad: [0; 6],
        };
        self.push_entry(EntryKind::Postcode, &entry)
    }

    pub fn write_interpolation(&mut self, record: &InterpolationRecord) -> Result<RecordId> {
        let address = &record.address;
        let street = self.opt_interned(address.street.as_deref())?;
        let place = self.opt_interned(address.place.as_deref())?;
        let locality = self.opt_interned(address.locality.as_deref())?;
        let region = self.opt_interned(address.region.as_deref())?;
        let postcode = self.opt_interned(address.postcode.as_deref())?;
        let country = self.opt_interned(address.country.as_deref())?;
        let interpolation_type = self.push_text_interned(&record.interpolation.kind)?;
        let anchor_ids = self.push_text(&record.anchor_ids.join("\n"))?;
        let geometry = self.encode_geometry(&record.geometry, record.representative_point)?;

        let entry = InterpolationEntry {
            source_object_id: record.source.object_id,
            display_lon: geometry.display_lon,
            display_lat: geometry.display_lat,
            geometry_start: geometry.geometry_start,
            street_start: street.start,
            place_start: place.start,
            locality_start: locality.start,
            region_start: region.start,
            postcode_start: postcode.start,
            country_start: country.start,
            interpolation_type_start: interpolation_type.start,
            anchor_ids_start: anchor_ids.start,
            geometry_len: geometry.geometry_len,
            street_len: len16(street.len, "street")?,
            place_len: len16(place.len, "place")?,
            locality_len: len16(locality.len, "locality")?,
            region_len: len16(region.len, "region")?,
            postcode_len: len16(postcode.len, "postcode")?,
            country_len: len16(country.len, "country")?,
            interpolation_type_len: len16(interpolation_type.len, "interpolation_type")?,
            anchor_ids_len: len16(anchor_ids.len, "anchor_ids")?,
            interpolation_start: record.interpolation.start,
            interpolation_end: record.interpolation.end,
            interpolation_step: record.interpolation.step,
            source_object: source_object_code(record.source.object_type) as u8,
            geometry_type: geometry.geometry_type,
            location_precision: LocationPrecisionCode::Centroid as u8,
            _pad: [0; 5],
        };
        self.push_entry(EntryKind::Interpolation, &entry)
    }

    pub fn write_street(&mut self, record: &StreetRecord) -> Result<RecordId> {
        let name = self.push_text_interned(&record.name)?;
        let geometry = self.encode_geometry(&record.geometry, record.representative_point)?;

        let entry = StreetEntry {
            source_object_id: record.source.object_id,
            display_lon: geometry.display_lon,
            display_lat: geometry.display_lat,
            geometry_start: geometry.geometry_start,
            name_start: name.start,
            geometry_len: geometry.geometry_len,
            name_len: len16(name.len, "name")?,
            source_object: source_object_code(record.source.object_type) as u8,
            geometry_type: geometry.geometry_type,
            location_precision: LocationPrecisionCode::Centroid as u8,
            _pad: [0; 7],
        };
        self.push_entry(EntryKind::Street, &entry)
    }

    pub fn finish(&mut self) -> Result<()> {
        // Backfill the directory header: record_count then blob_len.
        self.directory
            .flush()
            .context("failed to flush records directory")?;
        self.directory.seek(SeekFrom::Start(8))?;
        self.directory.write_all(&self.record_count.to_le_bytes())?;
        self.directory.write_all(&self.blob_len.to_le_bytes())?;
        self.directory.flush()?;
        // sync_all commits the final file size to the directory entry so later
        // metadata reads (e.g. the build report's dir_size) see it while these
        // handles are still open.
        self.directory.get_ref().sync_all()?;

        backfill_len(&mut self.blob, self.blob_len, "records blob")?;
        backfill_len(&mut self.strings, self.strings_len, "records strings")?;
        backfill_len(
            &mut self.geometries,
            self.geometries_len,
            "records geometries",
        )?;
        Ok(())
    }

    fn push_entry<T: bytemuck::NoUninit>(
        &mut self,
        kind: EntryKind,
        entry: &T,
    ) -> Result<RecordId> {
        let offset = self.blob_len;
        let bytes = bytemuck::bytes_of(entry);
        self.blob.write_all(bytes)?;
        self.blob_len += bytes.len() as u64;
        let packed = pack_directory_entry(kind, offset)?;
        self.directory.write_all(&packed.to_le_bytes())?;
        let id = self.record_count;
        self.record_count += 1;
        Ok(id)
    }

    fn encode_geometry(
        &mut self,
        geometry: &Geometry,
        display_point: [f64; 2],
    ) -> Result<GeometryFields> {
        let display_lon = quantize_coordinate(display_point[0])?;
        let display_lat = quantize_coordinate(display_point[1])?;
        match &geometry.value {
            GeometryValue::Point { .. } => Ok(GeometryFields {
                display_lon,
                display_lat,
                geometry_type: GeometryType::Point as u8,
                geometry_start: 0,
                geometry_len: 0,
            }),
            GeometryValue::LineString { coordinates } => {
                let span = self.push_line_string(coordinates)?;
                Ok(GeometryFields {
                    display_lon,
                    display_lat,
                    geometry_type: GeometryType::Linestring as u8,
                    geometry_start: span.start,
                    geometry_len: span.len,
                })
            }
            other => bail!("unsupported stored record geometry: {other:?}"),
        }
    }

    fn opt_text(&mut self, value: Option<&str>) -> Result<Span> {
        match value {
            Some(value) => self.push_text(value),
            None => Ok(Span::EMPTY),
        }
    }

    fn opt_interned(&mut self, value: Option<&str>) -> Result<Span> {
        match value {
            Some(value) => self.push_text_interned(value),
            None => Ok(Span::EMPTY),
        }
    }

    fn push_text(&mut self, value: &str) -> Result<Span> {
        let start = self.strings_len;
        let len = u32::try_from(value.len()).context("record text field exceeds 4 GiB")?;
        self.strings.write_all(value.as_bytes())?;
        self.strings_len += value.len() as u64;
        Ok(Span { start, len })
    }

    fn push_text_interned(&mut self, value: &str) -> Result<Span> {
        if let Some(&span) = self.intern.get(value) {
            return Ok(span);
        }
        let span = self.push_text(value)?;
        self.intern.insert(value.to_string(), span);
        Ok(span)
    }

    fn push_line_string(&mut self, coordinates: &[geojson::Position]) -> Result<Span> {
        let start = self.geometries_len;
        let count =
            u32::try_from(coordinates.len()).context("line string has too many positions")?;
        let mut written = 0u64;
        self.geometries.write_all(&count.to_le_bytes())?;
        written += 4;
        for position in coordinates {
            let [lon, lat, ..] = position.as_slice() else {
                bail!("line string position is missing lon/lat");
            };
            self.geometries
                .write_all(&quantize_coordinate(*lon)?.to_le_bytes())?;
            self.geometries
                .write_all(&quantize_coordinate(*lat)?.to_le_bytes())?;
            written += 8;
        }
        self.geometries_len += written;
        let len = u32::try_from(written).context("line string geometry exceeds 4 GiB")?;
        Ok(Span { start, len })
    }
}

/// A decoded record together with the layer name resolved from its entry kind.
enum Decoded {
    Address(AddressRecord),
    Street(StreetRecord),
    Place(PlaceRecord),
    Postcode(PostcodeRecord),
    Interpolation(InterpolationRecord),
}

impl Decoded {
    fn id(&self) -> String {
        match self {
            Decoded::Address(record) => record.id(),
            Decoded::Street(record) => record.id(),
            Decoded::Place(record) => record.id(),
            Decoded::Postcode(record) => record.id(),
            Decoded::Interpolation(record) => record.id(),
        }
    }

    fn label(&self) -> String {
        match self {
            Decoded::Address(record) => record.label(),
            Decoded::Street(record) => record.label(),
            Decoded::Place(record) => record.label(),
            Decoded::Postcode(record) => record.label(),
            Decoded::Interpolation(record) => record.label(),
        }
    }
}

struct FullRecord {
    decoded: Decoded,
    layer: &'static str,
    point: RecordPoint,
    source: RecordSource,
}

impl RecordsArchiveReader {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Ok(Self {
            store: RecordsStore::open(path.as_ref())?,
        })
    }

    pub fn len(&self) -> u64 {
        self.store.record_count()
    }

    pub fn summary(&self, record_id: RecordId) -> Result<RecordSummary> {
        let full = self.decode_full(record_id)?;
        Ok(RecordSummary {
            id: full.decoded.id(),
            layer: full.layer.to_string(),
            label: full.decoded.label(),
            point: Some(full.point),
            source: full.source,
        })
    }

    pub fn record_json(&self, record_id: RecordId) -> Result<Value> {
        let full = self.decode_full(record_id)?;
        match &full.decoded {
            Decoded::Address(record) => record_json(full.layer, record),
            Decoded::Street(record) => record_json(full.layer, record),
            Decoded::Place(record) => record_json(full.layer, record),
            Decoded::Postcode(record) => record_json(full.layer, record),
            Decoded::Interpolation(record) => record_json(full.layer, record),
        }
    }

    pub fn records_json_by_layer(&self, layer: &str, limit: usize) -> Result<Vec<Value>> {
        let (wanted_kind, wanted_place) = layer_filter(layer)?;
        let mut records = Vec::new();
        for record_id in 0..self.store.record_count() {
            let (kind, bytes) = self.store.entry(record_id)?;
            if kind != wanted_kind {
                continue;
            }
            if let Some(place_layer) = wanted_place {
                let entry: &PlaceEntry = cast_entry(bytes)?;
                if decode_place_layer_code(entry.place_layer)? != place_layer {
                    continue;
                }
            }
            records.push(self.record_json(record_id)?);
            if limit > 0 && records.len() >= limit {
                break;
            }
        }
        Ok(records)
    }

    pub fn address(&self, record_id: RecordId) -> Result<Option<AddressRecord>> {
        let (kind, bytes) = self.store.entry(record_id)?;
        if kind != EntryKind::Address {
            return Ok(None);
        }
        Ok(Some(self.decode_address(cast_entry(bytes)?)?))
    }

    pub fn interpolation(&self, record_id: RecordId) -> Result<Option<InterpolationRecord>> {
        let (kind, bytes) = self.store.entry(record_id)?;
        if kind != EntryKind::Interpolation {
            return Ok(None);
        }
        Ok(Some(self.decode_interpolation(cast_entry(bytes)?)?))
    }

    pub fn street(&self, record_id: RecordId) -> Result<Option<StreetRecord>> {
        let (kind, bytes) = self.store.entry(record_id)?;
        if kind != EntryKind::Street {
            return Ok(None);
        }
        Ok(Some(self.decode_street(cast_entry(bytes)?)?))
    }

    pub fn context(&self, record_id: RecordId) -> Result<Option<ContextRecord>> {
        let full = self.decode_full(record_id)?;
        match &full.decoded {
            Decoded::Postcode(postcode) => Ok(Some(ContextRecord {
                id: postcode.id(),
                layer: "postcode".to_string(),
                label: postcode.label(),
                name: postcode.name(),
                postcode: Some(postcode.postcode.clone()),
                point: Some(full.point),
            })),
            Decoded::Place(place) => Ok(Some(ContextRecord {
                id: place.id(),
                layer: full.layer.to_string(),
                label: place.label(),
                name: place.name.clone(),
                postcode: None,
                point: Some(full.point),
            })),
            _ => Ok(None),
        }
    }

    fn decode_full(&self, record_id: RecordId) -> Result<FullRecord> {
        let (kind, bytes) = self.store.entry(record_id)?;
        Ok(match kind {
            EntryKind::Address => {
                let entry: &AddressEntry = cast_entry(bytes)?;
                FullRecord {
                    decoded: Decoded::Address(self.decode_address(entry)?),
                    layer: "address",
                    point: record_point(
                        entry.geometry_type,
                        entry.location_precision,
                        entry.display_lon,
                        entry.display_lat,
                    )?,
                    source: osm_source(entry.source_object, entry.source_object_id)?,
                }
            }
            EntryKind::Street => {
                let entry: &StreetEntry = cast_entry(bytes)?;
                FullRecord {
                    decoded: Decoded::Street(self.decode_street(entry)?),
                    layer: "street",
                    point: record_point(
                        entry.geometry_type,
                        entry.location_precision,
                        entry.display_lon,
                        entry.display_lat,
                    )?,
                    source: osm_source(entry.source_object, entry.source_object_id)?,
                }
            }
            EntryKind::Place => {
                let entry: &PlaceEntry = cast_entry(bytes)?;
                let (place_layer, record) = self.decode_place(entry)?;
                FullRecord {
                    layer: place_layer.as_str(),
                    point: record_point(
                        entry.geometry_type,
                        entry.location_precision,
                        entry.display_lon,
                        entry.display_lat,
                    )?,
                    source: osm_source(entry.source_object, entry.source_object_id)?,
                    decoded: Decoded::Place(record),
                }
            }
            EntryKind::Postcode => {
                let entry: &PostcodeEntry = cast_entry(bytes)?;
                FullRecord {
                    decoded: Decoded::Postcode(self.decode_postcode(entry)?),
                    layer: "postcode",
                    point: record_point(
                        entry.geometry_type,
                        entry.location_precision,
                        entry.display_lon,
                        entry.display_lat,
                    )?,
                    source: self.derived_source_record(entry)?,
                }
            }
            EntryKind::Interpolation => {
                let entry: &InterpolationEntry = cast_entry(bytes)?;
                FullRecord {
                    decoded: Decoded::Interpolation(self.decode_interpolation(entry)?),
                    layer: "interpolation",
                    point: record_point(
                        entry.geometry_type,
                        entry.location_precision,
                        entry.display_lon,
                        entry.display_lat,
                    )?,
                    source: osm_source(entry.source_object, entry.source_object_id)?,
                }
            }
        })
    }

    fn decode_address(&self, entry: &AddressEntry) -> Result<AddressRecord> {
        Ok(AddressRecord {
            address: AddressComponents {
                number: self.required_text(entry.number_start, entry.number_len, "number")?,
                street: self.optional_text(entry.street_start, entry.street_len)?,
                place: self.optional_text(entry.place_start, entry.place_len)?,
                unit: self.optional_text(entry.unit_start, entry.unit_len)?,
                locality: self.optional_text(entry.locality_start, entry.locality_len)?,
                region: self.optional_text(entry.region_start, entry.region_len)?,
                postcode: self.optional_text(entry.postcode_start, entry.postcode_len)?,
                country: self.optional_text(entry.country_start, entry.country_len)?,
            },
            geometry: self.decode_geometry(
                entry.geometry_type,
                entry.geometry_start,
                entry.geometry_len,
                entry.display_lon,
                entry.display_lat,
            )?,
            location_precision: decode_location_precision(LocationPrecisionCode::from_u8(
                entry.location_precision,
            )?)?,
            source: osm_provenance(entry.source_object, entry.source_object_id)?,
        })
    }

    fn decode_street(&self, entry: &StreetEntry) -> Result<StreetRecord> {
        Ok(StreetRecord {
            name: self.required_text(entry.name_start, entry.name_len, "name")?,
            geometry: self.decode_geometry(
                entry.geometry_type,
                entry.geometry_start,
                entry.geometry_len,
                entry.display_lon,
                entry.display_lat,
            )?,
            representative_point: [
                dequantize_coordinate(entry.display_lon),
                dequantize_coordinate(entry.display_lat),
            ],
            source: osm_provenance(entry.source_object, entry.source_object_id)?,
        })
    }

    fn decode_place(&self, entry: &PlaceEntry) -> Result<(PlaceLayer, PlaceRecord)> {
        Ok((
            decode_place_layer_code(entry.place_layer)?,
            PlaceRecord {
                name: self.required_text(entry.name_start, entry.name_len, "name")?,
                place_type: self.required_text(
                    entry.place_type_start,
                    entry.place_type_len,
                    "place_type",
                )?,
                geometry: self.decode_geometry(
                    entry.geometry_type,
                    entry.geometry_start,
                    entry.geometry_len,
                    entry.display_lon,
                    entry.display_lat,
                )?,
                source: osm_provenance(entry.source_object, entry.source_object_id)?,
            },
        ))
    }

    fn decode_postcode(&self, entry: &PostcodeEntry) -> Result<PostcodeRecord> {
        Ok(PostcodeRecord {
            postcode: self.required_text(entry.postcode_start, entry.postcode_len, "postcode")?,
            geometry: self.decode_geometry(
                entry.geometry_type,
                entry.geometry_start,
                entry.geometry_len,
                entry.display_lon,
                entry.display_lat,
            )?,
            source: DerivedSourceProvenance {
                dataset: "osm".to_string(),
                derived_from: self.required_text(
                    entry.derived_from_start,
                    entry.derived_from_len,
                    "derived_from",
                )?,
                record_count: entry.derived_record_count,
            },
        })
    }

    fn decode_interpolation(&self, entry: &InterpolationEntry) -> Result<InterpolationRecord> {
        let anchor_ids = self
            .optional_text(entry.anchor_ids_start, entry.anchor_ids_len)?
            .map(|value| value.split('\n').map(str::to_string).collect())
            .unwrap_or_default();
        Ok(InterpolationRecord {
            address: InterpolationAddressComponents {
                street: self.optional_text(entry.street_start, entry.street_len)?,
                place: self.optional_text(entry.place_start, entry.place_len)?,
                locality: self.optional_text(entry.locality_start, entry.locality_len)?,
                region: self.optional_text(entry.region_start, entry.region_len)?,
                postcode: self.optional_text(entry.postcode_start, entry.postcode_len)?,
                country: self.optional_text(entry.country_start, entry.country_len)?,
            },
            interpolation: InterpolationRange {
                kind: self.required_text(
                    entry.interpolation_type_start,
                    entry.interpolation_type_len,
                    "interpolation_type",
                )?,
                start: entry.interpolation_start,
                end: entry.interpolation_end,
                step: entry.interpolation_step,
            },
            anchor_ids,
            representative_point: [
                dequantize_coordinate(entry.display_lon),
                dequantize_coordinate(entry.display_lat),
            ],
            geometry: self.decode_geometry(
                entry.geometry_type,
                entry.geometry_start,
                entry.geometry_len,
                entry.display_lon,
                entry.display_lat,
            )?,
            source: osm_provenance(entry.source_object, entry.source_object_id)?,
        })
    }

    fn derived_source_record(&self, entry: &PostcodeEntry) -> Result<RecordSource> {
        Ok(RecordSource {
            dataset: "osm".to_string(),
            object_type: None,
            object_id: None,
            derived_from: Some(self.required_text(
                entry.derived_from_start,
                entry.derived_from_len,
                "derived_from",
            )?),
            record_count: Some(entry.derived_record_count),
        })
    }

    fn decode_geometry(
        &self,
        geometry_type: u8,
        geometry_start: u64,
        geometry_len: u32,
        display_lon: i32,
        display_lat: i32,
    ) -> Result<Geometry> {
        match GeometryType::from_u8(geometry_type)? {
            GeometryType::Point => Ok(point_geometry(
                dequantize_coordinate(display_lon),
                dequantize_coordinate(display_lat),
            )),
            GeometryType::Linestring => self.decode_line_string(geometry_start, geometry_len),
        }
    }

    fn decode_line_string(&self, start: u64, len: u32) -> Result<Geometry> {
        let bytes = slice_arena(self.store.geometries(), start, len, "geometry")?;
        if bytes.len() < 4 {
            bail!("stored line string geometry is missing its point count");
        }
        let count = u32::from_le_bytes(bytes[0..4].try_into().expect("count slice")) as usize;
        let expected = 4 + count * 8;
        if bytes.len() != expected {
            bail!(
                "stored line string geometry has {} bytes, expected {expected}",
                bytes.len()
            );
        }

        let mut coordinates = Vec::with_capacity(count);
        let mut offset = 4;
        for _ in 0..count {
            let lon = i32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("lon slice"));
            offset += 4;
            let lat = i32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("lat slice"));
            offset += 4;
            coordinates.push(vec![dequantize_coordinate(lon), dequantize_coordinate(lat)].into());
        }

        Ok(Geometry::new(GeometryValue::LineString { coordinates }))
    }

    fn required_text(&self, start: u64, len: u16, field: &str) -> Result<String> {
        let bytes = slice_arena(self.store.strings(), start, u32::from(len), field)?;
        String::from_utf8(bytes.to_vec()).with_context(|| format!("{field} is not valid UTF-8"))
    }

    fn optional_text(&self, start: u64, len: u16) -> Result<Option<String>> {
        if len == 0 {
            return Ok(None);
        }
        Ok(Some(self.required_text(start, len, "optional text")?))
    }
}

fn create_with_header(
    path: &Path,
    magic: &[u8; 8],
    placeholder_bytes: usize,
) -> Result<BufWriter<File>> {
    let file =
        File::create(path).with_context(|| format!("failed to create {}", path.display()))?;
    let mut writer = BufWriter::new(file);
    writer.write_all(magic)?;
    writer.write_all(&vec![0u8; placeholder_bytes])?;
    Ok(writer)
}

fn backfill_len(writer: &mut BufWriter<File>, value: u64, name: &str) -> Result<()> {
    writer
        .flush()
        .with_context(|| format!("failed to flush {name}"))?;
    writer.seek(SeekFrom::Start(8))?;
    writer.write_all(&value.to_le_bytes())?;
    writer.flush()?;
    writer.get_ref().sync_all()?;
    Ok(())
}

fn cast_entry<T: bytemuck::AnyBitPattern>(bytes: &[u8]) -> Result<&T> {
    bytemuck::try_from_bytes(bytes).map_err(|err| anyhow!("record entry layout mismatch: {err}"))
}

fn slice_arena<'a>(arena: &'a [u8], start: u64, len: u32, field: &str) -> Result<&'a [u8]> {
    let start = usize::try_from(start).with_context(|| format!("{field} offset is too large"))?;
    let len = len as usize;
    let end = start
        .checked_add(len)
        .with_context(|| format!("{field} range overflows"))?;
    arena
        .get(start..end)
        .with_context(|| format!("{field} range {start}..{end} is outside archive arena"))
}

fn record_json(layer: &str, record: &impl Serialize) -> Result<Value> {
    let mut value = serde_json::to_value(record)?;
    let Some(object) = value.as_object_mut() else {
        bail!("record JSON must be an object");
    };
    object.insert("layer".to_string(), json!(layer));
    Ok(value)
}

fn record_point(
    geometry_type: u8,
    location_precision: u8,
    display_lon: i32,
    display_lat: i32,
) -> Result<RecordPoint> {
    Ok(RecordPoint {
        lon: dequantize_coordinate(display_lon),
        lat: dequantize_coordinate(display_lat),
        precision: point_precision(geometry_type, location_precision)?,
    })
}

fn point_precision(geometry_type: u8, location_precision: u8) -> Result<RecordPointPrecision> {
    if GeometryType::from_u8(geometry_type)? == GeometryType::Linestring {
        return Ok(RecordPointPrecision::RepresentativePoint);
    }
    match decode_location_precision(LocationPrecisionCode::from_u8(location_precision)?)? {
        LocationPrecision::Point => Ok(RecordPointPrecision::Point),
        LocationPrecision::Centroid => Ok(RecordPointPrecision::Centroid),
    }
}

fn osm_source(source_object: u8, object_id: i64) -> Result<RecordSource> {
    Ok(RecordSource {
        dataset: "osm".to_string(),
        object_type: Some(decode_source_object(SourceObject::from_u8(source_object)?)?),
        object_id: Some(object_id),
        derived_from: None,
        record_count: None,
    })
}

fn osm_provenance(source_object: u8, object_id: i64) -> Result<SourceProvenance> {
    Ok(SourceProvenance {
        dataset: "osm".to_string(),
        object_type: decode_source_object(SourceObject::from_u8(source_object)?)?,
        object_id,
        tags: None,
    })
}

fn source_object_code(object_type: OsmObjectType) -> SourceObject {
    match object_type {
        OsmObjectType::Node => SourceObject::Node,
        OsmObjectType::Way => SourceObject::Way,
        OsmObjectType::Relation => SourceObject::Relation,
    }
}

fn decode_source_object(source_object: SourceObject) -> Result<OsmObjectType> {
    match source_object {
        SourceObject::Node => Ok(OsmObjectType::Node),
        SourceObject::Way => Ok(OsmObjectType::Way),
        SourceObject::Relation => Ok(OsmObjectType::Relation),
        SourceObject::Derived => bail!("derived source is not an OSM object"),
    }
}

fn location_precision_code(precision: LocationPrecision) -> LocationPrecisionCode {
    match precision {
        LocationPrecision::Point => LocationPrecisionCode::Point,
        LocationPrecision::Centroid => LocationPrecisionCode::Centroid,
    }
}

fn decode_location_precision(precision: LocationPrecisionCode) -> Result<LocationPrecision> {
    match precision {
        LocationPrecisionCode::Point => Ok(LocationPrecision::Point),
        LocationPrecisionCode::Centroid => Ok(LocationPrecision::Centroid),
    }
}

fn place_layer_code(layer: PlaceLayer) -> u8 {
    match layer {
        PlaceLayer::Country => 0,
        PlaceLayer::Region => 1,
        PlaceLayer::District => 2,
        PlaceLayer::Place => 3,
        PlaceLayer::Locality => 4,
        PlaceLayer::Neighbourhood => 5,
    }
}

fn decode_place_layer_code(code: u8) -> Result<PlaceLayer> {
    Ok(match code {
        0 => PlaceLayer::Country,
        1 => PlaceLayer::Region,
        2 => PlaceLayer::District,
        3 => PlaceLayer::Place,
        4 => PlaceLayer::Locality,
        5 => PlaceLayer::Neighbourhood,
        other => bail!("unknown place layer code {other}"),
    })
}

fn layer_filter(layer: &str) -> Result<(EntryKind, Option<PlaceLayer>)> {
    Ok(match layer {
        "address" => (EntryKind::Address, None),
        "street" => (EntryKind::Street, None),
        "postcode" => (EntryKind::Postcode, None),
        "interpolation" => (EntryKind::Interpolation, None),
        "country" => (EntryKind::Place, Some(PlaceLayer::Country)),
        "region" => (EntryKind::Place, Some(PlaceLayer::Region)),
        "district" => (EntryKind::Place, Some(PlaceLayer::District)),
        "place" => (EntryKind::Place, Some(PlaceLayer::Place)),
        "locality" => (EntryKind::Place, Some(PlaceLayer::Locality)),
        "neighbourhood" => (EntryKind::Place, Some(PlaceLayer::Neighbourhood)),
        other => bail!("unknown record layer: {other}"),
    })
}

fn quantize_coordinate(value: f64) -> Result<i32> {
    if !value.is_finite() {
        bail!("coordinate must be finite");
    }
    let scaled = (value * COORDINATE_SCALE).round();
    if scaled < i32::MIN as f64 || scaled > i32::MAX as f64 {
        bail!("coordinate {value} is out of range");
    }
    Ok(scaled as i32)
}

fn dequantize_coordinate(value: i32) -> f64 {
    value as f64 / COORDINATE_SCALE
}

fn len16(len: u32, field: &str) -> Result<u16> {
    u16::try_from(len).with_context(|| format!("{field} text field exceeds 64 KiB"))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use geojson::GeometryValue;

    use super::*;

    #[test]
    fn round_trips_address_record() {
        let temp_dir = temp_archive_path("address");
        let _ = fs::remove_dir_all(&temp_dir);

        let mut writer = RecordsArchiveWriter::create(&temp_dir).expect("writer");
        let record = AddressRecord {
            address: AddressComponents {
                number: "10".to_string(),
                street: Some("King Street".to_string()),
                place: None,
                unit: Some("1200".to_string()),
                locality: Some("Toronto".to_string()),
                region: Some("ON".to_string()),
                postcode: Some("M5H".to_string()),
                country: Some("CA".to_string()),
            },
            geometry: point_geometry(-79.3832, 43.6532),
            location_precision: LocationPrecision::Point,
            source: SourceProvenance {
                dataset: "osm".to_string(),
                object_type: OsmObjectType::Node,
                object_id: 1,
                tags: Some(BTreeMap::new()),
            },
        };
        writer.write_address(&record).expect("write");
        writer.finish().expect("finish");

        let reader = RecordsArchiveReader::open(&temp_dir).expect("reader");
        let address = reader.address(0).expect("read").expect("address");
        assert_eq!(address.id(), "osm:node:1");
        assert_eq!(
            address.label(),
            "10 King Street, 1200, Toronto, ON, M5H, CA"
        );
        assert_eq!(address.address.unit.as_deref(), Some("1200"));
        assert_eq!(address.location_precision, LocationPrecision::Point);
        assert_eq!(address.source.tags, None);

        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn round_trips_line_string_record() {
        let temp_dir = temp_archive_path("line");
        let _ = fs::remove_dir_all(&temp_dir);

        let mut writer = RecordsArchiveWriter::create(&temp_dir).expect("writer");
        let record = StreetRecord {
            name: "King Street".to_string(),
            geometry: Geometry::new(GeometryValue::LineString {
                coordinates: vec![vec![-79.4, 43.6].into(), vec![-79.3, 43.7].into()],
            }),
            representative_point: [-79.35, 43.65],
            source: SourceProvenance::osm(OsmObjectType::Way, 9),
        };
        writer.write_street(&record).expect("write");
        writer.finish().expect("finish");

        let reader = RecordsArchiveReader::open(&temp_dir).expect("reader");
        let street = reader.street(0).expect("read").expect("street");
        assert_eq!(street.representative_point, [-79.35, 43.65]);
        match street.geometry.value {
            GeometryValue::LineString { coordinates } => {
                assert_eq!(coordinates.len(), 2);
                assert_eq!(coordinates[0].as_slice(), &[-79.4, 43.6]);
            }
            other => panic!("unexpected geometry {other:?}"),
        }

        let _ = fs::remove_dir_all(temp_dir);
    }

    fn temp_archive_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "open-geocode-records-{}-{name}",
            std::process::id()
        ))
    }
}
