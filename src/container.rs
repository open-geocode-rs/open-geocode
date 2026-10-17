//! Single-file Pack container.
//!
//! A Pack is one file: a short header, named sections, and a table of contents
//! at the end. Each section carries its own format version so one part of the
//! Pack can evolve without touching the others. One file is one thing to
//! upload, copy or swap, and readers memory-map it once and hand out borrowed
//! slices per section. Every section carries an XXH3 checksum, so a copy
//! damaged in transit is caught by [`Container::verify`] before it is served.
//!
//! ```text
//! header   magic "OGPACK01" | container version u32 | reserved u32
//! sections each starts on a 64-byte boundary
//! toc      varint count, then per section: name, version, offset, length, xxh3
//! trailer  toc offset u64 | toc length u64 | magic "OGPACKTC"
//! ```

use std::{
    collections::BTreeMap,
    fmt,
    fs::{File, OpenOptions},
    io::{self, BufWriter, Seek, SeekFrom, Write},
    ops::Deref,
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::error::{Error, Result};
use memmap2::{Mmap, MmapOptions};
pub use tantivy::directory::OwnedBytes as Bytes;

use rayon::prelude::*;
use xxhash_rust::xxh3::{Xxh3, xxh3_64};

use crate::util::codec::{get_string, get_u32, get_u64, put_str, put_u64, read_u64_le};

const MAGIC: &[u8; 8] = b"OGPACK01";
const TRAILER_MAGIC: &[u8; 8] = b"OGPACKTC";
const CONTAINER_VERSION: u32 = 2;
const HEADER_BYTES: u64 = 16;
const TRAILER_BYTES: usize = 24;
const SECTION_ALIGN: u64 = 64;
const WRITE_BUFFER_BYTES: usize = 4 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SectionInfo {
    pub version: u32,
    pub offset: u64,
    pub len: u64,
    /// XXH3-64 of the section bytes.
    pub checksum: u64,
}

#[derive(Debug)]
pub struct Writer {
    path: PathBuf,
    file: BufWriter<File>,
    offset: u64,
    sections: BTreeMap<String, SectionInfo>,
    open: Option<OpenSection>,
}

struct OpenSection {
    name: String,
    version: u32,
    start: u64,
    hasher: Xxh3,
}

impl fmt::Debug for OpenSection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenSection")
            .field("name", &self.name)
            .field("version", &self.version)
            .field("start", &self.start)
            .finish_non_exhaustive()
    }
}

impl Writer {
    pub fn create(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = File::create(&path).map_err(|source| Error::IoPath {
            action: "failed to create",
            path: path.clone(),
            source,
        })?;
        let mut file = BufWriter::with_capacity(WRITE_BUFFER_BYTES, file);
        file.write_all(MAGIC)?;
        file.write_all(&CONTAINER_VERSION.to_le_bytes())?;
        file.write_all(&0u32.to_le_bytes())?;
        Ok(Self {
            path,
            file,
            offset: HEADER_BYTES,
            sections: BTreeMap::new(),
            open: None,
        })
    }

    /// Reopen a finished Pack file that nothing reads yet to add sections
    /// after its last one. The table of contents is cut off here and written
    /// again, with the new sections, by [`Self::finish`].
    pub fn append(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        // The mapping is dropped before the file is cut: a mapped file cannot
        // be shortened on Windows.
        let sections = Container::open(&path)?.sections().clone();
        let end = sections
            .values()
            .map(|info| info.offset + info.len)
            .max()
            .unwrap_or(HEADER_BYTES);
        let mut file = OpenOptions::new()
            .write(true)
            .open(&path)
            .map_err(|source| Error::IoPath {
                action: "failed to open",
                path: path.clone(),
                source,
            })?;
        file.set_len(end).map_err(|source| Error::IoPath {
            action: "failed to cut the table of contents of",
            path: path.clone(),
            source,
        })?;
        file.seek(SeekFrom::Start(end))?;
        Ok(Self {
            path,
            file: BufWriter::with_capacity(WRITE_BUFFER_BYTES, file),
            offset: end,
            sections,
            open: None,
        })
    }

    /// Start a section. Bytes written until [`Self::end`] belong to it.
    pub fn begin(&mut self, name: &str, version: u32) -> Result<()> {
        if self.open.is_some() {
            return Err(Error::Inconsistent(format!(
                "section {name} started while another section is open"
            )));
        }
        if self.sections.contains_key(name) {
            return Err(Error::DuplicateSection {
                section: name.into(),
            });
        }
        let padding = (SECTION_ALIGN - self.offset % SECTION_ALIGN) % SECTION_ALIGN;
        self.file.write_all(&vec![0u8; padding as usize])?;
        self.offset += padding;
        self.open = Some(OpenSection {
            name: name.to_string(),
            version,
            start: self.offset,
            hasher: Xxh3::new(),
        });
        Ok(())
    }

