//! OpenAddresses as an [`AddressSource`]: the address files of the countries
//! a Pack covers, fetched from the OpenAddresses batch service and kept in a
//! cache between builds.
//!
//! Listing the sources is open, but downloading their data needs an
//! OpenAddresses account: its API token is read from [`TOKEN_VARIABLE`], or
//! from that line of a `.env` file in the working directory, so it never
//! lands in a shell history or a build report.

use std::{
    fs::{self, File},
    io::{self, BufWriter},
    path::{Path, PathBuf},
    time::Duration,
};

use serde::Deserialize;

use crate::{
    builder::addresses::{AddressFile, AddressSource, Coverage},
    error::{Error, Result},
};

pub const TOKEN_VARIABLE: &str = "OA_TOKEN";
const API: &str = "https://batch.openaddresses.io/api";
/// How long to wait before each retry of a download that failed on the way:
/// a dropped connection or a busy server. A Canada build fetches some 200
/// sources, and one stalled connect should not cost it the rest.
const RETRY_WAITS: [Duration; 3] = [
    Duration::from_secs(2),
    Duration::from_secs(8),
    Duration::from_secs(30),
];
/// The prefix the records of an OpenAddresses source name as their dataset:
/// `openaddresses:nz/countrywide`.
pub const DATASET_PREFIX: &str = "openaddresses:";

/// The OpenAddresses sources of the Pack's countries, downloaded into
/// `cache`.
#[derive(Clone)]
pub struct OpenAddresses {
    token: String,
    cache: PathBuf,
}

impl std::fmt::Debug for OpenAddresses {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OpenAddresses")
            .field("cache", &self.cache)
            .finish_non_exhaustive()
    }
}

/// One source's latest address data, as the service lists it.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Source {
    /// Its path in the OpenAddresses tree, `nz/countrywide`.
    pub source: String,
    /// Which of the source's data sets: most sources have one, a few have
    /// several (a city's and a county's).
    #[serde(default)]
    pub name: String,
    /// The job whose output is the latest data.
    pub job: u64,
    /// Bytes of the output; the service lists some sources without one.
    #[serde(default)]
    pub size: Option<u64>,
}

impl OpenAddresses {
    /// With the token [`token`] finds; fails when there is none.
    pub fn from_env(cache: impl Into<PathBuf>) -> Result<Self> {
        let token = token().ok_or_else(|| {
            Error::BadOptions(
                format!(
                    "downloading OpenAddresses data needs an account: sign up at \
                         https://batch.openaddresses.io, create an API token and set \
                         {TOKEN_VARIABLE} to it"
                )
                .into(),
            )
        })?;
        Ok(Self {
            token,
            cache: cache.into(),
        })
    }

    /// The sources whose data a Pack covering `countries` needs.
    pub fn sources(coverage: &Coverage) -> Result<Vec<Source>> {
        let url = format!("{API}/data?layer=addresses");
        let listed: Vec<Source> = agent()
            .get(&url)
            .call()
            .map_err(|error| fetch_error(&url, error))?
            .body_mut()
            .read_json()
            .map_err(|error| fetch_error(&url, error))?;
        Ok(select(listed, coverage))
    }

    fn download(&self, source: &Source) -> Result<PathBuf> {
        let path = self
            .cache
            .join(source.source.replace('/', "_"))
            .join(format!("{}.geojson.gz", source.job));
        if path.is_file() {
            return Ok(path);
        }
        let parent = path.parent().expect("cache files have a directory");
        fs::create_dir_all(parent).map_err(|error| io_error("failed to create", parent, error))?;
        let url = format!("{API}/job/{}/output/source.geojson.gz", source.job);
        // Written beside its final name, so an interrupted download is never
        // taken for a cached one.
        let partial = path.with_extension("gz.part");
        let mut waits = RETRY_WAITS.iter();
        loop {
            match self.fetch(&url, &partial) {
                Ok(()) => break,
                Err(Attempt::Final(error)) => return Err(error),
                Err(Attempt::Again(error)) => match waits.next() {
                    Some(wait) => std::thread::sleep(*wait),
                    None => return Err(error),
                },
            }
        }
        fs::rename(&partial, &path).map_err(|error| io_error("failed to move", &partial, error))?;
        Ok(path)
    }

