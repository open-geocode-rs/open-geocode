//! The parse benchmark: og's taggers and stored baseline predictions scored
//! on the same gold rows, per country and per [`Variant`].
//!
//! Gold rows live in `fixtures/parse/gold/<cc>.tsv`. Each is scored in three
//! forms, made from it at load time ([`variant`]), and a baseline's
//! predictions for one form live in
//! `fixtures/parse/predictions/<baseline>/<cc>.<variant>.tsv`, row for row
//! (see [`fixture`]). Every baseline scored must have every predictions file
//! of every country scored, so every number in one row of the report is on
//! identical rows. `docs/parse-eval.md` describes the data and how the
//! predictions were made.

pub mod align;
pub mod fixture;
pub mod mapping;
pub mod metrics;
pub mod variant;

use std::{
    collections::BTreeMap,
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use self::{
    align::{align_components, normalize_raw_line, prediction_row},
    fixture::{
        CountryFile, TaggedRow, country_files, read_predictions, read_rows, write_predictions,
        write_rows,
    },
    mapping::Scheme,
    metrics::{Counts, Summary},
    variant::Variant,
};
use crate::parse::{
    lattice::{
        LatticeTagger,
        dictionary::{Dictionaries, Field},
        dictionary_key,
    },
    legacy::LegacyTagger,
    types::{Label, Tagger},
};

/// An og tagger the benchmark can run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, clap::ValueEnum)]
pub enum TaggerKind {
    /// The old comma-based query parser ([`LegacyTagger`]).
    Legacy,
    /// The lattice parser, which reads a Pack.
    Lattice,
}

impl TaggerKind {
    pub fn system_name(self) -> &'static str {
        match self {
            TaggerKind::Legacy => "og-legacy",
            TaggerKind::Lattice => "og-lattice",
        }
    }

    fn open(self, pack: Option<&Path>) -> Result<Box<dyn Tagger>> {
        match self {
            TaggerKind::Legacy => Ok(Box::new(LegacyTagger)),
            TaggerKind::Lattice => open_lattice(pack),
        }
    }
}

fn open_lattice(pack: Option<&Path>) -> Result<Box<dyn Tagger>> {
    let pack = pack.context("--tagger lattice needs --pack")?;
    Ok(Box::new(LatticeTagger::open(pack)?))
}

/// A baseline's predictions file for one country and variant.
pub fn predictions_path(root: &Path, country: &str, variant: Variant) -> PathBuf {
    root.join(format!("{country}.{}.tsv", variant.name()))
}

#[derive(Debug, Clone)]
pub struct ParseBenchOptions {
    /// The gold directory, `<cc>.tsv` per country.
    pub gold: PathBuf,
    /// The root of stored baseline predictions, `<baseline>/<cc>.<variant>.tsv`.
    pub predictions: PathBuf,
    /// Countries to score; empty scores every gold file.
    pub countries: Vec<String>,
    pub pack: Option<PathBuf>,
    pub taggers: Vec<TaggerKind>,
    /// Baselines to score; `None` scores every one under `predictions`.
    pub baselines: Option<Vec<String>>,
    /// Worst rows of the first tagger to list, per variant.
    pub show_misses: usize,
    /// Also score every system apart on the rows whose names the Pack's
    /// dictionaries all hold and on the others (see [`Coverage`]).
    pub coverage: bool,
}

#[derive(Debug, Serialize)]
pub struct ParseBenchReport {
    /// og taggers first, then baselines.
    pub systems: Vec<String>,
    pub variants: Vec<&'static str>,
    /// Per country and variant, then per variant over every country
    /// (`country` "all") when there are several.
    pub groups: Vec<GroupReport>,
    /// The first tagger's worst rows, up to `show_misses` per variant.
    pub misses: Vec<Miss>,
    /// Per country and variant, the scores on covered and on uncovered rows,
    /// with `--coverage`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub coverage: Vec<CoverageGroup>,
}

/// Whether a gold row's names are all in the Pack's dictionaries: every
/// locality, region and street span of it, read as the parser reads it
/// ([`dictionary_key`]), is a name of that field, exactly. A span is a run of
/// tokens of one label within one part of the row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Coverage {
    Covered,
    Uncovered,
}

