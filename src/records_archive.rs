use std::{
    fs::{self, File},
    io::{BufWriter, Seek, SeekFrom, Write},
    path::Path,
};

use anyhow::{Context, Result, bail};
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
    records_flatdata::open_geocode as fd,
};

const COORDINATE_SCALE: f64 = 10_000_000.0;
const FLATDATA_PADDING: [u8; 8] = [0; 8];

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

struct CommonRecord {
    id: String,
    label: String,
    name: String,
}

pub struct RecordsArchiveWriter {
    builder: fd::RecordsArchiveBuilder,
    records: BufWriter<File>,
    record_bytes: u64,
    strings: Vec<u8>,
    geometries: Vec<u8>,
}

pub struct RecordsArchiveReader {
    archive: fd::RecordsArchive,
}

impl RecordsArchiveWriter {
    pub fn create(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        fs::create_dir_all(path).with_context(|| format!("failed to create {}", path.display()))?;

        let storage = flatdata::FileResourceStorage::new(path);
        let builder = fd::RecordsArchiveBuilder::new(storage)
            .with_context(|| format!("failed to create records archive in {}", path.display()))?;

        let schema_path = path.join("records.schema");
        let mut schema = File::create(&schema_path)
            .with_context(|| format!("failed to create {}", schema_path.display()))?;
        schema.write_all(fd::schema::records_archive::resources::RECORDS.as_bytes())?;

        let records_path = path.join("records");
        let mut records = BufWriter::new(
            File::create(&records_path)
                .with_context(|| format!("failed to create {}", records_path.display()))?,
        );
        records.write_all(&0_u64.to_le_bytes())?;

        Ok(Self {
            builder,
            records,
            record_bytes: 0,
            strings: Vec::new(),
            geometries: Vec::new(),
        })
    }

    pub fn write_address(&mut self, record: &AddressRecord) -> Result<()> {
        let mut stored = self.stored_record(
            fd::RecordLayer::Address,
            &record.id,
            &record.label,
            &record.name,
        )?;
        self.fill_osm_source(&mut stored, &record.source);
        self.fill_address_components(&mut stored, &record.address)?;
        stored.set_location_precision(location_precision_code(record.location_precision));
        self.fill_geometry(
            &mut stored,
            &record.geometry,
            point_coordinates(&record.geometry)?,
        )?;
        self.write_stored_record(&stored)
    }

    pub fn write_place(&mut self, record: &PlaceRecord, layer: PlaceLayer) -> Result<()> {
        let mut stored = self.stored_record(
            place_record_layer(layer),
            &record.id,
            &record.label,
            &record.name,
        )?;
        self.fill_osm_source(&mut stored, &record.source);
        let place_type = self.push_text(&record.place_type)?;
        stored.set_place_type_start(place_type.start);
        stored.set_place_type_len(place_type.len);
        stored.set_location_precision(fd::LocationPrecisionCode::Centroid);
        self.fill_geometry(
            &mut stored,
            &record.geometry,
            point_coordinates(&record.geometry)?,
        )?;
        self.write_stored_record(&stored)
    }

    pub fn write_postcode(&mut self, record: &PostcodeRecord) -> Result<()> {
        let mut stored = self.stored_record(
            fd::RecordLayer::Postcode,
            &record.id,
            &record.label,
            &record.name,
        )?;
        stored.set_source_object(fd::SourceObject::Derived);
        stored.set_source_object_id(0);
        stored.set_derived_record_count(record.source.record_count);
        let postcode = self.push_text(&record.postcode)?;
        stored.set_postcode_start(postcode.start);
        stored.set_postcode_len(postcode.len);
        let derived_from = self.push_text(&record.source.derived_from)?;
        stored.set_derived_from_start(derived_from.start);
        stored.set_derived_from_len(derived_from.len);
        stored.set_location_precision(fd::LocationPrecisionCode::Centroid);
        self.fill_geometry(
            &mut stored,
            &record.geometry,
            point_coordinates(&record.geometry)?,
        )?;
        self.write_stored_record(&stored)
    }

