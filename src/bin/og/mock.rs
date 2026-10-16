//! Canned build stages and Pack records for the preview. None of it
//! is read from disk or the network.

use crate::regions::Region;

pub struct Source {
    pub name: &'static str,
    pub detail: &'static str,
    pub arg: &'static str,
    pub download_mb: f64,
}

pub const SOURCES: [Source; 3] = [
    Source {
        name: "OpenStreetMap",
        detail: "streets, places, POIs and addresses",
        arg: "osm",
        download_mb: 0.0,
    },
    Source {
        name: "OpenAddresses",
        detail: "+4.1M address points",
        arg: "openaddresses",
        download_mb: 310.0,
    },
    Source {
        name: "Overture places",
        detail: "+1.9M POIs with categories",
        arg: "overture",
        download_mb: 1_120.0,
    },
];

/// Free disk and installed memory the preview pretends to see.
pub const FREE_DISK_GB: f64 = 212.0;
pub const RAM_GB: u32 = 16;

pub struct Stage {
    /// What the stage did, for when it is finished: `Scanned`.
    pub did: &'static str,
    /// What it is doing, for while it runs: `Scanning`.
    pub doing: &'static str,
    /// What it works on: `OSM objects`.
    pub object: String,
    pub explain: [&'static str; 2],
    /// Units of work, or `None` for a stage with no measurable total.
    pub total: Option<f64>,
    pub unit: &'static str,
    /// How long the preview takes to play the stage.
    pub play_secs: f64,
    /// How long the stage takes on a real Ontario build.
    pub real_secs: f64,
    pub summary: String,
}

impl Stage {
    /// `Scanning OSM objects`.
    pub fn doing_text(&self) -> String {
        format!("{} {}", self.doing, self.object)
    }
}

pub fn stages(region: &Region, sources: &[bool; 3]) -> Vec<Stage> {
    let mut stages = Vec::new();
    if region.local().is_none() {
        stages.push(Stage {
            did: "Downloaded",
            doing: "Downloading",
            object: "OpenStreetMap".to_string(),
            explain: [
                "Fetching the region from Geofabrik.",
                "The file is checked against its published checksum before use.",
            ],
            total: Some(region.size_mb()),
            unit: "MB",
            play_secs: 3.0,
            real_secs: 38.0,
            summary: size(region.size_mb()),
        });
    }
    for (index, source) in SOURCES.iter().enumerate().skip(1) {
        if sources[index] {
            stages.push(Stage {
                did: "Downloaded",
                doing: "Downloading",
                object: source.name.to_string(),
                explain: [
                    "Fetching the extra source for this region.",
                    "Its points are merged with OpenStreetMap later in the build.",
                ],
                total: Some(source.download_mb),
                unit: "MB",
                play_secs: 2.0,
                real_secs: 21.0,
                summary: size(source.download_mb),
            });
        }
    }
    let builder = [
        (
            ("Scanned", "Scanning", "OSM objects"),
            [
                "Reading every node, way and relation once.",
                "Places, streets, addresses and POIs are kept; the rest is skipped.",
            ],
            Some(48.2e6),
            "objects",
            4.0,
            72.0,
            "48.2M objects".to_string(),
        ),
        (
            ("Read", "Reading", "boundary ways"),
            [
                "Collecting the outline pieces of countries, provinces and cities.",
                "These become the areas every result is placed in.",
            ],
            Some(1.9e6),
            "ways",
            2.0,
            9.0,
            "1.9M ways".to_string(),
        ),
        (
            ("Joined", "Joining", "node coordinates"),
            [
                "Giving every street and building the coordinates of its points.",
                "Data larger than the memory budget spills to disk; that is normal.",
            ],
            Some(67.9e6),
            "nodes",
            5.0,
            151.0,
            "67.9M nodes".to_string(),
        ),
        (
            ("Assembled", "Assembling", "ways"),
            [
                "Turning ways into streets, buildings and address lines.",
                "House number ranges are laid along their streets here.",
            ],
            Some(9.4e6),
            "ways",
            3.5,
            108.0,
            "9.4M ways".to_string(),
        ),
        (
            ("Wrote", "Writing", "context records"),
            [
                "Building the admin areas so every result knows its city,",
                "region and country, even when its own tags leave them out.",
            ],
            Some(3_418.0),
            "areas",
            2.0,
            18.0,
            "3,418 areas".to_string(),
        ),
        (
            ("Wrote", "Writing", "records"),
            [
                "Writing every searchable place into the Pack.",
                "Duplicates from different sources are folded into one record.",
            ],
            Some(7.59e6),
            "records",
            3.5,
            122.0,
            "7.59M records".to_string(),
        ),
        (
            ("Built", "Building", "Pack indexes"),
            [
                "Building the text index for search and autocomplete,",
                "and the spatial index for reverse geocoding.",
            ],
            None,
            "",
            2.5,
            41.0,
            "text + spatial".to_string(),
        ),
        (
            ("Trained", "Training", "address parser"),
            [
                "Learning this region's street words and postcode shapes,",
                "so queries are read the way people here write addresses.",
            ],
            None,
            "",
            2.0,
            22.0,
            "18,904 street words".to_string(),
        ),
    ];
    for ((did, doing, object), explain, total, unit, play_secs, real_secs, summary) in builder {
        stages.push(Stage {
            did,
            doing,
            object: object.to_string(),
            explain,
            total,
            unit,
            play_secs,
            real_secs,
            summary,
        });
    }
    stages
}

pub const LOGS: &[(&str, &str)] = &[
    ("info", "reading data/pbf/ontario-latest.osm.pbf, 752 MB"),
    (
        "warn",
        "relation 1922411: admin_level missing, read as region",
    ),
    ("info", "spill run 1 written, 412 MB"),
    ("warn", "way 48211903: unclosed boundary ring, skipped"),
    ("info", "spill run 2 written, 398 MB"),
    ("info", "2,118 addresses without a street, kept as points"),
    (
        "warn",
        "node 9918231: housenumber '12-A rear' not understood, kept",
    ),
    ("info", "spill run 3 written, 287 MB"),
    ("info", "merged 391,204 duplicate addresses"),
    ("info", "text index: 7,590,748 documents"),
    ("info", "parser: 18,904 street words, 71 postcode shapes"),
];

pub struct PackStats {
    pub places: &'static str,
    pub streets: &'static str,
    pub addresses: &'static str,
    pub pois: &'static str,
    pub records: &'static str,
    pub size: &'static str,
}

/// A Pack the preview pretends is on disk. The numbers are invented.
pub struct SamplePack {
    pub name: &'static str,
    pub region: &'static str,
    pub sources: &'static str,
    pub records: &'static str,
    pub size: &'static str,
    pub built_ago: &'static str,
    pub built_long: &'static str,
    pub built_on: &'static str,
    pub path: &'static str,
    /// The date of newer data than the Pack was built from.
    pub update: Option<&'static str>,
}

pub const PACKS: [SamplePack; 3] = [
    SamplePack {
        name: "Ontario",
        region: "Canada / Ontario",
        sources: "OpenStreetMap, OpenAddresses",
        records: "7,590,748",
        size: "1.3 GB",
        built_ago: "2d ago",
        built_long: "2 days ago",
        built_on: "2026-10-14",
        path: "./packs/ontario",
        update: Some("2026-10-16"),
    },
    SamplePack {
        name: "Canada",
        region: "Canada",
        sources: "OpenStreetMap",
        records: "31,402,118",
        size: "9.8 GB",
        built_ago: "4d ago",
        built_long: "4 days ago",
        built_on: "2026-10-12",
        path: "./packs/canada",
        update: None,
    },
    SamplePack {
        name: "Germany",
        region: "Germany",
        sources: "OpenStreetMap",
        records: "58,119,730",
        size: "4.9 GB",
        built_ago: "12d ago",
        built_long: "12 days ago",
        built_on: "2026-10-04",
        path: "./packs/germany",
        update: None,
    },
];

pub const PACK: PackStats = PackStats {
    places: "61,203",
    streets: "412,880",
    addresses: "6,214,551",
    pois: "902,114",
    records: "7,590,748",
    size: "1.3 GB",
};

pub struct Record {
    pub id: &'static str,
    pub layer: &'static str,
    pub accuracy: &'static str,
    pub name: &'static str,
    pub label: &'static str,
    pub category: Option<&'static str>,
    pub lon: f64,
    pub lat: f64,
    pub housenumber: Option<&'static str>,
    pub street: Option<&'static str>,
    pub postcode: Option<&'static str>,
    pub locality: &'static str,
}

#[allow(clippy::too_many_arguments, reason = "one argument per record column")]
const fn place(
    id: &'static str,
    layer: &'static str,
    accuracy: &'static str,
    name: &'static str,
    label: &'static str,
    lat: f64,
    lon: f64,
    locality: &'static str,
) -> Record {
    Record {
        id,
        layer,
        accuracy,
        name,
        label,
        category: None,
        lon,
        lat,
        housenumber: None,
        street: None,
        postcode: None,
        locality,
    }
}

pub const RECORDS: &[Record] = &[
    Record {
        housenumber: Some("100"),
        street: Some("Queen Street West"),
        postcode: Some("M5H 2N2"),
        ..place(
            "osm:node:4521890112",
            "address",
            "rooftop",
            "100 Queen Street West",
            "100 Queen Street West, Toronto, ON M5H 2N2",
            43.6534817,
            -79.3839347,
            "Toronto",
        )
    },
    Record {
        category: Some("amenity:townhall"),
        housenumber: Some("100"),
        street: Some("Queen Street West"),
        ..place(
            "osm:way:23086656",
            "poi",
            "centroid",
            "Toronto City Hall",
            "Toronto City Hall, 100 Queen Street West, Toronto, ON",
            43.6533103,
            -79.3841202,
            "Toronto",
        )
    },
    place(
        "osm:way:4255371",
        "street",
        "street",
        "Queen Street West",
        "Queen Street West, Toronto, ON",
        43.6487511,
        -79.3963419,
        "Toronto",
    ),
    place(
        "osm:way:30218867",
        "street",
        "street",
        "Queen Street West",
        "Queen Street West, Brampton, ON",
        43.6858921,
        -79.7697402,
        "Brampton",
    ),
    Record {
        category: Some("leisure:park"),
        ..place(
            "osm:way:23086680",
            "poi",
            "centroid",
            "Nathan Phillips Square",
            "Nathan Phillips Square, Toronto, ON",
            43.6524861,
            -79.3835144,
            "Toronto",
        )
    },
    place(
        "osm:way:4255210",
        "street",
        "street",
        "Bay Street",
        "Bay Street, Toronto, ON",
        43.6530201,
        -79.3807533,
        "Toronto",
    ),
    Record {
        category: Some("tourism:attraction"),
        housenumber: Some("290"),
        street: Some("Bremner Boulevard"),
        ..place(
            "osm:way:15076542",
            "poi",
            "centroid",
            "CN Tower",
            "CN Tower, 290 Bremner Boulevard, Toronto, ON",
            43.6425662,
            -79.3870568,
            "Toronto",
        )
    },
    Record {
        category: Some("railway:station"),
        ..place(
            "osm:node:1390436741",
            "poi",
            "rooftop",
            "Union Station",
            "Union Station, 65 Front Street West, Toronto, ON",
            43.6452982,
            -79.3806098,
            "Toronto",
        )
    },
    Record {
        housenumber: Some("1"),
        street: Some("Yonge Street"),
        postcode: Some("M5E 1E5"),
        ..place(
            "osm:node:5873501223",
            "address",
            "rooftop",
            "1 Yonge Street",
            "1 Yonge Street, Toronto, ON M5E 1E5",
            43.6424132,
            -79.3754711,
            "Toronto",
        )
    },
    place(
        "osm:way:4255012",
        "street",
        "street",
        "Yonge Street",
        "Yonge Street, Toronto, ON",
        43.6701288,
        -79.3867643,
        "Toronto",
    ),
    place(
        "osm:way:4255460",
        "street",
        "street",
        "King Street West",
        "King Street West, Toronto, ON",
        43.6455402,
        -79.3920511,
        "Toronto",
    ),
    place(
        "osm:node:2405316",
        "locality",
        "approximate",
        "Toronto",
        "Toronto, ON",
        43.6534817,
        -79.3839347,
        "Toronto",
    ),
    place(
        "osm:node:18886011",
        "locality",
        "approximate",
        "Ottawa",
        "Ottawa, ON",
        45.4211435,
        -75.6900574,
        "Ottawa",
    ),
    Record {
        housenumber: Some("24"),
        street: Some("Sussex Drive"),
        postcode: Some("K1M 1M4"),
        ..place(
            "osm:way:37911812",
            "address",
            "centroid",
            "24 Sussex Drive",
            "24 Sussex Drive, Ottawa, ON K1M 1M4",
            45.4443453,
            -75.6939621,
            "Ottawa",
        )
    },
    Record {
        category: Some("tourism:attraction"),
        ..place(
            "osm:way:26574125",
            "poi",
            "centroid",
            "Parliament Hill",
            "Parliament Hill, Wellington Street, Ottawa, ON",
            45.4235937,
            -75.7009121,
            "Ottawa",
        )
    },
    place(
        "osm:node:16090735",
        "locality",
        "approximate",
        "Kingston",
        "Kingston, ON",
        44.2311717,
        -76.4859544,
        "Kingston",
    ),
    Record {
        category: Some("amenity:university"),
        ..place(
            "osm:way:28447925",
            "poi",
            "centroid",
            "Queen's University",
            "Queen's University, Kingston, ON",
            44.2253015,
            -76.4951183,
            "Kingston",
        )
    },
];

const LOCALITIES: &[&str] = &["toronto", "ottawa", "kingston", "brampton"];
const STREET_WORDS: &[&str] = &[
    "street",
    "avenue",
    "road",
    "drive",
    "boulevard",
    "west",
    "east",
    "north",
    "south",
];

/// Common abbreviations, read the way the parser expands them.
fn expand(token: &str) -> &str {
    match token {
        "st" => "street",
        "ave" | "av" => "avenue",
        "rd" => "road",
        "dr" => "drive",
        "blvd" => "boulevard",
        "w" => "west",
        "e" => "east",
        "n" => "north",
        "s" => "south",
        "ont" => "on",
        other => other,
    }
}

fn tokens(text: &str) -> Vec<String> {
    text.split(|c: char| c.is_whitespace() || c == ',')
        .filter(|token| !token.is_empty())
        .map(|token| token.to_lowercase())
        .collect()
}

/// A labelling of the query, as the lattice parser would print it.
pub fn parse(query: &str) -> Vec<(String, &'static str)> {
    let mut parts: Vec<(String, &'static str)> = Vec::new();
    let mut pending: Vec<String> = Vec::new();
    let flush = |pending: &mut Vec<String>, parts: &mut Vec<(String, &'static str)>| {
        if pending.is_empty() {
            return;
        }
        let label = if pending
            .iter()
            .any(|token| STREET_WORDS.contains(&expand(token)))
        {
            "street"
        } else {
            "name"
        };
        parts.push((pending.join(" "), label));
        pending.clear();
    };
    let raw = tokens(query);
    for (index, token) in raw.iter().enumerate() {
        let label = if index == 0 && token.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            Some("house_number")
        } else if LOCALITIES.contains(&token.as_str()) {
            Some("locality")
        } else if token == "on" || token == "ontario" {
            Some("region")
        } else if is_postcode_part(token) {
            Some("postcode")
        } else {
            None
        };
        match label {
            Some(label) => {
                flush(&mut pending, &mut parts);
                match parts.last_mut() {
                    Some(last) if last.1 == label && label == "postcode" => {
                        last.0 = format!("{} {token}", last.0);
                    }
                    _ => parts.push((token.clone(), label)),
                }
            }
            None => pending.push(token.clone()),
        }
    }
    flush(&mut pending, &mut parts);
    parts
}

fn is_postcode_part(token: &str) -> bool {
    // A Canadian postcode half: `m5h` or `2n2`.
    let shape: Vec<bool> = token.chars().map(|c| c.is_ascii_digit()).collect();
    let letters_ok = token.chars().all(|c| c.is_ascii_alphanumeric());
    letters_ok && (shape == [false, true, false] || shape == [true, false, true])
}

/// Forward search over the canned records, best first.
pub fn search(query: &str) -> Vec<(&'static Record, f64)> {
    let query: Vec<String> = tokens(query)
        .iter()
        .map(|token| expand(token).to_string())
        .collect();
    if query.is_empty() {
        return Vec::new();
    }
    let has_number = query[0].chars().all(|c| c.is_ascii_digit());
    let mut hits: Vec<(&'static Record, f64)> = RECORDS
        .iter()
        .filter_map(|record| {
            let haystack: Vec<String> = tokens(record.label)
                .iter()
                .map(|token| expand(token).to_string())
                .collect();
            let mut missed = 0;
            let mut sum = 0.0;
            for (index, token) in query.iter().enumerate() {
                let last = index + 1 == query.len();
                let score = if haystack.contains(token) {
                    1.0
                } else if last && haystack.iter().any(|word| word.starts_with(token.as_str())) {
                    0.8
                } else {
                    missed += 1;
                    0.0
                };
                sum += score;
            }
            let mut score = sum / query.len() as f64 * 0.5f64.powi(missed);
            if record.layer == "address" {
                let number_matches = record.housenumber == Some(query[0].as_str());
                score += if number_matches { 0.08 } else { -0.2 };
            } else if has_number {
                score -= 0.12;
            }
            let score = score.min(0.99);
            (score > 0.35).then_some((record, score))
        })
        .collect();
    hits.sort_by(|a, b| b.1.total_cmp(&a.1));
    hits.truncate(6);
    hits
}

/// Reads `lat, lon` from the query.
pub fn point(query: &str) -> Option<(f64, f64)> {
    let mut numbers = query
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|part| !part.is_empty())
        .map(str::parse::<f64>);
    let lat = numbers.next()?.ok()?;
    let lon = numbers.next()?.ok()?;
    ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)).then_some((lat, lon))
}

