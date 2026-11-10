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
//! fits both. Only the estimate before the scan reads counts from bytes: at
//! the densest input measured, so it errs towards needing more, but never
//! past what the planet holds.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{
    builder::osm::{
        MIN_BOUNDARY_BATCH_BYTES, NODE_SET_CHUNK_BYTES, NODE_SET_TABLE_BYTES, NODE_SLOT_BYTES,
    },
    error::{Error, Result},
    memory::{MAX_DERIVED_BUDGET_BYTES, MIN_BUDGET_BYTES},
    resources::{self, Resources},
};

const GERMANY_INPUT_BYTES: f64 = 4_861_056_214.0;
const GERMANY_NODE_REFS: f64 = 163_300_658.0;
const GERMANY_KEPT_WAYS: f64 = 21_178_798.0;
const GERMANY_RECORDS: f64 = 26_131_991.0;
const GERMANY_ADDRESSES: f64 = 19_719_186.0;
const GERMANY_STREETS: f64 = 4_103_850.0;
// The planet's counts (build report, 2026-10-26) cap the estimate, as no input holds more.
const PLANET_INPUT_BYTES: f64 = 95_631_115_419.0;
const PLANET_NEEDED_NODES: f64 = 1_646_327_702.0;
const PLANET_KEPT_WAYS: f64 = 156_619_174.0;
const PLANET_RECORDS: f64 = 269_486_139.0;
const PLANET_ADDRESSES: f64 = 173_821_696.0;
const PLANET_STREETS: f64 = 56_214_445.0;
/// Boundary vertices per input byte. Never measured: the spill study's
/// upper estimate of the planet's boundary heap, 15 GB, at
/// 64 B a vertex over 95.4 GB of PBF. The refined plan
/// counts them.
const BOUNDARY_REFS_PER_INPUT_BYTE: f64 = 0.0025;
const NODE_ID_SPACE: u64 = 14_300_000_000;

const RECORD_SPILL_BYTES: f64 = 200.0;
const ADDRESS_SPILL_BYTES: f64 = 88.0;
const ADDRESS_BOOK_BYTES: f64 = 33.0;
const PLACED_ADDRESS_BYTES: f64 = 20.0;
const STREET_SPILL_BYTES: f64 = 200.0;
const HOUSE_SPILL_BYTES: f64 = 50.0;
const PACK_BYTES_PER_RECORD: f64 = 102.0;
const PACK_BYTES_PER_IMPORTED_ROW: f64 = 25.0;

const WORKER_BYTES: u64 = 64 << 20;
const BASE_UNTRACKED_BYTES: u64 = 256 << 20;
const BOUNDARY_HEAP_BYTES_PER_REF: f64 = 4.0;
const BOUNDARY_SHAPE_BYTES_PER_REF: f64 = 30.0;
const BUDGET_PERCENT: u64 = 75;
const WARN_SHARE: f64 = 0.1;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ItemCounts {
    pub needed_nodes: u64,
    pub node_set_bytes: u64,
    pub kept_ways: u64,
    pub records: u64,
    pub addresses: u64,
    pub streets: u64,
    pub boundary_refs: u64,
    pub imported_rows: u64,
}