    /// One try at downloading `url` into `partial`.
    fn fetch(&self, url: &str, partial: &Path) -> std::result::Result<(), Attempt> {
        let mut response = agent()
            .get(url)
            .header("Authorization", &format!("Bearer {}", self.token))
            .call()
            .map_err(|error| match error {
                ureq::Error::StatusCode(401 | 403) => Attempt::Final(Error::Fetch {
                    url: url.to_string(),
                    message: format!(
                        "OpenAddresses refused the token in {TOKEN_VARIABLE}: check it on \
                         https://batch.openaddresses.io"
                    ),
                }),
                ureq::Error::StatusCode(status) if status != 429 && status < 500 => {
                    Attempt::Final(fetch_error(url, error))
                }
                error => Attempt::Again(fetch_error(url, error)),
            })?;
        let mut file = BufWriter::new(
            File::create(partial)
                .map_err(|error| Attempt::Final(io_error("failed to create", partial, error)))?,
        );
        io::copy(&mut response.body_mut().as_reader(), &mut file)
            .map_err(|error| Attempt::Again(fetch_error(url, error)))?;
        file.into_inner()
            .map_err(|error| {
                Attempt::Final(io_error("failed to write", partial, error.into_error()))
            })?
            .sync_all()
            .map_err(|error| Attempt::Final(io_error("failed to write", partial, error)))
    }
}

impl AddressSource for OpenAddresses {
    fn files(&self, coverage: &Coverage) -> Result<Vec<AddressFile>> {
        let sources = Self::sources(coverage)?;
        sources
            .iter()
            .map(|source| {
                // The name tells data sets of one source apart, when it has several.
                let shared = sources
                    .iter()
                    .filter(|other| other.source == source.source)
                    .count()
                    > 1;
                Ok(AddressFile {
                    dataset: if shared {
                        format!("{DATASET_PREFIX}{}/{}", source.source, source.name)
                    } else {
                        format!("{DATASET_PREFIX}{}", source.source)
                    },
                    path: self.download(source)?,
                })
            })
            .collect()
    }
}

/// The OpenAddresses API token: [`TOKEN_VARIABLE`] from the environment, else
/// from `.env` in the working directory. The environment wins, so CI and a
/// one-off override need no file.
pub fn token() -> Option<String> {
    let found = std::env::var(TOKEN_VARIABLE)
        .ok()
        .filter(|token| !token.trim().is_empty())
        .or_else(|| {
            let text = fs::read_to_string(".env").ok()?;
            dotenv_value(&text, TOKEN_VARIABLE)
        })?;
    let token = found.trim();
    (!token.is_empty()).then(|| token.to_string())
}

/// The value of `name` in `.env` text: `NAME=value` lines, `export` allowed,
/// the value optionally in quotes; `#` starts a comment line.
fn dotenv_value(text: &str, name: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let line = line.trim();
        let line = line.strip_prefix("export ").unwrap_or(line);
        let (key, value) = line.split_once('=')?;
        if key.trim() != name {
            return None;
        }
        let value = value.trim();
        let unquoted = ['"', '\''].iter().find_map(|quote| {
            value
                .strip_prefix(*quote)
                .and_then(|value| value.strip_suffix(*quote))
        });
        Some(unquoted.unwrap_or(value).to_string())
    })
}