impl Coverage {
    pub fn of(row: &TaggedRow, dictionaries: &Dictionaries) -> Self {
        let mut start = 0;
        for end in 1..=row.tokens.len() {
            let closes = end == row.tokens.len()
                || row.breaks.contains(&end)
                || row.tokens[end].label != row.tokens[start].label;
            if !closes {
                continue;
            }
            let field = match row.tokens[start].label {
                Label::Locality => Some(Field::Locality),
                Label::Region => Some(Field::Region),
                Label::Street => Some(Field::Street),
                _ => None,
            };
            if let Some(field) = field {
                let words = row.tokens[start..end]
                    .iter()
                    .map(|tagged| tagged.token.as_str())
                    .collect::<Vec<_>>();
                let known = dictionary_key(&words.join(" "))
                    .is_some_and(|key| dictionaries.contains(field, &key));
                if !known {
                    return Coverage::Uncovered;
                }
            }
            start = end;
        }
        Coverage::Covered
    }

    fn name(self) -> &'static str {
        match self {
            Coverage::Covered => "covered",
            Coverage::Uncovered => "uncovered",
        }
    }
}

#[derive(Debug, Serialize)]
pub struct CoverageGroup {
    pub country: String,
    pub variant: &'static str,
    pub coverage: Coverage,
    pub rows: usize,
    pub systems: BTreeMap<String, Summary>,
}

#[derive(Debug, Serialize)]
pub struct GroupReport {
    pub country: String,
    pub variant: &'static str,
    pub rows: usize,
    /// Scores per system, every system on the same rows.
    pub systems: BTreeMap<String, Summary>,
}

/// A gold row the first og tagger got wrong.
#[derive(Debug, Serialize)]
pub struct Miss {
    pub country: String,
    pub variant: &'static str,
    /// The gold row's line in `<cc>.tsv`.
    pub line: usize,
    pub query: String,
    pub wrong: usize,
    pub tokens: usize,
    pub gold: String,
    pub predicted: String,
    #[serde(skip)]
    pairs: Vec<(String, Label, Option<Label>)>,
}

/// One country's counts in one variant, per system.
struct VariantCounts {
    country: String,
    variant: Variant,
    rows: usize,
    by_system: BTreeMap<String, Counts>,
    /// Rows and counts per system on covered and on uncovered rows.
    by_coverage: BTreeMap<Coverage, (usize, BTreeMap<String, Counts>)>,
}

