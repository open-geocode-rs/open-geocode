use std::{
    borrow::Cow,
    collections::HashMap,
    fmt,
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use crate::error::{Error, Result};
use csv::{ByteRecord, ReaderBuilder, WriterBuilder};
use rayon::prelude::*;

use crate::{
    api::{self, Contexts, ForwardFeature},
    pack::Reader,
    search::{AddressGeocodeOptions, TextSearcher},
};

/// The audit columns' file name in `contract/`, which the contract tests
/// keep equal to [`audit_columns`].
pub const AUDIT_COLUMNS_FILE: &str = "batch-audit-columns.txt";

/// A column of the input: by its header name, matched without regard to
/// ASCII case, or by its place from 0, which also tells apart columns that
/// share a name or have none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Column {
    Named(String),
    At(usize),
}

#[derive(Clone)]
pub struct GeocodeOptions {
    pub pack: Option<PathBuf>,
    pub input: PathBuf,
    pub output: PathBuf,
    /// Where to write the audit CSV; none is written without one.
    pub audit: Option<PathBuf>,
    /// Whether the input's first row names its columns. Without one, the
    /// columns are named `column_1`, `column_2` and so on, and the output
    /// starts with those names.
    pub header_row: bool,
    /// Groups of columns, each joined into one query; tried in order until
    /// one resolves.
    pub address_field_groups: Vec<Vec<Column>>,
    pub locality_field: Option<Column>,
    pub region_field: Option<Column>,
    pub postcode_field: Option<Column>,
    pub layer: Option<String>,
    pub limit: usize,
    pub lat_column: String,
    pub lon_column: String,
    pub join: Option<CoordinateJoinOptions>,
    /// Told about every row as it is geocoded.
    pub observer: Option<Arc<dyn Observer>>,
    /// Set to stop the run: it then fails with [`Cancelled`] and writes
    /// no output.
    pub cancel: Arc<AtomicBool>,
}

impl fmt::Debug for GeocodeOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GeocodeOptions")
            .field("pack", &self.pack)
            .field("input", &self.input)
            .field("output", &self.output)
            .field("audit", &self.audit)
            .field("header_row", &self.header_row)
            .field("address_field_groups", &self.address_field_groups)
            .field("layer", &self.layer)
            .field("limit", &self.limit)
            .finish_non_exhaustive()
    }
}

