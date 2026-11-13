//! Aggregate statistics.

use crate::scan::Inventory;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct DriveStat {
    pub drive: String,
    pub size: u64,
    pub caches: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Stats {
    pub total_game_size: u64,
    pub per_drive: Vec<DriveStat>,
    pub top10: Vec<(String, u64)>,
    pub orphaned_size: u64,
    pub orphaned_count: usize,
    pub global_size: u64,
}

pub fn compute(inv: &Inventory) -> Stats {
    let mut per: Vec<DriveStat> = Vec::new();
    for e in &inv.entries {
        let d = inv.drive_name(e.library_index);
        match per.iter_mut().find(|p| p.drive == d) {
            Some(p) => {
                p.size += e.size;
                p.caches += 1;
            }
            None => per.push(DriveStat {
                drive: d,
                size: e.size,
                caches: 1,
            }),
        }
    }
    per.sort_by_key(|d| std::cmp::Reverse(d.size));
    let mut sorted: Vec<_> = inv.entries.iter().collect();
    sorted.sort_by_key(|e| std::cmp::Reverse(e.size));
    Stats {
        total_game_size: inv.entries.iter().map(|e| e.size).sum(),
        per_drive: per,
        top10: sorted
            .iter()
            .take(10)
            .map(|e| (e.name.clone(), e.size))
            .collect(),
        orphaned_size: inv
            .entries
            .iter()
            .filter(|e| e.orphaned())
            .map(|e| e.size)
            .sum(),
        orphaned_count: inv.entries.iter().filter(|e| e.orphaned()).count(),
        global_size: inv.globals.iter().map(|g| g.size).sum(),
    }
}
