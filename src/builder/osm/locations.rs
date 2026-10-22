//! Coordinates of the nodes ways need, found by node id.
//!
//! The scan marks every node a kept way or a boundary member references in
//! a bitmap over the id space ([`NodeSet`]). The node pass then keeps the
//! coordinates of marked nodes only, appended in id order, so a node's slot
//! is the number of marked ids below it: its rank, which a popcount index
//! answers in a few word reads ([`Locations`]). og needs about one node in
//! seven, so this stores 8 B per needed node plus 0.13 B per id of the id
//! space, against the 21 B per reference that sorting references and
//! coordinates by way wrote to scratch twice over (osmium's dense and sparse
//! indexes, Planetiler's LongLongMap and osm2pgsql's flat nodes store every
//! node, or 16 B per stored one).
//!
//! The bitmap is reserved in the memory budget. The coordinates are too when
//! they fit; otherwise they live in a memory-mapped scratch file, which the
//! page cache serves much like memory as long as it can hold it.

use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

use memmap2::MmapMut;
use osmpbf::{Element, PrimitiveBlock};
use rayon::prelude::*;

use crate::{
    builder::progress::{BuildStage, Progress},
    error::{Error, Result},
    extsort::{Owner, Scratch},
    memory::{Budget, Reservation},
};

use super::{
    address::{collect_addr_tags_from_map, collect_clean_tags},
    pbf::for_each_block,
};

/// Ids per chunk of the bitmap. Chunks are allocated when an id in them is
/// first marked, so an extract whose ways reference a few id ranges holds
/// those ranges only; 8 KiB chunks keep that granularity fine while the
/// chunk table stays at 16 MiB.
const CHUNK_BITS: u32 = 16;
const CHUNK_IDS: u64 = 1 << CHUNK_BITS;
const WORDS: usize = (CHUNK_IDS / 64) as usize;
/// Words per rank block: a lookup popcounts at most this many words.
const BLOCK_WORDS: usize = 8;
const BLOCKS: usize = WORDS / BLOCK_WORDS;
/// Node ids the set covers, from 0. OSM handed out 14.3 billion by October
/// 2026, about a billion a year; 2^36 leaves room for decades.
pub(crate) const ID_LIMIT: u64 = 1 << 36;
const CHUNKS: usize = (ID_LIMIT / CHUNK_IDS) as usize;
/// Bytes one allocated chunk holds: its bitmap and its rank index.
pub(crate) const CHUNK_BYTES: u64 = std::mem::size_of::<Chunk>() as u64;
/// Bytes of the chunk table, allocated with the set whatever it holds.
pub(crate) const TABLE_BYTES: u64 = (CHUNKS * std::mem::size_of::<OnceLock<Box<Chunk>>>()) as u64;
/// Bytes one stored coordinate takes: latitude and longitude in the PBF's
/// 1e-7 degree fixed point.
pub(crate) const LOCATION_BYTES: u64 = 8;