impl Default for GeocodeOptions {
    fn default() -> Self {
        Self {
            pack: None,
            input: PathBuf::new(),
            output: PathBuf::new(),
            audit: None,
            header_row: true,
            address_field_groups: Vec::new(),
            locality_field: None,
            region_field: None,
            postcode_field: None,
            layer: None,
            limit: 10,
            lat_column: "lat".to_string(),
            lon_column: "lon".to_string(),
            join: None,
            observer: None,
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// One geocoded row, as a [`Observer`] is told about it.
#[derive(Debug)]
pub struct Row<'a> {
    /// The row's place in the input, from 0, header excluded.
    pub index: usize,
    /// The address text searched, or the candidates tried, joined by ` | `.
    pub query: &'a str,
    /// What the row resolved to.
    pub feature: Option<&'a ForwardFeature>,
    /// How long the row took to geocode.
    pub elapsed: Duration,
}

/// Receives each row of a batch run as it is geocoded. Rows of one chunk are
/// geocoded in parallel and in an order of their own, so calls come from
/// several threads and not in input order.
pub trait Observer: Send + Sync {
    fn row(&self, row: &Row);
}

/// The error a batch run returns when it was asked to stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancelled;

impl fmt::Display for Cancelled {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("batch run cancelled")
    }
}

impl std::error::Error for Cancelled {}

#[derive(Debug, Clone)]
pub struct CoordinateJoinOptions {
    pub path: PathBuf,
    pub key_column: String,
    pub lat_column: String,
    pub lon_column: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GeocodeReport {
    pub rows: usize,
    pub resolved: usize,
}

/// What the audit CSV appends to an input row: how the row went, then the
/// columns of the feature it resolved to ([`api::flat_columns`]).
#[derive(Debug, Clone, Default)]
struct AuditFields {
    status: String,
    query: String,
    reason: String,
    lat: String,
    lon: String,
    /// One value per [`api::flat_columns`] column, empty when unresolved.
    result: Vec<String>,
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

/// The input columns a run reads, by place.
struct Fields {
    groups: Vec<Vec<usize>>,
    locality: Option<usize>,
    region: Option<usize>,
    postcode: Option<usize>,
}

impl Fields {
    /// The places of the columns `options` names, among `headers`. A column
    /// the input does not have is an error, not a column that is always
    /// empty.
    fn resolve(options: &GeocodeOptions, headers: &[String]) -> Result<Self> {
        let lookup = header_lookup(headers);
        let place = |column: &Column| -> Result<usize> {
            match column {
                Column::Named(name) => {
                    find_header(&lookup, name).ok_or_else(|| Error::MissingColumn {
                        input: options.input.clone(),
                        column: name.clone(),
                    })
                }
                Column::At(index) if *index < headers.len() => Ok(*index),
                Column::At(index) => Err(Error::MissingColumn {
                    input: options.input.clone(),
                    column: format!("at place {index}"),
                }),
            }
        };
        let optional = |column: &Option<Column>| column.as_ref().map(place).transpose();
        Ok(Self {
            groups: options
                .address_field_groups
                .iter()
                .map(|group| group.iter().map(place).collect())
                .collect::<Result<_>>()?,
            locality: optional(&options.locality_field)?,
            region: optional(&options.region_field)?,
            postcode: optional(&options.postcode_field)?,
        })
    }
}

const AUDIT_PREFIX: &str = "geocode_";

/// The columns the audit CSV appends to the input's, in order.
pub fn audit_columns() -> Vec<String> {
    let mut columns = vec![
        format!("{AUDIT_PREFIX}status"),
        format!("{AUDIT_PREFIX}query"),
    ];
    columns.extend(
        api::flat_columns(None)
            .into_iter()
            .map(|(name, _)| format!("{AUDIT_PREFIX}{name}")),
    );
    columns.push(format!("{AUDIT_PREFIX}reason"));
    columns
}

/// [`audit_columns`] one per line, as `contract/` records them.
pub fn audit_columns_text() -> String {
    audit_columns()
        .into_iter()
        .map(|column| format!("{column}\n"))
        .collect()
}

/// Rows read, geocoded and written per chunk. Chunks keep memory flat for
/// inputs of any size.
const BATCH_CHUNK_ROWS: usize = 50_000;

/// Geocode a CSV in streaming chunks. Within a chunk, rows are looked up in
/// parallel and grouped by postcode and place, so neighbouring lookups touch
/// the same index and record pages; output rows keep the input order.
///
/// The output and the audit carry the input's own bytes, row for row, with
/// the coordinates written in: only the columns a lookup reads are decoded,
/// so text in another encoding than UTF-8 comes out as it went in.
pub fn run_batch_geocode(options: GeocodeOptions) -> Result<GeocodeReport> {
    if options.address_field_groups.is_empty() == options.join.is_none() {
        return Err(Error::BadOptions(
            "provide either --address-fields or --join-coordinates-from, but not both".into(),
        ));
    }
    ensure_distinct_paths(&options.input, &options.output, options.audit.as_deref())?;

    let delimiter = detect_delimiter(&options.input)?;
    let mut reader = open_delimited(&options.input, delimiter)?;
    let mut records = reader.byte_records();
    let header = if options.header_row {
        records.next().transpose()?.unwrap_or_default()
    } else {
        unnamed_columns(&options.input)?.iter().collect()
    };
    let original_headers = cells(&header);
    let fields = Fields::resolve(&options, &original_headers)?;
    let mut output_headers = original_headers.clone();
    let coordinate_columns = ensure_coordinate_columns(
        &mut output_headers,
        &options.lat_column,
        &options.lon_column,
    )?;
    let mut output_header = header.clone();
    for added in &output_headers[original_headers.len()..] {
        output_header.push_field(added.as_bytes());
    }
    let mut audit_header = header;
    for column in audit_columns() {
        audit_header.push_field(column.as_bytes());
    }

    let resolver = match options.join.as_ref() {
        Some(join) => Resolver::Join {
            lookup: read_join_lookup(join)?,
            key_index: find_header(&header_lookup(&original_headers), &join.key_column)
                .ok_or_else(|| Error::MissingColumn {
                    input: join.path.clone(),
                    column: join.key_column.clone(),
                })?,
        },
        None => {
            let path = options.pack.as_ref().ok_or(Error::BadOptions(
                "--pack is required when geocoding address fields".into(),
            ))?;
            let open = || -> Result<(Arc<Reader>, TextSearcher)> {
                let pack = Arc::new(Reader::open(path)?);
                let searcher = TextSearcher::from_pack(Arc::clone(&pack))?;
                Ok((pack, searcher))
            };
            let (pack, searcher) = open()?;
            Resolver::Pack {
                pack,
                searcher: Box::new(searcher),
            }
        }
    };

    // The output starts as the input does, so a program that reads the
    // mark as UTF-8 reads both alike.
    let mark = starts_with_byte_order_mark(&options.input)?;
    let mut output = create_csv(&options.output, &output_header, delimiter, mark)?;
    let mut audit = options
        .audit
        .as_ref()
        .map(|path| create_csv(path, &audit_header, delimiter, mark))
        .transpose()?;
    let mut report = GeocodeReport {
        rows: 0,
        resolved: 0,
    };
    loop {
        if options.cancel.load(Ordering::Relaxed) {
            return Err(Cancelled.into());
        }
        let chunk = records
            .by_ref()
            .take(BATCH_CHUNK_ROWS)
            .map(|record| record.map_err(Error::from))
            .collect::<Result<Vec<_>>>()?;
        if chunk.is_empty() {
            break;
        }
        let audits = resolver.resolve(&options, &fields, &chunk, report.rows)?;
        for (row, row_audit) in chunk.iter().zip(audits) {
            let resolved = !row_audit.lat.trim().is_empty() && !row_audit.lon.trim().is_empty();
            output.write_record(&output_record(
                row,
                output_headers.len(),
                resolved.then_some((coordinate_columns, &row_audit)),
            ))?;
            report.resolved += usize::from(resolved);
            if let Some(audit) = &mut audit {
                audit.write_record(&audit_record(original_headers.len(), row, &row_audit))?;
            }
            report.rows += 1;
        }
    }
    output.finish()?;
    if let Some(audit) = audit {
        audit.finish()?;
    }
    Ok(report)
}

/// Field separators a delimited file may use, most common first.
const DELIMITERS: [u8; 4] = *b",;\t|";
/// Bytes read from the start of a file to find its separator.
const SNIFF_BYTES: usize = 64 << 10;

/// The field separator of a delimited text file: of `,` `;` tab and `|`,
/// the one that splits most of its first lines into the same number of
/// fields, more than one. A comma when none does.
pub fn detect_delimiter(path: &Path) -> Result<u8> {
    let mut sample = Vec::with_capacity(SNIFF_BYTES);
    File::open(path)
        .map_err(|source| Error::IoPath {
            action: "failed to open",
            path: path.to_path_buf(),
            source,
        })?
        .take(SNIFF_BYTES as u64)
        .read_to_end(&mut sample)?;
    // A line cut short by the sample's end would count against every
    // separator alike; leave it out.
    if sample.len() == SNIFF_BYTES
        && let Some(end) = sample.iter().rposition(|byte| *byte == b'\n')
    {
        sample.truncate(end + 1);
    }
    Ok(delimiter_of(&sample))
}

/// The field separator of delimited text, as [`detect_delimiter`] finds it.
pub fn delimiter_of(sample: &[u8]) -> u8 {
    let mut best = (b',', 0);
    for delimiter in DELIMITERS {
        let mut counts: HashMap<usize, usize> = HashMap::new();
        for record in ReaderBuilder::new()
            .flexible(true)
            .has_headers(false)
            .delimiter(delimiter)
            .from_reader(sample)
            .byte_records()
            .take(50)
            .filter_map(Result::ok)
            .filter(|record| record.len() > 1)
        {
            *counts.entry(record.len()).or_default() += 1;
        }
        let agreeing = counts.values().copied().max().unwrap_or(0);
        if agreeing > best.1 {
            best = (delimiter, agreeing);
        }
    }
    best.0
}

/// A reader of a delimited file, every row as data, quirks tolerated:
/// rows of different widths, and bytes that are not UTF-8.
pub fn open_delimited(path: &Path, delimiter: u8) -> Result<csv::Reader<File>> {
    ReaderBuilder::new()
        .flexible(true)
        .has_headers(false)
        .delimiter(delimiter)
        .from_path(path)
        .map_err(Error::from)
}

/// The byte order mark some files start with. Reading leaves it out.
const BYTE_ORDER_MARK: &[u8] = "\u{feff}".as_bytes();

/// Whether a file starts with [`BYTE_ORDER_MARK`].
fn starts_with_byte_order_mark(path: &Path) -> Result<bool> {
    let mut start = Vec::with_capacity(BYTE_ORDER_MARK.len());
    File::open(path)
        .map_err(|source| Error::IoPath {
            action: "failed to open",
            path: path.to_path_buf(),
            source,
        })?
        .take(BYTE_ORDER_MARK.len() as u64)
        .read_to_end(&mut start)?;
    Ok(start == BYTE_ORDER_MARK)
}

/// A cell as text. Bytes that are not UTF-8, such as a Latin-1 file's
/// accents, become replacement characters instead of failing the row.
pub fn text(cell: &[u8]) -> Cow<'_, str> {
    String::from_utf8_lossy(cell)
}

/// A row's cells as [`text`].
pub fn cells(record: &ByteRecord) -> Vec<String> {
    record.iter().map(|cell| text(cell).into_owned()).collect()
}

/// Names for the columns of a CSV without a header row: `column_1`,
/// `column_2` and so on, as many as the widest of its first rows has.
pub fn unnamed_columns(path: &Path) -> Result<Vec<String>> {
    let width = open_delimited(path, detect_delimiter(path)?)?
        .byte_records()
        .take(BATCH_CHUNK_ROWS)
        .filter_map(Result::ok)
        .map(|record| record.len())
        .max()
        .unwrap_or(0);
    Ok((1..=width)
        .map(|number| format!("column_{number}"))
        .collect())
}

enum Resolver {
    Pack {
        pack: Arc<Reader>,
        searcher: Box<TextSearcher>,
    },
    Join {
        lookup: HashMap<String, JoinedCoordinate>,
        key_index: usize,
    },
}

impl Resolver {
    fn resolve(
        &self,
        options: &GeocodeOptions,
        fields: &Fields,
        rows: &[ByteRecord],
        first_index: usize,
    ) -> Result<Vec<AuditFields>> {
        match self {
            Resolver::Join { lookup, key_index } => Ok(rows
                .iter()
                .map(|row| {
                    let key = row.get(*key_index).map(text).unwrap_or_default();
                    match lookup.get(key.as_ref()).filter(|joined| {
                        !joined.lat.trim().is_empty() && !joined.lon.trim().is_empty()
                    }) {
                        Some(joined) => audit_from_joined(joined),
                        None => AuditFields {
                            status: "unresolved".to_string(),
                            reason: "join key not found or joined coordinates are blank"
                                .to_string(),
                            result: column_values(None),
                            ..AuditFields::default()
                        },
                    }
                })
                .collect()),
            Resolver::Pack { pack, searcher } => {
                let mut order = (0..rows.len()).collect::<Vec<_>>();
                order.sort_by_cached_key(|&index| locality_key(&rows[index], fields));
                let mut results = order
                    .into_par_iter()
                    .map(|index| {
                        if options.cancel.load(Ordering::Relaxed) {
                            return Err(Cancelled.into());
                        }
                        let started = Instant::now();
                        let (audit, feature) =
                            geocode_row(pack, searcher, options, fields, &rows[index])?;
                        if let Some(observer) = &options.observer {
                            observer.row(&Row {
                                index: first_index + index,
                                query: &audit.query,
                                feature: feature.as_ref(),
                                elapsed: started.elapsed(),
                            });
                        }
                        Ok((index, audit))
                    })
                    .collect::<Result<Vec<_>>>()?;
                results.sort_unstable_by_key(|(index, _)| *index);
                Ok(results.into_iter().map(|(_, audit)| audit).collect())
            }
        }
    }
}

/// Group key: rows in the same postcode and place are geocoded together.
fn locality_key(row: &ByteRecord, fields: &Fields) -> [String; 4] {
    let field = |index: Option<usize>| {
        field_value(row, index)
            .map(|value| value.to_lowercase())
            .unwrap_or_default()
    };
    [
        field(fields.postcode).replace(' ', ""),
        field(fields.region),
        field(fields.locality),
        fields
            .groups
            .first()
            .and_then(|group| compose_query(row, group))
            .unwrap_or_default()
            .to_lowercase(),
    ]
}

/// `--address-fields` values: each a comma-separated group of column names.
pub fn parse_field_groups(values: &[String]) -> Result<Vec<Vec<Column>>> {
    values
        .iter()
        .map(|value| {
            let fields = value
                .split(',')
                .map(str::trim)
                .filter(|field| !field.is_empty())
                .map(|field| Column::Named(field.to_string()))
                .collect::<Vec<_>>();
            if fields.is_empty() {
                return Err(Error::BadOptions(
                    "address field group must include at least one column".into(),
                ));
            }
            Ok(fields)
        })
        .collect()
}

fn geocode_row(
    pack: &Reader,
    searcher: &TextSearcher,
    options: &GeocodeOptions,
    fields: &Fields,
    row: &ByteRecord,
) -> Result<(AuditFields, Option<ForwardFeature>)> {
    let locality = field_value(row, fields.locality);
    let region = field_value(row, fields.region);
    let postcode = field_value(row, fields.postcode);

    let mut tried_queries = Vec::new();
    for group in &fields.groups {
        let Some(address) = compose_query(row, group) else {
            continue;
        };
        let response = searcher.geocode_address(AddressGeocodeOptions {
            address: address.clone(),
            locality: locality.clone(),
            region: region.clone(),
            postcode: postcode.clone(),
            limit: options.limit,
            layer: options.layer.clone(),
            countries: Vec::new(),
        })?;
        tried_queries.push(address);
        if let Some(response) = response {
            let feature = api::forward_feature(&mut Contexts::new(pack), response.hit)?;
            let audit = AuditFields {
                status: "resolved".to_string(),
                query: response.query,
                reason: String::new(),
                lat: format_coordinate(feature.lat()),
                lon: format_coordinate(feature.lon()),
                result: column_values(Some(&feature)),
            };
            return Ok((audit, Some(feature)));
        }
    }

    let audit = AuditFields {
        status: "unresolved".to_string(),
        query: tried_queries.join(" | "),
        reason: "no engine result with matching coordinates and context".to_string(),
        result: column_values(None),
        ..AuditFields::default()
    };
    Ok((audit, None))
}

fn read_csv(path: &Path) -> Result<CsvTable> {
    let mut reader = ReaderBuilder::new()
        .flexible(true)
        .from_path(path)
        .map_err(Error::from)?;
    let headers = reader
        .headers()?
        .iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
    let mut rows = Vec::new();
    for record in reader.records() {
        rows.push(record?.iter().map(str::to_string).collect::<Vec<_>>());
    }
    Ok(CsvTable { headers, rows })
}

fn create_csv(
    path: &Path,
    header: &ByteRecord,
    delimiter: u8,
    byte_order_mark: bool,
) -> Result<PendingCsv> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|source| Error::IoPath {
            action: "failed to create",
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let file_name = path
        .file_name()
        .ok_or(Error::BadOptions("output is not a file path".into()))?
        .to_string_lossy();
    let temporary = path.with_file_name(format!(".{file_name}.{}.tmp", uuid::Uuid::new_v4()));
    let mut file = File::create(&temporary).map_err(|source| Error::IoPath {
        action: "failed to create",
        path: temporary.clone(),
        source,
    })?;
    if byte_order_mark {
        file.write_all(BYTE_ORDER_MARK)?;
    }
    let mut writer = WriterBuilder::new().delimiter(delimiter).from_writer(file);
    writer.write_byte_record(header)?;
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
    fn write_record(&mut self, record: &ByteRecord) -> Result<()> {
        Ok(self.writer.write_byte_record(record)?)
    }

    fn finish(mut self) -> Result<()> {
        self.writer.flush().map_err(|source| Error::IoPath {
            action: "failed to flush",
            path: self.path.clone(),
            source,
        })?;
        std::fs::rename(&self.temporary, &self.path)?;
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
fn ensure_distinct_paths(input: &Path, output: &Path, audit: Option<&Path>) -> Result<()> {
    let resolve = |path: &Path| -> Result<PathBuf> {
        if path.exists() {
            return path.canonicalize().map_err(|source| Error::IoPath {
                action: "failed to resolve",
                path: path.to_path_buf(),
                source,
            });
        }
        let parent = match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
            _ => PathBuf::from("."),
        };
        let parent = parent.canonicalize().unwrap_or(parent);
        Ok(parent.join(path.file_name().unwrap_or_default()))
    };
    let (input, output_path) = (resolve(input)?, resolve(output)?);
    let audit_path = audit.map(resolve).transpose()?;
    let mut pairs = vec![(&output_path, &input, "--output", "--input")];
    if let Some(audit_path) = &audit_path {
        pairs.push((audit_path, &input, "--audit", "--input"));
        pairs.push((&output_path, audit_path, "--output", "--audit"));
    }
    for (first, second, first_name, second_name) in pairs {
        if first == second {
            return Err(Error::BadOptions(
                format!(
                    "{first_name} and {second_name} name the same file ({}); choose a different path",
                    first.display()
                )
                .into(),
            ));
        }
    }
    Ok(())
}

fn read_join_lookup(join: &CoordinateJoinOptions) -> Result<HashMap<String, JoinedCoordinate>> {
    let table = read_csv(&join.path)?;
    let lookup = header_lookup(&table.headers);
    let key_index = find_header(&lookup, &join.key_column).ok_or_else(|| Error::MissingColumn {
        input: join.path.clone(),
        column: join.key_column.clone(),
    })?;
    let lat_index = find_header(&lookup, &join.lat_column).ok_or_else(|| Error::MissingColumn {
        input: join.path.clone(),
        column: join.lat_column.clone(),
    })?;
    let lon_index = find_header(&lookup, &join.lon_column).ok_or_else(|| Error::MissingColumn {
        input: join.path.clone(),
        column: join.lon_column.clone(),
    })?;
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
        return Err(Error::BadOptions(
            "latitude and longitude columns must be different".into(),
        ));
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

/// An input row as the output writes it: its own cells, as many as the
/// output has columns, with the coordinates written in when it resolved.
fn output_record(
    row: &ByteRecord,
    width: usize,
    resolved: Option<(CoordinateColumns, &AuditFields)>,
) -> ByteRecord {
    let mut record = ByteRecord::with_capacity(row.as_slice().len(), width);
    for index in 0..width {
        let cell = match resolved {
            Some((columns, audit)) if index == columns.lat_index => audit.lat.as_bytes(),
            Some((columns, audit)) if index == columns.lon_index => audit.lon.as_bytes(),
            _ => row.get(index).unwrap_or_default(),
        };
        record.push_field(cell);
    }
    record
}

/// The text of a cell, trimmed, or `None` when there is none.
fn field_value(row: &ByteRecord, index: Option<usize>) -> Option<String> {
    let value = text(row.get(index?)?);
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// The cells of `columns` with their whitespace collapsed, joined by `, `.
fn compose_query(row: &ByteRecord, columns: &[usize]) -> Option<String> {
    let parts = columns
        .iter()
        .filter_map(|index| {
            let value = text(row.get(*index)?)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            (!value.is_empty()).then_some(value)
        })
        .collect::<Vec<_>>();
    (!parts.is_empty()).then(|| parts.join(", "))
}

/// An input row as the audit writes it: its own cells, as many as the input
/// has columns, then the audit's.
fn audit_record(width: usize, row: &ByteRecord, audit: &AuditFields) -> ByteRecord {
    let mut record = output_record(row, width, None);
    record.push_field(audit.status.as_bytes());
    record.push_field(audit.query.as_bytes());
    for value in &audit.result {
        record.push_field(value.as_bytes());
    }
    record.push_field(audit.reason.as_bytes());
    record
}

fn column_values(feature: Option<&api::ForwardFeature>) -> Vec<String> {
    api::flat_columns(feature)
        .into_iter()
        .map(|(_, value)| value)
        .collect()
}

/// A joined row's audit: the joined file's own audit columns where it has
/// them (it may be an earlier run's audit CSV), and its coordinates.
fn audit_from_joined(joined: &JoinedCoordinate) -> AuditFields {
    let column = |name: &str| {
        joined
            .audit_fields
            .get(&format!("{AUDIT_PREFIX}{name}"))
            .cloned()
            .unwrap_or_default()
    };
    let result = api::flat_columns(None)
        .into_iter()
        .map(|(name, _)| match name {
            "lat" => joined.lat.clone(),
            "lon" => joined.lon.clone(),
            name => column(name),
        })
        .collect();
    AuditFields {
        status: Some(column("status"))
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "joined".to_string()),
        query: column("query"),
        reason: column("reason"),
        lat: joined.lat.clone(),
        lon: joined.lon.clone(),
        result,
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
        let named = |name: &str| Column::Named(name.to_string());
        assert_eq!(
            groups,
            vec![
                vec![named("street1"), named("city")],
                vec![named("street2"), named("postalcode")]
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
        let row = ByteRecord::from(vec!["  1333   Sheppard Ave E ", "Toronto", "M3C 1J4"]);
        let query = compose_query(&row, &[0, 2]);
        assert_eq!(query.as_deref(), Some("1333 Sheppard Ave E, M3C 1J4"));
    }

    #[test]
    fn geocodes_streamed_rows_in_input_order() {
        use crate::{
            pack::Writer,
            record::{
                AddressComponents, AddressRecord, LocationPrecision, ObjectType, SourceProvenance,
                point_geometry,
            },
        };

        let root =
            std::env::temp_dir().join(format!("open-geocode-batch-{}", uuid::Uuid::new_v4()));
        let mut writer = Writer::create(root.join("pack")).expect("writer");
        for (object_id, number, street, lon) in [
            (1, "10", "King Street", -79.40),
            (2, "20", "Queen Street", -79.41),
        ] {
            writer
                .write(
                    &AddressRecord {
                        address: AddressComponents {
                            areas: Vec::new(),
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
                        source: SourceProvenance::osm(ObjectType::Node, object_id),
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
        let report = run_batch_geocode(GeocodeOptions {
            pack: Some(root.join("pack")),
            input,
            output: root.join("out.csv"),
            audit: Some(root.join("audit.csv")),
            address_field_groups: vec![vec![Column::Named("street".into())]],
            limit: 5,
            ..GeocodeOptions::default()
        })
        .expect("batch");
        assert_eq!(
            report,
            GeocodeReport {
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

        // A file without a header row, watched row by row, with no audit.
        struct Rows(std::sync::Mutex<Vec<(usize, Option<String>)>>);
        impl Observer for Rows {
            fn row(&self, row: &Row) {
                self.0.lock().expect("rows").push((
                    row.index,
                    row.feature
                        .map(|feature| feature.properties.common.id.clone()),
                ));
            }
        }
        let bare = root.join("bare.csv");
        std::fs::write(&bare, "a,20 Queen St\nb,nowhere at all\n").expect("input");
        let rows = Arc::new(Rows(std::sync::Mutex::new(Vec::new())));
        let report = run_batch_geocode(GeocodeOptions {
            pack: Some(root.join("pack")),
            input: bare.clone(),
            output: root.join("bare-out.csv"),
            header_row: false,
            address_field_groups: vec![vec![Column::Named("column_2".into())]],
            observer: Some(Arc::clone(&rows) as Arc<dyn Observer>),
            ..GeocodeOptions::default()
        })
        .expect("batch without header");
        assert_eq!(report.resolved, 1);
        let mut seen = rows.0.lock().expect("rows").clone();
        seen.sort();
        assert_eq!(seen, vec![(0, Some("osm:node:2".to_string())), (1, None)]);
        let output = read_csv(&root.join("bare-out.csv")).expect("output");
        assert_eq!(output.headers, vec!["column_1", "column_2", "lat", "lon"]);

        // A run asked to stop writes nothing.
        let error = run_batch_geocode(GeocodeOptions {
            pack: Some(root.join("pack")),
            input: bare,
            output: root.join("stopped.csv"),
            header_row: false,
            address_field_groups: vec![vec![Column::Named("column_2".into())]],
            cancel: Arc::new(AtomicBool::new(true)),
            ..GeocodeOptions::default()
        })
        .unwrap_err();
        assert!(matches!(error, Error::Cancelled));
        assert!(!root.join("stopped.csv").exists());

        // A file as offices export it: a byte order mark, semicolons, a
        // Latin-1 byte, a short row. It is read as it is and written back
        // with its own separator and its own bytes.
        let messy = root.join("messy.csv");
        let mut bytes = b"\xef\xbb\xbfID;STRASSE;ORT\n".to_vec();
        bytes.extend_from_slice(b"1;20 Queen St;Z\xfcrich\n2;10 King Street\n");
        std::fs::write(&messy, bytes).expect("input");
        assert_eq!(detect_delimiter(&messy).expect("delimiter"), b';');
        let report = run_batch_geocode(GeocodeOptions {
            pack: Some(root.join("pack")),
            input: messy,
            output: root.join("messy-out.csv"),
            address_field_groups: vec![vec![Column::Named("strasse".into())]],
            ..GeocodeOptions::default()
        })
        .expect("batch of a messy file");
        assert_eq!(report.resolved, 2);
        let output = std::fs::read(root.join("messy-out.csv")).expect("output");
        let lines: Vec<&[u8]> = output.split(|byte| *byte == b'\n').collect();
        assert_eq!(lines[0], b"\xef\xbb\xbfID;STRASSE;ORT;lat;lon");
        assert_eq!(lines[1], b"1;20 Queen St;Z\xfcrich;43.6000000;-79.4100000");
        assert_eq!(lines[2], b"2;10 King Street;;43.6000000;-79.4000000");

        // Columns picked by place, where names cannot tell them apart; a
        // name the file does not have is an error.
        let twins = root.join("twins.csv");
        std::fs::write(&twins, "street,street,\nnowhere,10 King Street,x\n").expect("input");
        let report = run_batch_geocode(GeocodeOptions {
            pack: Some(root.join("pack")),
            input: twins.clone(),
            output: root.join("twins-out.csv"),
            address_field_groups: vec![vec![Column::At(1)]],
            ..GeocodeOptions::default()
        })
        .expect("batch by place");
        assert_eq!(report.resolved, 1);
        let error = run_batch_geocode(GeocodeOptions {
            pack: Some(root.join("pack")),
            input: twins,
            output: root.join("twins-missing.csv"),
            address_field_groups: vec![vec![Column::Named("adresse".into())]],
            ..GeocodeOptions::default()
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("no column"), "{error:#}");
    }

    #[test]
    fn finds_the_separator_most_lines_agree_on() {
        assert_eq!(delimiter_of(b"a,b,c\n1,2,3\n"), b',');
        assert_eq!(delimiter_of(b"a;b;c\n1;2,5;3\n"), b';');
        assert_eq!(delimiter_of(b"a\tb\n1\t2\n"), b'\t');
        assert_eq!(delimiter_of(b"a|b\n1|2\n"), b'|');
        // One field per line: nothing to choose, so a comma.
        assert_eq!(delimiter_of(b"100 queen st w\n24 sussex dr\n"), b',');
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
        let options = |output: PathBuf, audit: PathBuf| GeocodeOptions {
            pack: None,
            input: input.clone(),
            output,
            audit: Some(audit),
            limit: 5,
            join: Some(CoordinateJoinOptions {
                path: coordinates.clone(),
                key_column: "id".into(),
                lat_column: "lat".into(),
                lon_column: "lon".into(),
            }),
            ..GeocodeOptions::default()
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
        let audit = AuditFields {
            lat: "43.1".into(),
            lon: "-79.2".into(),
            ..AuditFields::default()
        };
        let columns = CoordinateColumns {
            lat_index: 1,
            lon_index: 2,
        };
        let row = ByteRecord::from(vec!["1"]);
        assert_eq!(
            output_record(&row, 3, Some((columns, &audit))),
            ByteRecord::from(vec!["1", "43.1", "-79.2"])
        );
        assert_eq!(
            output_record(&row, 3, None),
            ByteRecord::from(vec!["1", "", ""])
        );
    }
}
