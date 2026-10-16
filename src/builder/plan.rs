//! Sizing a build to the machine before it starts.
//!
//! [`plan_build`] probes what the process can use ([`Resources`]), derives
//! the memory budget and the worker threads from it, and estimates from the
//! input's size the memory, scratch disk and output disk the build needs.
//! Like Planetiler's resource check, it stops before any work when one does
//! not fit, naming each shortfall and how to fix it, unless forced; within
//! 10% of a limit it warns. The estimates are upper bounds measured on
//! builds of Germany and Japan, not a model of every input.

use std::path::Path;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::{
    memory::{MIN_BUDGET_BYTES, derived_budget},
    resources::{self, Resources},
};

/// Scratch bytes a build writes per input byte, at most, less what its budget
/// holds in memory: Germany (4.9 GB of PBF) spilled 15.0 GB with a 1 GiB
/// budget and 9.8 GB with 6.2 GiB, Japan 1.5 GB of 2.6 GB with 1 GiB.
const SCRATCH_PER_INPUT: f64 = 3.2;
/// Pack bytes per input byte: Germany 0.55, Japan 0.25.
const PACK_PER_INPUT: f64 = 0.6;
/// Heap a build holds outside its budget per input byte: boundary
/// geometry, maps of ways and members, the training sample. Germany held
/// about 1.6 GB beyond its 0.9 GB tracked peak.
const UNTRACKED_PER_INPUT: f64 = 0.35;
/// Heap one worker thread holds outside the budget at most: a batch of PBF
/// blobs decoded, and what they emit.
const WORKER_BYTES: u64 = 64 << 20;
/// Heap a build holds outside its budget besides its workers'.
fn untracked_base(input_bytes: u64) -> u64 {
    (input_bytes as f64 * UNTRACKED_PER_INPUT) as u64
}
/// Within this share of a limit, the check warns.
const WARN_SHARE: f64 = 0.1;

/// The sizes a build runs with and what the pre-flight check found.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct BuildPlan {
    pub resources: Resources,
    pub memory_budget_bytes: u64,
    /// Whether the budget was derived from the memory available, not set.
    pub memory_budget_derived: bool,
    /// Worker threads: the usable CPUs, fewer when the memory left beside
    /// the budget cannot hold a worker's buffers for each.
    pub threads: usize,
    pub memory_needed_bytes: u64,
    pub scratch_disk_needed_bytes: u64,
    pub scratch_disk_free_bytes: Option<u64>,
    pub output_disk_needed_bytes: u64,
    pub output_disk_free_bytes: Option<u64>,
    pub warnings: Vec<String>,
    /// Whether the build was told to run despite a shortfall.
    pub forced: bool,
}

impl BuildPlan {
    /// Heap the build is expected to hold outside its budget.
    pub fn untracked_bytes(&self, input_bytes: u64) -> u64 {
        untracked_bytes(input_bytes, self.threads)
    }
}

fn untracked_bytes(input_bytes: u64, threads: usize) -> u64 {
    untracked_base(input_bytes) + threads as u64 * WORKER_BYTES
}

/// The budget and the worker threads of a build of `input_bytes` on
/// `resources`. The workers' buffers lie outside the pool, so as LLVM sizes
/// link jobs by memory, there are no more workers than the memory left
/// beside the budget holds.
fn sizes(
    resources: &Resources,
    input_bytes: u64,
    budget_override: Option<usize>,
) -> (usize, usize) {
    let cpus = resources.usable_cpus.max(1);
    let budget = budget_override
        .unwrap_or_else(|| derived_budget(resources, untracked_bytes(input_bytes, cpus)));
    let beside = resources
        .available_memory_bytes
        .saturating_sub(budget as u64 + untracked_base(input_bytes));
    let threads = cpus.min((beside / WORKER_BYTES).max(1) as usize);
    (budget, threads)
}