pub fn run_parse_bench(options: &ParseBenchOptions) -> Result<ParseBenchReport> {
    ensure!(
        !options.taggers.is_empty(),
        "at least one --tagger is needed"
    );
    let files = country_files(&options.gold, &options.countries)?;
    let baselines = match &options.baselines {
        Some(baselines) => baselines.clone(),
        None => discover_baselines(&options.predictions)?,
    };
    let mut taggers = Vec::new();
    for kind in &options.taggers {
        taggers.push((kind.system_name(), kind.open(options.pack.as_deref())?));
    }
    let coverage_tagger = if options.coverage {
        let pack = options.pack.as_deref().context("--coverage needs --pack")?;
        Some(LatticeTagger::open(pack)?)
    } else {
        None
    };
    let mut systems = taggers
        .iter()
        .map(|(name, _)| name.to_string())
        .collect::<Vec<_>>();
    systems.extend(baselines.iter().cloned());

    let mut counts: Vec<VariantCounts> = Vec::new();
    let mut misses = Vec::new();
    for file in &files {
        let gold = read_rows(&file.path)?;
        // Variants keep the gold rows' tokens and labels, so a gold row's
        // coverage is its variants'.
        let coverage = coverage_tagger.as_ref().map(|tagger| {
            gold.iter()
                .map(|row| Coverage::of(row, tagger.dictionaries()))
                .collect::<Vec<_>>()
        });
        for variant in Variant::ALL {
            let rows = gold
                .iter()
                .map(|row| variant.apply(row))
                .collect::<Vec<_>>();
            let mut by_system = BTreeMap::new();
            let mut by_coverage = BTreeMap::<Coverage, (usize, BTreeMap<String, Counts>)>::new();
            if let Some(coverage) = &coverage {
                for row in coverage {
                    by_coverage.entry(*row).or_default().0 += 1;
                }
            }
            let mut add_row =
                |system: &str, line: usize, gold: &[Label], predicted: Option<&[Label]>| {
                    by_system
                        .entry(system.to_string())
                        .or_insert_with(Counts::default)
                        .add_row(gold, predicted);
                    if let Some(coverage) = &coverage {
                        by_coverage
                            .entry(coverage[line])
                            .or_default()
                            .1
                            .entry(system.to_string())
                            .or_default()
                            .add_row(gold, predicted);
                    }
                };
            let mut variant_misses = Vec::new();
            for (index, (name, tagger)) in taggers.iter().enumerate() {
                for (line, row) in rows.iter().enumerate() {
                    let tagged = tagger.tag(&row.query());
                    let aligned = tagged.len() == row.tokens.len()
                        && tagged
                            .iter()
                            .zip(&row.tokens)
                            .all(|(left, right)| left.token == right.token);
                    let predicted = aligned
                        .then(|| tagged.iter().map(|tagged| tagged.label).collect::<Vec<_>>());
                    add_row(name, line, &row.labels(), predicted.as_deref());
                    if index == 0 && options.show_misses > 0 {
                        let miss = miss(file, variant, line + 1, row, predicted.as_deref());
                        if miss.wrong > 0 {
                            variant_misses.push(miss);
                        }
                    }
                }
            }
            for baseline in &baselines {
                let path =
                    predictions_path(&options.predictions.join(baseline), &file.country, variant);
                ensure!(
                    path.is_file(),
                    "{} is missing, so {baseline} cannot be scored on the same rows; \
                     run `parse-fixture align` for it",
                    path.display()
                );
                let predicted = read_predictions(&path)?;
                ensure!(
                    predicted.len() == rows.len(),
                    "{} has {} rows for the {} of {}; regenerate it",
                    path.display(),
                    predicted.len(),
                    rows.len(),
                    file.path.display()
                );
                for (line, (row, predicted)) in rows.iter().zip(&predicted).enumerate() {
                    let Some(predicted) = predicted else {
                        // The baseline failed on the row: wrong on every token.
                        add_row(baseline, line, &row.labels(), None);
                        continue;
                    };
                    ensure!(
                        row.tokens.len() == predicted.tokens.len()
                            && row
                                .tokens
                                .iter()
                                .zip(&predicted.tokens)
                                .all(|(left, right)| left.token == right.token),
                        "{}:{} does not have the {} row's tokens; regenerate it",
                        path.display(),
                        line + 1,
                        variant.name()
                    );
                    add_row(baseline, line, &row.labels(), Some(&predicted.labels()));
                }
            }
            keep_worst(&mut variant_misses, options.show_misses);
            misses.extend(variant_misses);
            counts.push(VariantCounts {
                country: file.country.clone(),
                variant,
                rows: rows.len(),
                by_system,
                by_coverage,
            });
        }
    }

    let mut groups = counts
        .iter()
        .map(|counts| GroupReport {
            country: counts.country.clone(),
            variant: counts.variant.name(),
            rows: counts.rows,
            systems: summaries(&counts.by_system),
        })
        .collect::<Vec<_>>();
    if files.len() > 1 {
        for variant in Variant::ALL {
            let mut total = BTreeMap::<String, Counts>::new();
            let mut rows = 0;
            for file in counts.iter().filter(|counts| counts.variant == variant) {
                rows += file.rows;
                for (system, counts) in &file.by_system {
                    total.entry(system.clone()).or_default().merge(counts);
                }
            }
            groups.push(GroupReport {
                country: "all".to_string(),
                variant: variant.name(),
                rows,
                systems: summaries(&total),
            });
        }
    }

    let coverage = counts
        .iter()
        .flat_map(|counts| {
            counts
                .by_coverage
                .iter()
                .map(|(coverage, (rows, by_system))| CoverageGroup {
                    country: counts.country.clone(),
                    variant: counts.variant.name(),
                    coverage: *coverage,
                    rows: *rows,
                    systems: summaries(by_system),
                })
        })
        .collect();

    Ok(ParseBenchReport {
        systems,
        variants: Variant::ALL.iter().map(|variant| variant.name()).collect(),
        groups,
        misses,
        coverage,
    })
}

fn summaries(by_system: &BTreeMap<String, Counts>) -> BTreeMap<String, Summary> {
    by_system
        .iter()
        .map(|(system, counts)| (system.clone(), counts.summary()))
        .collect()
}

