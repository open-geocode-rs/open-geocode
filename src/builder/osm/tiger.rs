use std::{
    collections::{BTreeSet, HashMap},
    fs,
    io::{self, Cursor, Read},
    path::Path,
    time::Duration,
};

use rayon::prelude::*;
use serde::Deserialize;
use shapefile::{Polyline, dbase};

use crate::{
    error::{Error, Result},
    pack,
    record::Layer,
    road, spatial_index,
    tiger::Range,
};

const NAME: &str = "2026_Gaz_counties_national";
const URL: &str = concat!(
    "https://www2.census.gov/geo/docs/maps-data/data/gazetteer/",
    "2026_Gazetteer/2026_Gaz_counties_national.zip"
);
pub(super) const DATASET: &str = "tiger:2026:counties";
const MAX_ARCHIVE_BYTES: u64 = 1 << 20;
const MAX_TABLE_BYTES: u64 = 4 << 20;

#[derive(Deserialize)]
pub(super) struct County {
    #[serde(rename = "USPS")]
    pub state: String,
    #[serde(rename = "GEOID")]
    pub code: String,
    #[serde(rename = "NAME")]
    pub name: String,
    #[serde(rename = "INTPTLAT")]
    pub lat: f64,
    #[serde(rename = "INTPTLONG")]
    pub lon: f64,
}

pub(super) struct Gazetteer {
    pub counties: Vec<County>,
    pub states: HashMap<String, String>,
}

impl Gazetteer {
    pub(super) fn load(pack: &Path) -> Result<Self> {
        let bytes = cached_archive(pack, NAME, URL, MAX_ARCHIVE_BYTES, |bytes| {
            Self::read(bytes).map(drop)
        })?;
        Self::read(&bytes)
    }

    fn read(bytes: &[u8]) -> Result<Self> {
        let bad = |detail| Error::BadInput(format!("TIGER county gazetteer: {detail}"));
        if bytes.len() as u64 > MAX_ARCHIVE_BYTES {
            return Err(bad("archive exceeds maximum size".into()));
        }
        let mut archive =
            zip::ZipArchive::new(Cursor::new(bytes)).map_err(|error| bad(error.to_string()))?;
        let table = archive
            .by_name(&format!("{NAME}.txt"))
            .map_err(|error| bad(error.to_string()))?;
        if table.size() > MAX_TABLE_BYTES {
            return Err(bad("table exceeds maximum size".into()));
        }
        let mut csv = csv::ReaderBuilder::new()
            .delimiter(b'|')
            .trim(csv::Trim::All)
            .from_reader(table);
        let mut counties = Vec::new();
        let mut states = HashMap::new();
        for row in csv.deserialize::<County>() {
            let county = row.map_err(|error| bad(error.to_string()))?;
            if county.code.len() != 5 || !county.code.bytes().all(|digit| digit.is_ascii_digit()) {
                return Err(bad(format!("invalid county GEOID {:?}", county.code)));
            }
            let state = states
                .entry(format!("US-{}", county.state))
                .or_insert_with(|| county.code[..2].to_string());
            if *state != county.code[..2] {
                return Err(bad(format!("conflicting FIPS codes for {}", county.state)));
            }
            counties.push(county);
        }
        if counties.is_empty() {
            return Err(bad("no counties".into()));
        }
        Ok(Self { counties, states })
    }
}

fn cached_archive(
    pack: &Path,
    name: &str,
    url: &str,
    max_bytes: u64,
    check: impl FnOnce(&[u8]) -> Result<()>,
) -> Result<Vec<u8>> {
    let directory = pack.join("tiger");
    let path = directory.join(format!("{name}.zip"));
    match fs::read(&path) {
        Ok(bytes) => return Ok(bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(Error::IoPath {
                path,
                action: "failed to read",
                source,
            });
        }
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(120)))
        .build()
        .into();
    let mut response = agent.get(url).call().map_err(|error| Error::Fetch {
        url: url.to_string(),
        message: error.to_string(),
    })?;
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(max_bytes + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        return Err(Error::BadInput(format!(
            "TIGER archive {name} exceeds maximum size"
        )));
    }
    check(&bytes)?;
    fs::create_dir_all(&directory)?;
    let partial = directory.join(format!("{name}.{}.part", uuid::Uuid::new_v4()));
    let stored = fs::write(&partial, &bytes).and_then(|()| fs::rename(&partial, &path));
    if stored.is_err() {
        let _ = fs::remove_file(&partial);
    }
    stored?;
    Ok(bytes)
}

