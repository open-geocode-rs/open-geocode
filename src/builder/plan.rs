//! Sizing a build to the machine, before it starts and again once its input
//! is counted.
//!
//! A build is planned twice. [`plan_build`] runs before any work, knowing
//! only the machine ([`Resources`]) and the inputs' sizes: it keeps every
//! CPU, sizes the memory budget from the memory available less the heap the
//! build holds outside it, and checks memory and disk against item counts
//! estimated from the input's size. [`BuildPlan::refine`] runs again after
//! each input's scan, when the items are counted: how many node coordinates
//! the ways need, how many records, addresses and streets the build will
//! spill, how many boundary vertices it holds. It decides whether the node
//! coordinates stay in memory and checks the rest of the build against the
//! scratch disk. Both stop the build, unless forced, when it does not fit,
//! naming each shortfall and how to fix it, like Planetiler's resource
//! check; within 10% of a limit they warn.
//!
//! Every cost is per item, not per PBF byte: a PBF byte of Germany holds four
//! times the node references of a byte of Canada, so no ratio of input bytes
//! fits both. Only the estimate before the scan reads counts from bytes, at
//! the densest input measured, so it errs towards needing more.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{
    builder::osm::{
        BOUNDARY_SHAPE_BYTES_PER_COORD, NODE_SET_CHUNK_BYTES, NODE_SET_TABLE_BYTES, NODE_SLOT_BYTES,
    },
    error::{Error, Result},
    memory::{MAX_DERIVED_BUDGET_BYTES, MIN_BUDGET_BYTES},
    resources::{self, Resources},
};

// Item densities of Germany, the densest input measured, from its build
// report (2026-10): 4,861,056,214 B of PBF.
const GERMANY_INPUT_BYTES: f64 = 4_861_056_214.0;
/// Node references of kept ways and boundary members; distinct needed nodes
/// are fewer, so this bounds them.
const GERMANY_NODE_REFS: f64 = 163_300_658.0;
const GERMANY_KEPT_WAYS: f64 = 21_178_798.0;
const GERMANY_RECORDS: f64 = 26_131_991.0;
const GERMANY_ADDRESSES: f64 = 19_719_186.0;
const GERMANY_STREETS: f64 = 4_103_850.0;
/// Boundary vertices per input byte. Never measured: the spill study's
/// upper estimate of the planet's boundary heap, 15 GB, at
/// 64 B a vertex over 95.4 GB of PBF. The refined plan
/// counts them.
const BOUNDARY_REFS_PER_INPUT_BYTE: f64 = 0.0025;
/// Node ids OSM had handed out by October 2026 (planet statistics): the
/// bitmap of a large input spans them all.
const NODE_ID_SPACE: u64 = 14_300_000_000;

/// Scratch bytes a record takes in the records sorter: the spill study
/// estimates 100-200 B encoded; the top of it, since a record carries its
/// names and tags.
const RECORD_SPILL_BYTES: f64 = 200.0;
/// Scratch bytes an address takes in the address sorter: its street order
/// key, which holds what it states, with its point and source. New
/// Zealand's spill measured 146 B with a copy of the record beside the key;
/// the key alone is 87.7 B on a million rows shaped like OpenAddresses' and
/// 70.5 B on the test Packs' addresses.
const ADDRESS_SPILL_BYTES: f64 = 88.0;
/// Scratch bytes an address takes in the address book, which keeps it in
/// street order until its record is written: 32.8 B on the same million
/// rows, each key written as what it adds to the one before.
const ADDRESS_BOOK_BYTES: f64 = 33.0;
/// Scratch bytes an address takes in the records sorter, which carries its
/// place in the address book: 19.5 B on the same rows.
const PLACED_ADDRESS_BYTES: f64 = 20.0;
/// Scratch bytes a street way takes in the street sorter: the spill study
/// estimates 120-200 B.
const STREET_SPILL_BYTES: f64 = 200.0;
/// Scratch bytes an address takes in the house sorter while the imported
/// rows that repeat a house are found: a hash of the house, its point and
/// hashes of what tells two houses apart, 49.6 B on the same rows.
const HOUSE_SPILL_BYTES: f64 = 50.0;
/// Pack bytes per record: Germany's Pack was 2,666,692,193 B for 26,131,991
/// records. The Writer's scratch holds about as much again until the seal.
const PACK_BYTES_PER_RECORD: f64 = 102.0;
/// Pack bytes an imported address adds at most: an anchor of a run, its house
/// number term and its share of a block box. Most continue a run, at about 9 B.
const PACK_BYTES_PER_IMPORTED_ROW: f64 = 25.0;