/// Plan a build of `input_bytes` of PBF that spills to `scratch` and writes
/// its Pack under `output`, with the budget `budget_override` when one is
/// given. Fails, unless `force`, when memory or disk falls short.
pub fn plan_build(
    input_bytes: u64,
    scratch: &Path,
    output: &Path,
    budget_override: Option<usize>,
    force: bool,
) -> Result<BuildPlan> {
    let resources = Resources::probe();
    let (budget, threads) = sizes(&resources, input_bytes, budget_override);
    let mut plan = BuildPlan {
        memory_budget_bytes: budget as u64,
        memory_budget_derived: budget_override.is_none(),
        threads,
        // The least budget the build runs on, or the one it was given.
        memory_needed_bytes: budget_override.unwrap_or(MIN_BUDGET_BYTES) as u64
            + untracked_bytes(input_bytes, threads),
        scratch_disk_needed_bytes: ((input_bytes as f64 * SCRATCH_PER_INPUT) as u64)
            .saturating_sub(budget as u64),
        scratch_disk_free_bytes: resources::free_disk(scratch),
        output_disk_needed_bytes: (input_bytes as f64 * PACK_PER_INPUT) as u64,
        output_disk_free_bytes: resources::free_disk(output),
        resources,
        forced: force,
        ..BuildPlan::default()
    };
    let mut shortfalls = Vec::new();
    let mut check = |what: String, needed: u64, free: Option<u64>, fix: &str| {
        let Some(free) = free else {
            return;
        };
        if needed > free {
            shortfalls.push(format!(
                "{what}: needs about {} MiB, {} MiB free; {fix}",
                needed >> 20,
                free >> 20
            ));
        } else if needed as f64 > free as f64 * (1.0 - WARN_SHARE) {
            plan.warnings.push(format!(
                "{what}: needs about {} MiB of the {} MiB free",
                needed >> 20,
                free >> 20
            ));
        }
    };
    check(
        "memory".to_string(),
        plan.memory_needed_bytes,
        Some(plan.resources.available_memory_bytes),
        "close other programs, pass a smaller --memory-budget-mb, or build a smaller extract",
    );
    if resources::same_volume(scratch, output) {
        check(
            format!("disk for scratch and Pack at {}", scratch.display()),
            plan.scratch_disk_needed_bytes + plan.output_disk_needed_bytes,
            plan.scratch_disk_free_bytes,
            "free space there or pass --scratch-dir on another volume",
        );
    } else {
        check(
            format!("scratch disk at {}", scratch.display()),
            plan.scratch_disk_needed_bytes,
            plan.scratch_disk_free_bytes,
            "free space there or pass --scratch-dir on a larger volume",
        );
        check(
            format!("Pack disk at {}", output.display()),
            plan.output_disk_needed_bytes,
            plan.output_disk_free_bytes,
            "free space there",
        );
    }
    if !shortfalls.is_empty() && !force {
        bail!(
            "insufficient resources for this build, pass --force to build anyway:\n  {}",
            shortfalls.join("\n  ")
        );
    }
    plan.warnings.extend(shortfalls);
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_build_that_cannot_fit_stops_unless_forced() {
        let scratch = std::env::temp_dir();
        let error = plan_build(u64::MAX / 8, &scratch, &scratch, None, false).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("--force"), "{message}");
        assert!(message.contains("memory"), "{message}");
        assert!(message.contains("disk"), "{message}");
        let plan = plan_build(u64::MAX / 8, &scratch, &scratch, None, true).expect("forced");
        assert!(plan.forced);
        assert_eq!(plan.warnings.len(), 2, "{:?}", plan.warnings);
    }

    #[test]
    fn a_small_build_gets_a_derived_budget() {
        let scratch = std::env::temp_dir();
        let plan = plan_build(1 << 20, &scratch, &scratch, None, false).expect("plan");
        assert!(plan.memory_budget_derived);
        assert!(plan.memory_budget_bytes >= MIN_BUDGET_BYTES as u64);
        assert!(plan.threads >= 1 && plan.threads <= plan.resources.usable_cpus);
        let set = plan_build(1 << 20, &scratch, &scratch, Some(64 << 20), false).expect("plan");
        assert_eq!(set.memory_budget_bytes, 64 << 20);
        assert!(!set.memory_budget_derived);
    }

    #[test]
    fn workers_are_as_many_as_the_memory_beside_the_budget_holds() {
        let machine = |gib: u64| Resources {
            usable_cpus: 24,
            total_memory_bytes: 32 << 30,
            available_memory_bytes: gib << 30,
            ..Resources::default()
        };
        // Germany on this machine: the largest derived budget and every CPU.
        let germany = 4_861_056_214;
        assert_eq!(sizes(&machine(16), germany, None), (1 << 30, 24));
        // A small machine: a smaller budget and fewer workers.
        let (budget, threads) = sizes(&machine(3), germany, None);
        assert_eq!(budget, MIN_BUDGET_BYTES);
        assert!((1..24).contains(&threads), "{threads}");
        // A budget that takes the memory leaves one worker.
        assert_eq!(sizes(&machine(4), 0, Some(4 << 30)), (4 << 30, 1));
    }
}
