//! Pack writer and reader.
//!
//! A Pack is one container file (see [`crate::container`]) holding the record
//! store, admin context table, text index, spatial index and the address
//! parser's dictionaries and weights (see [`crate::parse::lattice`]). Builds write a new
//! generation next to the published one and switch the `CURRENT` pointer only
//! after the new Pack passes validation, so running servers never see a
//! partial Pack.
//!
//! ```text
//! <pack dir>/CURRENT                         id of the published generation
//! <pack dir>/generations/<id>/pack.ogp       the Pack
//! <pack dir>/generations/<id>/audit/         build report and rejections
//! ```
//!
//! Sealing writes the parts the writer holds in memory into the Pack file
//! first, then copies in the parts it streamed to scratch one at a time
//! ([`Sealing`]), so a build can checkpoint between parts and go on from
//! there.

use std::{
    collections::{BTreeMap, HashMap},
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::error::{Error, Result};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    container::{self, Container},
    context::{self, TupleWriter},
    extsort::Scratch,
    labels,
    memory::Budget,
    parse::lattice::{
        CountryCode,
        dictionary::{Builder, RecordNames},
    },
    record::{InterpolationRecord, Layer, Record, RejectedRecord, StreetRecord},
    records, spatial_index,
    text_index::{self, PostcodeAreas},
};

pub use crate::{
    context::{AdminContextTuple, RecordContext},
    records::{ContextRecord, Point, PointPrecision, Source, Summary},
};

pub type RecordId = u64;

mod publication;
pub use publication::{resolve_pack_path, unpublished_generations};

pub const PACK_FILE: &str = "pack.ogp";
pub const AUDIT_DIR: &str = "audit";
pub const REJECTIONS_FILE: &str = "rejections.jsonl";
pub const BUILD_REPORT_FILE: &str = "build-report.json";
pub const PACK_SCHEMA_VERSION: u32 = 9;

const MANIFEST_SECTION: &str = "manifest";
const MANIFEST_VERSION: u32 = 1;
const POSTCODE_ADDRESSES_SECTION: &str = "postcode_addresses";
const POSTCODE_ADDRESSES_VERSION: u32 = 1;
const SCRATCH_DIR: &str = ".scratch";
const DEFAULT_TEXT_INDEX_MEMORY_BYTES: usize = 1 << 30;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub schema_version: u32,
    pub crate_version: String,
    pub built_at_unix: u64,
    pub record_count: u64,
    pub layer_counts: BTreeMap<String, u64>,
    pub string_count: u64,
    pub context_tuple_count: u64,
    pub text_document_count: u64,
    pub spatial: SpatialManifest,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpatialManifest {
    pub point_count: u64,
    pub line_count: u64,
    pub block_count: u64,
    pub context_point_count: u64,
}

/// How many addresses state a postcode, per country: what one postcode's
/// addresses are a share of, so search weighs agreeing on a postcode by how
/// few records it covers in its country. Summed from the postcode records as
/// they are written, each counting the addresses it was derived from. `None`
/// holds the postcodes no country holds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PostcodeAddresses(BTreeMap<Option<CountryCode>, u64>);

/// Bytes of one [`PostcodeAddresses`] entry: the country as a `u16` (0 for
/// none) and the count, little-endian.
const POSTCODE_ADDRESSES_ENTRY_BYTES: usize = 10;

impl PostcodeAddresses {
    fn add(&mut self, country: Option<CountryCode>, addresses: u64) {
        *self.0.entry(country).or_default() += addresses;
    }

    /// The addresses of `country` that state a postcode.
    pub fn of(&self, country: Option<CountryCode>) -> u64 {
        self.0.get(&country).copied().unwrap_or_default()
    }

    /// Every country some postcode is in.
    pub fn countries(&self) -> impl Iterator<Item = CountryCode> + '_ {
        self.0.keys().flatten().copied()
    }

    fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.0.len() * POSTCODE_ADDRESSES_ENTRY_BYTES);
        for (country, addresses) in &self.0 {
            bytes.extend_from_slice(&country.map_or(0, |country| country.to_u16()).to_le_bytes());
            bytes.extend_from_slice(&addresses.to_le_bytes());
        }
        bytes
    }

    fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if !bytes.len().is_multiple_of(POSTCODE_ADDRESSES_ENTRY_BYTES) {
            return Err(Error::Decode("postcode address counts are truncated"));
        }
        let mut counts = BTreeMap::new();
        for entry in bytes.as_chunks::<POSTCODE_ADDRESSES_ENTRY_BYTES>().0 {
            let (country, addresses) = entry.split_at(2);
            let country = match u16::from_le_bytes([country[0], country[1]]) {
                0 => None,
                code => Some(CountryCode::from_u16(code).ok_or(Error::Decode(
                    "postcode address country is not a country code",
                ))?),
            };
            let addresses = u64::from_le_bytes(addresses.try_into().expect("eight bytes"));
            counts.insert(country, addresses);
        }
        Ok(Self(counts))
    }
}

#[derive(Debug, Clone)]
pub struct WriterOptions {
    /// Pool every buffer of the build reserves from.
    pub memory: Arc<Budget>,
    /// Memory for Tantivy's indexing buffers, taken from the pool.
    pub text_index_memory_bytes: usize,
}

