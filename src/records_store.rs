//! On-disk layout for the per-layer records store.
//!
//! Records are stored in a "directory + arena" shape that mirrors the
//! `CellDirectoryEntry` pattern in [`crate::spatial_index`]:
//!
//! ```text
//! records/
//!   directory   [magic | record_count:u64 | blob_len:u64], then N x u64 entries
//!               entry = (layer_kind:u8 << 56) | (byte_offset_into_blob:56)
//!   blob        [magic | len:u64], then per-layer structs back-to-back,
//!               each sized exactly for its layer (8-byte multiples)
//!   strings     [magic | len:u64], then the interned UTF-8 arena
//!   geometries  [magic | len:u64], then the quantized line-string arena
//! ```
//!
//! The global [`crate::pack::RecordId`] stays a plain dense `0..N` row index;
//! all per-layer complexity lives behind the directory. Each `*Entry` struct
//! holds only the fields its layer needs, replacing the former universal
//! 238-byte record and its 19 always-present text spans.

use std::{fs::File, mem, path::Path};

use anyhow::{Context, Result, bail};
use bytemuck::{Pod, Zeroable};
use memmap2::{Mmap, MmapOptions};

// File names under `records/`.
pub const DIRECTORY_FILE: &str = "directory";
pub const BLOB_FILE: &str = "blob";
pub const STRINGS_FILE: &str = "strings";
pub const GEOMETRIES_FILE: &str = "geometries";

// 8-byte magic headers.
pub const DIRECTORY_MAGIC: &[u8; 8] = b"OGRECDIR";
pub const BLOB_MAGIC: &[u8; 8] = b"OGRECBLB";
pub const STRINGS_MAGIC: &[u8; 8] = b"OGRECSTR";
pub const GEOMETRIES_MAGIC: &[u8; 8] = b"OGRECGEO";

/// `magic(8) + len(8)` for the blob and the two arenas.
pub const ARENA_HEADER_BYTES: usize = 16;
/// `magic(8) + record_count(8) + blob_len(8)` for the directory.
pub const DIRECTORY_HEADER_BYTES: usize = 24;

const LAYER_KIND_SHIFT: u64 = 56;
const OFFSET_MASK: u64 = (1u64 << LAYER_KIND_SHIFT) - 1;

/// Which per-layer table an entry belongs to. Stored in the top byte of every
/// directory entry. Place layers (country/region/...) share a single physical
/// table and are disambiguated by `PlaceEntry::place_layer`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum EntryKind {
    Address = 0,
    Street = 1,
    Place = 2,
    Postcode = 3,
    Interpolation = 4,
}

impl EntryKind {
    pub fn from_u8(value: u8) -> Result<Self> {
        Ok(match value {
            0 => Self::Address,
            1 => Self::Street,
            2 => Self::Place,
            3 => Self::Postcode,
            4 => Self::Interpolation,
            other => bail!("unknown record entry kind {other}"),
        })
    }
}

/// OSM source object kind. New on-disk encoding for schema v4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SourceObject {
    Node = 0,
    Way = 1,
    Relation = 2,
    Derived = 3,
}