#[derive(Debug)]
struct Chunk {
    words: [AtomicU64; WORDS],
    /// Marked ids in every chunk before this one, once ranked.
    base: u64,
    /// Marked ids in this chunk before each block, once ranked. A chunk has
    /// at most 65,024 before its last block, so u16 holds them.
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

/// The node ids ways need. Marked from many threads at once.
#[derive(Debug)]
pub(crate) struct NodeSet {
    chunks: Box<[OnceLock<Box<Chunk>>]>,
    reservation: Mutex<Reservation>,
}

/// Where an id falls: its chunk, its word in the chunk and its bit.
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
            // The set cannot spill: it is what lets the node pass skip
            // every node no way needs.
            reservation: Mutex::new(Reservation::forced(memory, TABLE_BYTES as usize)),
        }
    }

    /// Mark the nodes `ids` names.
    pub(crate) fn mark(&self, ids: &[i64]) -> Result<()> {
        for &id in ids {
            let (chunk, word, bit) = position(id).ok_or_else(|| {
                Error::BadInput(format!(
                    "a way references node {id}, outside the node ids 0 to {ID_LIMIT} a build \
                     can hold; ids below 0 are edits never uploaded to OSM"
                ))
            })?;
            let word = &self.chunk(chunk).words[word];
            // Most marks repeat a set bit; reading first keeps the cache
            // line shared between the threads marking it.
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

    /// How many ids are marked.
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

    /// Bytes the set holds: its table and the chunks marked so far.
    pub(crate) fn bytes(&self) -> u64 {
        self.reservation
            .lock()
            .expect("node set reservation")
            .bytes() as u64
    }

    /// Unmark every id from `low` up to, not including, `high`.
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

    /// Index ranks, once no id is marked or unmarked any more.
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

    /// The slot of a marked id, once ranked.
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

/// A node's coordinates in the PBF's fixed point.
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
fn degrees(value: i32) -> f64 {
    1e-9 * (i64::from(value) * 100) as f64
}

/// Where the coordinates live.
#[derive(Debug)]
pub(crate) enum Slots {
    Memory {
        locations: Vec<Location>,
        _reservation: Reservation,
    },
    Mapped(MappedSlots),
}

/// Coordinates in a scratch file, mapped into memory.
#[derive(Debug)]
pub(crate) struct MappedSlots {
    map: Option<MmapMut>,
    file: Option<File>,
    path: PathBuf,
    len: usize,
    /// The ledger's hold on the file's bytes, returned when it is removed.
    scratch: Arc<Scratch>,
    owner: Arc<Owner>,
    bytes: u64,
}

impl MappedSlots {
    fn create(scratch: &Arc<Scratch>, capacity: u64) -> Result<Self> {
        let bytes = capacity * LOCATION_BYTES;
        let owner = scratch.owner("node-locations");
        scratch.reserve(&owner, bytes)?;
        let path = scratch.path().join("node-locations.bin");
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
        if fs::remove_file(&self.path).is_ok() {
            self.scratch.release(&self.owner, self.bytes);
        }
    }
}

impl Slots {
    /// Room for `capacity` coordinates: in memory when `in_memory` and the
    /// budget grants it, else in a scratch file.
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

/// The coordinates of every needed node the input has, and the address tags
/// of the few whose tags a way reads.
#[derive(Debug)]
pub(crate) struct Locations {
    set: NodeSet,
    slots: Slots,
    tags: Vec<(i64, BTreeMap<String, String>)>,
}

/// What the node pass stored and read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct LoadStats {
    /// Ids marked before the pass.
    pub needed: u64,
    /// Needed nodes the input has.
    pub stored: u64,
    pub set_bytes: u64,
    pub location_bytes: u64,
    /// Whether the coordinates lie in a scratch file.
    pub mapped: bool,
    pub tagged: u64,
    pub read_bytes: u64,
}

impl Locations {
    pub(crate) fn get(&self, id: i64) -> Option<Location> {
        self.set.slot(id).map(|slot| self.slots.get(slot))
    }

    /// The address tags of a node whose tags were asked for, when it has any.
    pub(crate) fn tags(&self, id: i64) -> Option<&BTreeMap<String, String>> {
        self.tags
            .binary_search_by_key(&id, |(node, _)| *node)
            .ok()
            .map(|index| &self.tags[index].1)
    }
}

/// Nodes of one PBF block the set marks, in file order.
#[derive(Debug, Default)]
struct Found {
    nodes: Vec<(i64, Location)>,
    tags: Vec<(i64, BTreeMap<String, String>)>,
    first: Option<i64>,
    last: Option<i64>,
    /// Two ids out of order within the block.
    unsorted: Option<(i64, i64)>,
}

fn find(block: &PrimitiveBlock, set: &NodeSet, tag_ids: &[i64]) -> Found {
    let mut found = Found::default();
    let mut visit =
        |id: i64, lat_e7: i32, lon_e7: i32, tags: &dyn Fn() -> BTreeMap<String, String>| {
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
            ),
            Element::Node(node) => visit(
                node.id(),
                node.decimicro_lat(),
                node.decimicro_lon(),
                &|| addr_tags(|| node.tags()),
            ),
            Element::Way(_) | Element::Relation(_) => {}
        }
    }
    found
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

/// Read the node blocks of `input` and keep in `slots`, which has room for
/// every id `set` marks, the coordinates of the nodes `set` marks, and the
/// address tags of those of them `tag_ids` (sorted) names.
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
    let mut last: Option<i64> = None;
    // Marked ids the input lacks are unmarked as the pass goes, so that
    // every marked id below a stored node has a slot, and ranks stay dense.
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
                // A stored id is marked, so it lies in 0..ID_LIMIT.
                let id = id as u64;
                set.clear(cleared_to, id);
                cleared_to = id + 1;
                slots.push(location)?;
            }
            tags.extend(found.tags);
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
        read_bytes,
    };
    Ok((Locations { set, slots, tags }, stats))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let dir = std::env::temp_dir().join(format!("og-locations-{}", uuid::Uuid::new_v4()));
        let scratch = Scratch::create(&dir).expect("scratch");
        // A budget of nothing sends the coordinates to the file.
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
        let path = scratch.path().join("node-locations.bin");
        assert!(path.exists());
        drop(slots);
        assert!(!path.exists());
    }

    /// Not a test of behavior: prints what the store costs per needed node
    /// and per id, and how fast it marks and finds, on synthetic ids shaped
    /// like a large extract's (runs of nearby ids spread over the whole id
    /// space) and a city's (runs within a few ranges).
    #[test]
    #[ignore = "micro-benchmark; run with --ignored --nocapture"]
    fn costs_per_node_and_per_id() {
        for (name, needed, span, run) in [
            ("spread", 20_000_000i64, 14_300_000_000i64, 20),
            ("city", 1_000_000, 200_000_000, 200),
        ] {
            let mut ids = Vec::with_capacity(needed as usize);
            let mut state = 0x9e37_79b9_7f4a_7c15u64;
            while (ids.len() as i64) < needed {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let start = (state % span as u64) as i64;
                ids.extend((0..run).map(|step| start + step * 3));
            }
            let memory = Budget::unlimited();
            let started = std::time::Instant::now();
            let set = NodeSet::new(&memory);
            ids.par_chunks(4096)
                .for_each(|chunk| set.mark(chunk).expect("mark"));
            let marked = started.elapsed();
            let mut set = set;
            let distinct = set.len();
            set.rank();
            let started = std::time::Instant::now();
            let found = ids.par_iter().filter(|id| set.slot(**id).is_some()).count();
            let looked_up = started.elapsed();
            assert_eq!(found, ids.len());
            let bytes = set.bytes();
            println!(
                "{name}: {distinct} needed nodes over {span} ids: set {} MiB = {:.3} B per id \
                 spanned, {:.2} B per needed node plus {LOCATION_BYTES} B of coordinates; \
                 marked {:.1} M ids/s, found {:.1} M ids/s on {} threads",
                bytes >> 20,
                bytes as f64 / span as f64,
                bytes as f64 / distinct as f64,
                ids.len() as f64 / marked.as_secs_f64() / 1e6,
                ids.len() as f64 / looked_up.as_secs_f64() / 1e6,
                rayon::current_num_threads(),
            );
        }
    }
}
