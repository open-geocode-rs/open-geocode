//! The tagged-address TSV format every gold file and stored prediction
//! uses: libpostal's training layout, `lang<TAB>country<TAB>fields`, with the
//! fields separated by single spaces. A field is `token/label`, the token
//! normalized as the text index normalizes it and the label one of og's eight
//! names, or a lone `,` marking where the source separated two parts of the
//! address. A normalized token never holds `,`, `/` or whitespace, so the
//! format needs no escaping.

use std::{
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};

use crate::{
    parse::types::{Label, Tagged},
    util::fold::search_key,
};

/// The field that marks a part boundary.
pub const SEPARATOR: &str = ",";

/// One tagged address: its tokens in order, and where the source separated
/// its parts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaggedRow {
    pub lang: String,
    pub country: String,
    pub tokens: Vec<Tagged>,
    /// Token indexes a part boundary comes before, ascending, each in
    /// `1..tokens.len()`.
    pub breaks: Vec<usize>,
}

impl TaggedRow {
    /// The query a parser is given: the tokens joined by spaces, with a comma
    /// closing each part but the last ("30 w 26th st, new york").
    pub fn query(&self) -> String {
        let mut query = String::new();
        for (index, tagged) in self.tokens.iter().enumerate() {
            if index > 0 {
                if self.breaks.contains(&index) {
                    query.push(',');
                }
                query.push(' ');
            }
            query.push_str(&tagged.token);
        }
        query
    }

    pub fn labels(&self) -> Vec<Label> {
        self.tokens.iter().map(|tagged| tagged.label).collect()
    }

    /// The row with the same tokens and part boundaries and other labels.
    pub fn relabelled(&self, labels: &[Label]) -> Self {
        Self {
            tokens: self
                .tokens
                .iter()
                .zip(labels)
                .map(|(tagged, &label)| Tagged {
                    token: tagged.token.clone(),
                    label,
                })
                .collect(),
            ..self.clone()
        }
    }

    pub fn to_line(&self) -> String {
        let mut line = format!("{}\t{}\t", self.lang, self.country);
        for (index, tagged) in self.tokens.iter().enumerate() {
            if index > 0 {
                line.push(' ');
                if self.breaks.contains(&index) {
                    line.push_str(SEPARATOR);
                    line.push(' ');
                }
            }
            let _ = write!(line, "{}/{}", tagged.token, tagged.label.name());
        }
        line
    }

    pub fn parse_line(line: &str) -> Result<Self> {
        let mut columns = line.split('\t');
        let (Some(lang), Some(country), Some(fields), None) = (
            columns.next(),
            columns.next(),
            columns.next(),
            columns.next(),
        ) else {
            bail!("expected three tab-separated columns");
        };
        ensure!(!country.is_empty(), "the country column is empty");
        let mut tokens = Vec::new();
        let mut breaks = Vec::new();
        for field in fields.split(' ') {
            if field == SEPARATOR {
                ensure!(
                    !tokens.is_empty() && breaks.last() != Some(&tokens.len()),
                    "a separator must follow a token"
                );
                breaks.push(tokens.len());
                continue;
            }
            let Some((token, label)) = field.rsplit_once('/') else {
                bail!("field {field:?} is not token/label");
            };
            let label =
                Label::from_name(label).with_context(|| format!("unknown label {label:?}"))?;
            ensure!(
                search_key(token).as_deref() == Some(token),
                "token {token:?} is not normalized"
            );
            tokens.push(Tagged {
                token: token.to_string(),
                label,
            });
        }
        ensure!(!tokens.is_empty(), "the row has no tokens");
        ensure!(
            breaks.last() != Some(&tokens.len()),
            "a separator must precede a token"
        );
        Ok(Self {
            lang: lang.to_string(),
            country: country.to_string(),
            tokens,
            breaks,
        })
    }
}

/// A fixture file's rows.
pub fn read_rows(path: &Path) -> Result<Vec<TaggedRow>> {
    let text =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    text.lines()
        .enumerate()
        .filter(|(_, line)| !line.is_empty())
        .map(|(index, line)| {
            TaggedRow::parse_line(line).with_context(|| format!("{}:{}", path.display(), index + 1))
        })
        .collect()
}

pub fn write_rows(path: &Path, rows: &[TaggedRow]) -> Result<()> {
    write_lines(path, rows.iter().map(TaggedRow::to_line))
}

/// The fields column of a stored prediction for a row the baseline failed
/// on. A normalized token never holds `!`, so it is never a real row.
pub const FAILED: &str = "!failed";

/// A stored baseline prediction for one gold row: the gold tokens with the
/// baseline's labels, or `None` when the baseline failed on the row, which
/// scores as wrong on every token.
pub type Prediction = Option<TaggedRow>;