/// Heap one worker thread holds outside the budget at most: a batch of PBF
/// blobs decoded, and what they emit.
const WORKER_BYTES: u64 = 64 << 20;
/// Heap a build holds outside the budget besides its workers' and its
/// boundaries': maps of member ways, relation stubs, the training sample.
/// Germany held 1.6 GB beyond its tracked peak with 24 workers, which take
/// 1.5 GB of it; this doubles the rest.
const BASE_UNTRACKED_BYTES: u64 = 256 << 20;
/// Heap a boundary vertex holds: only its shape's, inside the memory budget.
/// The ways' vertices stay on disk until the boundary stage reads them back
/// by way id.
const BOUNDARY_VERTEX_BYTES: u64 = BOUNDARY_SHAPE_BYTES_PER_COORD as u64;
/// Share of the memory left beside the heap outside the budget that the
/// derived budget takes. The rest is page cache: the input the passes
/// reread, and the node coordinates when they are mapped from scratch.
const BUDGET_PERCENT: u64 = 75;
/// Within this share of a limit, the check warns.
const WARN_SHARE: f64 = 0.1;

/// What a build holds and spills, counted after the scan or estimated from
/// the input's size before it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ItemCounts {
    /// Distinct nodes the ways need: one coordinate slot each.
    pub needed_nodes: u64,
    /// Bytes of the bitmap that marks them and its rank index.
    pub node_set_bytes: u64,
    pub kept_ways: u64,
    /// Records from OSM, addresses and streets included.
    pub records: u64,
    pub addresses: u64,
    pub streets: u64,
    /// Vertices of boundary ways and boundary relation members.
    pub boundary_refs: u64,
    /// Addresses from outside OSM; 0 when not known before they are read.
    pub imported_rows: u64,
}

impl ItemCounts {
    /// The counts of an input of `input_bytes` at Germany's density.
    fn estimate(input_bytes: u64, imported_rows: u64) -> Self {
        let at = |germany: f64| (input_bytes as f64 * germany / GERMANY_INPUT_BYTES) as u64;
        let needed_nodes = at(GERMANY_NODE_REFS);
        Self {
            needed_nodes,
            node_set_bytes: node_set_bytes(needed_nodes),
            kept_ways: at(GERMANY_KEPT_WAYS),
            records: at(GERMANY_RECORDS),
            addresses: at(GERMANY_ADDRESSES),
            streets: at(GERMANY_STREETS),
            boundary_refs: (input_bytes as f64 * BOUNDARY_REFS_PER_INPUT_BYTE) as u64,
            imported_rows,
        }
    }

    /// These counts with the OSM ones of the inputs not yet read
    /// extrapolated from the share read: `read_bytes` of `input_bytes`. The
    /// node store is per input, so its counts are this input's.
    fn extrapolated(&self, read_bytes: u64, input_bytes: u64) -> Self {
        if read_bytes == 0 || read_bytes >= input_bytes {
            return self.clone();
        }
        let scale = |count: u64| (count as f64 * input_bytes as f64 / read_bytes as f64) as u64;
        Self {
            records: scale(self.records),
            addresses: scale(self.addresses),
            streets: scale(self.streets),
            kept_ways: scale(self.kept_ways),
            boundary_refs: scale(self.boundary_refs),
            ..self.clone()
        }
    }
}

/// Bytes of a node set holding `needed` nodes: a chunk per 65,536 ids, at
/// most one per needed node, never more than the id space has.
fn node_set_bytes(needed: u64) -> u64 {
    let chunks = needed.min(NODE_ID_SPACE.div_ceil(65_536));
    NODE_SET_TABLE_BYTES + chunks * NODE_SET_CHUNK_BYTES
}

