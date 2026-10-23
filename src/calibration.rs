//! `geocode calibrate`: fit a Pack's confidence model (see
//! [`crate::confidence`]) to address points the Pack was not built from.
//!
//! Each held-out row is a right answer whose query is unknown, so it is
//! written the way messy input looks: its street type dropped or
//! abbreviated, a typo in a long word, its postcode left out, a unit or a
//! business name before it, its parts out of order. Each of those happens
//! on a fair coin, so the queries range from clean to badly mangled, as real
//! ones do. Every query is searched, and each of its first answers is an
//! example: right when it is the row's house (its number and street) within
//! a distance of the row's point. A prefix of the clean query is put to
//! autocomplete the same way. The model is fit to the examples and written
//! beside the Pack.
//!
//! The rows must be held out: a row the Pack was built from is found as
//! itself, so a model fit to it rates every answer too high.

use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use rayon::prelude::*;
use serde::Serialize;
use xxhash_rust::xxh3::xxh3_64_with_seed;

use crate::{
    builder::addresses::{AddressFile, read_file},
    confidence::{Example, Model},
    error::{Error, Result},
    pack,
    parse::bench::variant::{Rng, abbreviation, typo},
    record::{AddressRecord, Layer},
    search::{TextAutocompleteOptions, TextSearchHit, TextSearchOptions, TextSearcher},
    spatial_index::haversine_m,
    text_index::street_search_text,
    util::{fold::search_key, geo::point_lon_lat},
};

/// Answers of one query taken as examples: the first few, so the model also
/// rates the answers below the first.
const ANSWERS_PER_QUERY: usize = 3;
/// Names of POIs drawn from the Pack to write before an address, as a
/// business that sends its own name with its address does.
const BUSINESS_NAMES: usize = 64;
/// Record draws per business name wanted, at most: a Pack may hold few POIs.
const DRAWS_PER_NAME: usize = 64;
/// Units written before an address that states none run up to this.
const MAX_UNIT: usize = 30;
/// Words this long or longer, all letters, may get the typo.
const MIN_TYPO_CHARS: usize = 5;
/// Characters of the street an autocomplete prefix keeps at least.
const MIN_PREFIX_STREET_CHARS: usize = 3;