    pub fn end(&mut self) -> Result<()> {
        let section = self
            .open
            .take()
            .ok_or(Error::Inconsistent("no open Pack section".into()))?;
        self.sections.insert(
            section.name,
            SectionInfo {
                version: section.version,
                offset: section.start,
                len: self.offset - section.start,
                checksum: section.hasher.digest(),
            },
        );
        Ok(())
    }

    pub fn add(&mut self, name: &str, version: u32, bytes: &[u8]) -> Result<()> {
        self.begin(name, version)?;
        self.write_all(bytes)?;
        self.end()
    }

    pub fn add_file(&mut self, name: &str, version: u32, path: &Path) -> Result<()> {
        self.begin(name, version)?;
        let mut source = File::open(path).map_err(|source| Error::IoPath {
            action: "failed to open",
            path: path.to_path_buf(),
            source,
        })?;
        io::copy(&mut source, self).map_err(|source| Error::IoPath {
            action: "failed to copy into the Pack",
            path: path.to_path_buf(),
            source,
        })?;
        self.end()
    }

    /// Write the table of contents and sync the file. Returns the Pack size.
    pub fn finish(mut self) -> Result<u64> {
        if self.open.is_some() {
            return Err(Error::Inconsistent(
                "Pack finished with an open section".into(),
            ));
        }
        let mut toc = Vec::new();
        put_u64(&mut toc, self.sections.len() as u64);
        for (name, info) in &self.sections {
            put_str(&mut toc, name);
            put_u64(&mut toc, u64::from(info.version));
            put_u64(&mut toc, info.offset);
            put_u64(&mut toc, info.len);
            put_u64(&mut toc, info.checksum);
        }
        let toc_offset = self.offset;
        self.file.write_all(&toc)?;
        self.file.write_all(&toc_offset.to_le_bytes())?;
        self.file.write_all(&(toc.len() as u64).to_le_bytes())?;
        self.file.write_all(TRAILER_MAGIC)?;
        self.file.flush()?;
        let file = self.file.get_mut();
        file.sync_all().map_err(|source| Error::IoPath {
            action: "failed to sync",
            path: self.path.clone(),
            source,
        })?;
        Ok(file.stream_position()?)
    }
}

impl Write for Writer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let Some(section) = &mut self.open else {
            return Err(io::Error::other("Pack bytes written outside a section"));
        };
        let written = self.file.write(bytes)?;
        section.hasher.update(&bytes[..written]);
        self.offset += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// Read-only view of a Pack file.
#[derive(Clone)]
pub struct Container {
    bytes: SharedMmap,
    sections: Arc<BTreeMap<String, SectionInfo>>,
}

impl std::fmt::Debug for Container {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Container")
            .field("bytes", &self.bytes.len())
            .field("sections", &self.sections.len())
            .finish()
    }
}