/// The sizes a build runs with and what the checks found.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct BuildPlan {
    pub resources: Resources,
    /// The memory budget: every buffer the build tracks, the node store and
    /// the sorters included.
    pub memory_budget_bytes: u64,
    /// Whether the budget was derived from the memory available, not set.
    pub memory_budget_derived: bool,
    /// The most one sorter may hold. Sorting and indexing in buffers far
    /// larger than the CPU caches cost more than the spill they save: Germany
    /// with a 6.2 GiB budget scanned, joined and wrote records slower than
    /// with 1 GiB. The rest of the budget serves the node store.
    pub sorter_bytes: u64,
    /// Worker threads: every usable CPU.
    pub threads: usize,
    /// The items the needs below are sized for.
    pub items: ItemCounts,
    /// Whether `items` were counted by a scan, not estimated from the
    /// inputs' size.
    pub counted: bool,
    pub input_bytes: u64,
    /// Heap the build is expected to hold outside its budget at its peak.
    pub untracked_bytes: u64,
    /// Heap outside the budget measured after the last scan; 0 when the
    /// allocator does not count.
    pub measured_untracked_bytes: u64,
    /// Whether the node coordinates fit in memory; else they are mapped from
    /// a scratch file.
    pub locations_in_memory: bool,
    pub memory_needed_bytes: u64,
    /// What the memory budget must hold beside the sorters' least: the node
    /// set and the boundary shapes.
    #[serde(default)]
    pub budget_needed_bytes: u64,
    /// Memory the build can hold: what was available before it started, or
    /// after the last scan, with what the build held then.
    pub memory_free_bytes: u64,
    /// The most scratch held at once, over every stage.
    pub scratch_disk_needed_bytes: u64,
    /// The most scratch and Pack held at once, when both share a volume.
    pub shared_disk_needed_bytes: u64,
    pub scratch_disk_free_bytes: Option<u64>,
    /// The scratch the build was given, when it was given a cap.
    pub scratch_budget_bytes: Option<u64>,
    pub output_disk_needed_bytes: u64,
    pub output_disk_free_bytes: Option<u64>,
    pub scratch_dir: PathBuf,
    pub output_dir: PathBuf,
    /// Needs within 10% of what is free.
    pub warnings: Vec<String>,
    /// Needs past what is free, each with how to fix it. A plan has any only
    /// when forced.
    pub shortfalls: Vec<String>,
    /// Whether the build was told to run despite a shortfall.
    pub forced: bool,
    /// Scratch a stopped build left that this one goes on with: counted as
    /// free, since the build's need includes it.
    #[serde(default)]
    pub held_scratch_bytes: u64,
}

/// What a build looks like after an input's scan: the items counted so far,
/// and the heap it holds.
#[derive(Debug, Clone, Default)]
pub(crate) struct Scanned {
    pub items: ItemCounts,
    /// Input bytes whose items `items` counts.
    pub read_bytes: u64,
    /// Bytes the heap holds; `None` when the allocator does not count.
    pub heap_bytes: Option<u64>,
    /// Bytes the budget's buffers hold.
    pub tracked_bytes: u64,
    /// What the process can use now.
    pub resources: Resources,
}

impl BuildPlan {
    /// Size the rest of the build from what the scan of an input counted, as
    /// [`plan_build`] sized all of it from the input's size. Fails, unless
    /// forced, when the rest does not fit.
    pub(crate) fn refine(&mut self, scanned: &Scanned) -> Result<()> {
        self.items = scanned
            .items
            .extrapolated(scanned.read_bytes, self.input_bytes);
        self.counted = true;
        self.measured_untracked_bytes = scanned
            .heap_bytes
            .map_or(0, |heap| heap.saturating_sub(scanned.tracked_bytes));
        let boundary = self.items.boundary_refs * BOUNDARY_VERTEX_BYTES;
        self.untracked_bytes = self
            .measured_untracked_bytes
            .max(untracked_beside_boundaries(self.threads));
        // The coordinates stay in memory when the budget holds them beside
        // what it holds now, and what the machine has free holds them beside
        // the boundary vertices the way pass is about to gather.
        let locations = self.items.needed_nodes * NODE_SLOT_BYTES;
        let available = scanned.resources.available_memory_bytes;
        self.locations_in_memory = scanned.tracked_bytes + locations <= self.memory_budget_bytes
            && locations + boundary <= available / 10 * 9;
        self.memory_free_bytes = available + scanned.heap_bytes.unwrap_or(scanned.tracked_bytes);
        self.size();
        self.check()
    }