const STREET_JOIN_M: f64 = 20.0;
const MAX_COUNTY_ARCHIVE_BYTES: u64 = 128 << 20;
const MAX_COUNTY_ENTRY_BYTES: u64 = 512 << 20;

pub(super) fn counties_for_pack(path: &Path, cache: &Path) -> Result<BTreeSet<String>> {
    let reader = pack::Reader::open(path)?;
    let spatial = spatial_index::Reader::open(reader.container(), reader.records().clone())?;
    let supported: BTreeSet<_> = Gazetteer::load(cache)?
        .counties
        .into_iter()
        .map(|county| county.code)
        .collect();
    const NAME: &str = "tl_2026_us_county";
    const URL: &str = "https://www2.census.gov/geo/tiger/TIGER2026/COUNTY/tl_2026_us_county.zip";
    let bad = |error: String| Error::BadInput(format!("TIGER {NAME}: {error}"));
    let bytes = cached_archive(cache, NAME, URL, MAX_COUNTY_ARCHIVE_BYTES, |bytes| {
        zip::ZipArchive::new(Cursor::new(bytes))
            .map(drop)
            .map_err(|error| bad(error.to_string()))
    })?;
    let mut archive =
        zip::ZipArchive::new(Cursor::new(bytes)).map_err(|error| bad(error.to_string()))?;
    let geometry = shapefile::ShapeReader::new(zip_entry(
        &mut archive,
        NAME,
        "shp",
        MAX_COUNTY_ENTRY_BYTES,
    )?)
    .map_err(|error| bad(error.to_string()))?;
    let attributes = dbase::Reader::new(zip_entry(
        &mut archive,
        NAME,
        "dbf",
        MAX_COUNTY_ENTRY_BYTES,
    )?)
    .map_err(|error| bad(error.to_string()))?;
    drop(archive);
    let mut source = shapefile::Reader::new(geometry, attributes);
    let mut counties = BTreeSet::new();
    // Census county equivalents need no matching OSM administrative record.
    for row in source.iter_shapes_and_records_as::<shapefile::Polygon, dbase::Record>() {
        let (shape, row) = row.map_err(|error| bad(error.to_string()))?;
        let code = dbf_text(&row, "GEOID")
            .filter(|code| code.len() == 5 && code.bytes().all(|byte| byte.is_ascii_digit()))
            .ok_or_else(|| bad("invalid county GEOID".into()))?;
        if !supported.contains(code) {
            continue;
        }
        let outline = geo::MultiPolygon::try_from(shape).map_err(|error| bad(error.to_string()))?;
        for polygon in &outline.0 {
            if spatial.has_street_or_address(polygon)? {
                counties.insert(code.to_string());
                break;
            }
        }
    }
    Ok(counties)
}