/// A prediction file's rows, one per gold row.
pub fn read_predictions(path: &Path) -> Result<Vec<Prediction>> {
    let text =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    text.lines()
        .enumerate()
        .filter(|(_, line)| !line.is_empty())
        .map(|(index, line)| {
            if line.split('\t').nth(2) == Some(FAILED) {
                return Ok(None);
            }
            TaggedRow::parse_line(line)
                .map(Some)
                .with_context(|| format!("{}:{}", path.display(), index + 1))
        })
        .collect()
}

/// Store a baseline's predictions for `gold`, row for row; a failed row keeps
/// its gold row's language and country.
pub fn write_predictions(path: &Path, gold: &[TaggedRow], predicted: &[Prediction]) -> Result<()> {
    ensure!(
        gold.len() == predicted.len(),
        "{} predictions for {} gold rows",
        predicted.len(),
        gold.len()
    );
    write_lines(
        path,
        gold.iter()
            .zip(predicted)
            .map(|(gold, predicted)| match predicted {
                Some(row) => row.to_line(),
                None => format!("{}\t{}\t{FAILED}", gold.lang, gold.country),
            }),
    )
}

fn write_lines(path: &Path, lines: impl Iterator<Item = String>) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let mut text = String::new();
    for line in lines {
        text.push_str(&line);
        text.push('\n');
    }
    fs::write(path, text).with_context(|| format!("failed to write {}", path.display()))
}

/// One country's gold file, `<dir>/<cc>.tsv`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct CountryFile {
    /// The lowercase country code, the file's stem.
    pub country: String,
    pub path: PathBuf,
}

/// The gold files of a gold directory, sorted by country: every `<cc>.tsv`
/// in it, or only those of `countries` when it is not empty, each of which
/// must exist.
pub fn country_files(dir: &Path, countries: &[String]) -> Result<Vec<CountryFile>> {
    ensure!(dir.is_dir(), "{} is not a directory", dir.display());
    let mut files = Vec::new();
    if countries.is_empty() {
        let mut paths = fs::read_dir(dir)
            .with_context(|| format!("failed to list {}", dir.display()))?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        paths.retain(|path| path.is_file() && path.extension().is_some_and(|ext| ext == "tsv"));
        for path in paths {
            let country = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .with_context(|| format!("{} has no usable name", path.display()))?
                .to_string();
            files.push(CountryFile { country, path });
        }
    } else {
        for country in countries {
            let path = dir.join(format!("{country}.tsv"));
            ensure!(path.is_file(), "{} does not exist", path.display());
            files.push(CountryFile {
                country: country.clone(),
                path,
            });
        }
    }
    files.sort();
    files.dedup();
    ensure!(!files.is_empty(), "no <cc>.tsv files in {}", dir.display());
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tagged(token: &str, label: Label) -> Tagged {
        Tagged {
            token: token.to_string(),
            label,
        }
    }

    #[test]
    fn a_row_round_trips_with_its_part_boundaries() {
        let line = "en\tus\t30/house_number w/street 26th/street st/street , new/locality york/locality , 10010/postcode";
        let row = TaggedRow::parse_line(line).expect("row");
        assert_eq!(row.lang, "en");
        assert_eq!(row.country, "us");
        assert_eq!(row.tokens[0], tagged("30", Label::HouseNumber));
        assert_eq!(row.breaks, vec![4, 6]);
        assert_eq!(row.query(), "30 w 26th st, new york, 10010");
        assert_eq!(row.to_line(), line);
    }

    #[test]
    fn a_row_without_separators_is_one_part() {
        let row = TaggedRow::parse_line("de\tde\tmunchen/locality 80331/postcode").expect("row");
        assert!(row.breaks.is_empty());
        assert_eq!(row.query(), "munchen 80331");
    }

    #[test]
    fn malformed_rows_are_rejected() {
        for line in [
            "en\tus",
            "en\tus\t",
            "en\tus\t30/house_number\textra",
            "en\tus\t30",
            "en\tus\t30/road",
            "en\tus\tMain/street",
            "en\tus\tmain-st/street",
            "en\tus\t, main/street",
            "en\tus\tmain/street ,",
            "en\tus\tmain/street , , york/locality",
            "en\t\tmain/street",
        ] {
            assert!(TaggedRow::parse_line(line).is_err(), "{line:?}");
        }
    }

    #[test]
    fn relabelling_keeps_tokens_and_boundaries() {
        let row = TaggedRow::parse_line("en\tca\tking/street , toronto/locality").expect("row");
        let other = row.relabelled(&[Label::Locality, Label::Region]);
        assert_eq!(other.to_line(), "en\tca\tking/locality , toronto/region");
    }
}
