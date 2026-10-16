//! What this machine has: memory, free disk, and what og itself holds. Read
//! from the operating system each time they are asked for.

use std::path::Path;

use sysinfo::{Disks, Pid, ProcessRefreshKind, ProcessesToUpdate, System};

pub struct Memory {
    pub total: u64,
    pub available: u64,
}

pub fn memory() -> Memory {
    let mut system = System::new();
    system.refresh_memory();
    Memory {
        total: system.total_memory(),
        available: system.available_memory(),
    }
}

/// Free bytes on the disk that holds `path`, or would hold it once created.
pub fn free_disk(path: &Path) -> Option<u64> {
    let mut existing = path;
    while !existing.exists() {
        existing = existing.parent()?;
    }
    let path = existing.canonicalize().ok()?;
    // Windows canonical paths carry the verbatim prefix, which no mount
    // point does: `\\?\D:\og` is on `D:\`.
    let path = match path.to_str().and_then(|text| text.strip_prefix(r"\\?\")) {
        Some(text) => std::path::PathBuf::from(text),
        None => path,
    };
    let disks = Disks::new_with_refreshed_list();
    disks
        .list()
        .iter()
        .filter(|disk| path.starts_with(disk.mount_point()))
        .max_by_key(|disk| disk.mount_point().as_os_str().len())
        .map(|disk| disk.available_space())
}

/// Memory og itself holds, resident.
pub struct ProcessMemory {
    system: System,
    pid: Option<Pid>,
}

impl ProcessMemory {
    pub fn new() -> Self {
        Self {
            system: System::new(),
            pid: sysinfo::get_current_pid().ok(),
        }
    }

    pub fn resident(&mut self) -> Option<u64> {
        let pid = self.pid?;
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            true,
            ProcessRefreshKind::nothing().with_memory(),
        );
        self.system.process(pid).map(|process| process.memory())
    }
}
