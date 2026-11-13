//! Planning and executing cache removal.

use crate::guard::{Allowlist, ProcessProbe};
use crate::scan::Inventory;
use serde::Serialize;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Default)]
pub struct Selection {
    pub appids: Vec<u32>,
    pub orphaned: bool,
    /// Indices into `Inventory::globals`.
    pub globals: Vec<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlanItem {
    pub label: String,
    pub appid: Option<u32>,
    pub path: PathBuf,
    pub size: u64,
    pub files: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Refused {
    pub label: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ClearPlan {
    pub items: Vec<PlanItem>,
    pub refused: Vec<Refused>,
    pub steam_running: bool,
    pub total_size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteMode {
    Trash,
    Permanent,
}

#[derive(Debug, Default)]
pub struct ClearReport {
    pub done: Vec<PlanItem>,
    pub failed: Vec<(PlanItem, String)>,
    pub freed: u64,
}

/// Resolve a selection into a plan, applying every guard.
pub fn plan_clear(
    inv: &Inventory,
    allow: &Allowlist,
    probe: &dyn ProcessProbe,
    sel: &Selection,
) -> ClearPlan {
    let running = probe.running_appids();
    let mut plan = ClearPlan {
        steam_running: probe.steam_running(),
        ..Default::default()
    };
    let mut seen: Vec<PathBuf> = Vec::new();
    let mut consider = |plan: &mut ClearPlan, item: PlanItem| {
        if seen.contains(&item.path) {
            return;
        }
        seen.push(item.path.clone());
        if let Some(a) = item.appid {
            if running.contains(&a) {
                plan.refused.push(Refused {
                    label: item.label,
                    reason: "game is currently running".into(),
                });
                return;
            }
        }
        if let Err(e) = allow.check(&item.path) {
            plan.refused.push(Refused {
                label: item.label,
                reason: e.to_string(),
            });
            return;
        }
        plan.total_size += item.size;
        plan.items.push(item);
    };

    for e in &inv.entries {
        if sel.appids.contains(&e.appid) || (sel.orphaned && e.orphaned()) {
            consider(
                &mut plan,
                PlanItem {
                    label: format!("{} ({})", e.name, e.appid),
                    appid: Some(e.appid),
                    path: e.path.clone(),
                    size: e.size,
                    files: e.files,
                },
            );
        }
    }
    for &i in &sel.globals {
        if let Some(g) = inv.globals.get(i) {
            consider(
                &mut plan,
                PlanItem {
                    label: g.label.clone(),
                    appid: None,
                    path: g.path.clone(),
                    size: g.size,
                    files: g.files,
                },
            );
        }
    }
    plan
}

/// Execute a plan. Allowlist and running-game checks are repeated per item, right before deleting.
pub fn execute(
    plan: &ClearPlan,
    mode: DeleteMode,
    allow: &Allowlist,
    probe: &dyn ProcessProbe,
) -> ClearReport {
    let mut rep = ClearReport::default();
    for item in &plan.items {
        let running = probe.running_appids();
        if item.appid.is_some_and(|a| running.contains(&a)) {
            rep.failed
                .push((item.clone(), "game is currently running".into()));
            continue;
        }
        let canon = match allow.check(&item.path) {
            Ok(c) => c,
            Err(e) => {
                rep.failed.push((item.clone(), e.to_string()));
                continue;
            }
        };
        let res = match mode {
            DeleteMode::Trash => trash::delete(&canon).map_err(|e| e.to_string()),
            // remove_dir_all never follows symlinks.
            DeleteMode::Permanent => fs::remove_dir_all(&canon).map_err(|e| e.to_string()),
        };
        match res {
            Ok(()) => {
                rep.freed += item.size;
                rep.done.push(item.clone());
            }
            Err(e) => rep.failed.push((item.clone(), e)),
        }
    }
    rep
}