/// Reverse search over the canned records, nearest first, in metres.
pub fn reverse(lat: f64, lon: f64) -> Vec<(&'static Record, f64)> {
    let mut hits: Vec<(&'static Record, f64)> = RECORDS
        .iter()
        .filter(|record| record.layer != "locality")
        .map(|record| (record, metres(lat, lon, record.lat, record.lon)))
        .filter(|(_, distance)| *distance < 5_000.0)
        .collect();
    hits.sort_by(|a, b| a.1.total_cmp(&b.1));
    hits.truncate(6);
    hits
}

fn metres(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (phi1, phi2) = (lat1.to_radians(), lat2.to_radians());
    let dphi = (lat2 - lat1).to_radians();
    let dlambda = (lon2 - lon1).to_radians();
    let a = (dphi / 2.0).sin().powi(2) + phi1.cos() * phi2.cos() * (dlambda / 2.0).sin().powi(2);
    6_371_000.0 * 2.0 * a.sqrt().asin()
}

pub fn size(mb: f64) -> String {
    if mb < 1.0 {
        format!("{:.0} KB", mb * 1_000.0)
    } else if mb >= 1_000.0 {
        format!("{:.1} GB", mb / 1_000.0)
    } else {
        format!("{mb:.0} MB")
    }
}

pub fn duration(secs: f64) -> String {
    let secs = secs.max(0.0).round() as u64;
    match secs {
        0..60 => format!("{secs}s"),
        60..3_600 => format!("{}m{:02}s", secs / 60, secs % 60),
        _ => format!("{}h{:02}m", secs / 3_600, secs % 3_600 / 60),
    }
}

/// A rough duration for estimates: `~ 9 min`, `~ 14 h`.
pub fn about(secs: f64) -> String {
    if secs >= 3_600.0 {
        format!("~ {:.0} h", secs / 3_600.0)
    } else {
        format!("~ {:.0} min", (secs / 60.0).ceil().max(1.0))
    }
}

pub fn count(value: f64) -> String {
    if value >= 1e6 {
        format!("{:.1}M", value / 1e6)
    } else if value >= 1e3 {
        format!("{:.1}K", value / 1e3)
    } else {
        format!("{value:.0}")
    }
}

/// Reads pasted CSV into one address per row. A header with a column named
/// like an address picks that column; without one every row is an address.
/// Returns the addresses and a short note on how they were read.
pub fn batch_parse(text: &str) -> (Vec<String>, String) {
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .from_reader(text.as_bytes());
    let records: Vec<csv::StringRecord> = reader.records().filter_map(Result::ok).collect();
    let column = records.first().and_then(|first| {
        first.iter().position(|field| {
            matches!(
                field.trim().to_lowercase().as_str(),
                "address" | "addr" | "full_address" | "query" | "q"
            )
        })
    });
    let (rows, note) = match column {
        Some(_) => (&records[1..], "address column"),
        None => (&records[..], "one address per row"),
    };
    let addresses = rows
        .iter()
        .map(|record| match column {
            Some(index) => record.get(index).unwrap_or("").trim().to_string(),
            None => record
                .iter()
                .map(str::trim)
                .filter(|field| !field.is_empty())
                .collect::<Vec<_>>()
                .join(", "),
        })
        .filter(|address| !address.is_empty())
        .collect();
    (addresses, note.to_string())
}