impl Default for WriterOptions {
    fn default() -> Self {
        Self {
            memory: Budget::unlimited(),
            text_index_memory_bytes: DEFAULT_TEXT_INDEX_MEMORY_BYTES,
        }
    }
}

/// The names of the areas a record lies in that its text document carries:
/// every name of each area but its country and the place area it lies in
/// (an island), which are indexed by their `name` alone. A country or an
/// island has hundreds of names, one per language, and every record in it
/// would carry them all; the parser's dictionaries still hold every one.
fn indexed_area_names<'a>(
    context: &RecordContext,
    names: &'a HashMap<RecordId, Vec<String>>,
) -> impl Iterator<Item = &'a str> {
    let tuple = &context.admin_context;
    let large = [tuple.country_record_id(), tuple.place_record_id()];
    context
        .admin_context
        .parent_record_ids()
        .filter_map(move |id| {
            let names = names.get(&id)?;
            Some(if large.contains(&Some(id)) {
                &names[..names.len().min(1)]
            } else {
                &names[..]
            })
        })
        .flatten()
        .map(String::as_str)
}

/// Writes records in their final order. Record ids are assigned sequentially.
#[derive(Debug)]
pub struct Writer {
    scratch: Arc<Scratch>,
    records: records::Writer,
    contexts: TupleWriter,
    text: text_index::Writer,
    spatial: spatial_index::Writer,
    layer_counts: BTreeMap<Layer, u64>,
    context_names: Arc<HashMap<RecordId, Vec<String>>>,
    country_codes: Arc<HashMap<RecordId, CountryCode>>,
    postcode_areas: Arc<PostcodeAreas>,
    postcode_addresses: PostcodeAddresses,
    dictionaries: Builder,
    /// Last, so it drops after every writer of a file in it (see
    /// [`Generation`]'s drop).
    generation: Generation,
}

/// A finished, unpublished Pack.
#[derive(Debug)]
pub struct SealedPack {
    generation: Generation,
    manifest: Manifest,
    bytes: u64,
    /// Where the sections end, before the table of contents.
    sections: container::State,
}

/// A Pack generation being built, beside the published one. Removed when
/// dropped unless it was published, or its build's scratch holds a
/// checkpoint the build may go on from: no reader can be using it.
#[derive(Debug)]
pub struct Generation {
    destination: PathBuf,
    path: PathBuf,
    keep: bool,
    scratch: Option<Arc<Scratch>>,
}

/// A file in an unpublished generation naming its build's scratch
/// directory, so a later build of the Pack finds a build to go on from.
pub const SCRATCH_POINTER_FILE: &str = "scratch";

impl Generation {
    /// A new, empty generation of the Pack at `destination`.
    pub fn create(destination: impl AsRef<Path>) -> Result<Self> {
        let destination = destination.as_ref().to_path_buf();
        Ok(Self {
            path: publication::create_generation(&destination)?,
            destination,
            keep: false,
            scratch: None,
        })
    }

    /// The unpublished generation at `path`, of the Pack at `destination`,
    /// to go on building.
    pub fn reopen(destination: impl AsRef<Path>, path: impl AsRef<Path>) -> Self {
        Self {
            destination: destination.as_ref().to_path_buf(),
            path: path.as_ref().to_path_buf(),
            keep: false,
            scratch: None,
        }
    }

    /// Create the build's scratch directory: in `scratch_dir` when given,
    /// else inside the generation. Its bytes are capped by `budget`.
    pub fn create_scratch(
        &mut self,
        scratch_dir: Option<&Path>,
        budget: Option<u64>,
    ) -> Result<Arc<Scratch>> {
        let scratch = Scratch::create(self.scratch_path(scratch_dir))?;
        scratch.set_budget(budget);
        self.scratch = Some(Arc::clone(&scratch));
        Ok(scratch)
    }

    /// Where the build's scratch directory goes.
    pub fn scratch_path(&self, scratch_dir: Option<&Path>) -> PathBuf {
        scratch_dir
            .map(|dir| dir.join(self.path.file_name().expect("generation name")))
            .unwrap_or_else(|| self.path.join(SCRATCH_DIR))
    }