/// Keep the `count` worst misses: most tokens wrong, then most of the row
/// wrong, then in fixture order.
fn keep_worst(misses: &mut Vec<Miss>, count: usize) {
    misses.sort_by(|left, right| {
        right
            .wrong
            .cmp(&left.wrong)
            .then_with(|| (right.wrong * left.tokens).cmp(&(left.wrong * right.tokens)))
            .then_with(|| left.country.cmp(&right.country))
            .then_with(|| left.line.cmp(&right.line))
    });
    misses.truncate(count);
}

/// The baselines with stored predictions: the directories under the root.
fn discover_baselines(root: &Path) -> Result<Vec<String>> {
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut names = Vec::new();
    for entry in fs::read_dir(root).with_context(|| format!("failed to list {}", root.display()))? {
        let entry = entry?;
        if entry.file_type()?.is_dir()
            && let Some(name) = entry.file_name().to_str()
        {
            names.push(name.to_string());
        }
    }
    names.sort();
    Ok(names)
}

fn miss(
    file: &CountryFile,
    variant: Variant,
    line: usize,
    row: &TaggedRow,
    predicted: Option<&[Label]>,
) -> Miss {
    let pairs = row
        .tokens
        .iter()
        .enumerate()
        .map(|(index, tagged)| {
            (
                tagged.token.clone(),
                tagged.label,
                predicted.map(|labels| labels[index]),
            )
        })
        .collect::<Vec<_>>();
    let wrong = pairs
        .iter()
        .filter(|(_, gold, predicted)| Some(*gold) != *predicted)
        .count();
    Miss {
        country: file.country.clone(),
        variant: variant.name(),
        line,
        query: row.query(),
        wrong,
        tokens: pairs.len(),
        gold: row.to_line(),
        predicted: predicted.map_or_else(
            || "(tokenized differently)".to_string(),
            |labels| row.relabelled(labels).to_line(),
        ),
        pairs,
    }
}

/// A headline number of a summary, as a fraction.
type Metric = fn(&Summary) -> f64;

impl ParseBenchReport {
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        let tables: [(&str, Metric); 2] = [
            (
                "Full-parse accuracy (% of rows with every token right)",
                |summary| summary.full_parse_accuracy,
            ),
            ("Token accuracy (% of tokens right)", |summary| {
                summary.token_accuracy
            }),
        ];
        for (title, metric) in tables {
            let _ = writeln!(out, "### {title}\n");
            self.header(&mut out, &["country", "variant", "rows"]);
            for group in &self.groups {
                let _ = write!(
                    out,
                    "| {} | {} | {} |",
                    group.country, group.variant, group.rows
                );
                for system in &self.systems {
                    match group.systems.get(system) {
                        Some(summary) => {
                            let _ = write!(out, " {:.1} |", 100.0 * metric(summary));
                        }
                        None => out.push_str(" - |"),
                    }
                }
                out.push('\n');
            }
            out.push('\n');
        }

        if !self.coverage.is_empty() {
            let _ = writeln!(
                out,
                "### Coverage split (full-parse / token accuracy, %)\n\n\
                 A row is covered when the Pack's dictionaries hold every one of its \
                 locality, region and street names exactly.\n"
            );
            self.header(&mut out, &["country", "variant", "coverage", "rows"]);
            for group in &self.coverage {
                let _ = write!(
                    out,
                    "| {} | {} | {} | {} |",
                    group.country,
                    group.variant,
                    group.coverage.name(),
                    group.rows
                );
                for system in &self.systems {
                    match group.systems.get(system) {
                        Some(summary) => {
                            let _ = write!(
                                out,
                                " {:.1} / {:.1} |",
                                100.0 * summary.full_parse_accuracy,
                                100.0 * summary.token_accuracy
                            );
                        }
                        None => out.push_str(" - |"),
                    }
                }
                out.push('\n');
            }
            out.push('\n');
        }

        let _ = writeln!(out, "### Per-label F1 (%)\n");
        self.header(&mut out, &["country", "variant", "label", "gold tokens"]);
        for group in &self.groups {
            for label in Label::ALL {
                let scores = self
                    .systems
                    .iter()
                    .map(|system| {
                        group.systems.get(system).and_then(|summary| {
                            summary
                                .labels
                                .iter()
                                .find(|score| score.label == label.name())
                        })
                    })
                    .collect::<Vec<_>>();
                let support = scores
                    .iter()
                    .flatten()
                    .map(|score| score.support)
                    .next()
                    .unwrap_or_default();
                if support == 0 {
                    continue;
                }
                let _ = write!(
                    out,
                    "| {} | {} | {} | {support} |",
                    group.country,
                    group.variant,
                    label.name()
                );
                for score in scores {
                    let _ = write!(out, " {:.1} |", 100.0 * score.map_or(0.0, |score| score.f1));
                }
                out.push('\n');
            }
        }