impl Container {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let file = File::open(path).map_err(|source| Error::IoPath {
            action: "failed to open",
            path: path.to_path_buf(),
            source,
        })?;
        // SAFETY: Pack files are immutable once published and mapped read-only.
        let mmap = unsafe { MmapOptions::new().map(&file) }.map_err(|source| Error::IoPath {
            action: "failed to mmap",
            path: path.to_path_buf(),
            source,
        })?;
        let sections = parse_toc(&mmap)?;
        Ok(Self {
            bytes: SharedMmap(Arc::new(mmap)),
            sections: Arc::new(sections),
        })
    }

    /// Size of the Pack file in bytes.
    pub fn file_size(&self) -> u64 {
        self.bytes.len() as u64
    }

    /// Tell the OS how a reader that looks things up all over the Pack
    /// (the server, parser training) will touch the mapping, given the memory
    /// available. A Pack that fits in half of it is read in now in large
    /// reads, so no lookup waits on a fault of its own (`MADV_POPULATE_READ`
    /// on Linux, `PrefetchVirtualMemory` on Windows); one that does not is
    /// read without readahead (`MADV_RANDOM`), which would otherwise pull in
    /// pages no lookup asked for and push out those it did. Mapped files
    /// stay safe while they fit (Crotty, Leis and Pavlo, CIDR 2022). The
    /// mapping is read-only and never flushed. Returns whether the Pack was
    /// read in; a hint the OS refuses changes nothing else.
    pub fn advise_random_reads(&self, available_memory_bytes: u64) -> bool {
        let fits = self.file_size() <= available_memory_bytes / 2;
        advise(&self.bytes.0, fits);
        fits
    }

    pub fn sections(&self) -> &BTreeMap<String, SectionInfo> {
        &self.sections
    }

    /// Check every section against its checksum. Reads the whole file, so it
    /// runs when a Pack is published or copied, not on every open.
    pub fn verify(&self) -> Result<()> {
        self.sections.par_iter().try_for_each(|(name, info)| {
            let bytes = self.bytes_of(info);
            if xxh3_64(&bytes) != info.checksum {
                return Err(Error::ChecksumMismatch {
                    section: name.clone(),
                });
            }
            Ok(())
        })
    }

    /// Bytes of a section whose version must match `version`. The handle keeps
    /// the mapping alive, so it can be stored next to the `Container`.
    pub fn section(&self, name: &str, version: u32) -> Result<Bytes> {
        let info = self.info(name)?;
        if info.version != version {
            return Err(Error::UnsupportedVersion {
                found: info.version,
            });
        }
        Ok(self.bytes_of(info))
    }

    /// Bytes of a section whose format is owned by another library (Tantivy).
    pub fn raw_section(&self, name: &str) -> Result<Bytes> {
        Ok(self.bytes_of(self.info(name)?))
    }

    fn info(&self, name: &str) -> Result<&SectionInfo> {
        self.sections.get(name).ok_or(Error::Inconsistent(format!(
            "Pack is missing section {name}"
        )))
    }

    fn bytes_of(&self, info: &SectionInfo) -> Bytes {
        // Bounds were validated when the table of contents was parsed.
        Bytes::new(self.bytes.clone())
            .slice(info.offset as usize..(info.offset + info.len) as usize)
    }
}

fn parse_toc(bytes: &[u8]) -> Result<BTreeMap<String, SectionInfo>> {
    if bytes.len() < HEADER_BYTES as usize + TRAILER_BYTES || &bytes[0..8] != MAGIC {
        return Err(Error::NotAPack);
    }
    let version = u32::from_le_bytes(bytes[8..12].try_into().expect("header slice"));
    if version != CONTAINER_VERSION {
        return Err(Error::UnsupportedVersion { found: version });
    }
    let trailer = bytes.len() - TRAILER_BYTES;
    if &bytes[trailer + 16..] != TRAILER_MAGIC {
        return Err(Error::Truncated);
    }
    let toc_offset = read_u64_le(bytes, trailer).expect("trailer slice") as usize;
    let toc_len = read_u64_le(bytes, trailer + 8).expect("trailer slice") as usize;
    let toc_end = toc_offset
        .checked_add(toc_len)
        .filter(|end| *end == trailer)
        .ok_or(Error::Inconsistent(
            "Pack table of contents is out of bounds".into(),
        ))?;
    let mut input = &bytes[toc_offset..toc_end];
    let count = get_u64(&mut input)?;
    let mut sections = BTreeMap::new();
    for _ in 0..count {
        let name = get_string(&mut input)?;
        let info = SectionInfo {
            version: get_u32(&mut input)?,
            offset: get_u64(&mut input)?,
            len: get_u64(&mut input)?,
            checksum: get_u64(&mut input)?,
        };
        let end = info
            .offset
            .checked_add(info.len)
            .ok_or(Error::Inconsistent("Pack section bounds overflow".into()))?;
        if info.offset < HEADER_BYTES || end > toc_offset as u64 {
            return Err(Error::SectionOutOfBounds { section: name });
        }
        sections.insert(name, info);
    }
    if !input.is_empty() {
        return Err(Error::TrailingBytes);
    }
    Ok(sections)
}

#[cfg(unix)]
fn advise(mmap: &Mmap, prefault: bool) {
    use memmap2::Advice;
    if !prefault {
        let _ = mmap.advise(Advice::Random);
        return;
    }
    // Before Linux 5.14 there is no populate advice; read ahead instead.
    #[cfg(target_os = "linux")]
    if mmap.advise(Advice::PopulateRead).is_ok() {
        return;
    }
    let _ = mmap.advise(Advice::WillNeed);
}

#[cfg(windows)]
fn advise(mmap: &Mmap, prefault: bool) {
    use windows_sys::Win32::System::{
        Memory::{PrefetchVirtualMemory, WIN32_MEMORY_RANGE_ENTRY},
        Threading::GetCurrentProcess,
    };
    // Windows has no advice against readahead on a mapped view.
    if !prefault || mmap.is_empty() {
        return;
    }
    let range = WIN32_MEMORY_RANGE_ENTRY {
        VirtualAddress: mmap.as_ptr().cast_mut().cast(),
        NumberOfBytes: mmap.len(),
    };
    // SAFETY: the range is the live mapping, which the call only reads in.
    unsafe {
        PrefetchVirtualMemory(GetCurrentProcess(), 1, &range, 0);
    }
}