    /// Set the needs from `items`.
    fn size(&mut self) {
        let items = &self.items;
        // The least budget the build runs on, or the one it was given.
        let least_budget = if self.memory_budget_derived {
            MIN_BUDGET_BYTES as u64
        } else {
            self.memory_budget_bytes
        };
        // The node set and the boundary shapes are held inside the budget,
        // beside the least the sorters need, so the budget is all the build
        // holds beside the heap no budget tracks.
        self.memory_needed_bytes = least_budget + self.untracked_bytes;
        self.budget_needed_bytes = items.node_set_bytes
            + items.boundary_refs * BOUNDARY_VERTEX_BYTES
            + MIN_BUDGET_BYTES as u64;
        let records = (items.records + items.imported_rows) as f64;
        let addresses = (items.addresses + items.imported_rows) as f64;
        let streets = items.streets as f64;
        let pack = items.records as f64 * PACK_BYTES_PER_RECORD
            + items.imported_rows as f64 * PACK_BYTES_PER_IMPORTED_ROW;
        // Coordinates held in memory are saved at the node pass's
        // checkpoint, so they are on disk through the way pass either way.
        let locations = (items.needed_nodes * NODE_SLOT_BYTES) as f64;
        // Scratch held at the end of each stage that peaks: the way pass
        // (the records other than addresses and streets, the OSM addresses,
        // the streets, mapped coordinates), address placement (every other
        // record, beside the street order's runs, the address book written
        // from them and the addresses placed, then with rows imported the
        // book and the houses), the record write (the records, the book and
        // the Writer's scratch) and the seal (the Writer's scratch, and the
        // Pack it is copied into).
        let ways = (items.records as f64 - items.addresses as f64 - streets).max(0.0)
            * RECORD_SPILL_BYTES
            + items.addresses as f64 * ADDRESS_SPILL_BYTES
            + streets * STREET_SPILL_BYTES
            + locations;
        let others = (records - addresses).max(0.0) * RECORD_SPILL_BYTES;
        let book = addresses * ADDRESS_BOOK_BYTES;
        let houses = match items.imported_rows {
            0 => 0.0,
            _ => book + addresses * HOUSE_SPILL_BYTES,
        };
        let placement =
            others + (addresses * (ADDRESS_SPILL_BYTES + PLACED_ADDRESS_BYTES) + book).max(houses);
        let write = others + addresses * PLACED_ADDRESS_BYTES + book + pack;
        let scratch = ways.max(placement).max(write);
        self.scratch_disk_needed_bytes = scratch as u64;
        self.shared_disk_needed_bytes = scratch.max(2.0 * pack) as u64;
        self.output_disk_needed_bytes = pack as u64;
    }