    pub fn write_interpolation(&mut self, record: &InterpolationRecord) -> Result<()> {
        let mut stored = self.stored_record(
            fd::RecordLayer::Interpolation,
            &record.id,
            &record.label,
            &record.name,
        )?;
        self.fill_osm_source(&mut stored, &record.source);
        self.fill_interpolation_address_components(&mut stored, &record.address)?;
        stored.set_interpolation_start(record.interpolation.start);
        stored.set_interpolation_end(record.interpolation.end);
        stored.set_interpolation_step(record.interpolation.step);
        let interpolation_type = self.push_text(&record.interpolation.kind)?;
        stored.set_interpolation_type_start(interpolation_type.start);
        stored.set_interpolation_type_len(interpolation_type.len);
        let anchor_ids = self.push_text(&record.anchor_ids.join("\n"))?;
        stored.set_anchor_ids_start(anchor_ids.start);
        stored.set_anchor_ids_len(anchor_ids.len);
        stored.set_location_precision(fd::LocationPrecisionCode::Centroid);
        self.fill_geometry(&mut stored, &record.geometry, record.representative_point)?;
        self.write_stored_record(&stored)
    }

    pub fn write_street(&mut self, record: &StreetRecord) -> Result<()> {
        let mut stored = self.stored_record(
            fd::RecordLayer::Street,
            &record.id,
            &record.label,
            &record.name,
        )?;
        self.fill_osm_source(&mut stored, &record.source);
        stored.set_location_precision(fd::LocationPrecisionCode::Centroid);
        self.fill_geometry(&mut stored, &record.geometry, record.representative_point)?;
        self.write_stored_record(&stored)
    }

    pub fn finish(&mut self) -> Result<()> {
        self.records.write_all(&FLATDATA_PADDING)?;
        self.records.flush()?;
        self.records.seek(SeekFrom::Start(0))?;
        self.records.write_all(&self.record_bytes.to_le_bytes())?;
        self.records.flush()?;

        self.builder
            .set_strings(&self.strings)
            .context("failed to write records string arena")?;
        self.builder
            .set_geometries(&self.geometries)
            .context("failed to write records geometry arena")?;
        Ok(())
    }

    fn stored_record(
        &mut self,
        layer: fd::RecordLayer,
        id: &str,
        label: &str,
        name: &str,
    ) -> Result<fd::StoredRecord> {
        let mut stored = fd::StoredRecord::new();
        stored.set_layer(layer);
        stored.set_reserved(0);
        let id = self.push_text(id)?;
        let label = self.push_text(label)?;
        let name = self.push_text(name)?;
        set_span(&mut stored, "id", id)?;
        set_span(&mut stored, "label", label)?;
        set_span(&mut stored, "name", name)?;
        Ok(stored)
    }

    fn write_stored_record(&mut self, stored: &fd::StoredRecord) -> Result<()> {
        self.records.write_all(stored.as_bytes())?;
        self.record_bytes += stored.as_bytes().len() as u64;
        Ok(())
    }

    fn fill_osm_source(&self, stored: &mut fd::StoredRecord, source: &SourceProvenance) {
        stored.set_source_object(source_object_code(source.object_type));
        stored.set_source_object_id(source.object_id);
        stored.set_derived_record_count(0);
    }

    fn fill_address_components(
        &mut self,
        stored: &mut fd::StoredRecord,
        address: &AddressComponents,
    ) -> Result<()> {
        let number = self.push_text(&address.number)?;
        stored.set_number_start(number.start);
        stored.set_number_len(number.len);
        self.set_optional_text(stored, TextField::Street, address.street.as_deref())?;
        self.set_optional_text(stored, TextField::Place, address.place.as_deref())?;
        self.set_optional_text(stored, TextField::Unit, address.unit.as_deref())?;
        self.set_optional_text(stored, TextField::Locality, address.locality.as_deref())?;
        self.set_optional_text(stored, TextField::Region, address.region.as_deref())?;
        self.set_optional_text(stored, TextField::Postcode, address.postcode.as_deref())?;
        self.set_optional_text(stored, TextField::Country, address.country.as_deref())
    }

