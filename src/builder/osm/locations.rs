use std::{
    collections::{BTreeMap, HashMap},
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

use memmap2::MmapMut;
use osmpbf::{Element, PrimitiveBlock};
use rayon::prelude::*;

use serde::{Deserialize, Serialize};

use crate::{
    builder::progress::{BuildStage, Progress},
    error::{Error, Result},
    extsort::{Owner, RunState, Scratch, ScratchFile, Spill, Spool, SpoolFile},
    memory::{Budget, Reservation},
    util::codec::{Delta, get_delta, get_u64, put_delta, put_u64},
};

use super::{
    address::{collect_addr_tags_from_map, collect_clean_tags},
    pbf::for_each_block,
};

const CHUNK_BITS: u32 = 16;
const CHUNK_IDS: u64 = 1 << CHUNK_BITS;
const WORDS: usize = (CHUNK_IDS / 64) as usize;
const BLOCK_WORDS: usize = 8;
const BLOCKS: usize = WORDS / BLOCK_WORDS;
pub(crate) const ID_LIMIT: u64 = 1 << 36;
const CHUNKS: usize = (ID_LIMIT / CHUNK_IDS) as usize;
pub(crate) const CHUNK_BYTES: u64 = std::mem::size_of::<Chunk>() as u64;
pub(crate) const TABLE_BYTES: u64 = (CHUNKS * std::mem::size_of::<OnceLock<Box<Chunk>>>()) as u64;
pub(crate) const LOCATION_BYTES: u64 = 8;
const LOCATIONS_LABEL: &str = "node-locations";

#[derive(Debug)]
struct Chunk {
    words: [AtomicU64; WORDS],
    base: u64,
    blocks: [u16; BLOCKS],
}

impl Chunk {
    fn new() -> Box<Self> {
        Box::new(Self {
            words: std::array::from_fn(|_| AtomicU64::new(0)),
            base: 0,
            blocks: [0; BLOCKS],
        })
    }

    fn word(&self, index: usize) -> u64 {
        self.words[index].load(Ordering::Relaxed)
    }
}

#[derive(Debug)]
pub(crate) struct NodeSet {
    chunks: Box<[OnceLock<Box<Chunk>>]>,
    reservation: Mutex<Reservation>,
}

fn position(id: i64) -> Option<(usize, usize, u32)> {
    let id = u64::try_from(id).ok().filter(|id| *id < ID_LIMIT)?;
    let within = id % CHUNK_IDS;
    Some((
        (id / CHUNK_IDS) as usize,
        (within / 64) as usize,
        (within % 64) as u32,
    ))
}

impl NodeSet {
    pub(crate) fn new(memory: &Arc<Budget>) -> Self {
        Self {
            chunks: (0..CHUNKS).map(|_| OnceLock::new()).collect(),
            reservation: Mutex::new(Reservation::forced(memory, TABLE_BYTES as usize)),
        }
    }

    pub(crate) fn mark(&self, ids: &[i64]) -> Result<()> {
        for &id in ids {
            let (chunk, word, bit) = position(id).ok_or_else(|| {
                Error::BadInput(format!(
                    "a way references node {id}, outside the node ids 0 to {ID_LIMIT} a build \
                     can hold; ids below 0 are edits never uploaded to OSM"
                ))
            })?;
            let word = &self.chunk(chunk).words[word];
            if word.load(Ordering::Relaxed) & (1 << bit) == 0 {
                word.fetch_or(1 << bit, Ordering::Relaxed);
            }
        }
        Ok(())
    }

    fn chunk(&self, index: usize) -> &Chunk {
        self.chunks[index].get_or_init(|| {
            self.reservation
                .lock()
                .expect("node set reservation")
                .force_grow(CHUNK_BYTES as usize);
            Chunk::new()
        })
    }

    fn contains(&self, id: i64) -> bool {
        position(id).is_some_and(|(chunk, word, bit)| {
            self.chunks[chunk]
                .get()
                .is_some_and(|chunk| chunk.word(word) & (1 << bit) != 0)
        })
    }

    pub(crate) fn len(&self) -> u64 {
        self.chunks
            .par_iter()
            .filter_map(OnceLock::get)
            .map(|chunk| {
                (0..WORDS)
                    .map(|word| u64::from(chunk.word(word).count_ones()))
                    .sum::<u64>()
            })
            .sum()
    }

    pub(crate) fn bytes(&self) -> u64 {
        self.reservation
            .lock()
            .expect("node set reservation")
            .bytes() as u64
    }

    fn clear(&self, low: u64, high: u64) {
        let high = high.min(ID_LIMIT);
        let mut id = low;
        while id < high {
            let chunk_index = (id / CHUNK_IDS) as usize;
            let chunk_end = (chunk_index as u64 + 1) * CHUNK_IDS;
            let end = high.min(chunk_end);
            if let Some(chunk) = self.chunks[chunk_index].get() {
                let mut at = id;
                while at < end {
                    let word = ((at % CHUNK_IDS) / 64) as usize;
                    let word_end = (at / 64 + 1) * 64;
                    let until = end.min(word_end);
                    let width = until - at;
                    let shift = at % 64;
                    let mask = if width == 64 {
                        u64::MAX
                    } else {
                        ((1u64 << width) - 1) << shift
                    };
                    chunk.words[word].fetch_and(!mask, Ordering::Relaxed);
                    at = until;
                }
            }
            id = end;
        }
    }

    pub(crate) fn save(&self, scratch: &Arc<Scratch>) -> Result<SpoolFile<ChunkBits>> {
        let mut spool = Spool::create(scratch, "node-set")?;
        for (index, chunk) in self.chunks.iter().enumerate() {
            if let Some(chunk) = chunk.get() {
                spool.write(&ChunkBits {
                    index: index as u64,
                    words: (0..WORDS).map(|word| chunk.word(word)).collect(),
                })?;
            }
        }
        spool.finish()
    }

    pub(crate) fn load(memory: &Arc<Budget>, saved: &SpoolFile<ChunkBits>) -> Result<Self> {
        let set = Self::new(memory);
        for chunk in saved.reader()? {
            let ChunkBits { index, words } = chunk?;
            let index = usize::try_from(index)
                .ok()
                .filter(|index| *index < CHUNKS && words.len() == WORDS)
                .ok_or(Error::Decode("node set chunk out of range"))?;
            let chunk = set.chunk(index);
            for (word, bits) in words.into_iter().enumerate() {
                chunk.words[word].store(bits, Ordering::Relaxed);
            }
        }
        Ok(set)
    }

    fn rank(&mut self) {
        let counts = self
            .chunks
            .par_iter_mut()
            .map(|slot| {
                let Some(chunk) = slot.get_mut() else {
                    return 0;
                };
                let mut count = 0u64;
                for block in 0..BLOCKS {
                    chunk.blocks[block] = count as u16;
                    count += (block * BLOCK_WORDS..(block + 1) * BLOCK_WORDS)
                        .map(|word| u64::from(chunk.word(word).count_ones()))
                        .sum::<u64>();
                }
                count
            })
            .collect::<Vec<_>>();
        let mut base = 0;
        for (slot, count) in self.chunks.iter_mut().zip(counts) {
            if let Some(chunk) = slot.get_mut() {
                chunk.base = base;
            }
            base += count;
        }
    }

    fn slot(&self, id: i64) -> Option<usize> {
        let (chunk, word, bit) = position(id)?;
        let chunk = self.chunks[chunk].get()?;
        let bits = chunk.word(word);
        if bits & (1 << bit) == 0 {
            return None;
        }
        let block = word / BLOCK_WORDS;
        let before = (block * BLOCK_WORDS..word)
            .map(|word| u64::from(chunk.word(word).count_ones()))
            .sum::<u64>()
            + u64::from((bits & ((1u64 << bit) - 1)).count_ones());
        Some((chunk.base + u64::from(chunk.blocks[block]) + before) as usize)
    }
}

#[derive(Debug)]
pub(crate) struct ChunkBits {
    index: u64,
    words: Vec<u64>,
}

impl Spill for ChunkBits {
    fn encode(&self, out: &mut Vec<u8>) {
        self.encode_after(&mut Delta::default(), out);
    }

    fn decode(input: &mut &[u8]) -> Result<Self> {
        Self::decode_after(&mut Delta::default(), input)
    }

    fn encode_after(&self, delta: &mut Delta, out: &mut Vec<u8>) {
        put_delta(out, delta, self.index as i64);
        put_u64(out, self.words.len() as u64);
        for word in &self.words {
            out.extend_from_slice(&word.to_le_bytes());
        }
    }

    fn decode_after(delta: &mut Delta, input: &mut &[u8]) -> Result<Self> {
        let index = get_delta(input, delta)? as u64;
        let count = usize::try_from(get_u64(input)?)
            .ok()
            .filter(|count| *count <= WORDS)
            .ok_or(Error::Decode("node set chunk too long"))?;
        let bytes = input
            .get(..count * 8)
            .ok_or(Error::Decode("node set chunk truncated"))?;
        *input = &input[count * 8..];
        let words = bytes
            .as_chunks::<8>()
            .0
            .iter()
            .map(|word| u64::from_le_bytes(*word))
            .collect();
        Ok(Self { index, words })
    }

    fn resident_bytes(&self) -> usize {
        std::mem::size_of::<Self>() + self.words.len() * 8
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Location {
    pub lat_e7: i32,
    pub lon_e7: i32,
}

impl Location {
    pub(crate) fn lat(self) -> f64 {
        degrees(self.lat_e7)
    }

    pub(crate) fn lon(self) -> f64 {
        degrees(self.lon_e7)
    }
}

/// Degrees from a 1e-7 fixed-point PBF coordinate, computed the way `osmpbf`
/// computes `lat()`/`lon()` (via nanodegrees), so a coordinate read through
/// the store is bit-identical to one read from the node itself.
pub(crate) fn degrees(value: i32) -> f64 {
    1e-9 * (i64::from(value) * 100) as f64
}

#[derive(Debug)]
pub(crate) enum Slots {
    Memory {
        locations: Vec<Location>,
        _reservation: Reservation,
    },
    Mapped(MappedSlots),
}

#[derive(Debug)]
pub(crate) struct MappedSlots {
    map: Option<MmapMut>,
    file: Option<File>,
    path: PathBuf,
    len: usize,
    scratch: Arc<Scratch>,
    owner: Arc<Owner>,
    bytes: u64,
}

impl MappedSlots {
    fn create(scratch: &Arc<Scratch>, capacity: u64) -> Result<Self> {
        let bytes = capacity * LOCATION_BYTES;
        let owner = scratch.owner(LOCATIONS_LABEL);
        scratch.reserve(&owner, bytes)?;
        let path = scratch.new_file(LOCATIONS_LABEL);
        let io = |source| Error::IoPath {
            action: "failed to create node location file",
            path: path.clone(),
            source,
        };
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .map_err(io)?;
        file.set_len(bytes).map_err(io)?;
        let map = if bytes == 0 {
            None
        } else {
            // SAFETY: the file was just created in the build's own scratch
            // directory and nothing else opens, truncates or maps it; it is
            // unmapped before it is closed and removed (see Drop).
            Some(unsafe { MmapMut::map_mut(&file) }.map_err(io)?)
        };
        Ok(Self {
            map,
            file: Some(file),
            path,
            len: 0,
            scratch: Arc::clone(scratch),
            owner,
            bytes,
        })
    }

    fn push(&mut self, location: Location) -> Result<()> {
        let start = self.len * LOCATION_BYTES as usize;
        let slot = self
            .map
            .as_mut()
            .and_then(|map| map.get_mut(start..start + LOCATION_BYTES as usize))
            .ok_or(Error::Decode("more node locations than were marked"))?;
        slot[..4].copy_from_slice(&location.lat_e7.to_le_bytes());
        slot[4..].copy_from_slice(&location.lon_e7.to_le_bytes());
        self.len += 1;
        Ok(())
    }

    fn reopen(scratch: &Arc<Scratch>, name: &str, len: usize) -> Result<Self> {
        let path = scratch.path().join(name);
        let failed = |source| Error::IoPath {
            action: "failed to reopen node location file",
            path: path.clone(),
            source,
        };
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .map_err(failed)?;
        let bytes = file.metadata().map_err(failed)?.len();
        if bytes < len as u64 * LOCATION_BYTES {
            return Err(Error::BadScratch {
                path: path.clone(),
                detail: "node location file is shorter than its checkpoint",
            });
        }
        let map = if bytes == 0 {
            None
        } else {
            // SAFETY: the file is in the build's own scratch directory, named
            // by its checkpoint, and nothing else opens, truncates or maps
            // it; it is unmapped before it is closed and removed (see Drop).
            Some(unsafe { MmapMut::map_mut(&file) }.map_err(failed)?)
        };
        let owner = scratch.owner(LOCATIONS_LABEL);
        scratch.adopt(&owner, bytes);
        Ok(Self {
            map,
            file: Some(file),
            path,
            len,
            scratch: Arc::clone(scratch),
            owner,
            bytes,
        })
    }

    fn get(&self, index: usize) -> Location {
        let start = index * LOCATION_BYTES as usize;
        let map = self.map.as_ref().expect("a ranked slot is stored");
        let slot = &map[start..start + LOCATION_BYTES as usize];
        Location {
            lat_e7: i32::from_le_bytes(slot[..4].try_into().expect("4 bytes")),
            lon_e7: i32::from_le_bytes(slot[4..].try_into().expect("4 bytes")),
        }
    }
}

impl Drop for MappedSlots {
    fn drop(&mut self) {
        // Windows refuses to remove a file that is still mapped or open.
        drop(self.map.take());
        drop(self.file.take());
        self.scratch.remove(&self.owner, &self.path, self.bytes);
    }
}

impl Slots {
    pub(crate) fn new(
        capacity: u64,
        in_memory: bool,
        memory: &Arc<Budget>,
        scratch: &Arc<Scratch>,
    ) -> Result<Self> {
        let mut reservation = Reservation::empty(memory);
        if in_memory
            && let Ok(bytes) = usize::try_from(capacity * LOCATION_BYTES)
            && reservation.try_grow(bytes)
        {
            return Ok(Self::Memory {
                locations: Vec::with_capacity(capacity as usize),
                _reservation: reservation,
            });
        }
        Ok(Self::Mapped(MappedSlots::create(scratch, capacity)?))
    }

    fn push(&mut self, location: Location) -> Result<()> {
        match self {
            Self::Memory { locations, .. } => {
                locations.push(location);
                Ok(())
            }
            Self::Mapped(mapped) => mapped.push(location),
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::Memory { locations, .. } => locations.len(),
            Self::Mapped(mapped) => mapped.len,
        }
    }

    fn get(&self, index: usize) -> Location {
        match self {
            Self::Memory { locations, .. } => locations[index],
            Self::Mapped(mapped) => mapped.get(index),
        }
    }
}

#[derive(Debug, Default)]
struct Entrances {
    ids: Vec<i64>,
    kinds: Vec<u32>,
    values: Vec<String>,
    index: HashMap<String, u32>,
}

impl Entrances {
    fn push(&mut self, id: i64, kind: &str) {
        let kind = match self.index.get(kind) {
            Some(kind) => *kind,
            None => {
                let next = self.values.len() as u32;
                self.values.push(kind.to_string());
                self.index.insert(kind.to_string(), next);
                next
            }
        };
        self.ids.push(id);
        self.kinds.push(kind);
    }

    fn get(&self, id: i64) -> Option<&str> {
        let at = self.ids.binary_search(&id).ok()?;
        Some(&self.values[self.kinds[at] as usize])
    }

    fn iter(&self) -> impl Iterator<Item = (i64, &str)> {
        self.ids
            .iter()
            .zip(&self.kinds)
            .map(|(id, kind)| (*id, self.values[*kind as usize].as_str()))
    }
}

#[derive(Debug)]
pub(crate) struct Locations {
    set: NodeSet,
    slots: Slots,
    tags: Vec<(i64, BTreeMap<String, String>)>,
    entrances: Entrances,
    saved: Option<Saved>,
}

#[derive(Debug)]
struct Saved {
    set: SpoolFile<ChunkBits>,
    tags: SpoolFile<Tagged>,
    entrances: SpoolFile<EntranceNode>,
    slots: Option<ScratchFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LocationsState {
    set: RunState,
    tags: RunState,
    entrances: RunState,
    slots: String,
    len: usize,
}

#[derive(Debug)]
pub(crate) struct Tagged(i64, BTreeMap<String, String>);

impl Spill for Tagged {
    fn encode(&self, out: &mut Vec<u8>) {
        crate::util::codec::put_i64(out, self.0);
        put_u64(out, self.1.len() as u64);
        for (key, value) in &self.1 {
            crate::util::codec::put_str(out, key);
            crate::util::codec::put_str(out, value);
        }
    }

    fn decode(input: &mut &[u8]) -> Result<Self> {
        let id = crate::util::codec::get_i64(input)?;
        let count = get_u64(input)?;
        let mut tags = BTreeMap::new();
        for _ in 0..count {
            let key = crate::util::codec::get_string(input)?;
            tags.insert(key, crate::util::codec::get_string(input)?);
        }
        Ok(Self(id, tags))
    }

    fn resident_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self
                .1
                .iter()
                .map(|(key, value)| key.len() + value.len() + 48)
                .sum::<usize>()
    }
}

#[derive(Debug)]
pub(crate) struct EntranceNode(i64, String);

impl Spill for EntranceNode {
    fn encode(&self, out: &mut Vec<u8>) {
        crate::util::codec::put_i64(out, self.0);
        crate::util::codec::put_str(out, &self.1);
    }

    fn decode(input: &mut &[u8]) -> Result<Self> {
        Ok(Self(
            crate::util::codec::get_i64(input)?,
            crate::util::codec::get_string(input)?,
        ))
    }

    fn resident_bytes(&self) -> usize {
        std::mem::size_of::<Self>() + self.1.len()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct LoadStats {
    pub needed: u64,
    pub stored: u64,
    pub set_bytes: u64,
    pub location_bytes: u64,
    pub mapped: bool,
    pub tagged: u64,
    pub entrances: u64,
    pub read_bytes: u64,
}

impl Locations {
    pub(crate) fn save(&mut self, scratch: &Arc<Scratch>) -> Result<LocationsState> {
        if self.saved.is_none() {
            let set = self.set.save(scratch)?;
            let mut tags = Spool::create(scratch, "node-tags")?;
            for (id, values) in &self.tags {
                tags.write(&Tagged(*id, values.clone()))?;
            }
            let mut entrances = Spool::create(scratch, "node-entrances")?;
            for (id, kind) in self.entrances.iter() {
                entrances.write(&EntranceNode(id, kind.to_string()))?;
            }
            let slots = match &self.slots {
                Slots::Memory { locations, .. } => {
                    let name = scratch.new_file(LOCATIONS_LABEL);
                    let name = name
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let mut file = std::io::BufWriter::with_capacity(
                        1 << 20,
                        ScratchFile::create(scratch, LOCATIONS_LABEL, &name)?,
                    );
                    for location in locations {
                        std::io::Write::write_all(&mut file, &location.lat_e7.to_le_bytes())?;
                        std::io::Write::write_all(&mut file, &location.lon_e7.to_le_bytes())?;
                    }
                    Some(file.into_inner().map_err(|error| error.into_error())?)
                }
                Slots::Mapped(mapped) => {
                    if let Some(map) = &mapped.map {
                        map.flush()?;
                    }
                    None
                }
            };
            self.saved = Some(Saved {
                set,
                tags: tags.finish()?,
                entrances: entrances.finish()?,
                slots,
            });
        }
        let saved = self.saved.as_ref().expect("saved");
        let slots = match (&saved.slots, &self.slots) {
            (Some(file), _) => file.name(),
            (None, Slots::Mapped(mapped)) => mapped
                .path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            (None, Slots::Memory { .. }) => unreachable!("coordinates in memory are saved"),
        };
        Ok(LocationsState {
            set: saved.set.state(),
            tags: saved.tags.state(),
            entrances: saved.entrances.state(),
            slots,
            len: self.slots.len(),
        })
    }

    pub(crate) fn files(&self, scratch: &Scratch) -> Vec<PathBuf> {
        let Some(saved) = &self.saved else {
            return Vec::new();
        };
        let mut files = saved.set.state().files(scratch).collect::<Vec<_>>();
        files.extend(saved.tags.state().files(scratch));
        files.extend(saved.entrances.state().files(scratch));
        match (&saved.slots, &self.slots) {
            (Some(file), _) => files.push(file.path().to_path_buf()),
            (None, Slots::Mapped(mapped)) => files.push(mapped.path.clone()),
            (None, Slots::Memory { .. }) => {}
        }
        files
    }

    pub(crate) fn restore(
        scratch: &Arc<Scratch>,
        memory: &Arc<Budget>,
        state: &LocationsState,
    ) -> Result<Self> {
        let set_file = SpoolFile::restore(scratch, &state.set)?;
        let mut set = NodeSet::load(memory, &set_file)?;
        set.rank();
        let tags_file = SpoolFile::<Tagged>::restore(scratch, &state.tags)?;
        let tags = tags_file
            .reader()?
            .map(|tagged| tagged.map(|Tagged(id, tags)| (id, tags)))
            .collect::<Result<Vec<_>>>()?;
        let entrances_file = SpoolFile::<EntranceNode>::restore(scratch, &state.entrances)?;
        let mut entrances = Entrances::default();
        for entrance in entrances_file.reader()? {
            let EntranceNode(id, kind) = entrance?;
            entrances.push(id, &kind);
        }
        let slots = Slots::Mapped(MappedSlots::reopen(scratch, &state.slots, state.len)?);
        Ok(Self {
            set,
            slots,
            tags,
            entrances,
            saved: Some(Saved {
                set: set_file,
                tags: tags_file,
                entrances: entrances_file,
                slots: None,
            }),
        })
    }

    pub(crate) fn get(&self, id: i64) -> Option<Location> {
        self.set.slot(id).map(|slot| self.slots.get(slot))
    }

    pub(crate) fn entrance(&self, id: i64) -> Option<&str> {
        self.entrances.get(id)
    }

    pub(crate) fn tags(&self, id: i64) -> Option<&BTreeMap<String, String>> {
        self.tags
            .binary_search_by_key(&id, |(node, _)| *node)
            .ok()
            .map(|index| &self.tags[index].1)
    }
}

#[derive(Debug, Default)]
struct Found {
    nodes: Vec<(i64, Location)>,
    tags: Vec<(i64, BTreeMap<String, String>)>,
    entrances: Vec<(i64, String)>,
    first: Option<i64>,
    last: Option<i64>,
    unsorted: Option<(i64, i64)>,
}

fn find(block: &PrimitiveBlock, set: &NodeSet, tag_ids: &[i64]) -> Found {
    let mut found = Found::default();
    let mut visit = |id: i64,
                     lat_e7: i32,
                     lon_e7: i32,
                     tags: &dyn Fn() -> BTreeMap<String, String>,
                     entrance: &dyn Fn() -> Option<String>| {
        if let Some(last) = found.last
            && id <= last
            && found.unsorted.is_none()
        {
            found.unsorted = Some((last, id));
        }
        found.first.get_or_insert(id);
        found.last = Some(id);
        if !set.contains(id) {
            return;
        }
        found.nodes.push((id, Location { lat_e7, lon_e7 }));
        if let Some(kind) = entrance() {
            found.entrances.push((id, kind));
        }
        if tag_ids.binary_search(&id).is_ok() {
            let tags = tags();
            if !tags.is_empty() {
                found.tags.push((id, tags));
            }
        }
    };
    for element in block.elements() {
        match element {
            Element::DenseNode(node) => visit(
                node.id(),
                node.decimicro_lat(),
                node.decimicro_lon(),
                &|| addr_tags(|| node.tags()),
                &|| entrance_kind(node.tags()),
            ),
            Element::Node(node) => visit(
                node.id(),
                node.decimicro_lat(),
                node.decimicro_lon(),
                &|| addr_tags(|| node.tags()),
                &|| entrance_kind(node.tags()),
            ),
            Element::Way(_) | Element::Relation(_) => {}
        }
    }
    found
}

pub(crate) fn entrance_kind<'a>(
    mut tags: impl Iterator<Item = (&'a str, &'a str)>,
) -> Option<String> {
    tags.find(|(key, _)| *key == "entrance")
        .map(|(_, value)| value.trim())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn addr_tags<'a, I: Iterator<Item = (&'a str, &'a str)>>(
    tags: impl Fn() -> I,
) -> BTreeMap<String, String> {
    if !tags().any(|(key, _)| key.starts_with("addr:")) {
        return BTreeMap::new();
    }
    collect_addr_tags_from_map(&collect_clean_tags(tags()))
}

fn unsorted(earlier: i64, later: i64) -> Error {
    Error::BadInput(format!(
        "node {later} follows node {earlier}: the input PBF must be sorted by id \
         (sort it with `osmium sort`)"
    ))
}

pub(crate) fn load(
    input: &Path,
    node_blobs: &[u64],
    set: NodeSet,
    tag_ids: &[i64],
    mut slots: Slots,
    progress: &Progress,
) -> Result<(Locations, LoadStats)> {
    let needed = set.len();
    let mut tags = Vec::new();
    let mut entrances = Entrances::default();
    let mut last: Option<i64> = None;
    let mut cleared_to = 0u64;
    let read_bytes = for_each_block(
        input,
        progress,
        BuildStage::NodeJoin,
        Some(node_blobs),
        |_, block| Ok(find(&block, &set, tag_ids)),
        |found| {
            if let Some((earlier, later)) = found.unsorted {
                return Err(unsorted(earlier, later));
            }
            if let (Some(last), Some(first)) = (last, found.first)
                && first <= last
            {
                return Err(unsorted(last, first));
            }
            last = found.last.or(last);
            for (id, location) in found.nodes {
                let id = id as u64;
                set.clear(cleared_to, id);
                cleared_to = id + 1;
                slots.push(location)?;
            }
            tags.extend(found.tags);
            for (id, kind) in found.entrances {
                entrances.push(id, &kind);
            }
            Ok(())
        },
    )?;
    set.clear(cleared_to, ID_LIMIT);
    let mut set = set;
    set.rank();
    let stats = LoadStats {
        needed,
        stored: slots.len() as u64,
        set_bytes: set.bytes(),
        location_bytes: needed * LOCATION_BYTES,
        mapped: matches!(slots, Slots::Mapped(_)),
        tagged: tags.len() as u64,
        entrances: entrances.ids.len() as u64,
        read_bytes,
    };
    Ok((
        Locations {
            set,
            slots,
            tags,
            entrances,
            saved: None,
        },
        stats,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    fn ranked(ids: &[i64]) -> NodeSet {
        let set = NodeSet::new(&Budget::unlimited());
        set.mark(ids).expect("mark");
        set
    }

    #[test]
    fn a_marked_id_is_found_at_its_rank_across_words_blocks_and_chunks() {
        let ids = [
            0,
            1,
            63,
            64,
            511,
            512,
            65_535,
            65_536,
            1 << 33,
            (1 << 36) - 1,
        ];
        let mut set = ranked(&ids);
        set.mark(&[64, 0]).expect("repeat");
        assert_eq!(set.len(), ids.len() as u64);
        set.rank();
        for (rank, id) in ids.iter().enumerate() {
            assert_eq!(set.slot(*id), Some(rank), "{id}");
        }
        for id in [2, 62, 65, 513, 65_537, 1 << 32] {
            assert_eq!(set.slot(id), None, "{id}");
        }
        assert!(set.mark(&[-1]).is_err());
        assert!(set.mark(&[1 << 36]).is_err());
    }

    #[test]
    fn clearing_unmarks_exactly_the_range() {
        let ids = (0..200_000).step_by(7).collect::<Vec<i64>>();
        let set = ranked(&ids);
        set.clear(14, 130_000);
        for id in &ids {
            let kept = *id < 14 || *id >= 130_000;
            assert_eq!(set.contains(*id), kept, "{id}");
        }
    }

    #[test]
    fn mapped_slots_read_back_what_was_written() {
        let dir = TempDir::new("locations");
        let scratch = Scratch::create(dir.join("scratch")).expect("scratch");
        let mut slots = Slots::new(3, true, &Budget::new(0), &scratch).expect("slots");
        assert!(matches!(slots, Slots::Mapped(_)));
        for value in [-900_000_000, 0, 1_800_000_000] {
            slots
                .push(Location {
                    lat_e7: value / 2,
                    lon_e7: value,
                })
                .expect("push");
        }
        assert!(
            slots
                .push(Location {
                    lat_e7: 0,
                    lon_e7: 0
                })
                .is_err()
        );
        assert_eq!(
            slots.get(2),
            Location {
                lat_e7: 900_000_000,
                lon_e7: 1_800_000_000
            }
        );
        assert_eq!(slots.get(0).lon_e7, -900_000_000);
        let Slots::Mapped(mapped) = &slots else {
            unreachable!("mapped above");
        };
        let path = mapped.path.clone();
        assert!(path.exists());
        drop(slots);
        assert!(!path.exists());
    }
}