    /// Tie the generation to `scratch`: it outlives a failed build while
    /// `scratch` holds a checkpoint.
    pub fn attach(&mut self, scratch: &Arc<Scratch>) {
        self.scratch = Some(Arc::clone(scratch));
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn destination(&self) -> &Path {
        &self.destination
    }
}

/// A generation no build keeps is removed as it drops. Windows keeps a
/// directory while a file in it is open, so whatever holds one drops first:
/// the structs holding a generation declare it last.
impl Drop for Generation {
    fn drop(&mut self) {
        let resumable = self
            .scratch
            .as_ref()
            .is_some_and(|scratch| scratch.resumable());
        if !self.keep && !resumable {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

/// Statistics a build reports after sealing the Pack.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct BuildStats {
    pub bytes: u64,
    pub sections: BTreeMap<String, u64>,
    pub text_index_bytes: u64,
}

impl Writer {
    pub fn create(destination: impl AsRef<Path>) -> Result<Self> {
        Self::create_with(destination, WriterOptions::default())
    }

    pub fn create_with(destination: impl AsRef<Path>, options: WriterOptions) -> Result<Self> {
        let mut generation = Generation::create(destination)?;
        let scratch = generation.create_scratch(None, None)?;
        Self::create_in(generation, scratch, options)
    }

    /// Write into `generation`, streaming to `scratch`.
    pub fn create_in(
        generation: Generation,
        scratch: Arc<Scratch>,
        options: WriterOptions,
    ) -> Result<Self> {
        Ok(Self {
            records: records::Writer::create(&scratch, &options.memory)?,
            contexts: TupleWriter::default(),
            text: text_index::Writer::create(
                &scratch,
                &options.memory,
                options.text_index_memory_bytes,
            )?,
            spatial: spatial_index::Writer::new(&scratch)?,
            scratch,
            generation,
            layer_counts: BTreeMap::new(),
            context_names: Arc::default(),
            country_codes: Arc::default(),
            postcode_areas: Arc::default(),
            postcode_addresses: PostcodeAddresses::default(),
            dictionaries: Builder::default(),
        })
    }

    /// Scratch directory shared with the builder's own sorters.
    pub fn scratch(&self) -> &Arc<Scratch> {
        &self.scratch
    }

    /// Directory of the generation being built, for build outputs that live
    /// beside the Pack file.
    pub fn generation(&self) -> &Path {
        &self.generation.path
    }

    pub fn record_count(&self) -> u64 {
        self.records.record_count()
    }

    /// Names of the records admin contexts point at, each area's `name`
    /// first, so the records written after this can be indexed under the
    /// areas they lie in.
    pub fn set_context_names(&mut self, names: HashMap<RecordId, Vec<String>>) {
        self.context_names = Arc::new(names);
    }

    /// Country codes of the country records admin contexts point at, so the
    /// parser's dictionaries know which country each name is used in.
    pub fn set_country_codes(&mut self, codes: HashMap<RecordId, CountryCode>) {
        // The same map indexes each record's country, so a Pack covering
        // several countries can be searched one at a time. Cloned rather
        // than shared: the dictionary builder consumes its copy, and the map
        // holds one entry per country record, not per record.
        self.country_codes = Arc::new(codes.clone());
        self.dictionaries.set_country_codes(codes);
    }

    /// The groups of regions each region lies in, by the region's id.
    pub fn set_region_groups(&mut self, groups: HashMap<RecordId, Vec<RecordId>>) {
        self.dictionaries.set_region_groups(groups);
    }

    /// Postcode centroids, so the records written after this that have no
    /// postcode of their own are ranked by the nearest one.
    pub fn set_postcode_areas(&mut self, areas: PostcodeAreas) {
        self.postcode_areas = Arc::new(areas);
    }

    pub fn write(&mut self, record: &Record, context: Option<RecordContext>) -> Result<RecordId> {
        let first = self.records.record_count();
        self.write_batch(std::slice::from_ref(&(record.clone(), context)))?;
        Ok(first)
    }

    /// Write records in order: [`BatchPreparer::prepare`], then [`Self::commit`].
    pub fn write_batch(&mut self, batch: &[(Record, Option<RecordContext>)]) -> Result<()> {
        let prepared = self.preparer().prepare(batch.to_vec())?;
        self.commit(prepared)
    }

    /// What computes the text documents, spatial cells and dictionary names
    /// of the records written next, apart from the writer, so a builder can
    /// prepare one batch while the writer appends the one before.
    pub fn preparer(&self) -> BatchPreparer {
        BatchPreparer {
            next: self.records.record_count(),
            fields: self.text.fields(),
            context_names: Arc::clone(&self.context_names),
            country_codes: Arc::clone(&self.country_codes),
            postcode_areas: Arc::clone(&self.postcode_areas),
        }
    }

    /// Append a prepared batch. The store, the text index and the spatial
    /// pairs take records one at a time, in order.
    pub fn commit(&mut self, batch: PreparedBatch) -> Result<()> {
        let first = self.records.record_count();
        if batch.first != first {
            return Err(Error::Inconsistent(format!(
                "batch prepared for record {} was committed at record {first}",
                batch.first
            )));
        }
        for (offset, prepared) in batch.records.into_iter().enumerate() {
            let PreparedRecord {
                record,
                context,
                document,
                names,
            } = prepared;
            let record_id = first + offset as u64;
            if let Some(context) = &context
                && names.is_place()
            {
                self.dictionaries.add_place(record_id, context, &names);
            }
            self.dictionaries.add(names, context.as_ref());
            let context = match context {
                Some(context) => self.contexts.intern(context)?,
                None => None,
            };
            self.records.write(&record, context)?;
            match document {
                text_index::Prepared::Document(document) => self.text.add(document)?,
                text_index::Prepared::Address(document) => self.text.add_address(document)?,
            }
            self.spatial.add(record_id, &record)?;
            if let Record::Postcode(postcode) = &record {
                self.postcode_addresses
                    .add(postcode.country, postcode.source.record_count);
            }
            *self.layer_counts.entry(record.layer()).or_default() += 1;
        }
        Ok(())
    }

    /// Write the Pack file without publishing it.
    pub fn seal(self) -> Result<(SealedPack, BuildStats)> {
        let mut sealing = self.into_sealing()?;
        while sealing.seal_part()?.is_some() {}
        sealing.finish()
    }

    /// Start the Pack file with the parts held in memory, the context
    /// table, the parser's dictionaries and the postcode counts, once every
    /// record is written. The parts streamed to scratch follow, one
    /// [`Sealing::seal_part`] at a time.
    pub fn into_sealing(self) -> Result<Sealing> {
        let Self {
            generation,
            scratch,
            records,
            contexts,
            text,
            spatial,
            layer_counts,
            context_names,
            postcode_addresses,
            dictionaries,
            ..
        } = self;
        let mut pack = container::Writer::create(generation.path.join(PACK_FILE))?;
        let counts = SealCounts {
            record_count: records.record_count(),
            string_count: records.string_count(),
            context_tuple_count: contexts.tuple_count(),
            layer_counts: layer_counts
                .iter()
                .map(|(layer, count)| (layer.as_str().to_string(), *count))
                .collect(),
            text: None,
            spatial: None,
        };
        contexts.finish(&mut pack)?;
        dictionaries.finish(&mut pack, &context_names)?;
        drop(context_names);
        pack.add(
            POSTCODE_ADDRESSES_SECTION,
            POSTCODE_ADDRESSES_VERSION,
            &postcode_addresses.to_bytes(),
        )?;
        Ok(Sealing {
            generation,
            scratch,
            pack,
            records: Some(records),
            text: Some(text),
            spatial: Some(spatial),
            counts,
        })
    }

    /// Seal and publish.
    pub fn finish(self) -> Result<Manifest> {
        self.seal()?.0.publish()
    }
}

/// A Pack file being sealed: the parts held in memory are in it, and the
/// parts streamed to scratch are copied in one at a time.
#[derive(Debug)]
pub struct Sealing {
    scratch: Arc<Scratch>,
    pack: container::Writer,
    records: Option<records::Writer>,
    text: Option<text_index::Writer>,
    spatial: Option<spatial_index::Writer>,
    counts: SealCounts,
    /// Last, so it drops after every writer of a file in it (see
    /// [`Generation`]'s drop).
    generation: Generation,
}

/// What the Pack manifest counts, gathered as the parts are sealed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SealCounts {
    record_count: u64,
    string_count: u64,
    context_tuple_count: u64,
    layer_counts: BTreeMap<String, u64>,
    text: Option<text_index::Commit>,
    spatial: Option<spatial_index::Commit>,
}

/// A Pack file being sealed, as a checkpoint names it: how far the file
/// was written, and the parts left to copy in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealingState {
    pack: container::State,
    records: Option<records::WriterState>,
    text: Option<text_index::WriterState>,
    spatial: Option<spatial_index::WriterState>,
    counts: SealCounts,
}

impl Sealing {
    /// Copy the next part streamed to scratch into the Pack file, removing
    /// its scratch: its name, or `None` once every part is in.
    pub fn seal_part(&mut self) -> Result<Option<&'static str>> {
        self.scratch.check_cancelled()?;
        if let Some(records) = self.records.take() {
            records.finish(&mut self.pack)?;
            return Ok(Some("records"));
        }
        if let Some(text) = self.text.take() {
            self.counts.text = Some(text.finish(&mut self.pack)?);
            return Ok(Some("text index"));
        }
        if let Some(spatial) = self.spatial.take() {
            self.counts.spatial = Some(spatial.finish(&mut self.pack)?);
            return Ok(Some("spatial index"));
        }
        Ok(None)
    }

