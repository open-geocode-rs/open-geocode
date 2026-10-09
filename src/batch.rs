use std::{
    collections::HashMap,
    fs::File,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use csv::{ReaderBuilder, WriterBuilder};
use rayon::prelude::*;

use crate::{
    pack::{RecordPointPrecision, RecordSource},
    record::OsmObjectType,
    search::{AddressGeocodeOptions, PackTextSearcher},
};

#[derive(Debug, Clone)]
pub struct BatchGeocodeOptions {
    pub pack: Option<PathBuf>,
    pub input: PathBuf,
    pub output: PathBuf,
    pub audit: PathBuf,
    pub address_field_groups: Vec<Vec<String>>,
    pub locality_field: Option<String>,
    pub region_field: Option<String>,
    pub postcode_field: Option<String>,
    pub layer: Option<String>,
    pub limit: usize,
    pub lat_column: String,
    pub lon_column: String,
    pub join: Option<CoordinateJoinOptions>,
}

#[derive(Debug, Clone)]
pub struct CoordinateJoinOptions {
    pub path: PathBuf,
    pub key_column: String,
    pub lat_column: String,
    pub lon_column: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchGeocodeReport {
    pub rows: usize,
    pub resolved: usize,
}

#[derive(Debug, Clone, Default)]
struct AuditFields {
    status: String,
    query: String,
    score: String,
    record_id: String,
    id: String,
    layer: String,
    label: String,
    lat: String,
    lon: String,
    precision: String,
    source_dataset: String,
    source_object_type: String,
    source_object_id: String,
    source_derived_from: String,
    source_record_count: String,
    reason: String,
}

#[derive(Debug, Clone)]
struct CsvTable {
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
}

#[derive(Debug, Clone, Copy)]
struct CoordinateColumns {
    lat_index: usize,
    lon_index: usize,
}

#[derive(Debug, Clone)]
struct JoinedCoordinate {
    lat: String,
    lon: String,
    audit_fields: HashMap<String, String>,
}

const AUDIT_HEADERS: [&str; 16] = [
    "geocode_status",
    "geocode_query",
    "geocode_score",
    "geocode_record_id",
    "geocode_id",
    "geocode_layer",
    "geocode_label",
    "geocode_lat",
    "geocode_lon",
    "geocode_precision",
    "geocode_source_dataset",
    "geocode_source_object_type",
    "geocode_source_object_id",
    "geocode_source_derived_from",
    "geocode_source_record_count",
    "geocode_reason",
];

/// Rows read, geocoded and written per chunk. Chunks keep memory flat for
/// inputs of any size.
const BATCH_CHUNK_ROWS: usize = 50_000;

/// Geocode a CSV in streaming chunks. Within a chunk, rows are looked up in
/// parallel and grouped by postcode and place, so neighbouring lookups touch
/// the same index and record pages; output rows keep the input order.
pub fn run_batch_geocode(options: BatchGeocodeOptions) -> Result<BatchGeocodeReport> {
    if options.address_field_groups.is_empty() == options.join.is_none() {
        bail!("provide either --address-fields or --join-coordinates-from, but not both");
    }
    ensure_distinct_paths(&options.input, &options.output, &options.audit)?;

    let mut reader = ReaderBuilder::new()
        .flexible(true)
        .from_path(&options.input)
        .with_context(|| format!("failed to open {}", options.input.display()))?;
    let original_headers = reader
        .headers()
        .with_context(|| {
            format!(
                "failed to read CSV headers from {}",
                options.input.display()
            )
        })?
        .iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
    let lookup = header_lookup(&original_headers);
    let mut output_headers = original_headers.clone();
    let coordinate_columns = ensure_coordinate_columns(
        &mut output_headers,
        &options.lat_column,
        &options.lon_column,
    )?;
    let mut audit_headers = original_headers.clone();
    audit_headers.extend(AUDIT_HEADERS.iter().map(|header| (*header).to_string()));

    let resolver = match options.join.as_ref() {
        Some(join) => Resolver::Join {
            lookup: read_join_lookup(join)?,
            key_index: find_header(&lookup, &join.key_column)
                .with_context(|| format!("missing join key column {:?}", join.key_column))?,
        },
        None => {
            let pack = options
                .pack
                .as_ref()
                .context("--pack is required when geocoding address fields")?;
            Resolver::Pack(Box::new(
                PackTextSearcher::open(pack)
                    .with_context(|| format!("failed to open Pack {}", pack.display()))?,
            ))
        }
    };

    let mut output = create_csv(&options.output, &output_headers)?;
    let mut audit = create_csv(&options.audit, &audit_headers)?;
    let mut report = BatchGeocodeReport {
        rows: 0,
        resolved: 0,
    };
    let mut records = reader.records();
    loop {
        let chunk = records
            .by_ref()
            .take(BATCH_CHUNK_ROWS)
            .map(|record| {
                Ok(record
                    .with_context(|| {
                        format!("failed to read CSV record from {}", options.input.display())
                    })?
                    .iter()
                    .map(str::to_string)
                    .collect::<Vec<_>>())
            })
            .collect::<Result<Vec<_>>>()?;
        if chunk.is_empty() {
            break;
        }
        let audits = resolver.resolve(&options, &lookup, &chunk)?;
        for (row, row_audit) in chunk.iter().zip(audits) {
            let mut output_row = padded_row(row, output_headers.len());
            if !row_audit.lat.trim().is_empty() && !row_audit.lon.trim().is_empty() {
                set_coordinate_values(
                    &mut output_row,
                    coordinate_columns,
                    &row_audit.lat,
                    &row_audit.lon,
                );
                report.resolved += 1;
            }
            output.write_record(&output_row)?;
            audit.write_record(audit_record(&original_headers, row, &row_audit))?;
            report.rows += 1;
        }
    }
    output.finish()?;
    audit.finish()?;
    Ok(report)
}

enum Resolver {
    Pack(Box<PackTextSearcher>),
    Join {
        lookup: HashMap<String, JoinedCoordinate>,
        key_index: usize,
    },
}

impl Resolver {
    fn resolve(
        &self,
        options: &BatchGeocodeOptions,
        headers: &HashMap<String, usize>,
        rows: &[Vec<String>],
    ) -> Result<Vec<AuditFields>> {
        match self {
            Resolver::Join { lookup, key_index } => Ok(rows
                .iter()
                .map(|row| {
                    let key = row.get(*key_index).map(String::as_str).unwrap_or_default();
                    match lookup.get(key).filter(|joined| {
                        !joined.lat.trim().is_empty() && !joined.lon.trim().is_empty()
                    }) {
                        Some(joined) => audit_from_joined(joined),
                        None => AuditFields {
                            status: "unresolved".to_string(),
                            reason: "join key not found or joined coordinates are blank"
                                .to_string(),
                            ..AuditFields::default()
                        },
                    }
                })
                .collect()),
            Resolver::Pack(searcher) => {
                let mut order = (0..rows.len()).collect::<Vec<_>>();
                order.sort_by_cached_key(|&index| locality_key(&rows[index], options, headers));
                let mut results = order
                    .into_par_iter()
                    .map(|index| {
                        geocode_row(searcher, options, headers, &rows[index])
                            .map(|audit| (index, audit))
                    })
                    .collect::<Result<Vec<_>>>()?;
                results.sort_unstable_by_key(|(index, _)| *index);
                Ok(results.into_iter().map(|(_, audit)| audit).collect())
            }
        }
    }
}

/// Group key: rows in the same postcode and place are geocoded together.
fn locality_key(
    row: &[String],
    options: &BatchGeocodeOptions,
    headers: &HashMap<String, usize>,
) -> [String; 4] {
    let field = |name: Option<&str>| {
        field_value(row, headers, name)
            .map(|value| value.to_lowercase())
            .unwrap_or_default()
    };
    [
        field(options.postcode_field.as_deref()).replace(' ', ""),
        field(options.region_field.as_deref()),
        field(options.locality_field.as_deref()),
        options
            .address_field_groups
            .first()
            .and_then(|group| compose_query(row, headers, group))
            .unwrap_or_default()
            .to_lowercase(),
    ]
}

pub fn parse_field_groups(values: &[String]) -> Result<Vec<Vec<String>>> {
    values
        .iter()
        .map(|value| {
            let fields = value
                .split(',')
                .map(str::trim)
                .filter(|field| !field.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>();
            if fields.is_empty() {
                bail!("address field group must include at least one column")
            }
            Ok(fields)
        })
        .collect()
}

fn geocode_row(
    searcher: &PackTextSearcher,
    options: &BatchGeocodeOptions,
    headers: &HashMap<String, usize>,
    row: &[String],
) -> Result<AuditFields> {
    let locality = field_value(row, headers, options.locality_field.as_deref());
    let region = field_value(row, headers, options.region_field.as_deref());
    let postcode = field_value(row, headers, options.postcode_field.as_deref());

    let mut tried_queries = Vec::new();
    for group in &options.address_field_groups {
        let Some(address) = compose_query(row, headers, group) else {
            continue;
        };
        let response = searcher.geocode_address(AddressGeocodeOptions {
            address: address.clone(),
            locality: locality.clone(),
            region: region.clone(),
            postcode: postcode.clone(),
            limit: options.limit,
            layer: options.layer.clone(),
        })?;
        tried_queries.push(address);
        if let Some(response) = response {
            let point = response
                .hit
                .record
                .point
                .expect("geocode_address only returns hits with points");
            return Ok(AuditFields {
                status: "resolved".to_string(),
                query: response.query,
                score: response.hit.score.to_string(),
                record_id: response.hit.record_id.to_string(),
                id: response.hit.record.id,
                layer: response.hit.record.layer,
                label: response.hit.record.label,
                lat: format_coordinate(point.lat),
                lon: format_coordinate(point.lon),
                precision: precision_name(point.precision).to_string(),
                ..source_audit_fields(response.hit.record.source)
            });
        }
    }

    Ok(AuditFields {
        status: "unresolved".to_string(),
        query: tried_queries.join(" | "),
        reason: "no engine result with matching coordinates and context".to_string(),
        ..AuditFields::default()
    })
}

fn read_csv(path: &Path) -> Result<CsvTable> {
    let mut reader = ReaderBuilder::new()
        .flexible(true)
        .from_path(path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    let headers = reader
        .headers()
        .with_context(|| format!("failed to read CSV headers from {}", path.display()))?
        .iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
    let mut rows = Vec::new();
    for record in reader.records() {
        rows.push(
            record
                .with_context(|| format!("failed to read CSV record from {}", path.display()))?
                .iter()
                .map(str::to_string)
                .collect::<Vec<_>>(),
        );
    }
    Ok(CsvTable { headers, rows })
}

fn create_csv(path: &Path, headers: &[String]) -> Result<PendingCsv> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let file_name = path
        .file_name()
        .with_context(|| format!("{} is not a file path", path.display()))?
        .to_string_lossy();
    let temporary = path.with_file_name(format!(".{file_name}.{}.tmp", uuid::Uuid::new_v4()));
    let file = File::create(&temporary)
        .with_context(|| format!("failed to create {}", temporary.display()))?;
    let mut writer = WriterBuilder::new().from_writer(file);
    writer
        .write_record(headers)
        .with_context(|| format!("failed to write headers to {}", path.display()))?;
    Ok(PendingCsv {
        writer,
        temporary,
        path: path.to_path_buf(),
        finished: false,
    })
}

/// A CSV written beside its destination and moved into place only once it is
/// complete, so a failed run never leaves a truncated file behind.
struct PendingCsv {
    writer: csv::Writer<File>,
    temporary: PathBuf,
    path: PathBuf,
    finished: bool,
}

impl PendingCsv {
    fn write_record<I, T>(&mut self, record: I) -> Result<()>
    where
        I: IntoIterator<Item = T>,
        T: AsRef<[u8]>,
    {
        self.writer
            .write_record(record)
            .with_context(|| format!("failed to write a row to {}", self.path.display()))
    }

    fn finish(mut self) -> Result<()> {
        self.writer
            .flush()
            .with_context(|| format!("failed to flush {}", self.path.display()))?;
        std::fs::rename(&self.temporary, &self.path)
            .with_context(|| format!("failed to write {}", self.path.display()))?;
        self.finished = true;
        Ok(())
    }
}

impl Drop for PendingCsv {
    fn drop(&mut self) {
        if !self.finished {
            let _ = std::fs::remove_file(&self.temporary);
        }
    }
}

/// Refuse output paths that name the input or each other, however they are
/// spelled.
fn ensure_distinct_paths(input: &Path, output: &Path, audit: &Path) -> Result<()> {
    let resolve = |path: &Path| -> Result<PathBuf> {
        if path.exists() {
            return path
                .canonicalize()
                .with_context(|| format!("failed to resolve {}", path.display()));
        }
        let parent = match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
            _ => PathBuf::from("."),
        };
        let parent = parent.canonicalize().unwrap_or(parent);
        Ok(parent.join(path.file_name().unwrap_or_default()))
    };
    let (input, output_path, audit_path) = (resolve(input)?, resolve(output)?, resolve(audit)?);
    for (first, second, first_name, second_name) in [
        (&output_path, &input, "--output", "--input"),
        (&audit_path, &input, "--audit", "--input"),
        (&output_path, &audit_path, "--output", "--audit"),
    ] {
        if first == second {
            bail!(
                "{first_name} and {second_name} name the same file ({}); choose a different path",
                first.display()
            );
        }
    }
    Ok(())
}

fn read_join_lookup(join: &CoordinateJoinOptions) -> Result<HashMap<String, JoinedCoordinate>> {
    let table = read_csv(&join.path)?;
    let lookup = header_lookup(&table.headers);
    let key_index = find_header(&lookup, &join.key_column)
        .with_context(|| format!("missing join key column {:?}", join.key_column))?;
    let lat_index = find_header(&lookup, &join.lat_column)
        .with_context(|| format!("missing join latitude column {:?}", join.lat_column))?;
    let lon_index = find_header(&lookup, &join.lon_column)
        .with_context(|| format!("missing join longitude column {:?}", join.lon_column))?;
    let mut coordinates = HashMap::new();
    for row in table.rows {
        let key = row.get(key_index).cloned().unwrap_or_default();
        if key.trim().is_empty() {
            continue;
        }
        let lat = row.get(lat_index).cloned().unwrap_or_default();
        let lon = row.get(lon_index).cloned().unwrap_or_default();
        let audit_fields = table
            .headers
            .iter()
            .enumerate()
            .filter_map(|(index, header)| {
                row.get(index).map(|value| (header.clone(), value.clone()))
            })
            .collect::<HashMap<_, _>>();
        let candidate = JoinedCoordinate {
            lat,
            lon,
            audit_fields,
        };
        coordinates
            .entry(key)
            .and_modify(|existing: &mut JoinedCoordinate| {
                if existing.lat.trim().is_empty() || existing.lon.trim().is_empty() {
                    *existing = candidate.clone();
                }
            })
            .or_insert(candidate);
    }
    Ok(coordinates)
}

fn header_lookup(headers: &[String]) -> HashMap<String, usize> {
    headers
        .iter()
        .enumerate()
        .map(|(index, header)| (header.to_ascii_lowercase(), index))
        .collect()
}

fn find_header(headers: &HashMap<String, usize>, name: &str) -> Option<usize> {
    headers.get(&name.to_ascii_lowercase()).copied()
}

fn ensure_coordinate_columns(
    headers: &mut Vec<String>,
    lat_column: &str,
    lon_column: &str,
) -> Result<CoordinateColumns> {
    if lat_column.eq_ignore_ascii_case(lon_column) {
        bail!("latitude and longitude columns must be different");
    }
    let mut lookup = header_lookup(headers);
    let lat_index = find_header(&lookup, lat_column).unwrap_or_else(|| {
        headers.push(lat_column.to_string());
        headers.len() - 1
    });
    lookup = header_lookup(headers);
    let lon_index = find_header(&lookup, lon_column).unwrap_or_else(|| {
        headers.push(lon_column.to_string());
        headers.len() - 1
    });
    Ok(CoordinateColumns {
        lat_index,
        lon_index,
    })
}

fn padded_row(row: &[String], len: usize) -> Vec<String> {
    let mut padded = row.to_vec();
    padded.resize(len, String::new());
    padded
}

fn set_coordinate_values(row: &mut [String], columns: CoordinateColumns, lat: &str, lon: &str) {
    row[columns.lat_index] = lat.to_string();
    row[columns.lon_index] = lon.to_string();
}

fn field_value(
    row: &[String],
    headers: &HashMap<String, usize>,
    field: Option<&str>,
) -> Option<String> {
    let index = find_header(headers, field?)?;
    let value = row.get(index)?.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn compose_query(
    row: &[String],
    headers: &HashMap<String, usize>,
    fields: &[String],
) -> Option<String> {
    let parts = fields
        .iter()
        .filter_map(|field| {
            let index = find_header(headers, field)?;
            let value = row
                .get(index)?
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            (!value.is_empty()).then_some(value)
        })
        .collect::<Vec<_>>();
    (!parts.is_empty()).then(|| parts.join(", "))
}

fn audit_record(headers: &[String], row: &[String], audit: &AuditFields) -> Vec<String> {
    let mut record = padded_row(row, headers.len());
    record.extend([
        audit.status.clone(),
        audit.query.clone(),
        audit.score.clone(),
        audit.record_id.clone(),
        audit.id.clone(),
        audit.layer.clone(),
        audit.label.clone(),
        audit.lat.clone(),
        audit.lon.clone(),
        audit.precision.clone(),
        audit.source_dataset.clone(),
        audit.source_object_type.clone(),
        audit.source_object_id.clone(),
        audit.source_derived_from.clone(),
        audit.source_record_count.clone(),
        audit.reason.clone(),
    ]);
    record
}

fn audit_from_joined(joined: &JoinedCoordinate) -> AuditFields {
    AuditFields {
        status: joined
            .audit_fields
            .get("geocode_status")
            .cloned()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "joined".to_string()),
        query: joined
            .audit_fields
            .get("geocode_query")
            .cloned()
            .unwrap_or_default(),
        score: joined
            .audit_fields
            .get("geocode_score")
            .cloned()
            .unwrap_or_default(),
        record_id: joined
            .audit_fields
            .get("geocode_record_id")
            .cloned()
            .unwrap_or_default(),
        id: joined
            .audit_fields
            .get("geocode_id")
            .cloned()
            .unwrap_or_default(),
        layer: joined
            .audit_fields
            .get("geocode_layer")
            .cloned()
            .unwrap_or_default(),
        label: joined
            .audit_fields
            .get("geocode_label")
            .cloned()
            .unwrap_or_default(),
        lat: joined.lat.clone(),
        lon: joined.lon.clone(),
        precision: joined
            .audit_fields
            .get("geocode_precision")
            .cloned()
            .unwrap_or_default(),
        source_dataset: joined
            .audit_fields
            .get("geocode_source_dataset")
            .cloned()
            .unwrap_or_default(),
        source_object_type: joined
            .audit_fields
            .get("geocode_source_object_type")
            .cloned()
            .unwrap_or_default(),
        source_object_id: joined
            .audit_fields
            .get("geocode_source_object_id")
            .cloned()
            .unwrap_or_default(),
        source_derived_from: joined
            .audit_fields
            .get("geocode_source_derived_from")
            .cloned()
            .unwrap_or_default(),
        source_record_count: joined
            .audit_fields
            .get("geocode_source_record_count")
            .cloned()
            .unwrap_or_default(),
        reason: joined
            .audit_fields
            .get("geocode_reason")
            .cloned()
            .unwrap_or_default(),
    }
}

fn source_audit_fields(source: RecordSource) -> AuditFields {
    AuditFields {
        source_dataset: source.dataset,
        source_object_type: source.object_type.map(object_type_name).unwrap_or_default(),
        source_object_id: source
            .object_id
            .map(|object_id| object_id.to_string())
            .unwrap_or_default(),
        source_derived_from: source.derived_from.unwrap_or_default(),
        source_record_count: source
            .record_count
            .map(|count| count.to_string())
            .unwrap_or_default(),
        ..AuditFields::default()
    }
}

fn object_type_name(object_type: OsmObjectType) -> String {
    match object_type {
        OsmObjectType::Node => "node",
        OsmObjectType::Way => "way",
        OsmObjectType::Relation => "relation",
    }
    .to_string()
}

fn precision_name(precision: RecordPointPrecision) -> &'static str {
    match precision {
        RecordPointPrecision::Point => "point",
        RecordPointPrecision::Centroid => "centroid",
        RecordPointPrecision::Estimated => "estimated",
        RecordPointPrecision::RepresentativePoint => "representative_point",
    }
}

fn format_coordinate(value: f64) -> String {
    format!("{value:.7}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_repeated_field_groups() {
        let groups = parse_field_groups(&[
            "street1, city".to_string(),
            "street2,postalcode".to_string(),
        ])
        .expect("parse groups");
        assert_eq!(
            groups,
            vec![
                vec!["street1".to_string(), "city".to_string()],
                vec!["street2".to_string(), "postalcode".to_string()]
            ]
        );
    }

    #[test]
    fn adds_missing_coordinate_columns() {
        let mut headers = vec!["id".to_string(), "address".to_string()];
        let columns = ensure_coordinate_columns(&mut headers, "lat", "lon").expect("columns");
        assert_eq!(headers, vec!["id", "address", "lat", "lon"]);
        assert_eq!(columns.lat_index, 2);
        assert_eq!(columns.lon_index, 3);
    }

    #[test]
    fn reuses_existing_coordinate_columns_case_insensitively() {
        let mut headers = vec!["id".to_string(), "LAT".to_string(), "Lon".to_string()];
        let columns = ensure_coordinate_columns(&mut headers, "lat", "lon").expect("columns");
        assert_eq!(headers, vec!["id", "LAT", "Lon"]);
        assert_eq!(columns.lat_index, 1);
        assert_eq!(columns.lon_index, 2);
    }

    #[test]
    fn composes_query_from_configured_fields() {
        let headers = vec![
            "street".to_string(),
            "city".to_string(),
            "postalcode".to_string(),
        ];
        let lookup = header_lookup(&headers);
        let row = vec![
            "  1333   Sheppard Ave E ".to_string(),
            "Toronto".to_string(),
            "M3C 1J4".to_string(),
        ];
        let query = compose_query(
            &row,
            &lookup,
            &["street".to_string(), "postalcode".to_string()],
        );
        assert_eq!(query.as_deref(), Some("1333 Sheppard Ave E, M3C 1J4"));
    }

    #[test]
    fn geocodes_streamed_rows_in_input_order() {
        use crate::{
            pack::PackWriter,
            record::{
                AddressComponents, AddressRecord, LocationPrecision, SourceProvenance,
                point_geometry,
            },
        };

        let root =
            std::env::temp_dir().join(format!("open-geocode-batch-{}", uuid::Uuid::new_v4()));
        let mut writer = PackWriter::create(root.join("pack")).expect("writer");
        for (object_id, number, street, lon) in [
            (1, "10", "King Street", -79.40),
            (2, "20", "Queen Street", -79.41),
        ] {
            writer
                .write(
                    &AddressRecord {
                        address: AddressComponents {
                            number: number.into(),
                            street: Some(street.into()),
                            place: None,
                            unit: None,
                            locality: None,
                            region: None,
                            postcode: None,
                            country: None,
                        },
                        geometry: point_geometry(lon, 43.6),
                        location_precision: LocationPrecision::Point,
                        source: SourceProvenance::osm(OsmObjectType::Node, object_id),
                    }
                    .into(),
                    None,
                )
                .expect("write");
        }
        writer.finish().expect("finish");

        let input = root.join("in.csv");
        std::fs::write(
            &input,
            "id,street\na,20 Queen St\nb,nowhere at all\nc,10 King Street\n",
        )
        .expect("input");
        let report = run_batch_geocode(BatchGeocodeOptions {
            pack: Some(root.join("pack")),
            input,
            output: root.join("out.csv"),
            audit: root.join("audit.csv"),
            address_field_groups: vec![vec!["street".into()]],
            locality_field: None,
            region_field: None,
            postcode_field: None,
            layer: None,
            limit: 5,
            lat_column: "lat".into(),
            lon_column: "lon".into(),
            join: None,
        })
        .expect("batch");
        assert_eq!(
            report,
            BatchGeocodeReport {
                rows: 3,
                resolved: 2
            }
        );
        let output = read_csv(&root.join("out.csv")).expect("output");
        assert_eq!(output.headers, vec!["id", "street", "lat", "lon"]);
        assert_eq!(
            output.rows[0],
            vec!["a", "20 Queen St", "43.6000000", "-79.4100000"]
        );
        assert_eq!(output.rows[1], vec!["b", "nowhere at all", "", ""]);
        assert_eq!(
            output.rows[2],
            vec!["c", "10 King Street", "43.6000000", "-79.4000000"]
        );
        let audit = read_csv(&root.join("audit.csv")).expect("audit");
        assert_eq!(audit.rows[1][2], "unresolved");
        assert_eq!(audit.rows[2][6], "osm:node:1");
    }

    #[test]
    fn refuses_to_overwrite_the_input() {
        let root =
            std::env::temp_dir().join(format!("open-geocode-batch-same-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("dir");
        let input = root.join("in.csv");
        let contents = "id,street
a,10 King Street
";
        std::fs::write(&input, contents).expect("input");
        let coordinates = root.join("coordinates.csv");
        std::fs::write(
            &coordinates,
            "id,lat,lon
a,43.6,-79.4
",
        )
        .expect("coordinates");
        let options = |output: PathBuf, audit: PathBuf| BatchGeocodeOptions {
            pack: None,
            input: input.clone(),
            output,
            audit,
            address_field_groups: Vec::new(),
            locality_field: None,
            region_field: None,
            postcode_field: None,
            layer: None,
            limit: 5,
            lat_column: "lat".into(),
            lon_column: "lon".into(),
            join: Some(CoordinateJoinOptions {
                path: coordinates.clone(),
                key_column: "id".into(),
                lat_column: "lat".into(),
                lon_column: "lon".into(),
            }),
        };
        // The same file spelled differently must be caught too.
        let same = root.join(".").join("in.csv");
        for (output, audit) in [
            (same.clone(), root.join("audit.csv")),
            (root.join("out.csv"), same),
            (root.join("out.csv"), root.join("out.csv")),
        ] {
            let error = run_batch_geocode(options(output, audit)).expect_err("same file");
            assert!(format!("{error:#}").contains("same file"), "{error:#}");
            assert_eq!(std::fs::read_to_string(&input).expect("input"), contents);
        }
    }

    #[test]
    fn pads_rows_before_writing_coordinates() {
        let mut row = padded_row(&["1".to_string()], 3);
        set_coordinate_values(
            &mut row,
            CoordinateColumns {
                lat_index: 1,
                lon_index: 2,
            },
            "43.1",
            "-79.2",
        );
        assert_eq!(row, vec!["1", "43.1", "-79.2"]);
    }
}