impl ItemCounts {
    fn estimate(input_bytes: u64, imported_rows: u64) -> Self {
        let bytes = input_bytes as f64;
        // A planet newer than the one counted is larger, at about the same density.
        let at = |germany: f64, planet: f64| {
            let dense = bytes * germany / GERMANY_INPUT_BYTES;
            let whole = planet * (bytes / PLANET_INPUT_BYTES).max(1.0);
            dense.min(whole) as u64
        };
        let needed_nodes = at(GERMANY_NODE_REFS, PLANET_NEEDED_NODES);
        Self {
            needed_nodes,
            node_set_bytes: node_set_bytes(needed_nodes),
            kept_ways: at(GERMANY_KEPT_WAYS, PLANET_KEPT_WAYS),
            records: at(GERMANY_RECORDS, PLANET_RECORDS),
            addresses: at(GERMANY_ADDRESSES, PLANET_ADDRESSES),
            streets: at(GERMANY_STREETS, PLANET_STREETS),
            boundary_refs: (input_bytes as f64 * BOUNDARY_REFS_PER_INPUT_BYTE) as u64,
            imported_rows,
        }
    }

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

fn node_set_bytes(needed: u64) -> u64 {
    let chunks = needed.min(NODE_ID_SPACE.div_ceil(65_536));
    NODE_SET_TABLE_BYTES + chunks * NODE_SET_CHUNK_BYTES
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct BuildPlan {
    pub resources: Resources,
    pub memory_budget_bytes: u64,
    pub memory_budget_derived: bool,
    pub sorter_bytes: u64,
    pub threads: usize,
    pub items: ItemCounts,
    pub counted: bool,
    pub input_bytes: u64,
    pub untracked_bytes: u64,
    pub measured_untracked_bytes: u64,
    pub locations_in_memory: bool,
    pub memory_needed_bytes: u64,
    #[serde(default)]
    pub budget_needed_bytes: u64,
    pub memory_free_bytes: u64,
    pub scratch_disk_needed_bytes: u64,
    pub shared_disk_needed_bytes: u64,
    pub scratch_disk_free_bytes: Option<u64>,
    pub scratch_budget_bytes: Option<u64>,
    pub output_disk_needed_bytes: u64,
    pub output_disk_free_bytes: Option<u64>,
    pub scratch_dir: PathBuf,
    pub output_dir: PathBuf,
    pub warnings: Vec<String>,
    pub shortfalls: Vec<String>,
    pub forced: bool,
    #[serde(default)]
    pub held_scratch_bytes: u64,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Scanned {
    pub items: ItemCounts,
    pub read_bytes: u64,
    pub heap_bytes: Option<u64>,
    pub tracked_bytes: u64,
    pub resources: Resources,
}

impl BuildPlan {
    pub(crate) fn refine(&mut self, scanned: &Scanned) -> Result<()> {
        self.items = scanned
            .items
            .extrapolated(scanned.read_bytes, self.input_bytes);
        self.counted = true;
        self.measured_untracked_bytes = scanned
            .heap_bytes
            .map_or(0, |heap| heap.saturating_sub(scanned.tracked_bytes));
        let boundary = boundary_heap_bytes(self.items.boundary_refs);
        self.untracked_bytes = self
            .measured_untracked_bytes
            .max(untracked_beside_boundaries(self.threads));
        let locations = self.items.needed_nodes * NODE_SLOT_BYTES;
        let available = scanned.resources.available_memory_bytes;
        self.locations_in_memory = scanned.tracked_bytes + locations <= self.memory_budget_bytes
            && locations + boundary <= available / 10 * 9;
        self.memory_free_bytes = available + scanned.heap_bytes.unwrap_or(scanned.tracked_bytes);
        self.size();
        self.check()
    }

    pub fn heap_limit_bytes(&self) -> u64 {
        self.memory_budget_bytes + untracked_beside_boundaries(self.threads)
    }

    /// Set the needs from `items`.
    fn size(&mut self) {
        let items = &self.items;
        let least_budget = if self.memory_budget_derived {
            MIN_BUDGET_BYTES as u64
        } else {
            self.memory_budget_bytes
        };
        self.memory_needed_bytes = least_budget + self.untracked_bytes;
        self.budget_needed_bytes = items.node_set_bytes
            + boundary_heap_bytes(items.boundary_refs)
            + MIN_BOUNDARY_BATCH_BYTES as u64
            + MIN_BUDGET_BYTES as u64;
        let records = (items.records + items.imported_rows) as f64;
        let addresses = (items.addresses + items.imported_rows) as f64;
        let streets = items.streets as f64;
        let pack = items.records as f64 * PACK_BYTES_PER_RECORD
            + items.imported_rows as f64 * PACK_BYTES_PER_IMPORTED_ROW;
        let locations = (items.needed_nodes * NODE_SLOT_BYTES) as f64;
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
        let shapes = items.boundary_refs as f64 * BOUNDARY_SHAPE_BYTES_PER_REF;
        let placement = shapes
            + others
            + (addresses * (ADDRESS_SPILL_BYTES + PLACED_ADDRESS_BYTES) + book).max(houses);
        let write = shapes + others + addresses * PLACED_ADDRESS_BYTES + book + pack;
        let scratch = ways.max(placement).max(write);
        self.scratch_disk_needed_bytes = scratch as u64;
        self.shared_disk_needed_bytes = scratch.max(2.0 * pack) as u64;
        self.output_disk_needed_bytes = pack as u64;
    }

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

fn boundary_heap_bytes(refs: u64) -> u64 {
    (refs as f64 * BOUNDARY_HEAP_BYTES_PER_REF) as u64
}

fn untracked_beside_boundaries(threads: usize) -> u64 {
    BASE_UNTRACKED_BYTES + threads as u64 * WORKER_BYTES
}

fn derived_budget(resources: &Resources, threads: usize) -> u64 {
    let usable = resources
        .available_memory_bytes
        .saturating_sub(untracked_beside_boundaries(threads));
    u64::try_from(u128::from(usable) * u128::from(BUDGET_PERCENT) / 100)
        .unwrap_or(u64::MAX)
        .max(MIN_BUDGET_BYTES as u64)
}

/// Worker threads the build's pool runs: as many as `RAYON_NUM_THREADS`
/// names, as the pool reads it, else every usable CPU.
fn worker_threads(resources: &Resources) -> usize {
    std::env::var("RAYON_NUM_THREADS")
        .ok()
        .and_then(|threads| threads.parse::<usize>().ok())
        .filter(|threads| *threads > 0)
        .unwrap_or(resources.usable_cpus)
        .max(1)
}

/// The plan before any work, on `resources`.
fn preflight(
    resources: Resources,
    threads: usize,
    input_bytes: u64,
    imported_rows: u64,
    budget_override: Option<usize>,
) -> BuildPlan {
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
    let resources = Resources::probe();
    let threads = worker_threads(&resources);
    let mut plan = BuildPlan {
        scratch_budget_bytes: scratch_budget,
        scratch_dir: scratch.to_path_buf(),
        output_dir: output.to_path_buf(),
        forced: force,
        held_scratch_bytes: held,
        ..preflight(
            resources,
            threads,
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
        assert_eq!(plan.threads, worker_threads(&plan.resources));
        assert!(plan.sorter_bytes <= MAX_DERIVED_BUDGET_BYTES as u64);
        let set =
            plan_build(1 << 20, 0, &scratch, &scratch, Some(64 << 20), None, false).expect("plan");
        assert_eq!(set.memory_budget_bytes, 64 << 20);
        assert_eq!(set.sorter_bytes, 64 << 20);
        assert!(!set.memory_budget_derived);
    }

    #[test]
    fn a_small_machine_keeps_the_least_budget_and_every_worker() {
        let small = preflight(machine(1), 24, 1 << 30, 0, None);
        assert_eq!(small.memory_budget_bytes, MIN_BUDGET_BYTES as u64);
        assert_eq!(small.threads, 24);
    }

    #[test]
    fn the_planet_fits_this_machine_before_and_after_its_scan() {
        let disk = 233_000_000_000;
        let planet = preflight(machine(28), 24, 95_380_942_829, 0, None);
        assert_eq!(planet.threads, 24);
        assert!(planet.memory_budget_bytes > 16 << 30);
        // The planet's counted build fit its 1.6 billion needed nodes in a 20 GiB budget.
        assert!(planet.locations_in_memory);
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

        let mut counted = planet;
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

        counted.items.imported_rows = 640_000_000;
        counted.size();
        assert!(
            counted.shared_disk_needed_bytes < disk / 10 * 7,
            "{} GiB",
            counted.shared_disk_needed_bytes >> 30
        );
    }

    #[test]
    fn the_planet_is_estimated_at_its_own_counts_not_germanys_density() {
        let planet = preflight(machine(28), 24, 95_631_115_419, 0, None);
        let items = &planet.items;
        assert_eq!(
            (
                items.needed_nodes,
                items.kept_ways,
                items.records,
                items.addresses,
                items.streets
            ),
            (
                1_646_327_702,
                156_619_174,
                269_486_139,
                173_821_696,
                56_214_445
            )
        );
        // Its build peaked at 65 GiB of scratch and Pack, so 80 GB free must pass the check.
        assert!(
            planet.shared_disk_needed_bytes < 80_000_000_000,
            "{} MiB",
            planet.shared_disk_needed_bytes >> 20
        );
        assert_eq!(planet.output_disk_needed_bytes, 27_487_586_178);
    }

    #[test]
    fn a_small_input_is_estimated_at_germanys_density() {
        let items = preflight(machine(28), 24, 1_000_000_000, 0, None).items;
        assert_eq!(
            (
                items.needed_nodes,
                items.kept_ways,
                items.records,
                items.addresses,
                items.streets
            ),
            (33_593_657, 4_356_830, 5_375_784, 4_056_564, 844_230)
        );
    }

    #[test]
    fn an_input_larger_than_the_planet_is_estimated_at_the_planets_density() {
        let items = preflight(machine(28), 24, 2 * 95_631_115_419, 0, None).items;
        assert_eq!(
            (
                items.needed_nodes,
                items.kept_ways,
                items.records,
                items.addresses,
                items.streets
            ),
            (
                2 * 1_646_327_702,
                2 * 156_619_174,
                2 * 269_486_139,
                2 * 173_821_696,
                2 * 56_214_445
            )
        );
    }

    #[test]
    fn coordinates_count_as_scratch_in_memory_or_mapped() {
        let mut plan = preflight(machine(28), 24, 200_000_000, 0, None);
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
            ..preflight(machine(28), 24, 1 << 20, 0, None)
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