    /// Where the sealing stands, for a checkpoint: the Pack file is synced.
    pub fn state(&mut self) -> Result<SealingState> {
        Ok(SealingState {
            pack: self.pack.state()?,
            records: self
                .records
                .as_mut()
                .map(records::Writer::state)
                .transpose()?,
            text: self
                .text
                .as_mut()
                .map(text_index::Writer::state)
                .transpose()?,
            spatial: match &mut self.spatial {
                Some(spatial) => Some(spatial.state(&self.scratch)?),
                None => None,
            },
            counts: self.counts.clone(),
        })
    }

    /// The scratch files and directories the parts left to copy are in.
    pub fn files(&self) -> Vec<PathBuf> {
        let mut files = Vec::new();
        if let Some(records) = &self.records {
            files.extend(records.files());
        }
        if let Some(text) = &self.text {
            files.push(text.path().to_path_buf());
        }
        if let Some(spatial) = &self.spatial {
            files.extend(spatial.files());
        }
        files
    }

    /// The sealing a checkpoint left, in `generation`, its parts in
    /// `scratch`.
    pub fn restore(
        generation: Generation,
        scratch: Arc<Scratch>,
        options: &WriterOptions,
        state: &SealingState,
    ) -> Result<Self> {
        let pack = container::Writer::resume(generation.path.join(PACK_FILE), &state.pack)?;
        Ok(Self {
            records: state
                .records
                .as_ref()
                .map(|records| records::Writer::restore(&scratch, &options.memory, records))
                .transpose()?,
            text: state
                .text
                .as_ref()
                .map(|text| {
                    text_index::Writer::restore(
                        &scratch,
                        &options.memory,
                        options.text_index_memory_bytes,
                        text,
                    )
                })
                .transpose()?,
            spatial: state
                .spatial
                .as_ref()
                .map(|spatial| spatial_index::Writer::restore(&scratch, spatial))
                .transpose()?,
            generation,
            scratch,
            pack,
            counts: state.counts.clone(),
        })
    }