    fn fill_interpolation_address_components(
        &mut self,
        stored: &mut fd::StoredRecord,
        address: &InterpolationAddressComponents,
    ) -> Result<()> {
        self.set_optional_text(stored, TextField::Street, address.street.as_deref())?;
        self.set_optional_text(stored, TextField::Place, address.place.as_deref())?;
        self.set_optional_text(stored, TextField::Locality, address.locality.as_deref())?;
        self.set_optional_text(stored, TextField::Region, address.region.as_deref())?;
        self.set_optional_text(stored, TextField::Postcode, address.postcode.as_deref())?;
        self.set_optional_text(stored, TextField::Country, address.country.as_deref())
    }

    fn set_optional_text(
        &mut self,
        stored: &mut fd::StoredRecord,
        field: TextField,
        value: Option<&str>,
    ) -> Result<()> {
        let span = match value {
            Some(value) => self.push_text(value)?,
            None => Span::EMPTY,
        };
        set_optional_span(stored, field, span);
        Ok(())
    }

    fn fill_geometry(
        &mut self,
        stored: &mut fd::StoredRecord,
        geometry: &Geometry,
        display_point: [f64; 2],
    ) -> Result<()> {
        stored.set_display_lon(quantize_coordinate(display_point[0])?);
        stored.set_display_lat(quantize_coordinate(display_point[1])?);

        match &geometry.value {
            GeometryValue::Point { .. } => {
                stored.set_geometry_type(fd::GeometryType::Point);
                stored.set_geometry_start(0);
                stored.set_geometry_len(0);
            }
            GeometryValue::LineString { coordinates } => {
                stored.set_geometry_type(fd::GeometryType::Linestring);
                let span = self.push_line_string(coordinates)?;
                stored.set_geometry_start(span.start);
                stored.set_geometry_len(span.len);
            }
            other => bail!("unsupported stored record geometry: {other:?}"),
        }

        Ok(())
    }

    fn push_text(&mut self, value: &str) -> Result<Span> {
        let start = self.strings.len() as u64;
        let len = u32::try_from(value.len()).context("record text field exceeds 4 GiB")?;
        self.strings.extend_from_slice(value.as_bytes());
        Ok(Span { start, len })
    }

    fn push_line_string(&mut self, coordinates: &[geojson::Position]) -> Result<Span> {
        let start = self.geometries.len() as u64;
        let count =
            u32::try_from(coordinates.len()).context("line string has too many positions")?;
        self.geometries.extend_from_slice(&count.to_le_bytes());
        for position in coordinates {
            let [lon, lat, ..] = position.as_slice() else {
                bail!("line string position is missing lon/lat");
            };
            self.geometries
                .extend_from_slice(&quantize_coordinate(*lon)?.to_le_bytes());
            self.geometries
                .extend_from_slice(&quantize_coordinate(*lat)?.to_le_bytes());
        }
        let len = u32::try_from(self.geometries.len() as u64 - start)
            .context("line string geometry exceeds 4 GiB")?;
        Ok(Span { start, len })
    }
}