pub(super) fn add_ranges(
    sealed: &mut pack::SealedPack,
    cache: &Path,
    counties: &BTreeSet<String>,
    progress: &crate::builder::progress::Progress,
) -> Result<Vec<(&'static str, u64)>> {
    let (ranges, block_sides, zip_sides) = {
        let pack = pack::Reader::open(sealed.path())?;
        let spatial = spatial_index::Reader::open(pack.container(), pack.records().clone())?;
        let mut ranges = Vec::new();
        let mut block_sides = Vec::new();
        let mut zip_sides = Vec::new();
        for county in counties {
            progress.check()?;
            let name = format!("tl_2026_{county}_addrfeat");
            let url = format!("https://www2.census.gov/geo/tiger/TIGER2026/ADDRFEAT/{name}.zip");
            let bad = |error| Error::BadInput(format!("TIGER {name}: {error}"));
            let bytes = cached_archive(cache, &name, &url, MAX_COUNTY_ARCHIVE_BYTES, |bytes| {
                zip::ZipArchive::new(Cursor::new(bytes))
                    .map(drop)
                    .map_err(|error| bad(error.to_string()))
            })?;
            let mut archive =
                zip::ZipArchive::new(Cursor::new(bytes)).map_err(|error| bad(error.to_string()))?;
            let mut entry =
                |extension| zip_entry(&mut archive, &name, extension, MAX_COUNTY_ENTRY_BYTES);
            let geometry = shapefile::ShapeReader::new(entry("shp")?)
                .map_err(|error| bad(error.to_string()))?;
            let attributes =
                dbase::Reader::new(entry("dbf")?).map_err(|error| bad(error.to_string()))?;
            let mut reader = shapefile::Reader::new(geometry, attributes);
            let matched = reader
                .iter_shapes_and_records_as::<Polyline, dbase::Record>()
                .par_bridge()
                .map(|row| {
                    progress.check()?;
                    let (shape, record) = row.map_err(|error| bad(error.to_string()))?;
                    match_ranges(&pack, &spatial, &shape, &record)
                })
                .try_reduce(Vec::new, |mut left, mut right| {
                    left.append(&mut right);
                    Ok(left)
                })?;
            let faces = county_faces(cache, county)?;
            for (range, [left, right]) in matched {
                for (side, face) in [(b'L', left), (b'R', right)] {
                    if let Some(face) = face.and_then(|id| faces.get(&id)) {
                        let key = range.side_key(side);
                        block_sides.push((key, face.block));
                        if side == range.side
                            && range.zip.is_none()
                            && let Some(zip) = face.zip
                        {
                            zip_sides.push((key, zip));
                        }
                    }
                }
                ranges.push(range);
            }
        }
        (ranges, block_sides, zip_sides)
    };
    [
        (crate::tiger::SECTION, crate::tiger::encode(ranges)?),
        (
            crate::tiger::SIDE_BLOCKS_SECTION,
            crate::tiger::encode_unambiguous(block_sides),
        ),
        (
            crate::tiger::SIDE_ZIPS_SECTION,
            crate::tiger::encode_unambiguous(zip_sides),
        ),
    ]
    .into_iter()
    .filter(|(_, bytes)| !bytes.is_empty())
    .map(|(section, bytes)| {
        sealed.add_section(section, crate::tiger::VERSION, &bytes)?;
        Ok((section, bytes.len() as u64))
    })
    .collect()
}