    /// Write the manifest and the table of contents, once every part is in.
    pub fn finish(mut self) -> Result<(SealedPack, BuildStats)> {
        if self.records.is_some() || self.text.is_some() || self.spatial.is_some() {
            return Err(Error::Inconsistent(
                "a Pack was finished before every part was sealed".into(),
            ));
        }
        let counts = &self.counts;
        let text = counts.text.unwrap_or(text_index::Commit {
            document_count: 0,
            bytes: 0,
        });
        let spatial = counts.spatial.clone().unwrap_or_default();
        let manifest = Manifest {
            schema_version: PACK_SCHEMA_VERSION,
            crate_version: env!("CARGO_PKG_VERSION").to_string(),
            built_at_unix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_secs())
                .unwrap_or_default(),
            record_count: counts.record_count,
            layer_counts: counts.layer_counts.clone(),
            string_count: counts.string_count,
            context_tuple_count: counts.context_tuple_count,
            text_document_count: text.document_count,
            spatial: SpatialManifest {
                point_count: spatial.point_count,
                line_count: spatial.line_count,
                block_count: spatial.block_count,
                context_point_count: spatial.context_point_count,
            },
        };
        self.pack.add(
            MANIFEST_SECTION,
            MANIFEST_VERSION,
            &serde_json::to_vec_pretty(&manifest)?,
        )?;
        let sections = self.pack.state()?;
        let bytes = self.pack.finish()?;
        let stats = BuildStats {
            bytes,
            sections: sections
                .sections
                .iter()
                .map(|(name, info)| (name.clone(), info.len))
                .collect(),
            text_index_bytes: text.bytes,
        };
        Ok((
            SealedPack {
                generation: self.generation,
                manifest,
                bytes,
                sections,
            },
            stats,
        ))
    }
}

/// A sealed Pack as a checkpoint names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealedState {
    manifest: Manifest,
    sections: container::State,
    stats: BuildStats,
}

impl SealedPack {
    /// The sealed Pack and what sealing it reported, for a checkpoint.
    pub fn state(&self, stats: &BuildStats) -> SealedState {
        SealedState {
            manifest: self.manifest.clone(),
            sections: self.sections.clone(),
            stats: stats.clone(),
        }
    }

    /// The Pack a checkpoint sealed, in `generation`. Sections a failed
    /// build added after the checkpoint are cut off again.
    pub fn restore(generation: Generation, state: &SealedState) -> Result<(Self, BuildStats)> {
        let pack = container::Writer::resume(generation.path.join(PACK_FILE), &state.sections)?;
        let bytes = pack.finish()?;
        Ok((
            Self {
                generation,
                manifest: state.manifest.clone(),
                bytes,
                sections: state.sections.clone(),
            },
            state.stats.clone(),
        ))
    }

    pub fn generation(&self) -> &Path {
        &self.generation.path
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// The Pack file, to read before it is published.
    pub fn path(&self) -> PathBuf {
        self.generation.path.join(PACK_FILE)
    }

    /// Add a section made from the sealed Pack itself, such as the parser
    /// weights trained on its records. Nothing reads the Pack file yet.
    pub fn add_section(&mut self, name: &str, version: u32, bytes: &[u8]) -> Result<()> {
        let mut pack = container::Writer::append(self.path())?;
        pack.add(name, version, bytes)?;
        self.bytes = pack.finish()?;
        Ok(())
    }

    /// Validate the Pack with the serving readers, then point `CURRENT` at it.
    pub fn publish(mut self) -> Result<Manifest> {
        publication::publish(
            &self.generation.destination,
            &self.generation.path,
            &self.manifest,
            &mut self.generation.keep,
        )?;
        Ok(self.manifest)
    }
}

pub struct Reader {
    path: PathBuf,
    container: Container,
    manifest: Manifest,
    records: records::Reader,
    context: context::Reader,
    postcode_addresses: PostcodeAddresses,
}

impl std::fmt::Debug for Reader {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Reader")
            .field("path", &self.path)
            .field("manifest", &self.manifest)
            .finish_non_exhaustive()
    }
}