impl RecordsArchiveReader {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let storage = flatdata::FileResourceStorage::new(path);
        let archive = fd::RecordsArchive::open(storage)
            .with_context(|| format!("failed to open records archive in {}", path.display()))?;
        Ok(Self { archive })
    }

    pub fn len(&self) -> u64 {
        self.archive.records().len() as u64
    }

    pub fn summary(&self, record_id: RecordId) -> Result<RecordSummary> {
        self.summary_from_stored(self.stored(record_id)?)
    }

    pub fn record_json(&self, record_id: RecordId) -> Result<Value> {
        self.record_json_from_stored(self.stored(record_id)?)
    }

    pub fn records_json_by_layer(&self, layer: &str, limit: usize) -> Result<Vec<Value>> {
        let wanted = layer_code(layer)?;
        let mut records = Vec::new();
        for stored in self.archive.records() {
            if stored.layer() != wanted {
                continue;
            }
            records.push(self.record_json_from_stored(stored)?);
            if limit > 0 && records.len() >= limit {
                break;
            }
        }
        Ok(records)
    }

    pub fn address(&self, record_id: RecordId) -> Result<Option<AddressRecord>> {
        let stored = self.stored(record_id)?;
        if stored.layer() != fd::RecordLayer::Address {
            return Ok(None);
        }
        Ok(Some(self.decode_address(stored)?))
    }

    pub fn interpolation(&self, record_id: RecordId) -> Result<Option<InterpolationRecord>> {
        let stored = self.stored(record_id)?;
        if stored.layer() != fd::RecordLayer::Interpolation {
            return Ok(None);
        }
        Ok(Some(self.decode_interpolation(stored)?))
    }

    pub fn street(&self, record_id: RecordId) -> Result<Option<StreetRecord>> {
        let stored = self.stored(record_id)?;
        if stored.layer() != fd::RecordLayer::Street {
            return Ok(None);
        }
        Ok(Some(self.decode_street(stored)?))
    }

    pub fn context(&self, record_id: RecordId) -> Result<Option<ContextRecord>> {
        let stored = self.stored(record_id)?;
        self.context_from_stored(stored)
    }

    fn stored(&self, record_id: RecordId) -> Result<&fd::StoredRecord> {
        let index = usize::try_from(record_id)
            .with_context(|| format!("record id {record_id} is too large"))?;
        let stored = self
            .archive
            .records()
            .get(index)
            .with_context(|| format!("record row {record_id} is out of range"))?;
        self.validate(stored)?;
        Ok(stored)
    }

    fn validate(&self, stored: &fd::StoredRecord) -> Result<()> {
        if stored.reserved() != 0 {
            bail!("stored record reserved bits are non-zero");
        }
        record_layer_name(stored.layer())?;
        Ok(())
    }

    fn summary_from_stored(&self, stored: &fd::StoredRecord) -> Result<RecordSummary> {
        let common = self.common_record(stored)?;
        Ok(RecordSummary {
            id: common.id,
            layer: record_layer_name(stored.layer())?.to_string(),
            label: common.label,
            point: Some(record_point(stored)?),
            source: self.record_source(stored)?,
        })
    }

    fn record_json_from_stored(&self, stored: &fd::StoredRecord) -> Result<Value> {
        match stored.layer() {
            fd::RecordLayer::Address => record_json(
                record_layer_name(stored.layer())?,
                &self.decode_address(stored)?,
            ),
            fd::RecordLayer::Country
            | fd::RecordLayer::District
            | fd::RecordLayer::Locality
            | fd::RecordLayer::Neighbourhood
            | fd::RecordLayer::Place
            | fd::RecordLayer::Region => {
                let (_, record) = self.decode_place(stored)?;
                record_json(record_layer_name(stored.layer())?, &record)
            }
            fd::RecordLayer::Postcode => record_json(
                record_layer_name(stored.layer())?,
                &self.decode_postcode(stored)?,
            ),
            fd::RecordLayer::Interpolation => record_json(
                record_layer_name(stored.layer())?,
                &self.decode_interpolation(stored)?,
            ),
            fd::RecordLayer::Street => record_json(
                record_layer_name(stored.layer())?,
                &self.decode_street(stored)?,
            ),
            other => bail!("unsupported record layer in archive: {other:?}"),
        }
    }

    fn decode_address(&self, stored: &fd::StoredRecord) -> Result<AddressRecord> {
        let common = self.common_record(stored)?;
        Ok(AddressRecord {
            id: common.id,
            label: common.label,
            name: common.name,
            address: self.address_components(stored)?,
            geometry: self.decode_geometry(stored)?,
            location_precision: decode_location_precision(stored.location_precision())?,
            source: self.decode_osm_source(stored)?,
        })
    }

    fn decode_place(&self, stored: &fd::StoredRecord) -> Result<(PlaceLayer, PlaceRecord)> {
        let common = self.common_record(stored)?;
        Ok((
            decode_place_layer(stored.layer())?,
            PlaceRecord {
                id: common.id,
                label: common.label,
                name: common.name,
                place_type: self.required_text(
                    stored.place_type_start(),
                    stored.place_type_len(),
                    "place_type",
                )?,
                geometry: self.decode_geometry(stored)?,
                source: self.decode_osm_source(stored)?,
            },
        ))
    }

    fn decode_postcode(&self, stored: &fd::StoredRecord) -> Result<PostcodeRecord> {
        let common = self.common_record(stored)?;
        Ok(PostcodeRecord {
            id: common.id,
            label: common.label,
            name: common.name,
            postcode: self.required_text(
                stored.postcode_start(),
                stored.postcode_len(),
                "postcode",
            )?,
            geometry: self.decode_geometry(stored)?,
            source: self.derived_source(stored)?,
        })
    }

    fn decode_interpolation(&self, stored: &fd::StoredRecord) -> Result<InterpolationRecord> {
        let common = self.common_record(stored)?;
        let anchor_ids = self
            .optional_text(stored.anchor_ids_start(), stored.anchor_ids_len())?
            .map(|value| value.split('\n').map(str::to_string).collect())
            .unwrap_or_default();
        Ok(InterpolationRecord {
            id: common.id,
            label: common.label,
            name: common.name,
            address: self.interpolation_address_components(stored)?,
            interpolation: InterpolationRange {
                kind: self.required_text(
                    stored.interpolation_type_start(),
                    stored.interpolation_type_len(),
                    "interpolation_type",
                )?,
                start: stored.interpolation_start(),
                end: stored.interpolation_end(),
                step: stored.interpolation_step(),
            },
            anchor_ids,
            representative_point: display_point(stored),
            geometry: self.decode_geometry(stored)?,
            source: self.decode_osm_source(stored)?,
        })
    }

    fn decode_street(&self, stored: &fd::StoredRecord) -> Result<StreetRecord> {
        let common = self.common_record(stored)?;
        Ok(StreetRecord {
            id: common.id,
            label: common.label,
            name: common.name,
            geometry: self.decode_geometry(stored)?,
            representative_point: display_point(stored),
            source: self.decode_osm_source(stored)?,
        })
    }

    fn context_from_stored(&self, stored: &fd::StoredRecord) -> Result<Option<ContextRecord>> {
        let Some(postcode) = (stored.layer() == fd::RecordLayer::Postcode)
            .then(|| self.required_text(stored.postcode_start(), stored.postcode_len(), "postcode"))
            .transpose()?
        else {
            if !is_place_layer(stored.layer()) {
                return Ok(None);
            }
            let common = self.common_record(stored)?;
            return Ok(Some(ContextRecord {
                id: common.id,
                layer: record_layer_name(stored.layer())?.to_string(),
                label: common.label,
                name: common.name,
                postcode: None,
                point: Some(record_point(stored)?),
            }));
        };

        let common = self.common_record(stored)?;
        Ok(Some(ContextRecord {
            id: common.id,
            layer: "postcode".to_string(),
            label: common.label,
            name: common.name,
            postcode: Some(postcode),
            point: Some(record_point(stored)?),
        }))
    }

    fn common_record(&self, stored: &fd::StoredRecord) -> Result<CommonRecord> {
        Ok(CommonRecord {
            id: self.required_text(stored.id_start(), stored.id_len(), "id")?,
            label: self.required_text(stored.label_start(), stored.label_len(), "label")?,
            name: self.required_text(stored.name_start(), stored.name_len(), "name")?,
        })
    }

    fn address_components(&self, stored: &fd::StoredRecord) -> Result<AddressComponents> {
        Ok(AddressComponents {
            number: self.required_text(stored.number_start(), stored.number_len(), "number")?,
            street: self.optional_text(stored.street_start(), stored.street_len())?,
            place: self.optional_text(stored.place_start(), stored.place_len())?,
            unit: self.optional_text(stored.unit_start(), stored.unit_len())?,
            locality: self.optional_text(stored.locality_start(), stored.locality_len())?,
            region: self.optional_text(stored.region_start(), stored.region_len())?,
            postcode: self.optional_text(stored.postcode_start(), stored.postcode_len())?,
            country: self.optional_text(stored.country_start(), stored.country_len())?,
        })
    }

    fn interpolation_address_components(
        &self,
        stored: &fd::StoredRecord,
    ) -> Result<InterpolationAddressComponents> {
        Ok(InterpolationAddressComponents {
            street: self.optional_text(stored.street_start(), stored.street_len())?,
            place: self.optional_text(stored.place_start(), stored.place_len())?,
            locality: self.optional_text(stored.locality_start(), stored.locality_len())?,
            region: self.optional_text(stored.region_start(), stored.region_len())?,
            postcode: self.optional_text(stored.postcode_start(), stored.postcode_len())?,
            country: self.optional_text(stored.country_start(), stored.country_len())?,
        })
    }

    fn record_source(&self, stored: &fd::StoredRecord) -> Result<RecordSource> {
        match stored.source_object() {
            fd::SourceObject::Derived => {
                let source = self.derived_source(stored)?;
                Ok(RecordSource {
                    dataset: source.dataset,
                    object_type: None,
                    object_id: None,
                    derived_from: Some(source.derived_from),
                    record_count: Some(source.record_count),
                })
            }
            source_object => Ok(RecordSource {
                dataset: "osm".to_string(),
                object_type: Some(decode_source_object(source_object)?),
                object_id: Some(stored.source_object_id()),
                derived_from: None,
                record_count: None,
            }),
        }
    }

    fn derived_source(&self, stored: &fd::StoredRecord) -> Result<DerivedSourceProvenance> {
        Ok(DerivedSourceProvenance {
            dataset: "osm".to_string(),
            derived_from: self.required_text(
                stored.derived_from_start(),
                stored.derived_from_len(),
                "derived_from",
            )?,
            record_count: stored.derived_record_count(),
        })
    }

    fn decode_geometry(&self, stored: &fd::StoredRecord) -> Result<Geometry> {
        match stored.geometry_type() {
            fd::GeometryType::Point => Ok(point_geometry(
                dequantize_coordinate(stored.display_lon()),
                dequantize_coordinate(stored.display_lat()),
            )),
            fd::GeometryType::Linestring => self.decode_line_string(stored),
            other => bail!("unsupported geometry type in archive: {other:?}"),
        }
    }

    fn decode_line_string(&self, stored: &fd::StoredRecord) -> Result<Geometry> {
        let bytes = self.bytes(
            self.archive.geometries().as_bytes(),
            stored.geometry_start(),
            stored.geometry_len(),
            "geometry",
        )?;
        if bytes.len() < 4 {
            bail!("stored line string geometry is missing its point count");
        }
        let count = u32::from_le_bytes(bytes[0..4].try_into().expect("count slice")) as usize;
        let expected = 4 + count * 16;
        if bytes.len() != expected {
            bail!(
                "stored line string geometry has {} bytes, expected {expected}",
                bytes.len()
            );
        }

        let mut coordinates = Vec::with_capacity(count);
        let mut offset = 4;
        for _ in 0..count {
            let lon = i64::from_le_bytes(bytes[offset..offset + 8].try_into().expect("lon slice"));
            offset += 8;
            let lat = i64::from_le_bytes(bytes[offset..offset + 8].try_into().expect("lat slice"));
            offset += 8;
            coordinates.push(vec![dequantize_coordinate(lon), dequantize_coordinate(lat)].into());
        }

        Ok(Geometry::new(GeometryValue::LineString { coordinates }))
    }

    fn decode_osm_source(&self, stored: &fd::StoredRecord) -> Result<SourceProvenance> {
        Ok(SourceProvenance {
            dataset: "osm".to_string(),
            object_type: decode_source_object(stored.source_object())?,
            object_id: stored.source_object_id(),
            tags: None,
        })
    }

    fn required_text(&self, start: u64, len: u32, field: &str) -> Result<String> {
        let bytes = self.bytes(self.archive.strings().as_bytes(), start, len, field)?;
        String::from_utf8(bytes.to_vec()).with_context(|| format!("{field} is not valid UTF-8"))
    }

    fn optional_text(&self, start: u64, len: u32) -> Result<Option<String>> {
        if len == 0 {
            return Ok(None);
        }
        Ok(Some(self.required_text(start, len, "optional text")?))
    }

    fn bytes<'a>(&self, arena: &'a [u8], start: u64, len: u32, field: &str) -> Result<&'a [u8]> {
        let start =
            usize::try_from(start).with_context(|| format!("{field} offset is too large"))?;
        let len = len as usize;
        let end = start
            .checked_add(len)
            .with_context(|| format!("{field} range overflows"))?;
        arena
            .get(start..end)
            .with_context(|| format!("{field} range {start}..{end} is outside archive arena"))
    }
}