fn match_ranges(
    pack: &pack::Reader,
    spatial: &spatial_index::Reader,
    shape: &Polyline,
    row: &dbase::Record,
) -> Result<Vec<(Range, [Option<u64>; 2])>> {
    let field = |name| match row.get(name) {
        Some(dbase::FieldValue::Character(Some(value))) => Some(value.trim()),
        _ => None,
    };
    let line = match row.get("TLID") {
        Some(dbase::FieldValue::Numeric(Some(value))) => *value as u32,
        _ => return Ok(Vec::new()),
    };
    let Some(name) = field("FULLNAME").filter(|name| !name.is_empty()) else {
        return Ok(Vec::new());
    };
    let sides = [
        (b'L', "LFROMHN", "LTOHN", "PARITYL", "ZIPL", "OFFSETL"),
        (b'R', "RFROMHN", "RTOHN", "PARITYR", "ZIPR", "OFFSETR"),
    ];
    let sides = sides
        .into_iter()
        .filter_map(|(side, from, to, parity, zip, offset)| {
            if field(offset) == Some("Y") {
                return None;
            }
            let from = field(from)?.parse::<u32>().ok()?;
            let to = field(to)?.parse::<u32>().ok()?;
            let step = match field(parity)? {
                "E" if from % 2 == 0 && to % 2 == 0 => 2,
                "O" if from % 2 == 1 && to % 2 == 1 => 2,
                "B" => 1,
                _ => return None,
            };
            let zip = field(zip)
                .filter(|zip| zip.len() == 5 && zip.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|zip| zip.parse().ok());
            Some((side, from, to, step, zip))
        })
        .collect::<Vec<_>>();
    if sides.is_empty() {
        return Ok(Vec::new());
    }
    let [part] = shape.parts().as_slice() else {
        return Ok(Vec::new());
    };
    let points: Vec<_> = part.iter().map(|point| [point.x, point.y]).collect();
    let [first, .., last] = points.as_slice() else {
        return Ok(Vec::new());
    };
    let length = points
        .windows(2)
        .map(|pair| road::planar_m(pair[0], pair[1]))
        .sum::<f64>();
    if length < 5.0 {
        return Ok(Vec::new());
    }
    let mut middle = length / 2.0;
    let mut midpoint = *first;
    for pair in points.windows(2) {
        let segment = road::planar_m(pair[0], pair[1]);
        if segment > 0.0 && middle <= segment {
            let t = middle / segment;
            midpoint = [
                pair[0][0] + (pair[1][0] - pair[0][0]) * t,
                pair[0][1] + (pair[1][1] - pair[0][1]) * t,
            ];
            break;
        }
        middle -= segment;
    }
    let positions = [*first, midpoint, *last];
    let mut matches = Vec::new();
    for hit in
        spatial.segment_candidates(midpoint[0], midpoint[1], Layer::Street, STREET_JOIN_M, 6)?
    {
        let names = pack.records().names(hit.record_id)?;
        if !names.iter().any(|osm| road::same_road(osm, name)) {
            continue;
        }
        let Some(projected) = spatial.project_on_line(hit.record_id, &positions)? else {
            continue;
        };
        let [(start, first_gap), (middle, mid_gap), (end, last_gap)] = projected.as_slice() else {
            continue;
        };
        if (middle - (start + end) / 2.0).abs() > (start - end).abs() * 0.25
            || (start - end).abs() < 1e-5
            || [*first_gap, *mid_gap, *last_gap]
                .into_iter()
                .any(|gap| gap > STREET_JOIN_M)
        {
            continue;
        }
        if [*first_gap, *last_gap].into_iter().sum::<f64>() > length * 0.5 {
            continue;
        }
        matches.push((
            first_gap + mid_gap + last_gap,
            hit.record_id,
            *start as f32,
            *end as f32,
        ));
    }
    matches.sort_unstable_by(|left, right| left.0.total_cmp(&right.0));
    let Some(&(gap, street, start, end)) = matches.first() else {
        return Ok(Vec::new());
    };
    if matches.get(1).is_some_and(|next| next.0 - gap < 12.0) {
        return Ok(Vec::new());
    }
    Ok(sides
        .into_iter()
        .map(|(side, from, to, step, zip)| {
            (
                Range {
                    street,
                    from,
                    to,
                    start,
                    end,
                    zip,
                    line,
                    step,
                    side,
                },
                [dbf_int(row, "TFIDL"), dbf_int(row, "TFIDR")],
            )
        })
        .collect())
}

fn zip_entry<R: Read + io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    name: &str,
    extension: &str,
    limit: u64,
) -> Result<Cursor<Vec<u8>>> {
    let bad = |error| Error::BadInput(format!("TIGER {name}: {error}"));
    let file = archive
        .by_name(&format!("{name}.{extension}"))
        .map_err(|error| bad(error.to_string()))?;
    if file.size() > limit {
        return Err(bad("entry exceeds maximum size".into()));
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(bad("entry exceeds maximum size".into()));
    }
    Ok(Cursor::new(bytes))
}

struct CensusBlock {
    code: u64,
    outline: geo::MultiPolygon<f64>,
    bounds: rstar::AABB<[f64; 2]>,
}

impl rstar::RTreeObject for CensusBlock {
    type Envelope = rstar::AABB<[f64; 2]>;

    fn envelope(&self) -> Self::Envelope {
        self.bounds
    }
}