impl Reader {
    /// Open a Pack directory (its published generation) or a Pack file.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = resolve_pack_path(path)?;
        let container = Container::open(&path)?;
        let manifest: Manifest =
            serde_json::from_slice(&container.section(MANIFEST_SECTION, MANIFEST_VERSION)?)?;
        if manifest.schema_version != PACK_SCHEMA_VERSION {
            return Err(Error::UnsupportedVersion {
                found: manifest.schema_version,
            });
        }
        let records = records::Reader::open(&container)?;
        if records.record_count() != manifest.record_count {
            return Err(Error::ManifestMismatch);
        }
        let context = context::Reader::open(&container)?;
        let postcode_addresses = PostcodeAddresses::from_bytes(
            &container.section(POSTCODE_ADDRESSES_SECTION, POSTCODE_ADDRESSES_VERSION)?,
        )?;
        Ok(Self {
            path,
            container,
            manifest,
            records,
            context,
            postcode_addresses,
        })
    }

    pub const fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// The Pack file opened by this reader.
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn container(&self) -> &Container {
        &self.container
    }

    /// Check every section of the Pack file against its checksum.
    pub fn verify(&self) -> Result<()> {
        self.container.verify()
    }

    /// See [`PostcodeAddresses`].
    pub fn postcode_addresses(&self) -> &PostcodeAddresses {
        &self.postcode_addresses
    }

    pub fn records(&self) -> &records::Reader {
        &self.records
    }

    /// Section sizes in bytes, by section name.
    pub fn section_sizes(&self) -> BTreeMap<String, u64> {
        self.container
            .sections()
            .iter()
            .map(|(name, info)| (name.clone(), info.len))
            .collect()
    }

    /// The summary search and autocomplete return. A POI whose address states
    /// no locality is labelled with the locality it lies in.
    pub fn record_summary(&self, record_id: RecordId) -> Result<Summary> {
        let mut summary = self.records.summary(record_id)?;
        if summary.layer == Layer::Poi.as_str()
            && let Record::Poi(poi) = self.records.record(record_id)?
            && poi
                .address
                .as_ref()
                .is_none_or(|address| address.locality.is_none())
            && let Some(locality) = self.locality(record_id)?
        {
            summary.label = labels::poi_label(&poi.name, poi.address.as_ref(), Some(&locality));
        }
        Ok(summary)
    }

    /// Name of the locality a record lies in, from its boundary context, or
    /// of its district where no locality covers it: a single-tier city such as
    /// Toronto is a district with no locality over its core.
    pub fn locality(&self, record_id: RecordId) -> Result<Option<String>> {
        let Some(area_id) = self.boundary_context(record_id)?.and_then(|context| {
            let tuple = &context.admin_context;
            tuple.locality_record_id().or(tuple.district_record_id())
        }) else {
            return Ok(None);
        };
        Ok(self.context_record(area_id)?.map(|record| record.name))
    }

    pub fn record_json(&self, record_id: RecordId) -> Result<Value> {
        self.records.record_json(record_id)
    }

    pub fn records_json_by_layer(&self, layer: &str, limit: usize) -> Result<Vec<Value>> {
        let layer = Layer::parse(layer).ok_or(Error::Decode("unknown record layer"))?;
        let mut records = Vec::new();
        for record_id in 0..self.records.record_count() {
            if self.records.header(record_id)?.layer != layer {
                continue;
            }
            records.push(self.record_json(record_id)?);
            if limit > 0 && records.len() >= limit {
                break;
            }
        }
        Ok(records)
    }

    pub fn find_by_source_id(&self, source_id: &str) -> Result<Option<RecordId>> {
        for record_id in 0..self.records.record_count() {
            if self.record_summary(record_id)?.id == source_id {
                return Ok(Some(record_id));
            }
        }
        Ok(None)
    }

    pub fn interpolation(&self, record_id: RecordId) -> Result<Option<InterpolationRecord>> {
        if self.records.header(record_id)?.layer != Layer::Interpolation {
            return Ok(None);
        }
        Ok(match self.records.record(record_id)? {
            Record::Interpolation(record) => Some(record),
            _ => None,
        })
    }

    pub fn street(&self, record_id: RecordId) -> Result<Option<StreetRecord>> {
        if self.records.header(record_id)?.layer != Layer::Street {
            return Ok(None);
        }
        Ok(match self.records.record(record_id)? {
            Record::Street(record) => Some(record),
            _ => None,
        })
    }

    pub fn context_record(&self, record_id: RecordId) -> Result<Option<ContextRecord>> {
        self.records.context_record(record_id)
    }

    pub fn boundary_context(&self, record_id: RecordId) -> Result<Option<RecordContext>> {
        self.records
            .header(record_id)?
            .context
            .map(|reference| self.context.resolve(reference))
            .transpose()
    }

    /// Rejected source objects recorded by the build that produced this Pack.
    pub fn rejections(&self, limit: usize) -> Result<Vec<RejectedRecord>> {
        let path = self
            .path
            .parent()
            .map(|dir| dir.join(AUDIT_DIR).join(REJECTIONS_FILE))
            .ok_or(Error::Inconsistent(
                "Pack file has no parent directory".into(),
            ))?;
        let file = fs::File::open(&path).map_err(|source| Error::IoPath {
            action: "failed to open build audit",
            path: path.clone(),
            source,
        })?;
        let mut rejections = Vec::new();
        for line in BufReader::new(file).lines() {
            if limit > 0 && rejections.len() >= limit {
                break;
            }
            rejections.push(serde_json::from_str(&line?)?);
        }
        Ok(rejections)
    }
}

/// Computes what a batch of records adds to the Pack's indexes, in parallel,
/// for the record ids they will be written at. See [`Writer::preparer`].
#[derive(Debug)]
pub struct BatchPreparer {
    next: RecordId,
    fields: text_index::Fields,
    context_names: Arc<HashMap<RecordId, Vec<String>>>,
    country_codes: Arc<HashMap<RecordId, CountryCode>>,
    postcode_areas: Arc<PostcodeAreas>,
}