fn set_span(stored: &mut fd::StoredRecord, field: &str, span: Span) -> Result<()> {
    match field {
        "id" => {
            stored.set_id_start(span.start);
            stored.set_id_len(span.len);
        }
        "label" => {
            stored.set_label_start(span.start);
            stored.set_label_len(span.len);
        }
        "name" => {
            stored.set_name_start(span.start);
            stored.set_name_len(span.len);
        }
        other => bail!("unsupported stored text field: {other}"),
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextField {
    Street,
    Place,
    Unit,
    Locality,
    Region,
    Postcode,
    Country,
}

fn set_optional_span(stored: &mut fd::StoredRecord, field: TextField, span: Span) {
    match field {
        TextField::Street => {
            stored.set_street_start(span.start);
            stored.set_street_len(span.len);
        }
        TextField::Place => {
            stored.set_place_start(span.start);
            stored.set_place_len(span.len);
        }
        TextField::Unit => {
            stored.set_unit_start(span.start);
            stored.set_unit_len(span.len);
        }
        TextField::Locality => {
            stored.set_locality_start(span.start);
            stored.set_locality_len(span.len);
        }
        TextField::Region => {
            stored.set_region_start(span.start);
            stored.set_region_len(span.len);
        }
        TextField::Postcode => {
            stored.set_postcode_start(span.start);
            stored.set_postcode_len(span.len);
        }
        TextField::Country => {
            stored.set_country_start(span.start);
            stored.set_country_len(span.len);
        }
    }
}

fn record_json(layer: &str, record: &impl Serialize) -> Result<Value> {
    let mut value = serde_json::to_value(record)?;
    let Some(object) = value.as_object_mut() else {
        bail!("record JSON must be an object");
    };
    object.insert("layer".to_string(), json!(layer));
    Ok(value)
}

fn layer_code(layer: &str) -> Result<fd::RecordLayer> {
    match layer {
        "address" => Ok(fd::RecordLayer::Address),
        "country" => Ok(fd::RecordLayer::Country),
        "district" => Ok(fd::RecordLayer::District),
        "interpolation" => Ok(fd::RecordLayer::Interpolation),
        "locality" => Ok(fd::RecordLayer::Locality),
        "neighbourhood" => Ok(fd::RecordLayer::Neighbourhood),
        "place" => Ok(fd::RecordLayer::Place),
        "postcode" => Ok(fd::RecordLayer::Postcode),
        "region" => Ok(fd::RecordLayer::Region),
        "street" => Ok(fd::RecordLayer::Street),
        other => bail!("unknown record layer: {other}"),
    }
}

fn record_layer_name(layer: fd::RecordLayer) -> Result<&'static str> {
    match layer {
        fd::RecordLayer::Address => Ok("address"),
        fd::RecordLayer::Country => Ok("country"),
        fd::RecordLayer::District => Ok("district"),
        fd::RecordLayer::Interpolation => Ok("interpolation"),
        fd::RecordLayer::Locality => Ok("locality"),
        fd::RecordLayer::Neighbourhood => Ok("neighbourhood"),
        fd::RecordLayer::Place => Ok("place"),
        fd::RecordLayer::Postcode => Ok("postcode"),
        fd::RecordLayer::Region => Ok("region"),
        fd::RecordLayer::Street => Ok("street"),
        other => bail!("unsupported record layer in archive: {other:?}"),
    }
}

fn place_record_layer(layer: PlaceLayer) -> fd::RecordLayer {
    match layer {
        PlaceLayer::Country => fd::RecordLayer::Country,
        PlaceLayer::Region => fd::RecordLayer::Region,
        PlaceLayer::District => fd::RecordLayer::District,
        PlaceLayer::Place => fd::RecordLayer::Place,
        PlaceLayer::Locality => fd::RecordLayer::Locality,
        PlaceLayer::Neighbourhood => fd::RecordLayer::Neighbourhood,
    }
}

fn is_place_layer(layer: fd::RecordLayer) -> bool {
    matches!(
        layer,
        fd::RecordLayer::Country
            | fd::RecordLayer::District
            | fd::RecordLayer::Locality
            | fd::RecordLayer::Neighbourhood
            | fd::RecordLayer::Place
            | fd::RecordLayer::Region
    )
}

fn decode_place_layer(layer: fd::RecordLayer) -> Result<PlaceLayer> {
    match layer {
        fd::RecordLayer::Country => Ok(PlaceLayer::Country),
        fd::RecordLayer::District => Ok(PlaceLayer::District),
        fd::RecordLayer::Locality => Ok(PlaceLayer::Locality),
        fd::RecordLayer::Neighbourhood => Ok(PlaceLayer::Neighbourhood),
        fd::RecordLayer::Place => Ok(PlaceLayer::Place),
        fd::RecordLayer::Region => Ok(PlaceLayer::Region),
        other => bail!("record layer {other:?} is not a place layer"),
    }
}

fn record_point(stored: &fd::StoredRecord) -> Result<RecordPoint> {
    Ok(RecordPoint {
        lon: dequantize_coordinate(stored.display_lon()),
        lat: dequantize_coordinate(stored.display_lat()),
        precision: point_precision(stored)?,
    })
}

fn display_point(stored: &fd::StoredRecord) -> [f64; 2] {
    [
        dequantize_coordinate(stored.display_lon()),
        dequantize_coordinate(stored.display_lat()),
    ]
}

fn point_precision(stored: &fd::StoredRecord) -> Result<RecordPointPrecision> {
    if stored.geometry_type() == fd::GeometryType::Linestring {
        return Ok(RecordPointPrecision::RepresentativePoint);
    }
    match decode_location_precision(stored.location_precision())? {
        LocationPrecision::Point => Ok(RecordPointPrecision::Point),
        LocationPrecision::Centroid => Ok(RecordPointPrecision::Centroid),
    }
}

fn source_object_code(object_type: OsmObjectType) -> fd::SourceObject {
    match object_type {
        OsmObjectType::Node => fd::SourceObject::Node,
        OsmObjectType::Way => fd::SourceObject::Way,
        OsmObjectType::Relation => fd::SourceObject::Relation,
    }
}

fn decode_source_object(source_object: fd::SourceObject) -> Result<OsmObjectType> {
    match source_object {
        fd::SourceObject::Node => Ok(OsmObjectType::Node),
        fd::SourceObject::Way => Ok(OsmObjectType::Way),
        fd::SourceObject::Relation => Ok(OsmObjectType::Relation),
        fd::SourceObject::Derived => bail!("derived source is not an OSM object"),
    }
}

fn location_precision_code(precision: LocationPrecision) -> fd::LocationPrecisionCode {
    match precision {
        LocationPrecision::Point => fd::LocationPrecisionCode::Point,
        LocationPrecision::Centroid => fd::LocationPrecisionCode::Centroid,
    }
}

fn decode_location_precision(precision: fd::LocationPrecisionCode) -> Result<LocationPrecision> {
    match precision {
        fd::LocationPrecisionCode::Point => Ok(LocationPrecision::Point),
        fd::LocationPrecisionCode::Centroid => Ok(LocationPrecision::Centroid),
        other => bail!("unsupported location precision in archive: {other:?}"),
    }
}

fn point_coordinates(geometry: &Geometry) -> Result<[f64; 2]> {
    let GeometryValue::Point { coordinates } = &geometry.value else {
        bail!("expected point geometry");
    };
    let [lon, lat, ..] = coordinates.as_slice() else {
        bail!("point geometry is missing lon/lat");
    };
    Ok([*lon, *lat])
}

fn quantize_coordinate(value: f64) -> Result<i64> {
    if !value.is_finite() {
        bail!("coordinate must be finite");
    }
    Ok((value * COORDINATE_SCALE).round() as i64)
}

fn dequantize_coordinate(value: i64) -> f64 {
    value as f64 / COORDINATE_SCALE
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
            id: "osm:node:1".to_string(),
            label: "10 King Street, Toronto, ON".to_string(),
            name: "10 King Street".to_string(),
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
        assert_eq!(address.id, "osm:node:1");
        assert_eq!(address.label, "10 King Street, Toronto, ON");
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
            id: "osm:way:9".to_string(),
            label: "King Street".to_string(),
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
