//! Operator attributes: key-value pairs an operator attaches to og ids, read
//! from a CSV file when the server starts and added to the features a
//! request asks them for (`tags`). They live beside the Pack, so adding or
//! changing one needs no rebuild.
//!
//! The file has the header `id,key,value`. An `id` is an og id as results
//! carry it (see [`SourceId`]), written the way a result writes it. An id
//! may have many rows, but each key once.

use std::{collections::BTreeMap, collections::HashMap, fs::File, path::Path};

use crate::{
    api::Common,
    error::{Error, Result},
    pack::SourceId,
};

const HEADER: [&str; 3] = ["id", "key", "value"];

/// Which attributes a request asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TagKeys {
    /// `tags=*`.
    All,
    /// `tags=maxspeed,ref`.
    Some(Vec<String>),
}

/// The attributes of every id, held in memory.
// Memory is the ceiling: every row stays resident as owned strings. A file
// too big for RAM wants a sorted file read through mmap, as the Pack is.
#[derive(Debug, Clone, Default)]
pub struct Attributes(HashMap<String, Vec<(String, String)>>);

impl Attributes {
    /// Read the attributes of the CSV file at `path`. Any problem in it is an
    /// error naming the file and the line.
    pub fn load(path: &Path) -> Result<Self> {
        let file = File::open(path).map_err(|source| Error::IoPath {
            action: "failed to open attributes",
            path: path.to_path_buf(),
            source,
        })?;
        Self::read(file, &path.display().to_string())
    }

    pub(crate) fn read(input: impl std::io::Read, name: &str) -> Result<Self> {
        let bad = |line: u64, detail: String| {
            Error::BadInput(format!("attributes {name}, line {line}: {detail}"))
        };
        let mut reader = csv::ReaderBuilder::new().from_reader(input);
        let line_of = |error: &csv::Error| error.position().map_or(1, csv::Position::line);
        let header = reader
            .headers()
            .map_err(|error| bad(line_of(&error), error.to_string()))?;
        if !header.iter().eq(HEADER) {
            return Err(bad(
                1,
                format!("the header must be `id,key,value`, not `{}`", {
                    header.iter().collect::<Vec<_>>().join(",")
                }),
            ));
        }
        let mut attributes = HashMap::<String, Vec<(String, String)>>::new();
        for row in reader.records() {
            let row = row.map_err(|error| bad(line_of(&error), error.to_string()))?;
            let line = row.position().map_or(0, csv::Position::line);
            let (id, key, value) = (&row[0], &row[1], &row[2]);
            let source = SourceId::parse(id).ok_or_else(|| {
                bad(
                    line,
                    format!("id {id:?} is not an og id such as osm:node:1383955044"),
                )
            })?;
            if source.to_string() != id {
                return Err(bad(
                    line,
                    format!("id {id:?} must be written {:?}, as results write it", {
                        source.to_string()
                    }),
                ));
            }
            if key.is_empty() {
                return Err(bad(line, format!("the key of {id} is empty")));
            }
            let pairs = attributes.entry(id.to_string()).or_default();
            if pairs.iter().any(|(known, _)| known == key) {
                return Err(bad(line, format!("{id} already has the key {key:?}")));
            }
            pairs.push((key.to_string(), value.to_string()));
        }
        Ok(Self(attributes))
    }

    /// Give `common` the attributes `keys` ask for that its id has. It stays
    /// as it was when there are none.
    pub fn attach(&self, common: &mut Common, keys: &TagKeys) {
        let Some(pairs) = self.0.get(&common.id) else {
            return;
        };
        let tags = pairs
            .iter()
            .filter(|(key, _)| match keys {
                TagKeys::All => true,
                TagKeys::Some(asked) => asked.contains(key),
            })
            .cloned()
            .collect::<BTreeMap<_, _>>();
        if !tags.is_empty() {
            common.tags = Some(tags);
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.0.values().map(Vec::len).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(text: &str) -> Result<Attributes> {
        Attributes::read(text.as_bytes(), "tags.csv")
    }

    fn error(text: &str) -> String {
        read(text).expect_err("a bad file").to_string()
    }

    #[test]
    fn reads_many_rows_per_id_and_quoted_values() {
        let attributes = read(
            "id,key,value\n\
             osm:way:5,maxspeed,50\n\
             osm:way:5,ref,\"A1, north\"\n\
             openaddresses:nz/countrywide:row:7,unit,\n\
             osm:node:1,name,Café\n",
        )
        .expect("a good file");
        assert_eq!(attributes.len(), 4);
        assert_eq!(
            attributes.0["osm:way:5"],
            [
                ("maxspeed".to_string(), "50".to_string()),
                ("ref".to_string(), "A1, north".to_string()),
            ]
        );
        assert_eq!(
            attributes.0["openaddresses:nz/countrywide:row:7"],
            [("unit".to_string(), String::new())]
        );
    }

    #[test]
    fn a_header_only_file_has_no_attributes() {
        assert_eq!(read("id,key,value\n").expect("a good file").len(), 0);
    }

    #[test]
    fn every_problem_names_the_file_and_its_line() {
        for (text, detail) in [
            (
                "id,key\nosm:node:1,a\n",
                "line 1: the header must be `id,key,value`, not `id,key`",
            ),
            (
                "key,id,value\nosm:node:1,a,b\n",
                "line 1: the header must be `id,key,value`, not `key,id,value`",
            ),
            (
                "id,key,value\nosm:node:1,a,b\nnode:2,a,b\n",
                "line 3: id \"node:2\" is not an og id such as osm:node:1383955044",
            ),
            (
                "id,key,value\nosm:way:3:interp:1-2,a,b\n",
                "line 2: id \"osm:way:3:interp:1-2\" is not an og id such as osm:node:1383955044",
            ),
            (
                "id,key,value\n\nosm:node:1,a,b\nosm:node:007,a,b\n",
                "line 4: id \"osm:node:007\" must be written \"osm:node:7\", as results write it",
            ),
            (
                "id,key,value\nosm:node:1,,b\n",
                "line 2: the key of osm:node:1 is empty",
            ),
            (
                "id,key,value\nosm:node:1,a,b\nosm:way:2,a,b\nosm:node:1,a,c\n",
                "line 4: osm:node:1 already has the key \"a\"",
            ),
            (
                "id,key,value\nosm:node:1,a,b\nosm:node:2,a\n",
                "line 3: CSV error: record 2 (line: 3, byte: 28): found record with 2 fields, but the previous record has 3 fields",
            ),
        ] {
            assert_eq!(
                error(text),
                format!("attributes tags.csv, {detail}"),
                "{text:?}"
            );
        }
    }

    #[test]
    fn a_file_that_is_not_there_names_its_path() {
        let missing = std::env::temp_dir().join("open-geocode-no-such-attributes.csv");
        let message = Attributes::load(&missing).expect_err("no file").to_string();
        assert!(message.contains("failed to open attributes"), "{message}");
        assert!(
            message.contains(&missing.display().to_string()),
            "{message}"
        );
    }
}