impl SourceObject {
    pub fn from_u8(value: u8) -> Result<Self> {
        Ok(match value {
            0 => Self::Node,
            1 => Self::Way,
            2 => Self::Relation,
            3 => Self::Derived,
            other => bail!("unknown source object code {other}"),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum GeometryType {
    Point = 0,
    Linestring = 1,
}

impl GeometryType {
    pub fn from_u8(value: u8) -> Result<Self> {
        Ok(match value {
            0 => Self::Point,
            1 => Self::Linestring,
            other => bail!("unknown geometry type {other}"),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum LocationPrecisionCode {
    Point = 0,
    Centroid = 1,
}

impl LocationPrecisionCode {
    pub fn from_u8(value: u8) -> Result<Self> {
        Ok(match value {
            0 => Self::Point,
            1 => Self::Centroid,
            other => bail!("unknown location precision code {other}"),
        })
    }
}

// Each `*Entry` is `#[repr(C)]` with fields ordered by descending alignment
// (i64/u64, then u32, then u8) and an explicit trailing pad so there is no
// implicit padding and `size % 8 == 0`. A `len == 0` text span means "absent".

/// Address row: number plus optional street/place/unit/locality/region/postcode/country.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Pod, Zeroable)]
pub struct AddressEntry {
    pub source_object_id: i64,
    pub display_lon: i64,
    pub display_lat: i64,
    pub geometry_start: u64,
    pub number_start: u64,
    pub street_start: u64,
    pub place_start: u64,
    pub unit_start: u64,
    pub locality_start: u64,
    pub region_start: u64,
    pub postcode_start: u64,
    pub country_start: u64,
    pub geometry_len: u32,
    pub number_len: u32,
    pub street_len: u32,
    pub place_len: u32,
    pub unit_len: u32,
    pub locality_len: u32,
    pub region_len: u32,
    pub postcode_len: u32,
    pub country_len: u32,
    pub source_object: u8,
    pub geometry_type: u8,
    pub location_precision: u8,
    pub _pad: [u8; 1],
}

/// Street row: a single name plus geometry.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Pod, Zeroable)]
pub struct StreetEntry {
    pub source_object_id: i64,
    pub display_lon: i64,
    pub display_lat: i64,
    pub geometry_start: u64,
    pub name_start: u64,
    pub geometry_len: u32,
    pub name_len: u32,
    pub source_object: u8,
    pub geometry_type: u8,
    pub location_precision: u8,
    pub _pad: [u8; 5],
}

/// Place row: name + place_type, with the specific place layer in `place_layer`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Pod, Zeroable)]
pub struct PlaceEntry {
    pub source_object_id: i64,
    pub display_lon: i64,
    pub display_lat: i64,
    pub geometry_start: u64,
    pub name_start: u64,
    pub place_type_start: u64,
    pub geometry_len: u32,
    pub name_len: u32,
    pub place_type_len: u32,
    pub source_object: u8,
    pub geometry_type: u8,
    pub location_precision: u8,
    pub place_layer: u8,
}

/// Postcode row: always a derived source, so the OSM object id is not stored.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Pod, Zeroable)]
pub struct PostcodeEntry {
    pub derived_record_count: u64,
    pub display_lon: i64,
    pub display_lat: i64,
    pub geometry_start: u64,
    pub postcode_start: u64,
    pub derived_from_start: u64,
    pub geometry_len: u32,
    pub postcode_len: u32,
    pub derived_from_len: u32,
    pub geometry_type: u8,
    pub location_precision: u8,
    pub _pad: [u8; 2],
}

/// Interpolation row: street-level address parts plus the interpolation range
/// and anchor ids. Has no house `number` or `unit`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Pod, Zeroable)]
pub struct InterpolationEntry {
    pub source_object_id: i64,
    pub display_lon: i64,
    pub display_lat: i64,
    pub geometry_start: u64,
    pub street_start: u64,
    pub place_start: u64,
    pub locality_start: u64,
    pub region_start: u64,
    pub postcode_start: u64,
    pub country_start: u64,
    pub interpolation_type_start: u64,
    pub anchor_ids_start: u64,
    pub geometry_len: u32,
    pub street_len: u32,
    pub place_len: u32,
    pub locality_len: u32,
    pub region_len: u32,
    pub postcode_len: u32,
    pub country_len: u32,
    pub interpolation_type_len: u32,
    pub anchor_ids_len: u32,
    pub interpolation_start: u32,
    pub interpolation_end: u32,
    pub interpolation_step: u32,
    pub source_object: u8,
    pub geometry_type: u8,
    pub location_precision: u8,
    pub _pad: [u8; 5],
}

// No implicit padding, every entry an 8-byte multiple. If any of these fail,
// the bytemuck casts in the reader/writer would be unsound.
const _: () = assert!(mem::size_of::<AddressEntry>() == 136);
const _: () = assert!(mem::size_of::<StreetEntry>() == 56);
const _: () = assert!(mem::size_of::<PlaceEntry>() == 64);
const _: () = assert!(mem::size_of::<PostcodeEntry>() == 64);
const _: () = assert!(mem::size_of::<InterpolationEntry>() == 152);
const _: () = assert!(mem::size_of::<AddressEntry>() % 8 == 0);
const _: () = assert!(mem::size_of::<StreetEntry>() % 8 == 0);
const _: () = assert!(mem::size_of::<PlaceEntry>() % 8 == 0);
const _: () = assert!(mem::size_of::<PostcodeEntry>() % 8 == 0);
const _: () = assert!(mem::size_of::<InterpolationEntry>() % 8 == 0);

/// Pack a directory entry from a layer kind and a byte offset into the blob.
pub fn pack_directory_entry(kind: EntryKind, offset: u64) -> Result<u64> {
    if offset > OFFSET_MASK {
        bail!("records blob offset {offset} exceeds 56-bit directory limit");
    }
    Ok(((kind as u64) << LAYER_KIND_SHIFT) | offset)
}

/// The layer-kind byte stored in the top of a directory entry.
pub fn directory_entry_kind(entry: u64) -> Result<EntryKind> {
    EntryKind::from_u8((entry >> LAYER_KIND_SHIFT) as u8)
}

/// The byte offset into the blob stored in the low 56 bits of a directory entry.
pub fn directory_entry_offset(entry: u64) -> u64 {
    entry & OFFSET_MASK
}

fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    let array: [u8; 8] = bytes.get(offset..offset + 8)?.try_into().ok()?;
    Some(u64::from_le_bytes(array))
}

/// Read-side view over the four memory-mapped records files.
pub struct RecordsStore {
    directory: Mmap,
    blob: Mmap,
    strings: Mmap,
    geometries: Mmap,
    record_count: u64,
    blob_len: u64,
}

impl RecordsStore {
    pub fn open(dir: &Path) -> Result<Self> {
        let directory = map_file(&dir.join(DIRECTORY_FILE))?;
        let blob = map_file(&dir.join(BLOB_FILE))?;
        let strings = map_file(&dir.join(STRINGS_FILE))?;
        let geometries = map_file(&dir.join(GEOMETRIES_FILE))?;

        check_magic(&directory, DIRECTORY_MAGIC, DIRECTORY_FILE)?;
        check_magic(&blob, BLOB_MAGIC, BLOB_FILE)?;
        check_magic(&strings, STRINGS_MAGIC, STRINGS_FILE)?;
        check_magic(&geometries, GEOMETRIES_MAGIC, GEOMETRIES_FILE)?;

        if directory.len() < DIRECTORY_HEADER_BYTES {
            bail!("records directory file is too short for its header");
        }
        let record_count = read_u64(&directory, 8).expect("validated directory header");
        let blob_len = read_u64(&directory, 16).expect("validated directory header");

        let expected_dir = DIRECTORY_HEADER_BYTES
            .checked_add(
                usize::try_from(record_count)
                    .ok()
                    .and_then(|count| count.checked_mul(8))
                    .context("records directory size overflows usize")?,
            )
            .context("records directory size overflows usize")?;
        if directory.len() != expected_dir {
            bail!(
                "records directory has {} bytes but expected {expected_dir}",
                directory.len()
            );
        }

        let blob_len_usize =
            usize::try_from(blob_len).context("records blob length overflows usize")?;
        if blob.len() != ARENA_HEADER_BYTES + blob_len_usize {
            bail!(
                "records blob has {} bytes but header declares {}",
                blob.len(),
                ARENA_HEADER_BYTES + blob_len_usize
            );
        }

        Ok(Self {
            directory,
            blob,
            strings,
            geometries,
            record_count,
            blob_len,
        })
    }

    pub fn record_count(&self) -> u64 {
        self.record_count
    }

    fn directory_entry(&self, index: u64) -> Result<u64> {
        if index >= self.record_count {
            bail!("record row {index} is out of range");
        }
        let offset = DIRECTORY_HEADER_BYTES + (index as usize) * 8;
        read_u64(&self.directory, offset).context("record directory entry is truncated")
    }

    /// The layer kind and raw entry bytes for `record_id`.
    pub fn entry(&self, record_id: u64) -> Result<(EntryKind, &[u8])> {
        let packed = self.directory_entry(record_id)?;
        let kind = directory_entry_kind(packed)?;
        let start = directory_entry_offset(packed);
        let end = if record_id + 1 < self.record_count {
            directory_entry_offset(self.directory_entry(record_id + 1)?)
        } else {
            self.blob_len
        };
        let start = usize::try_from(start).context("blob offset overflows usize")?;
        let end = usize::try_from(end).context("blob offset overflows usize")?;
        let bytes = self
            .blob
            .get(ARENA_HEADER_BYTES + start..ARENA_HEADER_BYTES + end)
            .with_context(|| format!("record {record_id} blob range is outside the arena"))?;
        Ok((kind, bytes))
    }

    pub fn strings(&self) -> &[u8] {
        &self.strings[ARENA_HEADER_BYTES..]
    }

    pub fn geometries(&self) -> &[u8] {
        &self.geometries[ARENA_HEADER_BYTES..]
    }
}

fn map_file(path: &Path) -> Result<Mmap> {
    let file =
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    // SAFETY: pack files are immutable once built and the map is read-only.
    unsafe { MmapOptions::new().map(&file) }
        .with_context(|| format!("failed to mmap {}", path.display()))
}

fn check_magic(bytes: &[u8], expected: &[u8; 8], name: &str) -> Result<()> {
    match bytes.get(0..8) {
        Some(magic) if magic == expected => Ok(()),
        Some(_) => bail!("records {name} file has an invalid magic header"),
        None => bail!("records {name} file is missing its magic header"),
    }
}