        if !self.misses.is_empty() {
            let system = &self.systems[0];
            let _ = writeln!(out, "\n### Worst rows for {system}\n");
            out.push_str("Wrong tokens are marked `*`, with the gold label first.\n\n```text\n");
            for miss in &self.misses {
                let _ = writeln!(
                    out,
                    "{}.tsv:{} {}  {} of {} wrong  {}",
                    miss.country, miss.line, miss.variant, miss.wrong, miss.tokens, miss.query
                );
                let width = miss
                    .pairs
                    .iter()
                    .map(|(token, _, _)| token.chars().count())
                    .max()
                    .unwrap_or_default();
                for (token, gold, predicted) in &miss.pairs {
                    let predicted = predicted.map_or("-", Label::name);
                    let mark = if predicted == gold.name() { ' ' } else { '*' };
                    let pad = width - token.chars().count();
                    let _ = writeln!(
                        out,
                        "  {mark} {token}{}  {:<12} {predicted}",
                        " ".repeat(pad),
                        gold.name()
                    );
                }
                out.push('\n');
            }
            out.push_str("```\n");
        }
        out
    }

    fn header(&self, out: &mut String, leading: &[&str]) {
        out.push('|');
        for column in leading
            .iter()
            .copied()
            .chain(self.systems.iter().map(String::as_str))
        {
            let _ = write!(out, " {column} |");
        }
        out.push_str("\n|");
        for (index, _) in leading.iter().enumerate() {
            out.push_str(if index + 1 < leading.len() {
                "---|"
            } else {
                "---:|"
            });
        }
        for _ in &self.systems {
            out.push_str("---:|");
        }
        out.push('\n');
    }
}

/// Rewrite each raw `<cc>.tsv` in `input`, labelled in `scheme`, as the gold
/// file `<output>/<cc>.tsv`, for every country or only `countries`. Returns
/// the rows written and dropped (no token left after normalization) per file.
pub fn normalize_fixtures(
    scheme: Scheme,
    input: &Path,
    countries: &[String],
    output: &Path,
) -> Result<Vec<(String, usize, usize)>> {
    let mut written = Vec::new();
    for file in country_files(input, countries)? {
        let text = fs::read_to_string(&file.path)
            .with_context(|| format!("failed to read {}", file.path.display()))?;
        let mut rows = Vec::new();
        let mut dropped = 0;
        for (index, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match normalize_raw_line(line, scheme)
                .with_context(|| format!("{}:{}", file.path.display(), index + 1))?
            {
                Some(row) => rows.push(row),
                None => dropped += 1,
            }
        }
        let target = output.join(format!("{}.tsv", file.country));
        write_rows(&target, &rows)?;
        written.push((target.display().to_string(), rows.len(), dropped));
    }
    Ok(written)
}

/// Write the query of every gold row in every variant, one per line, to
/// `<output>/<cc>.<variant>.txt`, for a baseline to parse.
pub fn write_queries(
    gold: &Path,
    countries: &[String],
    output: &Path,
) -> Result<Vec<(String, usize)>> {
    fs::create_dir_all(output).with_context(|| format!("failed to create {}", output.display()))?;
    let mut written = Vec::new();
    for file in country_files(gold, countries)? {
        let rows = read_rows(&file.path)?;
        for variant in Variant::ALL {
            let target = output.join(format!("{}.{}.txt", file.country, variant.name()));
            let mut text = String::new();
            for row in &rows {
                text.push_str(&variant.apply(row).query());
                text.push('\n');
            }
            fs::write(&target, text)
                .with_context(|| format!("failed to write {}", target.display()))?;
            written.push((target.display().to_string(), rows.len()));
        }
    }
    Ok(written)
}

/// One baseline answer per line of `<input>/<cc>.<variant>.jsonl`.
#[derive(Debug, Deserialize)]
struct RawPrediction {
    /// `(label, value)` pairs in the baseline's own scheme, in order.
    #[serde(default)]
    components: Vec<(String, String)>,
    #[serde(default)]
    error: Option<String>,
}