    /// Check the needs against what is free, failing unless forced.
    fn check(&mut self) -> Result<()> {
        self.warnings.clear();
        let held = self.held_scratch_bytes;
        self.scratch_disk_free_bytes =
            resources::free_disk(&self.scratch_dir).map(|free| free + held);
        self.output_disk_free_bytes = resources::free_disk(&self.output_dir).map(|free| {
            if resources::same_volume(&self.scratch_dir, &self.output_dir) {
                free + held
            } else {
                free
            }
        });
        let counted = if self.counted {
            "counted"
        } else {
            "estimated from the input's size"
        };
        let scratch_free = match (self.scratch_disk_free_bytes, self.scratch_budget_bytes) {
            (Some(free), Some(budget)) => Some(free.min(budget)),
            (free, budget) => free.or(budget),
        };
        let scratch_fix = if self.scratch_budget_bytes.is_some() {
            "free space there, raise --scratch-budget-mb, or pass --scratch-dir on another volume"
        } else {
            "free space there or pass --scratch-dir on another volume"
        };
        let mut needs = vec![(
            format!("memory ({counted})"),
            self.memory_needed_bytes,
            Some(self.memory_free_bytes),
            "close other programs, pass a smaller --memory-budget-mb, or build a smaller extract",
        )];
        if resources::same_volume(&self.scratch_dir, &self.output_dir) {
            needs.push((
                format!(
                    "disk for scratch and Pack at {} ({counted})",
                    self.scratch_dir.display()
                ),
                self.shared_disk_needed_bytes,
                scratch_free,
                scratch_fix,
            ));
        } else {
            needs.push((
                format!("scratch disk at {} ({counted})", self.scratch_dir.display()),
                self.scratch_disk_needed_bytes,
                scratch_free,
                scratch_fix,
            ));
            needs.push((
                format!("Pack disk at {} ({counted})", self.output_dir.display()),
                self.output_disk_needed_bytes,
                self.output_disk_free_bytes,
                "free space there",
            ));
        }
        // The node set and the boundary shapes live inside the budget. Before
        // a scan their size is a guess from the input's size, so it warns;
        // counted, a budget that cannot hold them stops the build.
        let budget_need = (
            format!("memory budget for the node set and boundary shapes ({counted})"),
            self.budget_needed_bytes,
            Some(self.memory_budget_bytes),
            "raise --memory-budget-mb",
        );
        if self.counted {
            needs.push(budget_need);
        } else if budget_need.1 > self.memory_budget_bytes {
            self.warnings.push(format!(
                "{}: needs about {} MiB of a {} MiB budget; counted after the first scan",
                budget_need.0,
                budget_need.1 >> 20,
                self.memory_budget_bytes >> 20
            ));
        }
        let mut shortfalls = Vec::new();
        for (what, needed, free, fix) in needs {
            let Some(free) = free else {
                continue;
            };
            if needed > free {
                shortfalls.push(format!(
                    "{what}: needs about {} MiB, {} MiB free; {fix}",
                    needed >> 20,
                    free >> 20
                ));
            } else if needed as f64 > free as f64 * (1.0 - WARN_SHARE) {
                self.warnings.push(format!(
                    "{what}: needs about {} MiB of the {} MiB free",
                    needed >> 20,
                    free >> 20
                ));
            }
        }
        if !shortfalls.is_empty() && !self.forced {
            return Err(Error::OutOfResources(format!(
                "insufficient resources for this build, pass --force to build anyway:\n  {}",
                shortfalls.join("\n  ")
            )));
        }
        self.shortfalls = shortfalls;
        Ok(())
    }
}

fn untracked_beside_boundaries(threads: usize) -> u64 {
    BASE_UNTRACKED_BYTES + threads as u64 * WORKER_BYTES
}

/// The budget of a build on `resources` with `threads` workers: a share of
/// the memory left beside the heap it holds outside the budget, at least
/// [`MIN_BUDGET_BYTES`]. Boundary heap is not set aside: before the scan it
/// is a guess, and the budget's heap ceiling makes tracked buffers spill as
/// it grows.
fn derived_budget(resources: &Resources, threads: usize) -> u64 {
    let usable = resources
        .available_memory_bytes
        .saturating_sub(untracked_beside_boundaries(threads));
    u64::try_from(u128::from(usable) * u128::from(BUDGET_PERCENT) / 100)
        .unwrap_or(u64::MAX)
        .max(MIN_BUDGET_BYTES as u64)
}

/// The plan before any work, on `resources`.
fn preflight(
    resources: Resources,
    input_bytes: u64,
    imported_rows: u64,
    budget_override: Option<usize>,
) -> BuildPlan {
    let threads = resources.usable_cpus.max(1);
    let memory_budget_bytes = budget_override.map_or_else(
        || derived_budget(&resources, threads),
        |budget| budget as u64,
    );
    let items = ItemCounts::estimate(input_bytes, imported_rows);
    let mut plan = BuildPlan {
        memory_budget_bytes,
        memory_budget_derived: budget_override.is_none(),
        sorter_bytes: memory_budget_bytes.min(MAX_DERIVED_BUDGET_BYTES as u64),
        threads,
        locations_in_memory: items.node_set_bytes + items.needed_nodes * NODE_SLOT_BYTES
            <= memory_budget_bytes,
        memory_free_bytes: resources.available_memory_bytes,
        untracked_bytes: untracked_beside_boundaries(threads),
        items,
        input_bytes,
        resources,
        ..BuildPlan::default()
    };
    plan.size();
    plan
}