/// Of the listed sources, those `coverage` needs. A country covered whole takes
/// its `countrywide` source alone when it has one, since that holds what the
/// others do, else all of its sources; a region of a country covered in part
/// takes the sources under its path (`CA-ON` the ones under `ca/on/`), and
/// never the countrywide one, which would bring in the rest of the country.
/// A data set listed twice is taken once, at its latest job.
fn select(mut listed: Vec<Source>, coverage: &Coverage) -> Vec<Source> {
    listed.sort_by(|left, right| {
        (&left.source, &left.name)
            .cmp(&(&right.source, &right.name))
            .then(right.job.cmp(&left.job))
    });
    listed.dedup_by(|later, earlier| later.source == earlier.source && later.name == earlier.name);
    let mut selected = Vec::new();
    for subdivision in &coverage.subdivisions {
        let prefix = format!(
            "{}/",
            subdivision.to_ascii_lowercase().replacen('-', "/", 1)
        );
        selected.extend(
            listed
                .iter()
                .filter(|source| source.source.starts_with(&prefix))
                .cloned(),
        );
    }
    for country in &coverage.countries {
        let prefix = format!("{}/", country.to_ascii_lowercase());
        let of_country = listed
            .iter()
            .filter(|source| source.source.starts_with(&prefix))
            .collect::<Vec<_>>();
        let countrywide = format!("{prefix}countrywide");
        let whole = of_country
            .iter()
            .filter(|source| source.source == countrywide)
            .map(|source| (*source).clone())
            .collect::<Vec<_>>();
        if whole.is_empty() {
            selected.extend(of_country.into_iter().cloned());
        } else {
            selected.extend(whole);
        }
    }
    selected
}

/// How a download try ended: worth another try, or not.
enum Attempt {
    Again(Error),
    Final(Error),
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_response(Some(Duration::from_secs(60)))
        .user_agent(concat!(
            "open-geocode/",
            env!("CARGO_PKG_VERSION"),
            " (+https://github.com/open-geocode-rs/open-geocode)"
        ))
        .build()
        .into()
}

fn fetch_error(url: &str, error: impl std::fmt::Display) -> Error {
    Error::Fetch {
        url: url.to_string(),
        message: error.to_string(),
    }
}

fn io_error(action: &'static str, path: &Path, source: io::Error) -> Error {
    Error::IoPath {
        action,
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn source(name: &str, job: u64) -> Source {
        Source {
            source: name.to_string(),
            name: "addresses".to_string(),
            job,
            size: None,
        }
    }

    #[test]
    fn reads_a_listed_source_without_a_size() {
        let listed: Vec<Source> = serde_json::from_str(
            r#"[{"id":1,"source":"us/ny/x","name":"city","job":5,"size":null,"layer":"addresses"}]"#,
        )
        .expect("listing");
        assert_eq!((listed[0].job, listed[0].size), (5, None));
    }

    #[test]
    fn reads_the_token_line_of_a_dotenv_file() {
        let text = "# OA_TOKEN=commented\nOTHER=1\nexport OA_TOKEN = \"oa.abc\"\n";
        assert_eq!(
            dotenv_value(text, TOKEN_VARIABLE).as_deref(),
            Some("oa.abc")
        );
        assert_eq!(
            dotenv_value("OA_TOKEN='oa.x'", TOKEN_VARIABLE).as_deref(),
            Some("oa.x")
        );
        assert_eq!(dotenv_value("OA_TOKENS=1", TOKEN_VARIABLE), None);
    }

    fn listed() -> Vec<Source> {
        vec![
            source("au/countrywide", 7),
            source("au/nsw/statewide", 3),
            source("ca/countrywide", 4),
            source("ca/on/city_of_toronto", 5),
            source("ca/on/city_of_ottawa", 6),
            source("ca/on/city_of_ottawa", 9),
            source("ca/qc/montreal", 2),
            source("cat/x", 1),
            source("nz/countrywide", 2),
            source("us/ny/nyc", 8),
        ]
    }

    #[test]
    fn takes_a_countrywide_source_alone_else_every_source_of_the_country() {
        let coverage = Coverage {
            countries: BTreeSet::from(["AU".to_string(), "US".to_string()]),
            subdivisions: BTreeSet::new(),
        };
        assert_eq!(
            select(listed(), &coverage),
            vec![source("au/countrywide", 7), source("us/ny/nyc", 8)]
        );
    }

    #[test]
    fn takes_only_the_sources_of_a_region_when_its_country_is_covered_in_part() {
        let coverage = Coverage {
            countries: BTreeSet::new(),
            subdivisions: BTreeSet::from(["CA-ON".to_string()]),
        };
        assert_eq!(
            select(listed(), &coverage),
            vec![
                source("ca/on/city_of_ottawa", 9),
                source("ca/on/city_of_toronto", 5),
            ],
            "not ca/countrywide, which holds Quebec too"
        );
    }
}