/// Align a baseline's raw answers to every variant of every gold file (or
/// only `countries`) and store them as `<output>/<cc>.<variant>.tsv`, row for
/// row. Fails on a missing answers file or answer, so a stored predictions
/// file always covers its whole gold file; an answer that records the
/// baseline's failure is stored as a failed row, which scores as wrong.
/// Returns the rows written and failed per file.
pub fn align_predictions(
    scheme: Scheme,
    gold: &Path,
    countries: &[String],
    input: &Path,
    output: &Path,
) -> Result<Vec<(String, usize, usize)>> {
    let mut written = Vec::new();
    for file in country_files(gold, countries)? {
        let gold_rows = read_rows(&file.path)?;
        for variant in Variant::ALL {
            let raw_path = input.join(format!("{}.{}.jsonl", file.country, variant.name()));
            let text = fs::read_to_string(&raw_path)
                .with_context(|| format!("failed to read {}", raw_path.display()))?;
            let rows = gold_rows
                .iter()
                .map(|row| variant.apply(row))
                .collect::<Vec<_>>();
            let answers = text
                .lines()
                .filter(|line| !line.is_empty())
                .collect::<Vec<_>>();
            ensure!(
                answers.len() == rows.len(),
                "{} has {} answers for {} gold rows",
                raw_path.display(),
                answers.len(),
                rows.len()
            );
            let mut predicted = Vec::with_capacity(rows.len());
            for (index, (row, answer)) in rows.iter().zip(answers).enumerate() {
                let location = || format!("{}:{}", raw_path.display(), index + 1);
                let answer: RawPrediction = serde_json::from_str(answer).with_context(location)?;
                if answer.error.is_some() {
                    predicted.push(None);
                    continue;
                }
                let labels =
                    align_components(row, &answer.components, scheme).with_context(location)?;
                predicted.push(Some(prediction_row(row, &labels)?));
            }
            let target = predictions_path(output, &file.country, variant);
            write_predictions(&target, &rows, &predicted)?;
            let failed = predicted.iter().filter(|row| row.is_none()).count();
            written.push((target.display().to_string(), predicted.len(), failed));
        }
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("og-parse-bench-{name}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn a_row_is_covered_when_the_dictionaries_hold_each_of_its_names() {
        use crate::parse::lattice::dictionary::Entry;
        let entry = Entry {
            count: 1,
            country: None,
            levels: 0,
        };
        let dictionaries = Dictionaries::from_entries([
            (Field::Locality, "浜松市", entry),
            (Field::Locality, "本州", entry),
            (Field::Region, "静岡県", entry),
            (Field::Region, "shizuoka", entry),
        ]);
        let coverage =
            |line: &str| Coverage::of(&TaggedRow::parse_line(line).expect("row"), &dictionaries);
        // Each part is its own span: "本州" and "浜松市" are two names.
        assert_eq!(
            coverage(concat!(
                "ja	jp	日/country 本/country , 静/region 岡/region 県/region , ",
                "本/locality 州/locality , 浜/locality 松/locality 市/locality , seven/other"
            )),
            Coverage::Covered
        );
        // A name in the wrong field is not covered.
        assert_eq!(
            coverage("ja	jp	浜/region 松/region 市/region"),
            Coverage::Uncovered
        );
        assert_eq!(
            coverage("ja_rm	jp	mori/locality , shizuoka/region"),
            Coverage::Uncovered
        );
        assert_eq!(
            coverage("ja_rm	jp	セブン/other , shizuoka/region , japan/country"),
            Coverage::Covered
        );
    }

    fn options(gold: PathBuf, predictions: PathBuf, show_misses: usize) -> ParseBenchOptions {
        ParseBenchOptions {
            gold,
            predictions,
            countries: Vec::new(),
            pack: None,
            taggers: vec![TaggerKind::Legacy],
            baselines: None,
            show_misses,
            coverage: false,
        }
    }

    /// Store `predict`'s labels for every variant of every row of `gold`.
    fn store_predictions(
        gold: &Path,
        predictions: &Path,
        country: &str,
        predict: impl Fn(&TaggedRow) -> Option<Vec<Label>>,
    ) {
        let rows = read_rows(&gold.join(format!("{country}.tsv"))).expect("gold");
        for variant in Variant::ALL {
            let rows = rows
                .iter()
                .map(|row| variant.apply(row))
                .collect::<Vec<_>>();
            let predicted = rows
                .iter()
                .map(|row| predict(row).map(|labels| row.relabelled(&labels)))
                .collect::<Vec<_>>();
            write_predictions(
                &predictions_path(predictions, country, variant),
                &rows,
                &predicted,
            )
            .expect("predictions");
        }
    }

    #[test]
    fn every_system_is_scored_per_country_and_variant_on_the_same_rows() {
        let root = temp_dir("bench");
        let gold = root.join("gold");
        fs::create_dir_all(&gold).expect("dir");
        fs::write(
            gold.join("us.tsv"),
            "en\tus\t30/house_number main/street st/street , boston/locality\nen\tus\tboston/locality\n",
        )
        .expect("gold");
        fs::write(gold.join("ca.tsv"), "en\tca\ttoronto/locality\n").expect("gold");
        let predictions = root.join("predictions");
        // The baseline gets everything right but calls Boston a region on
        // its own.
        for country in ["us", "ca"] {
            store_predictions(&gold, &predictions.join("base"), country, |row| {
                Some(if row.country == "us" && row.tokens.len() == 1 {
                    vec![Label::Region]
                } else {
                    row.labels()
                })
            });
        }

        let report = run_parse_bench(&options(gold, predictions, 5)).expect("report");
        assert_eq!(report.systems, vec!["og-legacy", "base"]);
        assert_eq!(report.variants, vec!["as-is", "stripped", "messy"]);
        // Two countries and a total, three variants each.
        assert_eq!(report.groups.len(), 9);
        let group = |country: &str, variant: &str| {
            report
                .groups
                .iter()
                .find(|group| group.country == country && group.variant == variant)
                .expect("group")
        };
        let us = group("us", "as-is");
        assert_eq!(us.rows, 2);
        assert_eq!(us.systems["base"].rows_correct, 1);
        assert_eq!(us.systems["base"].full_parse_accuracy, 0.5);
        assert_eq!(us.systems["og-legacy"].rows, 2);
        let all = group("all", "messy");
        assert_eq!(all.rows, 3);
        assert_eq!(all.systems["base"].rows_correct, 2);
        assert!(report.misses.iter().all(|miss| miss.wrong > 0));

        let markdown = report.to_markdown();
        assert!(markdown.contains("| us | as-is | 2 |"), "{markdown}");
        assert!(markdown.contains("| all | messy | 3 |"), "{markdown}");
        assert!(markdown.contains("### Per-label F1 (%)"), "{markdown}");
        assert!(markdown.contains("Worst rows for og-legacy"), "{markdown}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_baseline_missing_a_variant_is_an_error_not_a_partial_score() {
        let root = temp_dir("missing");
        fs::create_dir_all(root.join("gold")).expect("dir");
        fs::write(root.join("gold/us.tsv"), "en\tus\tmain/street\n").expect("gold");
        store_predictions(
            &root.join("gold"),
            &root.join("predictions/base"),
            "us",
            |row| Some(row.labels()),
        );
        fs::remove_file(predictions_path(
            &root.join("predictions/base"),
            "us",
            Variant::Messy,
        ))
        .expect("remove");
        let error = run_parse_bench(&options(root.join("gold"), root.join("predictions"), 0))
            .expect_err("missing");
        assert!(error.to_string().contains("us.messy.tsv"), "{error:#}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn stale_predictions_are_an_error_not_a_score() {
        let root = temp_dir("stale");
        fs::create_dir_all(root.join("gold")).expect("dir");
        fs::write(root.join("gold/us.tsv"), "en\tus\tmain/street\n").expect("gold");
        let base = root.join("predictions/base");
        store_predictions(&root.join("gold"), &base, "us", |row| Some(row.labels()));
        fs::write(
            predictions_path(&base, "us", Variant::AsIs),
            "en\tus\tmaine/street\n",
        )
        .expect("predictions");
        let error = run_parse_bench(&options(root.join("gold"), root.join("predictions"), 0))
            .expect_err("stale");
        assert!(error.to_string().contains("regenerate"), "{error:#}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fixtures_normalize_queries_export_and_answers_align_back() {
        let root = temp_dir("pipeline");
        let raw = root.join("raw");
        fs::create_dir_all(&raw).expect("dir");
        fs::write(
            raw.join("us.tsv"),
            "en\tus\t30/house_number W./road 26th/road St/road ,/SEP New/city York/city\nen\tus\t-/road\n",
        )
        .expect("raw");
        fs::write(raw.join("ca.tsv"), "en\tca\tToronto/city\n").expect("raw");
        let gold = root.join("gold");
        let written = normalize_fixtures(Scheme::Libpostal, &raw, &["us".to_string()], &gold)
            .expect("normalize");
        assert_eq!(written.len(), 1, "only the countries asked for");
        assert_eq!((written[0].1, written[0].2), (1, 1));
        assert!(!gold.join("ca.tsv").exists());

        let queries = root.join("queries");
        let written = write_queries(&gold, &[], &queries).expect("queries");
        assert_eq!(written.len(), 3);
        assert_eq!(
            fs::read_to_string(queries.join("us.as-is.txt")).expect("queries"),
            "30 w 26th st, new york\n"
        );
        assert_eq!(
            fs::read_to_string(queries.join("us.stripped.txt")).expect("queries"),
            "30 w 26th st new york\n"
        );
        let messy = fs::read_to_string(queries.join("us.messy.txt")).expect("queries");
        assert!(!messy.contains(','), "{messy}");

        let answers = root.join("answers");
        fs::create_dir_all(&answers).expect("dir");
        for variant in Variant::ALL {
            // The baseline answers with the variant's own query text.
            let query = fs::read_to_string(queries.join(format!("us.{}.txt", variant.name())))
                .expect("query");
            let answer = serde_json::json!({ "components": [["road", query.trim()]] });
            fs::write(
                answers.join(format!("us.{}.jsonl", variant.name())),
                format!("{answer}\n"),
            )
            .expect("answers");
        }
        let predictions = root.join("predictions/libpostal");
        let written = align_predictions(Scheme::Libpostal, &gold, &[], &answers, &predictions)
            .expect("align");
        assert_eq!(written.len(), 3);
        assert_eq!(
            fs::read_to_string(predictions_path(&predictions, "us", Variant::AsIs))
                .expect("predictions"),
            "en\tus\t30/street w/street 26th/street st/street , new/street york/street\n"
        );
        assert_eq!(
            fs::read_to_string(predictions_path(&predictions, "us", Variant::Stripped))
                .expect("predictions"),
            "en\tus\t30/street w/street 26th/street st/street new/street york/street\n"
        );
        let report = run_parse_bench(&ParseBenchOptions {
            baselines: Some(vec!["libpostal".to_string()]),
            ..options(gold, root.join("predictions"), 0)
        })
        .expect("report");
        assert_eq!(report.groups.len(), 3);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_row_the_baseline_failed_on_is_kept_and_scored_wrong() {
        let root = temp_dir("failed");
        fs::create_dir_all(root.join("gold")).expect("dir");
        fs::write(
            root.join("gold/us.tsv"),
            "en\tus\tboston/locality\nen\tus\tother/other\n",
        )
        .expect("gold");
        fs::create_dir_all(root.join("answers")).expect("dir");
        for variant in Variant::ALL {
            fs::write(
                root.join(format!("answers/us.{}.jsonl", variant.name())),
                "{\"components\": [[\"city\", \"boston\"]]}\n{\"error\": \"timed out\"}\n",
            )
            .expect("answers");
        }
        let predictions = root.join("predictions");
        let written = align_predictions(
            Scheme::Libpostal,
            &root.join("gold"),
            &[],
            &root.join("answers"),
            &predictions.join("base"),
        )
        .expect("align");
        assert_eq!((written[0].1, written[0].2), (2, 1));
        assert_eq!(
            fs::read_to_string(predictions_path(
                &predictions.join("base"),
                "us",
                Variant::AsIs
            ))
            .expect("predictions"),
            "en\tus\tboston/locality\nen\tus\t!failed\n"
        );

        let report = run_parse_bench(&options(root.join("gold"), predictions, 0)).expect("report");
        let base = &report.groups[0].systems["base"];
        assert_eq!(base.rows, 2);
        assert_eq!(base.rows_correct, 1);
        assert_eq!(base.rows_misaligned, 1);
        let _ = fs::remove_dir_all(root);
    }
}