pub(super) fn add_blocks(
    sealed: &mut pack::SealedPack,
    cache: &Path,
    counties: &BTreeSet<String>,
    progress: &crate::builder::progress::Progress,
    historical: bool,
) -> Result<u64> {
    use geo::{BoundingRect, Contains};

    let codes = {
        let pack = pack::Reader::open(sealed.path())?;
        let spatial = spatial_index::Reader::open(pack.container(), pack.records().clone())?;
        let mut codes = Vec::new();
        let states: BTreeSet<_> = counties.iter().map(|county| &county[..2]).collect();
        for state in states {
            progress.check()?;
            let (name, folder, field) = if historical {
                (
                    format!("tl_2010_{state}_tabblock10"),
                    "TIGER2010/TABBLOCK/2010",
                    "GEOID10",
                )
            } else {
                (
                    format!("tl_2026_{state}_tabblock20"),
                    "TIGER2026/TABBLOCK20",
                    "GEOID20",
                )
            };
            let url = format!("https://www2.census.gov/geo/tiger/{folder}/{name}.zip");
            let bad = |error| Error::BadInput(format!("TIGER {name}: {error}"));
            let bytes = cached_archive(cache, &name, &url, 512 << 20, |bytes| {
                zip::ZipArchive::new(Cursor::new(bytes))
                    .map(drop)
                    .map_err(|error| bad(error.to_string()))
            })?;
            let mut archive =
                zip::ZipArchive::new(Cursor::new(bytes)).map_err(|error| bad(error.to_string()))?;
            let geometry =
                shapefile::ShapeReader::new(zip_entry(&mut archive, &name, "shp", 2 << 30)?)
                    .map_err(|error| bad(error.to_string()))?;
            let attributes = dbase::Reader::new(zip_entry(&mut archive, &name, "dbf", 512 << 20)?)
                .map_err(|error| bad(error.to_string()))?;
            drop(archive);
            let mut reader = shapefile::Reader::new(geometry, attributes);
            let mut areas = Vec::new();
            for row in reader.iter_shapes_and_records_as::<shapefile::Polygon, dbase::Record>() {
                progress.check()?;
                let (shape, row) = row.map_err(|error| bad(error.to_string()))?;
                let code = match row.get(field) {
                    Some(dbase::FieldValue::Character(Some(code))) if code.len() == 15 => code
                        .parse::<u64>()
                        .map_err(|error| bad(error.to_string()))?,
                    _ => return Err(bad(format!("block has no {field}"))),
                };
                let outline =
                    geo::MultiPolygon::try_from(shape).map_err(|error| bad(error.to_string()))?;
                if let Some(bounds) = outline.bounding_rect() {
                    areas.push(CensusBlock {
                        code,
                        outline,
                        bounds: rstar::AABB::from_corners(bounds.min().into(), bounds.max().into()),
                    });
                }
            }
            drop(reader);
            let tree = rstar::RTree::bulk_load(areas);
            let extent = tree.root().envelope();
            let (min, max) = (extent.lower(), extent.upper());
            if tree.size() == 0 {
                continue;
            }
            spatial.address_points_in_box([min[0], min[1], max[0], max[1]], |id, point| {
                progress.check()?;
                let target = geo::Point::new(point[0], point[1]);
                let mut areas = tree
                    .locate_in_envelope_intersecting(rstar::AABB::from_point(point))
                    .filter(|block| block.outline.contains(&target));
                if let Some(area) = areas.next()
                    && areas.next().is_none()
                {
                    codes.push((id, area.code));
                }
                Ok(())
            })?;
        }
        codes
    };
    let bytes = crate::tiger::encode_blocks(codes);
    if !bytes.is_empty() {
        let section = if historical {
            crate::tiger::HISTORICAL_SECTION
        } else {
            crate::tiger::BLOCKS_SECTION
        };
        sealed.add_section(section, crate::tiger::VERSION, &bytes)?;
    }
    Ok(bytes.len() as u64)
}

fn dbf_int(row: &dbase::Record, field: &str) -> Option<u64> {
    match row.get(field)? {
        dbase::FieldValue::Numeric(Some(value)) if *value > 0.0 => Some(*value as u64),
        _ => None,
    }
}

fn dbf_text<'a>(row: &'a dbase::Record, field: &str) -> Option<&'a str> {
    match row.get(field)? {
        dbase::FieldValue::Character(Some(value)) => Some(value.trim()),
        _ => None,
    }
}

struct Face {
    block: u64,
    zip: Option<u64>,
}

