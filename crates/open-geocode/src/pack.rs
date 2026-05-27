use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{
    builder::{
        progress::stage_progress,
        report::{BuilderReport, PackWriteTimings},
    },
    record::{NormalizedRecord, RejectedRecord},
    spatial_index::{PackSpatialIndexWriter, SpatialIndexCommit},
    text_index::{
        TEXT_INDEX_RELATIVE_PATH, TantivyTextIndexWriter, TextIndexCommit, TextIndexWriteMetrics,
    },
};

pub type RecordId = u64;

pub trait RecordWriter {
    fn write_record(&mut self, record: NormalizedRecord) -> Result<RecordId>;
    fn write_rejection(&mut self, rejection: RejectedRecord) -> Result<()>;
}

#[derive(Debug)]
pub struct PackWriter {
    path: PathBuf,
    records: File,
    offsets: File,
    rejections: File,
    rejection_offsets: File,
    text_index: TantivyTextIndexWriter,
    spatial_index: PackSpatialIndexWriter,
    write_timings: PackWriteTimings,
    text_index_timing_nanos: TextIndexTimingNanos,
    record_count: u64,
    rejection_count: u64,
    layer_counts: BTreeMap<String, u64>,
}

#[derive(Debug)]
pub struct PackReader {
    path: PathBuf,
    manifest: PackManifest,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PackManifest {
    pub schema_version: u32,
    pub crate_version: String,
    pub built_at_unix: u64,
    pub record_count: u64,
    pub rejection_count: u64,
    pub layer_counts: BTreeMap<String, u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_index: Option<PackTextIndex>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spatial_index: Option<PackSpatialIndex>,
    pub files: BTreeMap<String, PackFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PackTextIndex {
    pub path: String,
    pub schema_version: u32,
    pub document_count: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PackSpatialIndex {
    pub path: String,
    pub schema_version: u32,
    pub point_count: u64,
    pub segment_count: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PackFile {
    pub path: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OffsetEntry {
    offset: u64,
    length: u64,
    layer_code: u16,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct TextIndexTimingNanos {
    text_projection_ns: u128,
    text_prefix_generation_ns: u128,
    tantivy_document_build_ns: u128,
    tantivy_add_document_ns: u128,
}

impl TextIndexTimingNanos {
    fn record(&mut self, metrics: TextIndexWriteMetrics) {
        self.text_projection_ns += metrics.text_projection_ns;
        self.text_prefix_generation_ns += metrics.text_prefix_generation_ns;
        self.tantivy_document_build_ns += metrics.tantivy_document_build_ns;
        self.tantivy_add_document_ns += metrics.tantivy_add_document_ns;
    }

    fn apply_to(self, timings: &mut PackWriteTimings) {
        timings.text_projection_ms = nanos_to_millis(self.text_projection_ns);
        timings.text_prefix_generation_ms = nanos_to_millis(self.text_prefix_generation_ns);
        timings.tantivy_document_build_ms = nanos_to_millis(self.tantivy_document_build_ns);
        timings.tantivy_add_document_ms = nanos_to_millis(self.tantivy_add_document_ns);
        timings.text_index_write_ms = nanos_to_millis(
            self.text_projection_ns
                + self.text_prefix_generation_ns
                + self.tantivy_document_build_ns
                + self.tantivy_add_document_ns,
        );
    }
}

const PACK_SCHEMA_VERSION: u32 = 1;
const RECORDS_MAGIC: &[u8; 8] = b"OGREC001";
const OFFSETS_MAGIC: &[u8; 8] = b"OGOFF001";
const REJECTIONS_MAGIC: &[u8; 8] = b"OGREJ001";
const REJECTION_OFFSETS_MAGIC: &[u8; 8] = b"OGROF001";
const OFFSET_HEADER_BYTES: u64 = 16;
const OFFSET_ENTRY_BYTES: u64 = 24;

impl PackWriter {
    pub fn create(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if path.exists() {
            if path.is_dir() {
                fs::remove_dir_all(&path)
                    .with_context(|| format!("failed to clear pack {}", path.display()))?;
            } else {
                fs::remove_file(&path)
                    .with_context(|| format!("failed to remove {}", path.display()))?;
            }
        }

        fs::create_dir_all(path.join("records"))
            .with_context(|| format!("failed to create {}", path.join("records").display()))?;
        fs::create_dir_all(path.join("audit"))
            .with_context(|| format!("failed to create {}", path.join("audit").display()))?;

        let mut records = File::create(path.join("records").join("records.bin"))
            .with_context(|| "failed to create records.bin")?;
        records.write_all(RECORDS_MAGIC)?;

        let mut offsets = File::create(path.join("records").join("offsets.bin"))
            .with_context(|| "failed to create offsets.bin")?;
        write_offset_header(&mut offsets, OFFSETS_MAGIC, 0)?;

        let mut rejections = File::create(path.join("audit").join("rejections.bin"))
            .with_context(|| "failed to create rejections.bin")?;
        rejections.write_all(REJECTIONS_MAGIC)?;

        let mut rejection_offsets = File::create(path.join("audit").join("rejection_offsets.bin"))
            .with_context(|| "failed to create rejection_offsets.bin")?;
        write_offset_header(&mut rejection_offsets, REJECTION_OFFSETS_MAGIC, 0)?;

        let text_index = TantivyTextIndexWriter::create(&path)?;

        Ok(Self {
            path,
            records,
            offsets,
            rejections,
            rejection_offsets,
            text_index,
            spatial_index: PackSpatialIndexWriter::default(),
            write_timings: PackWriteTimings::default(),
            text_index_timing_nanos: TextIndexTimingNanos::default(),
            record_count: 0,
            rejection_count: 0,
            layer_counts: BTreeMap::new(),
        })
    }

    pub fn finish(mut self, report: &mut BuilderReport) -> Result<PackManifest> {
        let runtime_finalize_started = Instant::now();

        let started = Instant::now();
        write_offset_header(&mut self.offsets, OFFSETS_MAGIC, self.record_count)?;
        write_offset_header(
            &mut self.rejection_offsets,
            REJECTION_OFFSETS_MAGIC,
            self.rejection_count,
        )?;
        self.write_timings.final_offset_header_ms += elapsed_ms(started);

        let started = Instant::now();
        self.records.flush()?;
        self.offsets.flush()?;
        self.rejections.flush()?;
        self.rejection_offsets.flush()?;
        self.write_timings.table_flush_ms += elapsed_ms(started);

        let text_progress = stage_progress("6/7 finalize text index");
        let text_index_flush_metrics = self.text_index.flush()?;
        self.text_index_timing_nanos
            .record(text_index_flush_metrics);
        self.text_index_timing_nanos
            .apply_to(&mut self.write_timings);

        let started = Instant::now();
        let text_index_commit = self.text_index.commit()?;
        self.write_timings.text_index_commit_ms += elapsed_ms(started);

        let started = Instant::now();
        let text_index_bytes = dir_size(self.path.join(TEXT_INDEX_RELATIVE_PATH))?;
        self.write_timings.text_index_size_ms += elapsed_ms(started);
        text_progress.finish_with_message("6/7 finalize text index complete");

        let spatial_index = std::mem::take(&mut self.spatial_index);
        let spatial_progress = stage_progress("7/7 finalize spatial index");
        let started = Instant::now();
        let spatial_index_commit = spatial_index.finish(&self.path)?;
        self.write_timings.spatial_index_finish_ms += elapsed_ms(started);
        self.write_timings.spatial_point_pair_generation_ms =
            spatial_index_commit.build_timings.point_pair_generation_ms;
        self.write_timings.spatial_segment_pair_generation_ms = spatial_index_commit
            .build_timings
            .segment_pair_generation_ms;
        self.write_timings.spatial_pair_sort_dedupe_ms =
            spatial_index_commit.build_timings.pair_sort_dedupe_ms;
        self.write_timings.spatial_cell_directory_build_ms =
            spatial_index_commit.build_timings.cell_directory_build_ms;
        self.write_timings.spatial_file_write_ms = spatial_index_commit.build_timings.file_write_ms;

        let started = Instant::now();
        let spatial_index_bytes = dir_size(self.path.join(&spatial_index_commit.relative_path))?;
        self.write_timings.spatial_index_size_ms += elapsed_ms(started);
        spatial_progress.finish_with_message("7/7 finalize spatial index complete");

        let started = Instant::now();
        report.record_table_bytes = file_len(self.path.join("records").join("records.bin"))?;
        report.offset_table_bytes = file_len(self.path.join("records").join("offsets.bin"))?;
        report.rejection_table_bytes = file_len(self.path.join("audit").join("rejections.bin"))?;
        report.rejection_offset_table_bytes =
            file_len(self.path.join("audit").join("rejection_offsets.bin"))?;
        self.write_timings.table_size_ms += elapsed_ms(started);

        report.text_index_path = TEXT_INDEX_RELATIVE_PATH.to_string();
        report.text_index_schema_version = text_index_commit.schema_version;
        report.text_index_document_count = text_index_commit.document_count;
        report.text_index_bytes = text_index_bytes;
        report.text_index_prefix = self.text_index.prefix_stats();
        report.spatial_index_path = spatial_index_commit.relative_path.clone();
        report.spatial_index_schema_version = spatial_index_commit.schema_version;
        report.spatial_index_point_count = spatial_index_commit.point_count;
        report.spatial_index_segment_count = spatial_index_commit.segment_count;
        report.spatial_index_bytes = spatial_index_bytes;
        self.write_timings.runtime_finalize_ms += elapsed_ms(runtime_finalize_started);
        report.phases.pack_finish_ms = self.write_timings.runtime_finalize_ms;
        report.phases.total_ms += self.write_timings.runtime_finalize_ms;
        report.pack_write = self.write_timings.clone();

        let report_path = self.path.join("audit").join("build-report.json");
        let report_file = File::create(&report_path)
            .with_context(|| format!("failed to create {}", report_path.display()))?;
        serde_json::to_writer_pretty(report_file, report)
            .with_context(|| format!("failed to write {}", report_path.display()))?;

        let manifest = self.manifest(
            text_index_commit,
            text_index_bytes,
            spatial_index_commit,
            spatial_index_bytes,
        )?;
        let manifest_path = self.path.join("manifest.json");
        let manifest_file = File::create(&manifest_path)
            .with_context(|| format!("failed to create {}", manifest_path.display()))?;
        serde_json::to_writer_pretty(manifest_file, &manifest)
            .with_context(|| format!("failed to write {}", manifest_path.display()))?;

        Ok(manifest)
    }

    fn manifest(
        &self,
        text_index_commit: TextIndexCommit,
        text_index_bytes: u64,
        spatial_index_commit: SpatialIndexCommit,
        spatial_index_bytes: u64,
    ) -> Result<PackManifest> {
        let mut files = BTreeMap::new();
        for relative in [
            "records/records.bin",
            "records/offsets.bin",
            "audit/rejections.bin",
            "audit/rejection_offsets.bin",
            "audit/build-report.json",
        ] {
            insert_pack_file(&mut files, &self.path, relative)?;
        }
        insert_pack_files_under(&mut files, &self.path, TEXT_INDEX_RELATIVE_PATH)?;
        insert_pack_files_under(&mut files, &self.path, &spatial_index_commit.relative_path)?;

        Ok(PackManifest {
            schema_version: PACK_SCHEMA_VERSION,
            crate_version: env!("CARGO_PKG_VERSION").to_string(),
            built_at_unix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_secs())
                .unwrap_or_default(),
            record_count: self.record_count,
            rejection_count: self.rejection_count,
            layer_counts: self.layer_counts.clone(),
            text_index: Some(PackTextIndex {
                path: TEXT_INDEX_RELATIVE_PATH.to_string(),
                schema_version: text_index_commit.schema_version,
                document_count: text_index_commit.document_count,
                bytes: text_index_bytes,
            }),
            spatial_index: Some(PackSpatialIndex {
                path: spatial_index_commit.relative_path.clone(),
                schema_version: spatial_index_commit.schema_version,
                point_count: spatial_index_commit.point_count,
                segment_count: spatial_index_commit.segment_count,
                bytes: spatial_index_bytes,
            }),
            files,
        })
    }

    fn append_chunk(
        data: &mut File,
        offsets: &mut File,
        row: u64,
        layer_code: u16,
        bytes: &[u8],
    ) -> Result<()> {
        let offset = data.seek(SeekFrom::End(0))?;
        data.write_all(bytes)?;
        write_offset_entry(
            offsets,
            row,
            OffsetEntry {
                offset,
                length: bytes.len() as u64,
                layer_code,
            },
        )
    }
}

impl RecordWriter for PackWriter {
    fn write_record(&mut self, record: NormalizedRecord) -> Result<RecordId> {
        let record_id = self.record_count;
        let layer = record.layer();
        let layer_code = layer_code(layer)?;

        let started = Instant::now();
        let bytes = rmp_serde::to_vec_named(&record).context("failed to encode record")?;
        self.write_timings.record_encode_ms += elapsed_ms(started);

        let started = Instant::now();
        Self::append_chunk(
            &mut self.records,
            &mut self.offsets,
            record_id,
            layer_code,
            &bytes,
        )?;
        self.write_timings.record_table_write_ms += elapsed_ms(started);

        let text_index_metrics = self.text_index.add_record(record_id, &record)?;
        self.text_index_timing_nanos.record(text_index_metrics);

        let started = Instant::now();
        self.spatial_index.add_record(record_id, &record)?;
        self.write_timings.spatial_index_write_ms += elapsed_ms(started);

        self.record_count += 1;
        *self.layer_counts.entry(layer.to_string()).or_default() += 1;
        Ok(record_id)
    }

    fn write_rejection(&mut self, rejection: RejectedRecord) -> Result<()> {
        let row = self.rejection_count;
        let layer_code = rejection
            .layer_hint
            .as_deref()
            .map(layer_code)
            .transpose()?
            .unwrap_or(0);
        let started = Instant::now();
        let bytes = rmp_serde::to_vec_named(&rejection).context("failed to encode rejection")?;
        self.write_timings.rejection_encode_ms += elapsed_ms(started);

        let started = Instant::now();
        Self::append_chunk(
            &mut self.rejections,
            &mut self.rejection_offsets,
            row,
            layer_code,
            &bytes,
        )?;
        self.write_timings.rejection_table_write_ms += elapsed_ms(started);

        self.rejection_count += 1;
        Ok(())
    }
}

impl PackReader {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let manifest_path = path.join("manifest.json");
        let manifest_file = File::open(&manifest_path)
            .with_context(|| format!("failed to open {}", manifest_path.display()))?;
        let manifest = serde_json::from_reader(manifest_file)
            .with_context(|| format!("failed to parse {}", manifest_path.display()))?;
        Ok(Self { path, manifest })
    }

    pub const fn manifest(&self) -> &PackManifest {
        &self.manifest
    }

    pub fn read_record(&self, record_id: RecordId) -> Result<NormalizedRecord> {
        let entry = self.record_offset(record_id)?;
        let bytes = self.read_chunk("records/records.bin", RECORDS_MAGIC, entry)?;
        rmp_serde::from_slice(&bytes).context("failed to decode record")
    }

    pub fn read_rejection(&self, row: u64) -> Result<RejectedRecord> {
        let entry = self.rejection_offset(row)?;
        let bytes = self.read_chunk("audit/rejections.bin", REJECTIONS_MAGIC, entry)?;
        rmp_serde::from_slice(&bytes).context("failed to decode rejection")
    }

    pub fn records_by_layer(&self, layer: &str, limit: usize) -> Result<Vec<NormalizedRecord>> {
        let wanted = layer_code(layer)?;
        let count = offset_count(self.path.join("records").join("offsets.bin"), OFFSETS_MAGIC)?;
        let mut records = Vec::new();
        for record_id in 0..count {
            let entry = self.record_offset(record_id)?;
            if entry.layer_code != wanted {
                continue;
            }
            records.push(self.read_record(record_id)?);
            if limit > 0 && records.len() >= limit {
                break;
            }
        }
        Ok(records)
    }

    pub fn rejections(&self, limit: usize) -> Result<Vec<RejectedRecord>> {
        let count = offset_count(
            self.path.join("audit").join("rejection_offsets.bin"),
            REJECTION_OFFSETS_MAGIC,
        )?;
        let count = if limit == 0 {
            count
        } else {
            count.min(limit as u64)
        };
        let mut rejections = Vec::new();
        for row in 0..count {
            rejections.push(self.read_rejection(row)?);
        }
        Ok(rejections)
    }

    pub fn record_by_source_id(&self, source_id: &str) -> Result<Option<NormalizedRecord>> {
        for record_id in 0..self.manifest.record_count {
            let record = self.read_record(record_id)?;
            if record.id() == source_id {
                return Ok(Some(record));
            }
        }
        Ok(None)
    }

    fn record_offset(&self, record_id: RecordId) -> Result<OffsetEntry> {
        read_offset_entry(
            self.path.join("records").join("offsets.bin"),
            OFFSETS_MAGIC,
            record_id,
        )
    }

    fn rejection_offset(&self, row: u64) -> Result<OffsetEntry> {
        read_offset_entry(
            self.path.join("audit").join("rejection_offsets.bin"),
            REJECTION_OFFSETS_MAGIC,
            row,
        )
    }

    fn read_chunk(
        &self,
        relative: &str,
        expected_magic: &[u8; 8],
        entry: OffsetEntry,
    ) -> Result<Vec<u8>> {
        let path = self.path.join(relative);
        let mut file =
            File::open(&path).with_context(|| format!("failed to open {}", path.display()))?;
        let mut magic = [0; 8];
        file.read_exact(&mut magic)?;
        if &magic != expected_magic {
            bail!("{} has an invalid magic header", path.display());
        }

        file.seek(SeekFrom::Start(entry.offset))?;
        let mut bytes = vec![0; entry.length as usize];
        file.read_exact(&mut bytes)?;
        Ok(bytes)
    }
}

fn write_offset_header(file: &mut File, magic: &[u8; 8], count: u64) -> Result<()> {
    file.seek(SeekFrom::Start(0))?;
    file.write_all(magic)?;
    file.write_all(&count.to_le_bytes())?;
    file.seek(SeekFrom::End(0))?;
    Ok(())
}

fn write_offset_entry(file: &mut File, row: u64, entry: OffsetEntry) -> Result<()> {
    file.seek(SeekFrom::Start(
        OFFSET_HEADER_BYTES + row * OFFSET_ENTRY_BYTES,
    ))?;
    file.write_all(&entry.offset.to_le_bytes())?;
    file.write_all(&entry.length.to_le_bytes())?;
    file.write_all(&entry.layer_code.to_le_bytes())?;
    file.write_all(&[0; 6])?;
    Ok(())
}

fn read_offset_entry(
    path: impl AsRef<Path>,
    expected_magic: &[u8; 8],
    row: u64,
) -> Result<OffsetEntry> {
    let path = path.as_ref();
    let mut file =
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let count = read_offset_header(&mut file, path, expected_magic)?;
    if row >= count {
        bail!("record row {row} is out of range; table has {count} rows");
    }

    file.seek(SeekFrom::Start(
        OFFSET_HEADER_BYTES + row * OFFSET_ENTRY_BYTES,
    ))?;
    let offset = read_u64(&mut file)?;
    let length = read_u64(&mut file)?;
    let layer_code = read_u16(&mut file)?;
    let mut reserved = [0; 6];
    file.read_exact(&mut reserved)?;
    Ok(OffsetEntry {
        offset,
        length,
        layer_code,
    })
}

fn offset_count(path: impl AsRef<Path>, expected_magic: &[u8; 8]) -> Result<u64> {
    let path = path.as_ref();
    let mut file =
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    read_offset_header(&mut file, path, expected_magic)
}

fn read_offset_header(file: &mut File, path: &Path, expected_magic: &[u8; 8]) -> Result<u64> {
    let mut magic = [0; 8];
    file.read_exact(&mut magic)?;
    if &magic != expected_magic {
        bail!("{} has an invalid magic header", path.display());
    }
    read_u64(file)
}

fn read_u64(file: &mut File) -> Result<u64> {
    let mut bytes = [0; 8];
    file.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

fn read_u16(file: &mut File) -> Result<u16> {
    let mut bytes = [0; 2];
    file.read_exact(&mut bytes)?;
    Ok(u16::from_le_bytes(bytes))
}

fn elapsed_ms(started: Instant) -> u128 {
    started.elapsed().as_millis()
}

fn nanos_to_millis(nanos: u128) -> u128 {
    nanos / 1_000_000
}

fn file_len(path: impl AsRef<Path>) -> Result<u64> {
    let path = path.as_ref();
    Ok(fs::metadata(path)
        .with_context(|| format!("failed to stat {}", path.display()))?
        .len())
}

fn dir_size(path: impl AsRef<Path>) -> Result<u64> {
    let path = path.as_ref();
    let mut bytes = 0;
    for entry in fs::read_dir(path).with_context(|| format!("failed to read {}", path.display()))? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            bytes += dir_size(entry.path())?;
        } else if metadata.is_file() {
            bytes += metadata.len();
        }
    }
    Ok(bytes)
}

fn insert_pack_file(
    files: &mut BTreeMap<String, PackFile>,
    pack_path: &Path,
    relative: &str,
) -> Result<()> {
    let bytes = file_len(pack_path.join(relative))?;
    files.insert(
        relative.to_string(),
        PackFile {
            path: relative.to_string(),
            bytes,
        },
    );
    Ok(())
}

fn insert_pack_files_under(
    files: &mut BTreeMap<String, PackFile>,
    pack_path: &Path,
    relative_dir: &str,
) -> Result<()> {
    let root = pack_path.join(relative_dir);
    for entry in
        fs::read_dir(&root).with_context(|| format!("failed to read {}", root.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            let relative = pack_relative_path(pack_path, &path)?;
            insert_pack_files_under(files, pack_path, &relative)?;
        } else if metadata.is_file() {
            let relative = pack_relative_path(pack_path, &path)?;
            insert_pack_file(files, pack_path, &relative)?;
        }
    }
    Ok(())
}

fn pack_relative_path(pack_path: &Path, path: &Path) -> Result<String> {
    let relative = path
        .strip_prefix(pack_path)
        .with_context(|| format!("{} is not inside {}", path.display(), pack_path.display()))?;
    Ok(relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/"))
}

fn layer_code(layer: &str) -> Result<u16> {
    match layer {
        "address" => Ok(1),
        "country" => Ok(2),
        "district" => Ok(3),
        "interpolation" => Ok(4),
        "locality" => Ok(5),
        "neighbourhood" => Ok(6),
        "place" => Ok(7),
        "postcode" => Ok(8),
        "region" => Ok(9),
        "street" => Ok(10),
        other => bail!("unknown record layer: {other}"),
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    #[derive(Default)]
    pub(crate) struct MemoryRecordWriter {
        pub(crate) records: Vec<NormalizedRecord>,
        pub(crate) rejections: Vec<RejectedRecord>,
    }

    impl RecordWriter for MemoryRecordWriter {
        fn write_record(&mut self, record: NormalizedRecord) -> Result<RecordId> {
            let record_id = self.records.len() as u64;
            self.records.push(record);
            Ok(record_id)
        }

        fn write_rejection(&mut self, rejection: RejectedRecord) -> Result<()> {
            self.rejections.push(rejection);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, path::Path};

    use tantivy::{collector::TopDocs, query::QueryParser};

    use crate::record::{
        AddressComponents, AddressRecord, LocationPrecision, OsmObjectType, SourceProvenance,
        point_geometry,
    };
    use crate::text_index::{TextIndexFields, open_text_index};

    use super::*;

    #[test]
    fn writes_and_reads_binary_records_by_row_layer_and_source_id() {
        let temp_dir =
            std::env::temp_dir().join(format!("open-geocode-pack-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp_dir);

        let mut writer = PackWriter::create(&temp_dir).expect("writer");
        writer
            .write_record(NormalizedRecord::address(address_record(
                "osm:node:1",
                "10 King Street",
            )))
            .expect("write first");
        writer
            .write_record(NormalizedRecord::address(address_record(
                "osm:node:2",
                "20 Queen Street",
            )))
            .expect("write second");
        let mut report = BuilderReport::default();
        writer.finish(&mut report).expect("finish");

        let reader = PackReader::open(&temp_dir).expect("reader");
        assert_eq!(reader.manifest().record_count, 2);
        assert_eq!(reader.read_record(1).expect("row 1").id(), "osm:node:2");
        assert_eq!(
            reader
                .record_by_source_id("osm:node:1")
                .expect("lookup")
                .expect("record")
                .label(),
            "10 King Street"
        );
        assert_eq!(
            reader
                .records_by_layer("address", 10)
                .expect("layer records")
                .len(),
            2
        );
        assert!(report.record_table_bytes > 8);
        assert!(report.offset_table_bytes > 16);
        assert!(report.text_index_bytes > 0);
        assert_eq!(report.text_index_document_count, 2);
        assert!(report.text_index_prefix.autocomplete_prefix_terms_total > 0);
        assert!(
            report
                .text_index_prefix
                .autocomplete_prefix_terms_by_layer
                .contains_key("address")
        );
        assert!(report.spatial_index_bytes > 0);
        assert_eq!(report.spatial_index_point_count, 2);
        assert_eq!(
            report.phases.pack_finish_ms,
            report.pack_write.runtime_finalize_ms
        );
        assert_eq!(
            reader
                .manifest()
                .text_index
                .as_ref()
                .expect("text index")
                .document_count,
            2
        );
        assert_eq!(
            reader
                .manifest()
                .spatial_index
                .as_ref()
                .expect("spatial index")
                .point_count,
            2
        );
        assert!(
            reader
                .manifest()
                .files
                .keys()
                .any(|path| path.starts_with("text/tantivy/"))
        );
        assert!(
            reader
                .manifest()
                .files
                .contains_key("spatial/v2/manifest.json")
        );
        assert_eq!(text_hit_record_id(&temp_dir, "queen").expect("text hit"), 1);

        let _ = fs::remove_dir_all(temp_dir);
    }

    fn text_hit_record_id(pack_path: &Path, query: &str) -> Result<RecordId> {
        let index = open_text_index(pack_path)?;
        let schema = index.schema();
        let fields = TextIndexFields::from_schema(&schema)?;
        let reader = index.reader()?;
        let searcher = reader.searcher();
        let query_parser = QueryParser::for_index(&index, vec![fields.content_text]);
        let parsed_query = query_parser.parse_query(query)?;
        let top_docs = searcher.search(&parsed_query, &TopDocs::with_limit(1))?;
        let Some((_score, doc_address)) = top_docs.first() else {
            bail!("no text hit for {query}");
        };
        let record_id_reader = searcher
            .segment_reader(doc_address.segment_ord)
            .fast_fields()
            .u64("record_id")?;
        record_id_reader
            .values_for_doc(doc_address.doc_id)
            .next()
            .with_context(|| format!("missing record_id for text hit {query}"))
    }

    fn address_record(id: &str, label: &str) -> AddressRecord {
        AddressRecord {
            id: id.to_string(),
            label: label.to_string(),
            name: label.to_string(),
            address: AddressComponents {
                number: "10".to_string(),
                street: Some("King Street".to_string()),
                place: None,
                unit: None,
                locality: None,
                region: None,
                postcode: None,
                country: None,
            },
            geometry: point_geometry(-79.0, 43.0),
            location_precision: LocationPrecision::Point,
            source: SourceProvenance {
                dataset: "osm".to_string(),
                object_type: OsmObjectType::Node,
                object_id: 1,
                tags: Some(BTreeMap::new()),
            },
        }
    }
}