#[derive(Clone)]
struct SharedMmap(Arc<Mmap>);

impl Deref for SharedMmap {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.0
    }
}

// SAFETY: the bytes live in the mapping owned by the `Arc`, so their address
// does not change when the handle moves or is cloned.
unsafe impl stable_deref_trait::StableDeref for SharedMmap {}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "open-geocode-container-{name}-{}",
            uuid::Uuid::new_v4()
        ))
    }

    #[test]
    fn round_trips_aligned_sections() {
        let path = temp_file("round-trip");
        let source = temp_file("source");
        std::fs::write(&source, b"from a file").expect("source");
        let mut writer = Writer::create(&path).expect("writer");
        writer.add("alpha", 3, b"abc").expect("alpha");
        writer.begin("beta", 1).expect("begin");
        writer.write_all(b"streamed ").expect("write");
        writer.write_all(b"bytes").expect("write");
        writer.end().expect("end");
        writer.add_file("gamma", 1, &source).expect("gamma");
        writer.add("empty", 1, b"").expect("empty");
        let size = writer.finish().expect("finish");

        let container = Container::open(&path).expect("open");
        assert_eq!(container.file_size(), size);
        assert_eq!(
            container.section("alpha", 3).expect("alpha").as_slice(),
            b"abc"
        );
        assert_eq!(
            container.section("beta", 1).expect("beta").as_slice(),
            b"streamed bytes"
        );
        assert_eq!(
            container.section("gamma", 1).expect("gamma").as_slice(),
            b"from a file"
        );
        assert_eq!(
            container.section("empty", 1).expect("empty").as_slice(),
            b""
        );
        assert_eq!(container.sections()["beta"].offset % SECTION_ALIGN, 0);
        assert_eq!(
            container.raw_section("gamma").expect("raw").as_slice(),
            b"from a file"
        );
        let error = container.section("alpha", 2).unwrap_err().to_string();
        assert!(error.contains("rebuild the Pack"), "{error}");
        assert!(container.section("missing", 1).is_err());
    }

    #[test]
    fn verify_catches_a_flipped_byte_inside_a_section() {
        let path = temp_file("checksum");
        let mut writer = Writer::create(&path).expect("writer");
        writer.add("alpha", 1, &[7u8; 4096]).expect("alpha");
        writer.add("beta", 1, b"untouched").expect("beta");
        writer.finish().expect("finish");
        Container::open(&path)
            .expect("open")
            .verify()
            .expect("intact");

        let offset = Container::open(&path).expect("open").sections()["alpha"].offset as usize;
        let mut bytes = std::fs::read(&path).expect("read");
        bytes[offset + 2000] ^= 1;
        std::fs::write(&path, &bytes).expect("corrupt");
        // Structure is intact, so the file still opens; the checksum catches it.
        let container = Container::open(&path).expect("open");
        let error = container.verify().unwrap_err().to_string();
        assert!(error.contains("alpha"), "{error}");
    }

    #[test]
    fn appends_sections_to_a_finished_file() {
        let path = temp_file("append");
        let mut writer = Writer::create(&path).expect("writer");
        writer.add("alpha", 1, b"first").expect("alpha");
        writer.finish().expect("finish");

        let mut writer = Writer::append(&path).expect("append");
        writer.add("beta", 2, b"added later").expect("beta");
        assert!(writer.add("alpha", 1, b"again").is_err());
        let size = writer.finish().expect("finish");

        let container = Container::open(&path).expect("open");
        assert_eq!(container.file_size(), size);
        container.verify().expect("intact");
        assert_eq!(
            container.section("alpha", 1).expect("alpha").as_slice(),
            b"first"
        );
        assert_eq!(
            container.section("beta", 2).expect("beta").as_slice(),
            b"added later"
        );
        assert_eq!(container.sections()["beta"].offset % SECTION_ALIGN, 0);
    }

    #[test]
    fn rejects_truncated_files() {
        let path = temp_file("truncated");
        let mut writer = Writer::create(&path).expect("writer");
        writer.add("alpha", 1, b"abcdef").expect("alpha");
        writer.finish().expect("finish");
        let bytes = std::fs::read(&path).expect("read");
        std::fs::write(&path, &bytes[..bytes.len() - 3]).expect("truncate");
        assert!(Container::open(&path).is_err());
    }
}