#[derive(Debug, Clone)]
pub struct Options {
    pub pack: PathBuf,
    /// Held-out address points, read like a build's `--addresses`.
    pub files: Vec<AddressFile>,
    /// Rows drawn from each file.
    pub rows: usize,
    /// Messy queries written of each row.
    pub variants: usize,
    pub seed: u64,
    /// How far from its row's point an answer may lie and still be right.
    pub radius_m: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    /// The sidecar file the model was written to.
    pub model: PathBuf,
    pub rows: usize,
    pub queries: usize,
    pub examples: usize,
    pub correct: usize,
    /// Mean squared difference between each example's confidence under the
    /// fitted model and its outcome: lower is better, 0.25 is a coin.
    pub brier: f64,
    /// By the country the answers lie in; `--` for answers in none.
    pub countries: BTreeMap<String, CountryReport>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct CountryReport {
    pub examples: usize,
    pub correct: usize,
    /// Mean confidence the fitted model gives them, to set beside the share
    /// that is right.
    pub mean_confidence: f64,
}

/// Fit the Pack's confidence model to `options.files` and write it beside
/// the Pack, replacing the model it had.
pub fn run(options: &Options) -> Result<Report> {
    if options.files.is_empty() || options.rows == 0 {
        return Err(Error::BadOptions(
            "calibration needs held-out address rows: give --addresses and a --rows above 0".into(),
        ));
    }
    let pack = Arc::new(pack::Reader::open(&options.pack)?);
    let searcher = TextSearcher::from_pack(Arc::clone(&pack))?;
    let mut rng = Rng::new(options.seed);
    let names = business_names(&pack, &mut rng)?;
    let mut rows = Vec::new();
    for file in &options.files {
        rows.extend(draw_rows(file, options.rows, options.seed)?);
    }
    let per_row = rows
        .par_iter()
        .enumerate()
        .map(|(index, row)| {
            let mut rng = Rng::new(xxh3_64_with_seed(&index.to_le_bytes(), options.seed));
            row_examples(&searcher, row, options, &names, &mut rng)
        })
        .collect::<Result<Vec<_>>>()?;
    let queries = per_row.iter().map(|(queries, _)| queries).sum();
    let examples = per_row
        .into_iter()
        .flat_map(|(_, examples)| examples)
        .collect::<Vec<_>>();
    let model = Model::fit(&examples);
    let path = model.save(&pack)?;

    let mut countries = BTreeMap::<String, CountryReport>::new();
    let mut squared = 0.0;
    for example in &examples {
        let rated = f64::from(model.rate(&example.features));
        let outcome = f64::from(u8::from(example.correct));
        squared += (rated - outcome).powi(2);
        let country = example
            .features
            .country
            .map_or_else(|| "--".to_string(), |country| country.to_string());
        let report = countries.entry(country).or_default();
        report.examples += 1;
        report.correct += usize::from(example.correct);
        report.mean_confidence += rated;
    }
    for report in countries.values_mut() {
        report.mean_confidence /= report.examples as f64;
    }
    Ok(Report {
        model: path,
        rows: rows.len(),
        queries,
        correct: examples.iter().filter(|example| example.correct).count(),
        brier: if examples.is_empty() {
            0.0
        } else {
            squared / examples.len() as f64
        },
        examples: examples.len(),
        countries,
    })
}

/// `count` rows of `file`, drawn evenly by reservoir sampling, so a file of
/// any size is read once and held to `count` rows.
fn draw_rows(file: &AddressFile, count: usize, seed: u64) -> Result<Vec<AddressRecord>> {
    let mut rng = Rng::new(xxh3_64_with_seed(file.dataset.as_bytes(), seed));
    let mut drawn = Vec::with_capacity(count);
    let mut seen = 0usize;
    read_file(file, |record| {
        if drawn.len() < count {
            drawn.push(record);
        } else {
            let at = rng.below(seen + 1);
            if at < count {
                drawn[at] = record;
            }
        }
        seen += 1;
        Ok(None)
    })?;
    Ok(drawn)
}

/// Names of POIs of the Pack, drawn at random: names of the languages and
/// places the Pack covers, as no fixed list could give for every country.
fn business_names(pack: &pack::Reader, rng: &mut Rng) -> Result<Vec<String>> {
    let records = pack.records();
    let total = usize::try_from(records.record_count())?;
    let mut names = Vec::new();
    if total == 0 {
        return Ok(names);
    }
    for _ in 0..BUSINESS_NAMES * DRAWS_PER_NAME {
        if names.len() == BUSINESS_NAMES {
            break;
        }
        let id = rng.below(total) as u64;
        if records.header(id)?.layer == Layer::Poi
            && let Some(name) = records.names(id)?.into_iter().next()
        {
            names.push(name);
        }
    }
    Ok(names)
}

/// The queries written of one row and the examples their answers give.
fn row_examples(
    searcher: &TextSearcher,
    row: &AddressRecord,
    options: &Options,
    names: &[String],
    rng: &mut Rng,
) -> Result<(usize, Vec<Example>)> {
    let mut examples = Vec::new();
    let mut queries = 0;
    let mut take = |hits: Vec<TextSearchHit>| {
        for hit in hits.into_iter().take(ANSWERS_PER_QUERY) {
            examples.push(Example {
                correct: is_row(&hit, row, options.radius_m),
                features: hit.features,
            });
        }
    };
    for _ in 0..options.variants {
        let query = messy_query(row, names, |word| searcher.describes_street(word), rng);
        queries += 1;
        take(searcher.search(TextSearchOptions {
            query,
            limit: ANSWERS_PER_QUERY,
            layer: None,
            countries: Vec::new(),
            focus: None,
            bbox: None,
        })?);
    }
    if let Some(prefix) = typed_prefix(row, rng) {
        queries += 1;
        take(searcher.autocomplete(TextAutocompleteOptions {
            query: prefix,
            limit: ANSWERS_PER_QUERY,
            layer: None,
            countries: Vec::new(),
            focus: None,
            bbox: None,
        })?);
    }
    Ok((queries, examples))
}

/// Whether `hit` is the row's house: its number and street, within
/// `radius_m` of the row's point. A street standing in for the number is
/// not, nor is the house next door.
fn is_row(hit: &TextSearchHit, row: &AddressRecord, radius_m: f64) -> bool {
    let (Some(point), Some([lon, lat])) = (hit.record.point, point_lon_lat(&row.geometry)) else {
        return false;
    };
    let street = |street: Option<&str>| street.and_then(street_search_text);
    haversine_m(point.lon, point.lat, lon, lat) <= radius_m
        && hit
            .record
            .address
            .housenumber
            .as_deref()
            .and_then(search_key)
            == search_key(&row.address.number)
        && street(hit.record.address.street.as_deref()) == street(row.address.street.as_deref())
}

/// The row as a messy query (see the module docs). `describes_street` tells
/// a street type from a word of the street's name.
fn messy_query(
    row: &AddressRecord,
    names: &[String],
    describes_street: impl Fn(&str) -> bool,
    rng: &mut Rng,
) -> String {
    let address = &row.address;
    let mut street = address
        .street
        .iter()
        .flat_map(|street| street.split_whitespace())
        .map(str::to_string)
        .collect::<Vec<_>>();
    let typed = |word: &String| search_key(word).unwrap_or_default();
    if rng.coin()
        && street.iter().any(|word| !describes_street(&typed(word)))
        && let Some(at) = street
            .iter()
            .rposition(|word| describes_street(&typed(word)))
    {
        match abbreviation(&typed(&street[at])).filter(|_| rng.coin()) {
            Some(short) => street[at] = short.to_string(),
            None => {
                street.remove(at);
            }
        }
    }
    let mut first = std::iter::once(address.number.clone())
        .chain(street)
        .collect::<Vec<_>>()
        .join(" ");
    let mut parts = Vec::new();
    if rng.coin() {
        if rng.coin() || names.is_empty() {
            let unit = address
                .unit
                .clone()
                .unwrap_or_else(|| (rng.below(MAX_UNIT) + 1).to_string());
            first = format!("#{unit} {first}");
        } else {
            parts.push(names[rng.below(names.len())].clone());
        }
    }
    parts.push(first);
    parts.extend(address.locality.clone());
    parts.extend(address.region.clone());
    if !rng.coin() {
        parts.extend(address.postcode.clone());
    }
    if rng.coin() {
        typo_in_a_long_word(&mut parts, rng);
    }
    if rng.coin() {
        rng.shuffle(&mut parts);
    }
    parts.join(", ")
}

/// One typo in one word of [`MIN_TYPO_CHARS`] letters or more, when the
/// parts have one.
fn typo_in_a_long_word(parts: &mut [String], rng: &mut Rng) {
    let long = |word: &str| {
        word.chars().count() >= MIN_TYPO_CHARS && word.chars().all(char::is_alphabetic)
    };
    let words = parts
        .iter()
        .enumerate()
        .flat_map(|(part, text)| {
            text.split_whitespace()
                .enumerate()
                .filter(|(_, word)| long(word))
                .map(move |(word, _)| (part, word))
        })
        .collect::<Vec<_>>();
    if words.is_empty() {
        return;
    }
    let (part, at) = words[rng.below(words.len())];
    let mut words = parts[part]
        .split_whitespace()
        .map(str::to_string)
        .collect::<Vec<_>>();
    let mut word = words[at].to_lowercase();
    typo(&mut word, rng);
    words[at] = word;
    parts[part] = words.join(" ");
}

/// The house number and street as typed so far: cut after at least a few
/// characters of the street, `None` for a row with no street.
fn typed_prefix(row: &AddressRecord, rng: &mut Rng) -> Option<String> {
    let street = row.address.street.as_deref()?;
    let typed = format!("{} {street}", row.address.number);
    let least = row.address.number.chars().count() + 1 + MIN_PREFIX_STREET_CHARS;
    let total = typed.chars().count();
    if total < least {
        return Some(typed);
    }
    let keep = least + rng.below(total - least + 1);
    Some(
        typed
            .chars()
            .take(keep)
            .collect::<String>()
            .trim_end()
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use crate::record::{AddressComponents, LocationPrecision, SourceProvenance, point_geometry};

    use super::*;

    fn row() -> AddressRecord {
        AddressRecord {
            disputed: false,
            street_postcodes: Vec::new(),
            address: AddressComponents {
                number: "10".into(),
                street: Some("King Street West".into()),
                place: None,
                unit: None,
                locality: Some("Toronto".into()),
                areas: Vec::new(),
                region: Some("ON".into()),
                postcode: Some("M5V 1A1".into()),
                country: None,
            },
            geometry: point_geometry(-79.0, 43.0),
            location_precision: LocationPrecision::Point,
            source: SourceProvenance::imported("held-out", 1),
        }
    }

    #[test]
    fn messy_queries_keep_the_number_and_vary_the_rest() {
        let names = vec!["Queen Bakery".to_string()];
        let describes = |word: &str| matches!(word, "street" | "west");
        let mut rng = Rng::new(3);
        let queries = (0..200)
            .map(|_| messy_query(&row(), &names, describes, &mut rng))
            .collect::<std::collections::BTreeSet<_>>();
        assert!(queries.len() > 40, "{queries:?}");
        assert!(queries.iter().all(|query| query.contains("10")));
        assert!(queries.contains("10 King Street West, Toronto, ON, M5V 1A1"));
        assert!(
            queries
                .iter()
                .any(|query| query.contains("King Street") && !query.contains("West")),
            "the street type is dropped or shortened"
        );
        assert!(queries.iter().any(|query| query.contains("Queen Bakery")));
        assert!(queries.iter().any(|query| query.contains('#')));
        assert!(queries.iter().any(|query| !query.contains("M5V")));
        assert!(queries.iter().any(|query| query.starts_with("Toronto")));
    }

    #[test]
    fn fits_a_model_the_pack_then_rates_answers_with() {
        let dir =
            std::env::temp_dir().join(format!("open-geocode-calibration-{}", uuid::Uuid::new_v4()));
        let mut writer = crate::pack::Writer::create(dir.join("pack")).expect("writer");
        let mut lines = vec!["LON,LAT,NUMBER,STREET,CITY,POSTCODE".to_string()];
        for (index, street) in ["King Street West", "Queen Street East"].iter().enumerate() {
            for number in 1..=20 {
                let lon = -79.0 + index as f64 * 0.01 + f64::from(number) * 0.0001;
                let mut record = row();
                record.address.number = number.to_string();
                record.address.street = Some(street.to_string());
                record.geometry = point_geometry(lon, 43.0);
                record.source = SourceProvenance::osm(
                    crate::record::ObjectType::Node,
                    i64::from(number) + 100 * index as i64,
                );
                writer.write(&record.into(), None).expect("address");
                // Every other house is held out.
                if number % 2 == 0 {
                    lines.push(format!("{lon},43.0,{number},{street},Toronto,M5V 1A1"));
                }
            }
        }
        writer.finish().expect("finish");
        let csv = dir.join("held-out.csv");
        std::fs::write(&csv, lines.join("\n") + "\n").expect("csv");

        let report = run(&Options {
            pack: dir.join("pack"),
            files: vec![AddressFile {
                dataset: "held-out".into(),
                path: csv,
                entry: None,
            }],
            rows: 15,
            variants: 2,
            seed: 1,
            radius_m: 100.0,
        })
        .expect("calibrate");
        assert_eq!(report.rows, 15);
        assert_eq!(report.queries, 15 * 3);
        assert!(report.examples > 0);
        assert!(report.correct > 0, "{report:?}");
        assert!(report.model.is_file());
        assert!((0.0..=0.25).contains(&report.brier), "{report:?}");

        let searcher = TextSearcher::open(dir.join("pack")).expect("searcher");
        assert!(searcher.confidence_model().is_some());
        let hits = searcher
            .search(TextSearchOptions {
                query: "4 King Street West".into(),
                limit: 3,
                layer: None,
                countries: Vec::new(),
                focus: None,
                bbox: None,
            })
            .expect("search");
        assert!(hits.iter().all(|hit| {
            hit.confidence
                .is_some_and(|confidence| (0.0..=1.0).contains(&confidence))
        }));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_prefix_keeps_the_number_and_some_of_the_street() {
        let mut rng = Rng::new(5);
        for _ in 0..50 {
            let prefix = typed_prefix(&row(), &mut rng).expect("prefix");
            assert!(prefix.starts_with("10 Kin"), "{prefix}");
            assert!("10 King Street West".starts_with(&prefix));
        }
    }
}
