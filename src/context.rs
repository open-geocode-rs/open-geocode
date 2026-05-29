use std::{
    collections::HashMap,
    fs::{self, File},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use memmap2::{Mmap, MmapOptions};
use serde::{Deserialize, Serialize};

use crate::{pack::RecordId, util::fs::dir_size};

pub const CONTEXT_RELATIVE_DIR: &str = "context/v1";
pub const CONTEXT_SCHEMA_VERSION: u32 = 1;
pub const CONTEXT_FLAG_AMBIGUOUS_ADMIN: u16 = 1;

const CONTEXT_MANIFEST_FILE: &str = "manifest.json";
const ADMIN_TUPLES_FILE: &str = "admin_tuples.bin";
const RECORD_CONTEXTS_FILE: &str = "record_contexts.bin";
const ADMIN_TUPLES_MAGIC: &[u8; 8] = b"OGCTXTP1";
const RECORD_CONTEXTS_MAGIC: &[u8; 8] = b"OGCTXRC1";
const COUNTED_HEADER_BYTES: usize = 16;
const ADMIN_TUPLE_ENTRY_BYTES: usize = 48;
const RECORD_CONTEXT_ENTRY_BYTES: usize = 32;
const MISSING_ID: u64 = u64::MAX;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AdminContextTuple {
    pub country_record_id: Option<RecordId>,
    pub region_record_id: Option<RecordId>,
    pub district_record_id: Option<RecordId>,
    pub locality_record_id: Option<RecordId>,
    pub neighbourhood_record_id: Option<RecordId>,
    pub place_record_id: Option<RecordId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextAssignmentMethod {
    BoundaryDerived,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordContextEntry {
    pub record_id: RecordId,
    pub admin_context_tuple_id: Option<u64>,
    pub postcode_record_id: Option<RecordId>,
    pub assignment_method: ContextAssignmentMethod,
    pub flags: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRecordContext {
    pub record_id: RecordId,
    pub admin_context: Option<AdminContextTuple>,
    pub postcode_record_id: Option<RecordId>,
    pub assignment_method: ContextAssignmentMethod,
    pub flags: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextCommit {
    pub schema_version: u32,
    pub relative_path: String,
    pub admin_tuple_count: u64,
    pub record_context_count: u64,
    pub bytes: u64,
}

#[derive(Debug, Default)]
pub struct PackContextWriter {
    admin_tuples: Vec<AdminContextTuple>,
    admin_tuple_ids: HashMap<AdminContextTuple, u64>,
    record_contexts: Vec<RecordContextEntry>,
}

#[derive(Debug)]
pub struct PackContextReader {
    manifest: ContextManifest,
    admin_tuples: CountedMmap,
    record_contexts: CountedMmap,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ContextManifest {
    schema_version: u32,
    admin_tuple_count: u64,
    record_context_count: u64,
}

#[derive(Debug)]
struct CountedMmap {
    bytes: Mmap,
    count: u64,
    entry_bytes: usize,
}

impl AdminContextTuple {
    pub fn is_empty(self) -> bool {
        self.country_record_id.is_none()
            && self.region_record_id.is_none()
            && self.district_record_id.is_none()
            && self.locality_record_id.is_none()
            && self.neighbourhood_record_id.is_none()
            && self.place_record_id.is_none()
    }

    pub fn parent_record_ids(self) -> impl Iterator<Item = RecordId> {
        [
            self.place_record_id,
            self.neighbourhood_record_id,
            self.locality_record_id,
            self.district_record_id,
            self.region_record_id,
            self.country_record_id,
        ]
        .into_iter()
        .flatten()
    }
}

impl PackContextWriter {
    pub fn add_record_context(
        &mut self,
        record_id: RecordId,
        admin_context: AdminContextTuple,
        postcode_record_id: Option<RecordId>,
        flags: u16,
    ) {
        if admin_context.is_empty() && postcode_record_id.is_none() {
            return;
        }
        let admin_context_tuple_id = (!admin_context.is_empty()).then(|| {
            if let Some(tuple_id) = self.admin_tuple_ids.get(&admin_context) {
                *tuple_id
            } else {
                let tuple_id = self.admin_tuples.len() as u64;
                self.admin_tuples.push(admin_context);
                self.admin_tuple_ids.insert(admin_context, tuple_id);
                tuple_id
            }
        });

        self.record_contexts.push(RecordContextEntry {
            record_id,
            admin_context_tuple_id,
            postcode_record_id,
            assignment_method: ContextAssignmentMethod::BoundaryDerived,
            flags,
        });
    }

    pub fn finish(mut self, pack_path: &Path) -> Result<ContextCommit> {
        let root = pack_path.join(CONTEXT_RELATIVE_DIR);
        fs::create_dir_all(&root)
            .with_context(|| format!("failed to create {}", root.display()))?;

        self.record_contexts
            .sort_by_key(|context| context.record_id);
        self.record_contexts
            .dedup_by_key(|context| context.record_id);

        write_admin_tuples(&root.join(ADMIN_TUPLES_FILE), &self.admin_tuples)?;
        write_record_contexts(&root.join(RECORD_CONTEXTS_FILE), &self.record_contexts)?;

        let manifest = ContextManifest {
            schema_version: CONTEXT_SCHEMA_VERSION,
            admin_tuple_count: self.admin_tuples.len() as u64,
            record_context_count: self.record_contexts.len() as u64,
        };
        let manifest_path = root.join(CONTEXT_MANIFEST_FILE);
        let manifest_file = File::create(&manifest_path)
            .with_context(|| format!("failed to create {}", manifest_path.display()))?;
        serde_json::to_writer_pretty(manifest_file, &manifest)
            .with_context(|| format!("failed to write {}", manifest_path.display()))?;

        Ok(ContextCommit {
            schema_version: CONTEXT_SCHEMA_VERSION,
            relative_path: CONTEXT_RELATIVE_DIR.to_string(),
            admin_tuple_count: self.admin_tuples.len() as u64,
            record_context_count: self.record_contexts.len() as u64,
            bytes: dir_size(&root)?,
        })
    }
}

impl PackContextReader {
    pub fn open(pack_path: impl AsRef<Path>) -> Result<Self> {
        let root = pack_path.as_ref().join(CONTEXT_RELATIVE_DIR);
        let manifest_path = root.join(CONTEXT_MANIFEST_FILE);
        let manifest_file = File::open(&manifest_path)
            .with_context(|| format!("failed to open {}", manifest_path.display()))?;
        let manifest: ContextManifest = serde_json::from_reader(manifest_file)
            .with_context(|| format!("failed to parse {}", manifest_path.display()))?;
        if manifest.schema_version != CONTEXT_SCHEMA_VERSION {
            bail!(
                "unsupported context schema version {}; expected {}",
                manifest.schema_version,
                CONTEXT_SCHEMA_VERSION
            );
        }

        let admin_tuples = open_counted_mmap(
            root.join(ADMIN_TUPLES_FILE),
            ADMIN_TUPLES_MAGIC,
            ADMIN_TUPLE_ENTRY_BYTES,
        )?;
        let record_contexts = open_counted_mmap(
            root.join(RECORD_CONTEXTS_FILE),
            RECORD_CONTEXTS_MAGIC,
            RECORD_CONTEXT_ENTRY_BYTES,
        )?;
        if admin_tuples.count != manifest.admin_tuple_count {
            bail!("context admin tuple count does not match manifest");
        }
        if record_contexts.count != manifest.record_context_count {
            bail!("record context count does not match manifest");
        }

        Ok(Self {
            manifest,
            admin_tuples,
            record_contexts,
        })
    }

    pub const fn manifest_schema_version(&self) -> u32 {
        self.manifest.schema_version
    }

    pub fn record_context(&self, record_id: RecordId) -> Result<Option<ResolvedRecordContext>> {
        let Some(entry) = self.find_record_context(record_id)? else {
            return Ok(None);
        };
        let admin_context = entry
            .admin_context_tuple_id
            .map(|tuple_id| self.admin_tuple(tuple_id))
            .transpose()?;
        Ok(Some(ResolvedRecordContext {
            record_id: entry.record_id,
            admin_context,
            postcode_record_id: entry.postcode_record_id,
            assignment_method: entry.assignment_method,
            flags: entry.flags,
        }))
    }

    fn find_record_context(&self, record_id: RecordId) -> Result<Option<RecordContextEntry>> {
        let mut low = 0;
        let mut high = self.record_contexts.count;
        while low < high {
            let mid = low + (high - low) / 2;
            let entry = read_record_context_entry(&self.record_contexts, mid)
                .with_context(|| format!("failed to read record context {mid}"))?;
            if entry.record_id == record_id {
                return Ok(Some(entry));
            }
            if entry.record_id < record_id {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        Ok(None)
    }

    fn admin_tuple(&self, tuple_id: u64) -> Result<AdminContextTuple> {
        read_admin_tuple_entry(&self.admin_tuples, tuple_id)
            .with_context(|| format!("failed to read admin context tuple {tuple_id}"))
    }
}

fn write_admin_tuples(path: &Path, tuples: &[AdminContextTuple]) -> Result<()> {
    write_counted_file(path, ADMIN_TUPLES_MAGIC, tuples.len() as u64, |file| {
        for tuple in tuples {
            for value in [
                tuple.country_record_id,
                tuple.region_record_id,
                tuple.district_record_id,
                tuple.locality_record_id,
                tuple.neighbourhood_record_id,
                tuple.place_record_id,
            ] {
                file.write_all(&encode_optional_id(value).to_le_bytes())?;
            }
        }
        Ok(())
    })
}

fn write_record_contexts(path: &Path, contexts: &[RecordContextEntry]) -> Result<()> {
    write_counted_file(path, RECORD_CONTEXTS_MAGIC, contexts.len() as u64, |file| {
        for context in contexts {
            file.write_all(&context.record_id.to_le_bytes())?;
            file.write_all(&encode_optional_id(context.admin_context_tuple_id).to_le_bytes())?;
            file.write_all(&encode_optional_id(context.postcode_record_id).to_le_bytes())?;
            file.write_all(&[assignment_method_code(context.assignment_method)])?;
            file.write_all(&context.flags.to_le_bytes())?;
            file.write_all(&[0; 5])?;
        }
        Ok(())
    })
}

fn write_counted_file(
    path: &Path,
    magic: &[u8; 8],
    count: u64,
    write_entries: impl FnOnce(&mut BufWriter<File>) -> Result<()>,
) -> Result<()> {
    let file =
        File::create(path).with_context(|| format!("failed to create {}", path.display()))?;
    let mut file = BufWriter::new(file);
    file.write_all(magic)?;
    file.write_all(&count.to_le_bytes())?;
    write_entries(&mut file)?;
    file.flush()?;
    Ok(())
}

fn open_counted_mmap(
    path: PathBuf,
    expected_magic: &[u8; 8],
    entry_bytes: usize,
) -> Result<CountedMmap> {
    let file = File::open(&path).with_context(|| format!("failed to open {}", path.display()))?;
    // SAFETY: Pack context files are immutable after build and this map is read-only.
    let bytes = unsafe { MmapOptions::new().map(&file) }
        .with_context(|| format!("failed to mmap {}", path.display()))?;
    if bytes.len() < COUNTED_HEADER_BYTES {
        bail!("{} is too short for a counted context file", path.display());
    }
    let Some(magic) = bytes.get(0..8) else {
        bail!("{} is missing magic", path.display());
    };
    if magic != expected_magic {
        bail!("{} has an invalid magic header", path.display());
    }
    let count = read_u64(&bytes, 8).expect("validated counted header");
    let entries_bytes = usize::try_from(count)
        .ok()
        .and_then(|count| count.checked_mul(entry_bytes))
        .context("context file entry count overflows usize")?;
    let expected_len = COUNTED_HEADER_BYTES
        .checked_add(entries_bytes)
        .context("context file length overflows usize")?;
    if bytes.len() != expected_len {
        bail!(
            "{} has {} bytes but expected {}",
            path.display(),
            bytes.len(),
            expected_len
        );
    }
    Ok(CountedMmap {
        bytes,
        count,
        entry_bytes,
    })
}

fn read_admin_tuple_entry(file: &CountedMmap, index: u64) -> Option<AdminContextTuple> {
    let offset = entry_offset(file, index)?;
    Some(AdminContextTuple {
        country_record_id: decode_optional_id(read_u64(&file.bytes, offset)?),
        region_record_id: decode_optional_id(read_u64(&file.bytes, offset + 8)?),
        district_record_id: decode_optional_id(read_u64(&file.bytes, offset + 16)?),
        locality_record_id: decode_optional_id(read_u64(&file.bytes, offset + 24)?),
        neighbourhood_record_id: decode_optional_id(read_u64(&file.bytes, offset + 32)?),
        place_record_id: decode_optional_id(read_u64(&file.bytes, offset + 40)?),
    })
}

fn read_record_context_entry(file: &CountedMmap, index: u64) -> Option<RecordContextEntry> {
    let offset = entry_offset(file, index)?;
    let method = assignment_method_from_code(*file.bytes.get(offset + 24)?)?;
    Some(RecordContextEntry {
        record_id: read_u64(&file.bytes, offset)?,
        admin_context_tuple_id: decode_optional_id(read_u64(&file.bytes, offset + 8)?),
        postcode_record_id: decode_optional_id(read_u64(&file.bytes, offset + 16)?),
        assignment_method: method,
        flags: read_u16(&file.bytes, offset + 25)?,
    })
}

fn entry_offset(file: &CountedMmap, index: u64) -> Option<usize> {
    if index >= file.count {
        return None;
    }
    let index = usize::try_from(index).ok()?;
    let offset = COUNTED_HEADER_BYTES.checked_add(index.checked_mul(file.entry_bytes)?)?;
    (offset + file.entry_bytes <= file.bytes.len()).then_some(offset)
}

fn encode_optional_id(value: Option<u64>) -> u64 {
    value.unwrap_or(MISSING_ID)
}

fn decode_optional_id(value: u64) -> Option<u64> {
    (value != MISSING_ID).then_some(value)
}

fn assignment_method_code(method: ContextAssignmentMethod) -> u8 {
    match method {
        ContextAssignmentMethod::BoundaryDerived => 1,
    }
}

fn assignment_method_from_code(value: u8) -> Option<ContextAssignmentMethod> {
    match value {
        1 => Some(ContextAssignmentMethod::BoundaryDerived),
        _ => None,
    }
}

fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    let array: [u8; 8] = bytes.get(offset..offset + 8)?.try_into().ok()?;
    Some(u64::from_le_bytes(array))
}

fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    let array: [u8; 2] = bytes.get(offset..offset + 2)?.try_into().ok()?;
    Some(u16::from_le_bytes(array))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_interned_context_tuples_and_reads_by_record_id() {
        let temp_dir =
            std::env::temp_dir().join(format!("open-geocode-context-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp_dir);

        let mut writer = PackContextWriter::default();
        let tuple = AdminContextTuple {
            country_record_id: Some(1),
            region_record_id: Some(2),
            locality_record_id: Some(3),
            ..AdminContextTuple::default()
        };
        writer.add_record_context(10, tuple, None, 0);
        writer.add_record_context(11, tuple, Some(99), CONTEXT_FLAG_AMBIGUOUS_ADMIN);
        let commit = writer.finish(&temp_dir).expect("finish");

        assert_eq!(commit.admin_tuple_count, 1);
        assert_eq!(commit.record_context_count, 2);

        let reader = PackContextReader::open(&temp_dir).expect("reader");
        let context = reader.record_context(11).expect("lookup").expect("context");
        assert_eq!(context.admin_context, Some(tuple));
        assert_eq!(context.postcode_record_id, Some(99));
        assert_eq!(context.flags, CONTEXT_FLAG_AMBIGUOUS_ADMIN);
        assert!(reader.record_context(12).expect("lookup").is_none());

        let _ = fs::remove_dir_all(temp_dir);
    }
}