impl BatchPreparer {
    /// Prepare the records that follow the last batch this prepared.
    pub fn prepare(
        &mut self,
        batch: Vec<(Record, Option<RecordContext>)>,
    ) -> Result<PreparedBatch> {
        let first = self.next;
        let records = batch
            .into_par_iter()
            .enumerate()
            .map(|(offset, (record, context))| {
                let record_id = first + offset as u64;
                let names = context
                    .iter()
                    .flat_map(|context| indexed_area_names(context, &self.context_names))
                    .collect::<Vec<_>>();
                // The country the record lies in, from the country record its
                // admin context points at. A record outside every country
                // boundary has none and no country filter finds it.
                let country = context
                    .as_ref()
                    .and_then(|context| context.admin_context.country_record_id())
                    .and_then(|id| self.country_codes.get(&id))
                    .map(CountryCode::as_str);
                Ok(PreparedRecord {
                    document: self.fields.prepare(
                        record_id,
                        &record,
                        &names,
                        &self.postcode_areas,
                        country,
                    ),
                    names: RecordNames::of(&record),
                    record,
                    context,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        self.next += records.len() as u64;
        Ok(PreparedBatch { first, records })
    }
}

/// Records with their text documents, spatial cells and dictionary names,
/// ready for [`Writer::commit`].
#[derive(Debug)]
pub struct PreparedBatch {
    first: RecordId,
    records: Vec<PreparedRecord>,
}

impl PreparedBatch {
    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// The records with the ids they will be written at.
    pub fn records(&self) -> impl Iterator<Item = (RecordId, &Record)> {
        self.records
            .iter()
            .enumerate()
            .map(|(offset, prepared)| (self.first + offset as u64, &prepared.record))
    }
}

#[derive(Debug)]
struct PreparedRecord {
    record: Record,
    context: Option<RecordContext>,
    document: text_index::Prepared,
    names: RecordNames,
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use crate::{
        record::{
            AddressComponents, AddressRecord, LocationPrecision, ObjectType, SourceProvenance,
            point_geometry,
        },
        search::{TextSearchOptions, TextSearcher},
    };

    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("open-geocode-{name}-{}", uuid::Uuid::new_v4()))
    }

    fn address_record(object_id: i64, number: &str, street: &str) -> Record {
        Record::Address(AddressRecord {
            disputed: false,
            street_postcodes: Vec::new(),
            address: AddressComponents {
                areas: Vec::new(),
                number: number.to_string(),
                street: Some(street.to_string()),
                place: None,
                unit: None,
                locality: None,
                region: None,
                postcode: None,
                country: None,
            },
            geometry: point_geometry(-79.0, 43.0),
            location_precision: LocationPrecision::Point,
            source: SourceProvenance::osm(ObjectType::Node, object_id),
        })
    }

    fn write_pack(root: &Path, records: &[Record]) -> Manifest {
        let mut writer = Writer::create(root).expect("writer");
        for record in records {
            writer.write(record, None).expect("write");
        }
        writer.finish().expect("finish")
    }

    fn first_hit(root: &Path, query: &str) -> Option<String> {
        TextSearcher::open(root)
            .expect("searcher")
            .search(TextSearchOptions {
                query: query.into(),
                limit: 1,
                layer: None,
                countries: Vec::new(),
                focus: None,
                bbox: None,
            })
            .expect("search")
            .first()
            .map(|hit| hit.record.id.clone())
    }

    #[test]
    fn writes_one_file_and_reads_records_by_row_layer_and_source_id() {
        let root = temp_root("pack");
        let manifest = write_pack(
            &root,
            &[
                address_record(1, "10", "King Street"),
                address_record(2, "20", "Queen Street"),
            ],
        );
        assert_eq!(manifest.record_count, 2);
        assert_eq!(manifest.text_document_count, 2);
        assert_eq!(manifest.spatial.point_count, 2);

        let reader = Reader::open(&root).expect("reader");
        assert_eq!(
            reader.path().file_name().and_then(|name| name.to_str()),
            Some(PACK_FILE)
        );
        assert_eq!(reader.record_summary(1).expect("row 1").id, "osm:node:2");
        assert_eq!(
            reader.find_by_source_id("osm:node:1").expect("lookup"),
            Some(0)
        );
        assert_eq!(
            reader
                .records_json_by_layer("address", 10)
                .expect("layer")
                .len(),
            2
        );
        assert!(
            reader
                .records_json_by_layer("street", 10)
                .expect("layer")
                .is_empty()
        );
        assert!(reader.records_json_by_layer("nope", 10).is_err());
        let sections = reader.section_sizes();
        assert!(sections.contains_key("records/blocks"));
        assert!(sections.keys().any(|name| name.starts_with("text/")));
        assert!(sections.contains_key("spatial/blocks"));
        assert_eq!(first_hit(&root, "queen").as_deref(), Some("osm:node:2"));

        // The Pack file alone is a complete Pack.
        let copy = temp_root("copy").with_extension("ogp");
        fs::copy(reader.path(), &copy).expect("copy");
        assert_eq!(first_hit(&copy, "king").as_deref(), Some("osm:node:1"));
        assert!(!reader.path().parent().unwrap().join(SCRATCH_DIR).exists());
    }

    #[test]
    fn failed_osm_rebuild_preserves_published_pack() {
        let root = temp_root("rebuild");
        write_pack(&root, &[address_record(1, "10", "King Street")]);
        let generations = fs::read_dir(root.join("generations")).expect("dir").count();

        let result = crate::builder::build_osm_pack(crate::builder::BuildOsmOptions {
            inputs: vec![root.join("missing.osm.pbf")],
            pack: root.clone(),
            ..crate::builder::BuildOsmOptions::default()
        });
        assert!(result.is_err());
        assert_eq!(first_hit(&root, "king").as_deref(), Some("osm:node:1"));
        assert_eq!(
            fs::read_dir(root.join("generations")).expect("dir").count(),
            generations,
            "the failed generation is removed"
        );
    }

    #[test]
    fn publishes_replacement_without_changing_existing_readers() {
        let root = temp_root("publish");
        write_pack(&root, &[address_record(1, "10", "King Street")]);
        let original = Reader::open(&root).expect("original reader");
        let searcher = TextSearcher::open(&root).expect("original searcher");

        let mut replacement = Writer::create(&root).expect("replacement");
        replacement
            .write(&address_record(2, "20", "Queen Street"), None)
            .expect("address");
        assert_eq!(first_hit(&root, "king").as_deref(), Some("osm:node:1"));
        replacement.finish().expect("publish");

        assert_eq!(first_hit(&root, "queen").as_deref(), Some("osm:node:2"));
        assert_eq!(
            original.record_summary(0).expect("original").id,
            "osm:node:1"
        );
        let hits = searcher
            .search(TextSearchOptions {
                query: "king".into(),
                limit: 5,
                layer: None,
                countries: Vec::new(),
                focus: None,
                bbox: None,
            })
            .expect("existing searcher after publish");
        assert_eq!(hits[0].record.id, "osm:node:1");
    }

    #[test]
    fn validation_failure_does_not_publish_replacement() {
        let root = temp_root("validation");
        write_pack(&root, &[address_record(1, "10", "King Street")]);
        let published = fs::read(root.join("CURRENT")).expect("pointer");

        let mut replacement = Writer::create(&root).expect("replacement");
        replacement
            .write(&address_record(2, "20", "Queen Street"), None)
            .expect("address");
        let (sealed, _) = replacement.seal().expect("seal");
        let generation = sealed.generation().to_path_buf();
        // Damage the table of contents of the unpublished Pack.
        let pack = generation.join(PACK_FILE);
        let mut bytes = fs::read(&pack).expect("read");
        let len = bytes.len();
        bytes[len - 1] ^= 0xff;
        fs::File::create(&pack)
            .and_then(|mut file| file.write_all(&bytes))
            .expect("corrupt");
        assert!(sealed.publish().is_err());
        assert_eq!(fs::read(root.join("CURRENT")).expect("pointer"), published);
        assert!(!generation.exists());
        assert_eq!(first_hit(&root, "king").as_deref(), Some("osm:node:1"));
    }

    #[test]
    fn publishing_refuses_a_pack_corrupted_inside_a_section() {
        let root = temp_root("checksum");
        write_pack(&root, &[address_record(1, "10", "King Street")]);
        let published = fs::read(root.join("CURRENT")).expect("pointer");

        let mut replacement = Writer::create(&root).expect("replacement");
        replacement
            .write(&address_record(2, "20", "Queen Street"), None)
            .expect("address");
        let (sealed, _) = replacement.seal().expect("seal");
        let pack = sealed.generation().join(PACK_FILE);
        let offset = Container::open(&pack).expect("open").sections()["records/strings"].offset;
        let mut bytes = fs::read(&pack).expect("read");
        let last = bytes.len() - 1;
        bytes[(offset as usize + 20).min(last)] ^= 0x55;
        fs::write(&pack, &bytes).expect("corrupt");

        let error = sealed.publish().unwrap_err();
        assert!(format!("{error:#}").contains("checksum"), "{error:#}");
        assert_eq!(fs::read(root.join("CURRENT")).expect("pointer"), published);
    }

    #[test]
    fn a_failure_after_the_pointer_switch_keeps_the_published_generation() {
        let root = temp_root("switch-failure");
        write_pack(&root, &[address_record(1, "10", "King Street")]);
        let mut replacement = Writer::create(&root).expect("replacement");
        replacement
            .write(&address_record(2, "20", "Queen Street"), None)
            .expect("address");
        let (sealed, _) = replacement.seal().expect("seal");

        publication::FAIL_AFTER_SWITCH.set(true);
        let result = sealed.publish();
        publication::FAIL_AFTER_SWITCH.set(false);
        assert!(result.is_err());
        // CURRENT already names the new generation, so it must still exist.
        assert_eq!(first_hit(&root, "queen").as_deref(), Some("osm:node:2"));
    }

    #[test]
    fn text_index_memory_fits_the_budget_and_waits_for_records() {
        let root = temp_root("text-memory");
        let memory = Budget::new(64 << 20);
        let mut writer = Writer::create_with(
            &root,
            WriterOptions {
                memory: Arc::clone(&memory),
                text_index_memory_bytes: 16 << 20,
            },
        )
        .expect("writer");
        assert_eq!(
            memory.used(),
            0,
            "nothing is reserved before records arrive"
        );
        writer
            .write(&address_record(1, "10", "King Street"), None)
            .expect("address");
        assert!(memory.used() > 0);
        assert!(
            memory.peak() <= memory.limit(),
            "peak {} of {}",
            memory.peak(),
            memory.limit()
        );
        writer.finish().expect("finish");
        assert_eq!(first_hit(&root, "king").as_deref(), Some("osm:node:1"));
    }

    #[test]
    fn rejects_paths_without_a_pack() {
        let root = temp_root("empty");
        fs::create_dir_all(&root).expect("dir");
        assert!(Reader::open(&root).is_err());
    }
}