/// Plan a build of `input_bytes` of PBF and `imported_rows` addresses from
/// outside OSM (0 when not known) that spills to `scratch`, at most
/// `scratch_budget` bytes when given, and writes its Pack under `output`,
/// with the budget `budget_override` when one is given. Fails, unless
/// `force`, when memory or disk falls short of the estimate.
pub fn plan_build(
    input_bytes: u64,
    imported_rows: u64,
    scratch: &Path,
    output: &Path,
    budget_override: Option<usize>,
    scratch_budget: Option<u64>,
    force: bool,
) -> Result<BuildPlan> {
    plan_build_holding(
        input_bytes,
        imported_rows,
        scratch,
        output,
        budget_override,
        scratch_budget,
        force,
        0,
    )
}

/// [`plan_build`] for a build that goes on with `held` bytes of scratch a
/// stopped one left.
#[allow(clippy::too_many_arguments)]
pub(crate) fn plan_build_holding(
    input_bytes: u64,
    imported_rows: u64,
    scratch: &Path,
    output: &Path,
    budget_override: Option<usize>,
    scratch_budget: Option<u64>,
    force: bool,
    held: u64,
) -> Result<BuildPlan> {
    let mut plan = BuildPlan {
        scratch_budget_bytes: scratch_budget,
        scratch_dir: scratch.to_path_buf(),
        output_dir: output.to_path_buf(),
        forced: force,
        held_scratch_bytes: held,
        ..preflight(
            Resources::probe(),
            input_bytes,
            imported_rows,
            budget_override,
        )
    };
    plan.check()?;
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine(available_gib: u64) -> Resources {
        Resources {
            usable_cpus: 24,
            total_memory_bytes: 32 << 30,
            available_memory_bytes: available_gib << 30,
            ..Resources::default()
        }
    }

    #[test]
    fn a_build_that_cannot_fit_stops_unless_forced() {
        let scratch = std::env::temp_dir();
        let error = plan_build(u64::MAX / 8, 0, &scratch, &scratch, None, None, false).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("--force"), "{message}");
        assert!(message.contains("disk"), "{message}");
        let plan =
            plan_build(u64::MAX / 8, 0, &scratch, &scratch, None, None, true).expect("forced");
        assert!(plan.forced);
        assert_eq!(plan.shortfalls.len(), 1, "{:?}", plan.shortfalls);
        // Before a scan, what the budget must hold is a guess from the
        // input's size: it warns, and only a count stops the build.
        assert!(
            plan.warnings
                .iter()
                .any(|warning| warning.contains("node set and boundary shapes")),
            "{:?}",
            plan.warnings
        );
    }

    #[test]
    fn a_scratch_budget_caps_the_disk_a_build_may_use() {
        let scratch = std::env::temp_dir();
        let error =
            plan_build(1 << 30, 0, &scratch, &scratch, None, Some(1 << 20), false).unwrap_err();
        assert!(
            format!("{error:#}").contains("--scratch-budget-mb"),
            "{error:#}"
        );
    }

    #[test]
    fn a_small_build_gets_a_derived_budget_and_every_cpu() {
        let scratch = std::env::temp_dir();
        let plan = plan_build(1 << 20, 0, &scratch, &scratch, None, None, false).expect("plan");
        assert!(plan.memory_budget_derived);
        assert!(plan.memory_budget_bytes >= MIN_BUDGET_BYTES as u64);
        assert_eq!(plan.threads, plan.resources.usable_cpus.max(1));
        assert!(plan.sorter_bytes <= MAX_DERIVED_BUDGET_BYTES as u64);
        let set =
            plan_build(1 << 20, 0, &scratch, &scratch, Some(64 << 20), None, false).expect("plan");
        assert_eq!(set.memory_budget_bytes, 64 << 20);
        assert_eq!(set.sorter_bytes, 64 << 20);
        assert!(!set.memory_budget_derived);
    }

    #[test]
    fn the_budget_is_what_the_machine_leaves_and_never_costs_a_worker() {
        // 28 GiB available on 24 CPUs: 1.75 GiB for the workers and the
        // base heap, three quarters of the rest for the budget.
        assert_eq!(
            derived_budget(&machine(28), 24),
            ((28u64 << 30) - (1792 << 20)) / 4 * 3
        );
        let small = preflight(machine(1), 1 << 30, 0, None);
        assert_eq!(small.memory_budget_bytes, MIN_BUDGET_BYTES as u64);
        assert_eq!(small.threads, 24);
    }

    #[test]
    fn the_planet_fits_this_machine_before_and_after_its_scan() {
        // Planet 2026-10: 95.4 GB of PBF; 32 GB of RAM, about 28 GiB
        // available; 233 GB of scratch on the Pack's volume.
        let disk = 233_000_000_000;
        let planet = preflight(machine(28), 95_380_942_829, 0, None);
        assert_eq!(planet.threads, 24);
        assert!(planet.memory_budget_bytes > 16 << 30);
        // Germany's density reads 3.2 billion references, against the
        // planet's 1.75 billion, so the estimate errs high and still fits.
        assert!(planet.items.needed_nodes > 3_000_000_000);
        assert!(!planet.locations_in_memory);
        assert!(
            planet.shared_disk_needed_bytes < disk / 10 * 9,
            "{} GiB",
            planet.shared_disk_needed_bytes >> 30
        );
        assert!(
            planet.memory_needed_bytes < 28 << 30,
            "{} GiB",
            planet.memory_needed_bytes >> 30
        );

        // After the scan, at the spill study's planet counts.
        let mut counted = planet.clone();
        counted.items = ItemCounts {
            needed_nodes: 1_600_000_000,
            node_set_bytes: node_set_bytes(1_600_000_000),
            kept_ways: 165_000_000,
            records: 280_000_000,
            addresses: 190_000_000,
            streets: 56_400_000,
            boundary_refs: 150_000_000,
            imported_rows: 0,
        };
        counted.locations_in_memory = true;
        counted.size();
        assert!(
            counted.shared_disk_needed_bytes < 100_000_000_000,
            "{} GiB",
            counted.shared_disk_needed_bytes >> 30
        );
        assert!(counted.memory_needed_bytes < 28 << 30);

        // And with OpenAddresses' 640 million rows, each sort carrying a
        // compact key or a place in the address book, not the address.
        counted.items.imported_rows = 640_000_000;
        counted.size();
        assert!(
            counted.shared_disk_needed_bytes < disk / 10 * 7,
            "{} GiB",
            counted.shared_disk_needed_bytes >> 30
        );
    }

    #[test]
    fn coordinates_count_as_scratch_in_memory_or_mapped() {
        let mut plan = preflight(machine(28), 200_000_000, 0, None);
        plan.items.needed_nodes = 0;
        plan.size();
        let none = plan.scratch_disk_needed_bytes;
        plan.items.needed_nodes = 1_000_000_000;
        plan.locations_in_memory = true;
        plan.size();
        let in_memory = plan.scratch_disk_needed_bytes;
        plan.locations_in_memory = false;
        plan.size();
        let mapped = plan.scratch_disk_needed_bytes;
        assert_eq!(mapped, in_memory);
        assert!(mapped > none, "{mapped} > {none}");
    }

    #[test]
    fn a_scan_that_counts_more_than_the_scratch_holds_stops_the_build() {
        let dir = std::env::temp_dir();
        let mut plan = BuildPlan {
            scratch_dir: dir.clone(),
            output_dir: dir,
            scratch_budget_bytes: Some(1 << 30),
            ..preflight(machine(28), 1 << 20, 0, None)
        };
        plan.check().expect("a small input fits");
        let scanned = Scanned {
            items: ItemCounts {
                needed_nodes: 1_000,
                records: 100_000_000,
                addresses: 50_000_000,
                ..ItemCounts::default()
            },
            read_bytes: 1 << 20,
            heap_bytes: Some(1 << 30),
            tracked_bytes: 1 << 20,
            resources: machine(28),
        };
        let error = plan.clone().refine(&scanned).unwrap_err();
        assert!(format!("{error:#}").contains("(counted)"), "{error:#}");
        plan.forced = true;
        plan.refine(&scanned).expect("forced");
        assert!(plan.counted && plan.locations_in_memory);
        assert_eq!(plan.shortfalls.len(), 1, "{:?}", plan.shortfalls);
        assert_eq!(plan.memory_free_bytes, (28 << 30) + (1 << 30));
    }

    #[test]
    fn counts_of_inputs_not_yet_read_are_extrapolated() {
        let read = ItemCounts {
            needed_nodes: 10,
            records: 100,
            addresses: 50,
            ..ItemCounts::default()
        };
        let all = read.extrapolated(1_000, 3_000);
        assert_eq!((all.records, all.addresses), (300, 150));
        assert_eq!(all.needed_nodes, 10, "the node store is per input");
    }
}
