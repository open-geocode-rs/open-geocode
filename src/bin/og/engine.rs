//! An open Pack and the questions the query shell asks it: search, reverse
//! and how the parser reads a query. Answers are the Runtime's own GeoJSON,
//! the same the HTTP API returns.

use std::{path::Path, sync::Arc, time::Instant};

use anyhow::{Context, Result};
use open_geocode::{
    api::{self, ForwardCollection, ReverseCollection},
    pack::PackReader,
    parse::lattice::LatticeTagger,
    reverse::{PackReverseGeocoder, ReverseGeocodeOptions},
    search::{AddressGeocodeOptions, DEFAULT_SEARCH_LIMIT, PackTextSearcher, TextSearchOptions},
};
use serde::Serialize;

pub struct Engine {
    pack: Arc<PackReader>,
    searcher: PackTextSearcher,
    reverse: PackReverseGeocoder,
    tagger: LatticeTagger,
}

/// One feature of an answer, as a row of the results shows it.
#[derive(Clone)]
pub struct Hit {
    pub layer: String,
    pub name: String,
    pub label: String,
    pub lat: f64,
    pub lon: f64,
    /// Search's relevance, or reverse's distance in metres.
    pub measure: Measure,
}

#[derive(Clone, Copy)]
pub enum Measure {
    Score(f32),
    /// Reverse gives no distance for the areas around the point.
    Distance(Option<f64>),
}

/// An answer as the HTTP API would give it.
#[derive(Clone)]
pub enum Collection {
    Forward(ForwardCollection),
    Reverse(ReverseCollection),
}

impl Collection {
    /// Pretty JSON of the whole answer, or of it with only feature `only`.
    pub fn json(&self, only: Option<usize>) -> String {
        /// Keep only feature `index`, when one was asked for.
        fn only_one<F: Clone>(features: &[F], only: Option<usize>) -> Vec<F> {
            match only {
                Some(index) => features.iter().skip(index).take(1).cloned().collect(),
                None => features.to_vec(),
            }
        }
        let text = match self {
            Self::Forward(collection) => serde_json::to_string_pretty(&api::ForwardCollection {
                features: only_one(&collection.features, only),
                ..collection.clone()
            }),
            Self::Reverse(collection) => serde_json::to_string_pretty(&api::ReverseCollection {
                features: only_one(&collection.features, only),
                ..collection.clone()
            }),
        };
        text.expect("an answer serializes")
    }
}

pub struct Answer {
    pub hits: Vec<Hit>,
    pub collection: Collection,
    /// How long the engine took, in milliseconds.
    pub millis: f64,
}

/// A query as the parser reads it: words with one label each, joined while
/// the label stays the same.
pub struct Reading {
    pub parts: Vec<(String, &'static str)>,
}

impl Engine {
    pub fn open(path: &Path) -> Result<Self> {
        let pack = Arc::new(
            PackReader::open(path).with_context(|| format!("failed to open {}", path.display()))?,
        );
        let searcher = PackTextSearcher::from_pack(Arc::clone(&pack))?;
        let reverse = PackReverseGeocoder::from_pack(Arc::clone(&pack))?;
        let tagger = LatticeTagger::from_container(pack.container())?;
        Ok(Self {
            pack,
            searcher,
            reverse,
            tagger,
        })
    }

    pub fn records(&self) -> u64 {
        self.pack.manifest().record_count
    }

    /// What `/search?q=` answers.
    pub fn search(&self, query: &str) -> Result<Answer> {
        let started = Instant::now();
        let query = query.trim().to_string();
        let hits = self.searcher.search(TextSearchOptions {
            query: query.clone(),
            limit: DEFAULT_SEARCH_LIMIT,
            layer: None,
        })?;
        let collection = api::forward_collection(&self.pack, query, hits)?;
        let millis = started.elapsed().as_secs_f64() * 1_000.0;
        let hits = collection
            .features
            .iter()
            .map(|feature| {
                hit(
                    &feature.properties.common,
                    &feature.geometry,
                    Measure::Score(feature.properties.score),
                )
            })
            .collect();
        Ok(Answer {
            hits,
            collection: Collection::Forward(collection),
            millis,
        })
    }

    /// What a batch run answers for one row's address: its one match, or
    /// none.
    pub fn geocode_address(&self, address: &str) -> Result<Answer> {
        let started = Instant::now();
        let address = address.trim().to_string();
        let found = self.searcher.geocode_address(AddressGeocodeOptions {
            address: address.clone(),
            locality: None,
            region: None,
            postcode: None,
            limit: DEFAULT_SEARCH_LIMIT,
            layer: None,
        })?;
        let collection = api::forward_collection(
            &self.pack,
            address,
            found.into_iter().map(|found| found.hit).collect(),
        )?;
        let millis = started.elapsed().as_secs_f64() * 1_000.0;
        let hits = collection
            .features
            .iter()
            .map(|feature| {
                hit(
                    &feature.properties.common,
                    &feature.geometry,
                    Measure::Score(feature.properties.score),
                )
            })
            .collect();
        Ok(Answer {
            hits,
            collection: Collection::Forward(collection),
            millis,
        })
    }

    /// What `/reverse?lat=&lon=` answers.
    pub fn reverse(&self, lat: f64, lon: f64) -> Result<Answer> {
        let started = Instant::now();
        let response = self.reverse.reverse(ReverseGeocodeOptions { lon, lat })?;
        let collection = api::reverse_collection(&self.pack, &response)?;
        let millis = started.elapsed().as_secs_f64() * 1_000.0;
        let hits = collection
            .features
            .iter()
            .map(|feature| {
                hit(
                    &feature.properties.common,
                    &feature.geometry,
                    Measure::Distance(feature.properties.distance_m),
                )
            })
            .collect();
        Ok(Answer {
            hits,
            collection: Collection::Reverse(collection),
            millis,
        })
    }

    /// The parser's best reading of a query.
    pub fn read(&self, query: &str) -> Reading {
        let mut parts: Vec<(String, &'static str)> = Vec::new();
        if let Some(best) = self.tagger.parse(query).into_iter().next() {
            for tagged in best.pieces {
                let label = tagged.label.name();
                match parts.last_mut() {
                    Some(last) if last.1 == label => {
                        last.0.push(' ');
                        last.0.push_str(&tagged.token);
                    }
                    _ => parts.push((tagged.token, label)),
                }
            }
        }
        Reading { parts }
    }
}

fn hit(common: &api::Common, geometry: &api::Point, measure: Measure) -> Hit {
    Hit {
        layer: common.layer.as_str().to_string(),
        name: common.name.clone(),
        label: common.label.clone(),
        lat: geometry.lat(),
        lon: geometry.lon(),
        measure,
    }
}

/// A unit enum's name as it is serialized: `rooftop`.
pub fn name_of(value: &impl Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// Reads `lat, lon` from a query.
pub fn point(query: &str) -> Option<(f64, f64)> {
    let mut numbers = query
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|part| !part.is_empty())
        .map(str::parse::<f64>);
    let lat = numbers.next()?.ok()?;
    let lon = numbers.next()?.ok()?;
    if numbers.next().is_some() {
        return None;
    }
    ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)).then_some((lat, lon))
}