fn county_faces(cache: &Path, county: &str) -> Result<HashMap<u64, Face>> {
    let name = format!("tl_2026_{county}_faces");
    let url = format!("https://www2.census.gov/geo/tiger/TIGER2026/FACES/{name}.zip");
    let bad = |error: String| Error::BadInput(format!("TIGER {name}: {error}"));
    let bytes = cached_archive(cache, &name, &url, MAX_COUNTY_ARCHIVE_BYTES, |bytes| {
        zip::ZipArchive::new(Cursor::new(bytes))
            .map(drop)
            .map_err(|error| bad(error.to_string()))
    })?;
    let mut archive =
        zip::ZipArchive::new(Cursor::new(bytes)).map_err(|error| bad(error.to_string()))?;
    let mut reader = dbase::Reader::new(zip_entry(
        &mut archive,
        &name,
        "dbf",
        MAX_COUNTY_ENTRY_BYTES,
    )?)
    .map_err(|error| bad(error.to_string()))?;
    let mut faces = HashMap::new();
    for row in reader.iter_records() {
        let row = row.map_err(|error| bad(error.to_string()))?;
        let Some(id) = dbf_int(&row, "TFID") else {
            continue;
        };
        let mut block = 0;
        for (field, scale) in [
            ("STATEFP20", 100),
            ("COUNTYFP20", 1000),
            ("TRACTCE20", 1_000_000),
            ("BLOCKCE20", 10_000),
        ] {
            let Some(value) = dbf_text(&row, field)
                .and_then(|v| v.parse::<u64>().ok())
                .filter(|v| *v < scale)
            else {
                block = 0;
                break;
            };
            block = block * scale + value;
        }
        if block != 0 {
            let zip = dbf_text(&row, "ZCTA5CE20")
                .filter(|v| v.len() == 5)
                .and_then(|v| v.parse().ok());
            faces.insert(id, Face { block, zip });
        }
    }
    Ok(faces)
}

pub(super) fn add_zcta(
    sealed: &mut pack::SealedPack,
    cache: &Path,
    counties: &BTreeSet<String>,
    progress: &crate::builder::progress::Progress,
) -> Result<u64> {
    use geo::Contains;

    let zip_codes = {
        let pack = pack::Reader::open(sealed.path())?;
        let spatial = spatial_index::Reader::open(pack.container(), pack.records().clone())?;
        let mut codes = Vec::new();
        for county in counties {
            let name = format!("tl_2026_{county}_faces");
            let url = format!("https://www2.census.gov/geo/tiger/TIGER2026/FACES/{name}.zip");
            let bad = |error: String| Error::BadInput(format!("TIGER {name}: {error}"));
            let bytes = cached_archive(cache, &name, &url, MAX_COUNTY_ARCHIVE_BYTES, |bytes| {
                zip::ZipArchive::new(Cursor::new(bytes))
                    .map(drop)
                    .map_err(|error| bad(error.to_string()))
            })?;
            let mut archive =
                zip::ZipArchive::new(Cursor::new(bytes)).map_err(|error| bad(error.to_string()))?;
            let geometry = shapefile::ShapeReader::new(zip_entry(
                &mut archive,
                &name,
                "shp",
                MAX_COUNTY_ENTRY_BYTES,
            )?)
            .map_err(|error| bad(error.to_string()))?;
            let attributes = dbase::Reader::new(zip_entry(
                &mut archive,
                &name,
                "dbf",
                MAX_COUNTY_ENTRY_BYTES,
            )?)
            .map_err(|error| bad(error.to_string()))?;
            drop(archive);
            let mut reader = shapefile::Reader::new(geometry, attributes);
            // Census faces carry the ZCTA code along with their other
            // geocodes, so a county build need not fetch national polygons.
            for row in reader.iter_shapes_and_records_as::<shapefile::Polygon, dbase::Record>() {
                progress.check()?;
                let (shape, row) = row.map_err(|error| bad(error.to_string()))?;
                let Some(code) = dbf_text(&row, "ZCTA5CE20")
                    .filter(|s| s.len() == 5)
                    .and_then(|s| s.parse::<u64>().ok())
                else {
                    continue;
                };
                let bounds = shape.bbox();
                let (min, max) = (bounds.min, bounds.max);
                let outline =
                    geo::MultiPolygon::try_from(shape).map_err(|error| bad(error.to_string()))?;
                spatial.address_points_in_box([min.x, min.y, max.x, max.y], |id, point| {
                    if outline.contains(&geo::Point::new(point[0], point[1])) {
                        let summary = pack.record_summary(id)?;
                        if summary.address.postcode.is_none() && summary.street_postcode.is_none() {
                            codes.push((id, code));
                        }
                    }
                    Ok(())
                })?;
            }
        }
        codes
    };
    let bytes = crate::tiger::encode_unambiguous(zip_codes);
    if !bytes.is_empty() {
        sealed.add_section(crate::tiger::ZCTA_SECTION, crate::tiger::VERSION, &bytes)?;
    }
    Ok(bytes.len() as u64)
}
