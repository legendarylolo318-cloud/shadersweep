//! shadersweep GUI (Slint). All heavy work runs on worker threads.

use anyhow::{Context, Result};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use shadersweep_core::gpu;
use shadersweep_core::guard::{Allowlist, ProcessProbe, ProcfsProbe};
use shadersweep_core::ops::{self, ClearPlan, DeleteMode, Selection};
use shadersweep_core::scan::{self, Inventory, ScanOptions};
use shadersweep_core::settings::{self, PreCaching};
use shadersweep_core::stats;
use shadersweep_core::util::{format_date, human_size};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

slint::include_modules!();

// glibc 2.35/2.43 added new symbol versions of these libm functions; binding to the
// x86_64 baseline versions keeps the AppImage runnable on older distributions
// (results are identical, the new versions are only faster).
#[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
std::arch::global_asm!(
    ".symver acosf, acosf@GLIBC_2.2.5",
    ".symver atan2f, atan2f@GLIBC_2.2.5",
    ".symver hypotf, hypotf@GLIBC_2.2.5",
);

#[derive(Clone, PartialEq)]
enum Detail {
    Game(PathBuf),
    Global(PathBuf),
}

struct Extras {
    gpu_banner: String,
    precache: String,
}

struct App {
    ui: slint::Weak<MainWindow>,
    inv: Inventory,
    allow: Allowlist,
    selected: HashSet<PathBuf>,
    detail: Option<Detail>,
    scanning: bool,
    pending: Option<(ClearPlan, bool)>,
    watcher: Option<RecommendedWatcher>,
    auto: bool,
    gpu_banner: String,
    /// A clear is running on a worker thread.
    clearing: bool,
    /// File-watcher events before this instant are our own (scan/clear) and are ignored.
    quiet_until: Instant,
    /// Proton prefix sizes, computed once per prefix per scan (`None` = being computed).
    compat_sizes: HashMap<PathBuf, Option<String>>,
    /// Last result of the background process probe.
    running: HashSet<u32>,
    steam_running: bool,
}

/// Minimum time between two automatic (watcher-triggered) rescans.
const AUTO_RESCAN_MIN_GAP: Duration = Duration::from_secs(10);

/// Only real content changes should trigger an auto-rescan. In particular `Access` events
/// (inotify OPEN/CLOSE_NOWRITE, which notify subscribes to) are produced by our own scan
/// reading the tree and used to cause an endless rescan loop.
fn is_relevant(ev: &notify::Event) -> bool {
    use notify::event::{EventKind, ModifyKind};
    matches!(
        ev.kind,
        EventKind::Create(_)
            | EventKind::Remove(_)
            | EventKind::Modify(ModifyKind::Name(_) | ModifyKind::Data(_) | ModifyKind::Any)
    )
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

fn with_app<R>(f: impl FnOnce(&mut App, MainWindow) -> R) -> Option<R> {
    APP.with(|a| {
        let mut b = a.try_borrow_mut().ok()?;
        let app = b.as_mut()?;
        let ui = app.ui.upgrade()?;
        Some(f(app, ui))
    })
}

fn tr(s: impl AsRef<str>) -> SharedString {
    SharedString::from(s.as_ref())
}

impl App {
    fn refresh(&mut self, ui: &MainWindow) {
        let search = ui.get_search().to_lowercase();
        let orphan_only = ui.get_orphan_only();
        let col = ui.get_sort_col();
        let asc = ui.get_sort_asc();
        let max = self
            .inv
            .entries
            .iter()
            .map(|e| e.size)
            .max()
            .unwrap_or(1)
            .max(1) as f32;
        let mut idx: Vec<usize> = (0..self.inv.entries.len())
            .filter(|&i| {
                let e = &self.inv.entries[i];
                (!orphan_only || e.orphaned())
                    && (search.is_empty()
                        || e.name.to_lowercase().contains(&search)
                        || e.appid.to_string().contains(&search))
            })
            .collect();
        let inv = &self.inv;
        idx.sort_by(|&a, &b| {
            let (x, y) = (&inv.entries[a], &inv.entries[b]);
            let o = match col {
                0 => x.name.to_lowercase().cmp(&y.name.to_lowercase()),
                1 => x.appid.cmp(&y.appid),
                2 => x.library.cmp(&y.library),
                4 => x.modified.cmp(&y.modified),
                5 => x.installed.cmp(&y.installed),
                _ => x.size.cmp(&y.size),
            };
            if asc {
                o
            } else {
                o.reverse()
            }
        });
        let rows: Vec<Row> = idx
            .iter()
            .map(|&i| {
                let e = &self.inv.entries[i];
                Row {
                    id: i as i32,
                    name: tr(&e.name),
                    appid: tr(e.appid.to_string()),
                    library: tr(self.inv.drive_name(e.library_index)),
                    size: tr(human_size(e.size)),
                    frac: e.size as f32 / max,
                    modified: tr(format_date(e.modified)),
                    status: tr(if e.installed { "yes" } else { "orphaned" }),
                    orphan: e.orphaned(),
                    selected: self.selected.contains(&e.path),
                }
            })
            .collect();
        let shown_size: u64 = idx.iter().map(|&i| self.inv.entries[i].size).sum();
        ui.set_rows(ModelRc::new(VecModel::from(rows)));

        let globals: Vec<Row> = self
            .inv
            .globals
            .iter()
            .enumerate()
            .map(|(i, g)| Row {
                id: i as i32,
                name: tr(&g.label),
                appid: tr(""),
                library: tr(""),
                size: tr(human_size(g.size)),
                frac: 0.0,
                modified: tr(format_date(g.modified)),
                status: tr(""),
                orphan: false,
                selected: self.selected.contains(&g.path),
            })
            .collect();
        ui.set_globals(ModelRc::new(VecModel::from(globals)));

        let st = stats::compute(&self.inv);
        ui.set_stats_text(tr(format!(
            "Game caches {} ({})  ·  Orphaned {} ({})  ·  Driver caches {}",
            human_size(st.total_game_size),
            self.inv.entries.len(),
            human_size(st.orphaned_size),
            st.orphaned_count,
            human_size(st.global_size)
        )));
        ui.set_orphan_count(st.orphaned_count as i32);
        ui.set_orphan_size(tr(human_size(st.orphaned_size)));
        ui.set_total_text(tr(format!(
            "Showing {} caches · {}",
            idx.len(),
            human_size(shown_size)
        )));
        let sel: u64 = self
            .inv
            .entries
            .iter()
            .filter(|e| self.selected.contains(&e.path))
            .map(|e| e.size)
            .chain(
                self.inv
                    .globals
                    .iter()
                    .filter(|g| self.selected.contains(&g.path))
                    .map(|g| g.size),
            )
            .sum();
        ui.set_selected_count(self.selected.len() as i32);
        ui.set_selected_size(tr(human_size(sel)));

        let mut ov = String::new();
        for d in &st.per_drive {
            ov.push_str(&format!(
                "{}: {} ({} caches)\n",
                d.drive,
                human_size(d.size),
                d.caches
            ));
        }
        ov.push_str("\nTop 10 largest:\n");
        for (n, s) in &st.top10 {
            ov.push_str(&format!("  {:>10}  {n}\n", human_size(*s)));
        }
        if !self.inv.warnings.is_empty() {
            ov.push_str("\nWarnings:\n");
            for w in &self.inv.warnings {
                ov.push_str(&format!("  {w}\n"));
            }
        }
        ui.set_overview_text(tr(ov));
        self.refresh_detail(ui);
    }

    fn refresh_detail(&mut self, ui: &MainWindow) {
        let Some(d) = self.detail.clone() else {
            ui.set_detail_title(tr(""));
            return;
        };
        let (title, text, compat) = match &d {
            Detail::Game(p) => match self.inv.entries.iter().find(|e| &e.path == p) {
                Some(e) => {
                    let mut t = format!(
                        "AppID {} · {}\nLibrary: {}\n\nCache folder:\n{}\n\n{} · {} files · last modified {}\n\nBreakdown:\n",
                        e.appid,
                        if e.is_shortcut { "non-Steam shortcut" } else if e.installed { "installed" } else { "ORPHANED (game not installed)" },
                        e.library.display(),
                        e.path.display(),
                        human_size(e.size),
                        e.files,
                        format_date(e.modified)
                    );
                    for c in &e.children {
                        t.push_str(&format!(
                            "  {:>10}  {} ({} files)\n",
                            human_size(c.size),
                            c.name,
                            c.files
                        ));
                    }
                    (e.name.clone(), t, e.compat_path.clone())
                }
                None => (String::new(), String::new(), None),
            },
            Detail::Global(p) => match self.inv.globals.iter().find(|g| &g.path == p) {
                Some(g) => {
                    let mut t = format!("Global driver cache\n\n{}\n\n{} · {} files · last modified {}\n\nBreakdown:\n", g.path.display(), human_size(g.size), g.files, format_date(g.modified));
                    for c in g.children.iter().take(12) {
                        t.push_str(&format!(
                            "  {:>10}  {} ({} files)\n",
                            human_size(c.size),
                            c.name,
                            c.files
                        ));
                    }
                    (g.label.clone(), t, None)
                }
                None => (String::new(), String::new(), None),
            },
        };
        ui.set_detail_title(tr(&title));
        ui.set_detail_text(tr(text));
        ui.set_detail_extra(tr(""));
        if let Some(cp) = compat {
            const HDR: &str = "Proton prefix (info only — contains saves, never touched):";
            match self.compat_sizes.get(&cp) {
                Some(Some(sz)) => {
                    ui.set_detail_extra(tr(format!("{HDR}\n{}\n{sz}", cp.display())));
                }
                Some(None) => {
                    ui.set_detail_extra(tr(format!("{HDR}\n{}\n(sizing…)", cp.display())));
                }
                None => {
                    ui.set_detail_extra(tr(format!("{HDR}\n{}\n(sizing…)", cp.display())));
                    self.compat_sizes.insert(cp.clone(), None);
                    let weak = ui.as_weak();
                    std::thread::spawn(move || {
                        let st = scan::dir_stats(&cp);
                        let sz = format!("{} · {} files", human_size(st.size), st.files);
                        let _ = weak.upgrade_in_event_loop(move |_| {
                            with_app(|app, ui| {
                                if let Some(slot) = app.compat_sizes.get_mut(&cp) {
                                    *slot = Some(sz);
                                }
                                app.refresh_detail(&ui);
                            });
                        });
                    });
                }
            }
        }
    }

    fn start_scan(&mut self, ui: &MainWindow) {
        if self.scanning {
            return;
        }
        self.scanning = true;
        ui.set_scanning(true);
        ui.set_progress(0.0);
        ui.set_status_text(tr("Scanning…"));
        let weak = ui.as_weak();
        std::thread::spawn(move || {
            let Ok(opts) = ScanOptions::from_env() else {
                let _ = weak.upgrade_in_event_loop(|_| {
                    with_app(|app, ui| {
                        app.scanning = false;
                        ui.set_scanning(false);
                        ui.set_status_text(tr("Scan failed: cannot determine the home directory"));
                    });
                });
                return;
            };
            let w2 = weak.clone();
            let inv = scan::scan(&opts, &move |p| {
                let f = p.done as f32 / p.total.max(1) as f32;
                let _ = w2.upgrade_in_event_loop(move |ui| ui.set_progress(f));
            });
            // GPU + settings checks are cheap but vulkaninfo can take a moment: stay off the UI thread.
            let cur = gpu::detect_system(true);
            let banner = match gpu::default_state_file().map(|f| gpu::check_and_record(&f, &cur)) {
                Some(Ok(Some(prev))) => format!(
                    "GPU/driver changed since last run ({} → {}). Consider clearing the global driver caches below.",
                    prev.summary(),
                    cur.summary()
                ),
                _ => String::new(),
            };
            let precache = match inv.installs.first().map(settings::shader_precaching) {
                Some(PreCaching::Enabled) => "Shader Pre-Caching: on",
                Some(PreCaching::Disabled) => "Shader Pre-Caching: off",
                _ => "Shader Pre-Caching: not readable from config",
            }
            .to_string();
            let extras = Extras {
                gpu_banner: banner,
                precache,
            };
            let _ = weak.upgrade_in_event_loop(move |_| {
                with_app(|app, ui| app.apply_scan(inv, extras, &ui));
            });
        });
    }

    fn apply_scan(&mut self, inv: Inventory, extras: Extras, ui: &MainWindow) {
        self.allow = Allowlist::from_inventory(&inv);
        let alive: HashSet<&PathBuf> = inv
            .entries
            .iter()
            .map(|e| &e.path)
            .chain(inv.globals.iter().map(|g| &g.path))
            .collect();
        self.selected.retain(|p| alive.contains(p));
        if let Some(Detail::Game(p) | Detail::Global(p)) = &self.detail {
            if !alive.contains(p) {
                self.detail = None;
            }
        }
        ui.set_status_text(tr(format!("Scanned in {} ms", inv.scan_ms)));
        if std::env::var_os("SHADERSWEEP_LOG").is_some() {
            eprintln!(
                "shadersweep: scanned {} caches ({} driver caches) in {} libraries in {} ms",
                inv.entries.len(),
                inv.globals.len(),
                inv.libraries.len(),
                inv.scan_ms
            );
            for w in &inv.warnings {
                eprintln!("shadersweep: warning: {w}");
            }
        }
        self.inv = inv;
        self.compat_sizes.clear();
        self.scanning = false;
        // Our own reads may still be queued in the watcher; don't let them trigger a rescan.
        self.quiet_until = Instant::now() + Duration::from_secs(2);
        ui.set_scanning(false);
        if !extras.gpu_banner.is_empty() {
            self.gpu_banner = extras.gpu_banner;
        }
        ui.set_gpu_banner(tr(&self.gpu_banner));
        ui.set_precache_text(tr(extras.precache));
        self.refresh(ui);
        self.rewatch();
        self.update_steam_banner(ui);
    }

    fn update_steam_banner(&self, ui: &MainWindow) {
        let running = &self.running;
        let msg = if !running.is_empty() {
            let names: Vec<String> = running
                .iter()
                .map(|id| {
                    self.inv
                        .entries
                        .iter()
                        .find(|e| e.appid == *id)
                        .map(|e| e.name.clone())
                        .unwrap_or_else(|| id.to_string())
                })
                .collect();
            format!(
                "Running game(s): {}. Their caches cannot be cleared until they exit.",
                names.join(", ")
            )
        } else if self.steam_running {
            "Steam is running. Clearing works, but closing Steam first is safest.".to_string()
        } else {
            String::new()
        };
        ui.set_steam_banner(tr(msg));
    }

    fn rewatch(&mut self) {
        self.watcher = None;
        if !self.auto {
            return;
        }
        let (tx, rx) = mpsc::channel::<()>();
        let Ok(mut w) = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if res.is_ok_and(|ev| is_relevant(&ev)) {
                let _ = tx.send(());
            }
        }) else {
            return;
        };
        let mut any = false;
        for d in self.inv.watch_dirs() {
            any |= w.watch(&d, RecursiveMode::Recursive).is_ok();
        }
        if !any {
            return;
        }
        let weak = self.ui.clone();
        std::thread::spawn(move || {
            let mut last = Instant::now() - AUTO_RESCAN_MIN_GAP;
            // Debounce: wait for a quiet period before asking for a rescan.
            while rx.recv().is_ok() {
                while rx.recv_timeout(Duration::from_secs(3)).is_ok() {}
                let gap = last.elapsed();
                if gap < AUTO_RESCAN_MIN_GAP {
                    std::thread::sleep(AUTO_RESCAN_MIN_GAP - gap);
                    while rx.try_recv().is_ok() {}
                }
                last = Instant::now();
                let posted = weak.upgrade_in_event_loop(|_| {
                    with_app(|app, ui| {
                        let ours = app.scanning || app.clearing || Instant::now() < app.quiet_until;
                        if app.auto && !ours {
                            app.start_scan(&ui);
                        }
                    });
                });
                if posted.is_err() {
                    break;
                }
            }
        });
        self.watcher = Some(w);
    }

    fn request_clear(&mut self, ui: &MainWindow, kind: i32, rebuild: bool) {
        let mut sel = Selection::default();
        match kind {
            0 => {
                for e in &self.inv.entries {
                    if self.selected.contains(&e.path) {
                        sel.paths.push(e.path.clone());
                    }
                }
                for (i, g) in self.inv.globals.iter().enumerate() {
                    if self.selected.contains(&g.path) {
                        sel.globals.push(i);
                    }
                }
            }
            1 => sel.orphaned = true,
            _ => match &self.detail {
                Some(Detail::Game(p)) => {
                    if self.inv.entries.iter().any(|e| &e.path == p) {
                        sel.paths.push(p.clone());
                    }
                }
                Some(Detail::Global(p)) => {
                    if let Some(i) = self.inv.globals.iter().position(|g| &g.path == p) {
                        sel.globals.push(i);
                    }
                }
                None => {}
            },
        }
        if self.clearing {
            ui.set_status_text(tr("A clear is already in progress"));
            return;
        }
        // Planning reads /proc and stats every path: do it off the UI thread.
        let inv = self.inv.clone();
        let allow = self.allow.clone();
        let weak = ui.as_weak();
        std::thread::spawn(move || {
            let plan = ops::plan_clear(&inv, &allow, &ProcfsProbe::default(), &sel);
            let _ = weak.upgrade_in_event_loop(move |_| {
                with_app(|app, ui| app.show_confirm(&ui, plan, rebuild));
            });
        });
    }

    fn show_confirm(&mut self, ui: &MainWindow, plan: ClearPlan, rebuild: bool) {
        if plan.items.is_empty() {
            let why: Vec<String> = plan
                .refused
                .iter()
                .map(|r| format!("{}: {}", r.label, r.reason))
                .collect();
            ui.set_status_text(tr(if why.is_empty() {
                "Nothing to clear".to_string()
            } else {
                format!("Refused — {}", why.join("; "))
            }));
            return;
        }
        let mut text = String::new();
        for i in &plan.items {
            text.push_str(&format!(
                "{:>10}  {}\n            {}\n",
                human_size(i.size),
                i.label,
                i.path.display()
            ));
        }
        let mut note = String::new();
        if rebuild {
            note.push_str("Rebuild = clear. shadersweep does not compile shaders itself: Steam re-downloads pre-cached shaders (if Shader Pre-Caching is on) or the game recompiles them on its next launch (first launch may stutter). ");
        }
        if plan.steam_running {
            note.push_str("Steam is running — close it first if you can. ");
        }
        for r in &plan.refused {
            note.push_str(&format!("Skipped {}: {}. ", r.label, r.reason));
        }
        ui.set_confirm_text(tr(text));
        ui.set_confirm_note(tr(note));
        ui.set_confirm_total(tr(format!(
            "{} item(s), {} total",
            plan.items.len(),
            human_size(plan.total_size)
        )));
        ui.set_confirm_permanent(false);
        ui.set_confirm_visible(true);
        self.pending = Some((plan, rebuild));
    }

    fn confirm(&mut self, ui: &MainWindow, permanent: bool) {
        ui.set_confirm_visible(false);
        let Some((plan, _)) = self.pending.take() else {
            return;
        };
        ui.set_status_text(tr("Clearing…"));
        self.clearing = true;
        ui.set_clearing(true);
        let allow = self.allow.clone();
        let weak = ui.as_weak();
        std::thread::spawn(move || {
            let mode = if permanent {
                DeleteMode::Permanent
            } else {
                DeleteMode::Trash
            };
            let rep = ops::execute(&plan, mode, &allow, &ProcfsProbe::default());
            let mut msg = format!(
                "{} {} cache(s), freed {}",
                if permanent { "Deleted" } else { "Trashed" },
                rep.done.len(),
                human_size(rep.freed)
            );
            if let Some((i, e)) = rep.failed.first() {
                msg.push_str(&format!(
                    " — {} failed (first: {}: {e})",
                    rep.failed.len(),
                    i.label
                ));
            }
            let _ = weak.upgrade_in_event_loop(move |_| {
                with_app(|app, ui| {
                    app.clearing = false;
                    ui.set_clearing(false);
                    app.quiet_until = Instant::now() + Duration::from_secs(5);
                    app.selected.clear();
                    app.start_scan(&ui);
                    ui.set_status_text(tr(msg));
                });
            });
        });
    }
}

fn main() -> Result<()> {
    let _ = slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("software".into())
        .select();
    let ui = MainWindow::new().context("creating window")?;
    APP.with(|a| {
        *a.borrow_mut() = Some(App {
            ui: ui.as_weak(),
            inv: Inventory::default(),
            allow: Allowlist::default(),
            selected: HashSet::new(),
            detail: None,
            scanning: false,
            pending: None,
            watcher: None,
            auto: false,
            gpu_banner: String::new(),
            clearing: false,
            quiet_until: Instant::now(),
            compat_sizes: HashMap::new(),
            running: HashSet::new(),
            steam_running: false,
        });
    });

    ui.on_rescan(|| {
        with_app(|a, ui| a.start_scan(&ui));
    });
    ui.on_filters_changed(|| {
        with_app(|a, ui| a.refresh(&ui));
    });
    ui.on_sort_by(|c| {
        with_app(|a, ui| {
            if ui.get_sort_col() == c {
                ui.set_sort_asc(!ui.get_sort_asc());
            } else {
                ui.set_sort_col(c);
                ui.set_sort_asc(c != 3 && c != 4);
            }
            a.refresh(&ui);
        });
    });
    ui.on_toggle(|id| {
        with_app(|a, ui| {
            if let Some(e) = a.inv.entries.get(id as usize) {
                let p = e.path.clone();
                if !a.selected.remove(&p) {
                    a.selected.insert(p);
                }
            }
            a.refresh(&ui);
        });
    });
    ui.on_toggle_global(|id| {
        with_app(|a, ui| {
            if let Some(g) = a.inv.globals.get(id as usize) {
                let p = g.path.clone();
                if !a.selected.remove(&p) {
                    a.selected.insert(p);
                }
            }
            a.refresh(&ui);
        });
    });
    ui.on_open_row(|id| {
        with_app(|a, ui| {
            a.detail = a
                .inv
                .entries
                .get(id as usize)
                .map(|e| Detail::Game(e.path.clone()));
            a.refresh_detail(&ui);
        });
    });
    ui.on_open_global(|id| {
        with_app(|a, ui| {
            a.detail = a
                .inv
                .globals
                .get(id as usize)
                .map(|g| Detail::Global(g.path.clone()));
            a.refresh_detail(&ui);
        });
    });
    ui.on_select_all(|on| {
        with_app(|a, ui| {
            a.selected.clear();
            if on {
                let search = ui.get_search().to_lowercase();
                for e in &a.inv.entries {
                    if (!ui.get_orphan_only() || e.orphaned())
                        && (search.is_empty()
                            || e.name.to_lowercase().contains(&search)
                            || e.appid.to_string().contains(&search))
                    {
                        a.selected.insert(e.path.clone());
                    }
                }
            }
            a.refresh(&ui);
        });
    });
    ui.on_request_clear(|k, r| {
        with_app(|a, ui| a.request_clear(&ui, k, r));
    });
    ui.on_confirm_ok(|perm| {
        with_app(|a, ui| a.confirm(&ui, perm));
    });
    ui.on_confirm_cancel(|| {
        with_app(|a, ui| {
            a.pending = None;
            ui.set_confirm_visible(false);
        });
    });
    ui.on_auto_rescan_changed(|on| {
        with_app(|a, _| {
            a.auto = on;
            a.rewatch();
        });
    });

    // Re-check for running Steam / games every few seconds, on a worker thread
    // (reading every /proc/<pid>/environ is blocking I/O). Exits with the event loop.
    let weak = ui.as_weak();
    std::thread::spawn(move || loop {
        let probe = ProcfsProbe::default();
        let running = probe.running_appids();
        let steam = probe.steam_running();
        let posted = weak.upgrade_in_event_loop(move |_| {
            with_app(|a, ui| {
                if a.running != running || a.steam_running != steam {
                    a.running = running;
                    a.steam_running = steam;
                    a.update_steam_banner(&ui);
                }
            });
        });
        if posted.is_err() {
            break;
        }
        std::thread::sleep(Duration::from_secs(4));
    });

    with_app(|a, ui| a.start_scan(&ui));
    ui.run().context("running UI")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{
        AccessKind, AccessMode, CreateKind, DataChange, EventKind, MetadataKind, ModifyKind,
        RemoveKind, RenameMode,
    };

    fn ev(kind: EventKind) -> notify::Event {
        notify::Event::new(kind)
    }

    #[test]
    fn scan_reads_do_not_trigger_rescans() {
        assert!(!is_relevant(&ev(EventKind::Access(AccessKind::Open(
            AccessMode::Any
        )))));
        assert!(!is_relevant(&ev(EventKind::Access(AccessKind::Close(
            AccessMode::Read
        )))));
        assert!(!is_relevant(&ev(EventKind::Modify(ModifyKind::Metadata(
            MetadataKind::Any
        )))));
        assert!(!is_relevant(&ev(EventKind::Other)));
        assert!(is_relevant(&ev(EventKind::Create(CreateKind::Folder))));
        assert!(is_relevant(&ev(EventKind::Remove(RemoveKind::Folder))));
        assert!(is_relevant(&ev(EventKind::Modify(ModifyKind::Name(
            RenameMode::From
        )))));
        assert!(is_relevant(&ev(EventKind::Modify(ModifyKind::Data(
            DataChange::Content
        )))));
    }

    /// Real inotify: a scan of a watched tree produces only events that are filtered out.
    #[test]
    fn watcher_ignores_our_own_scan() {
        let t = tempfile::tempdir().unwrap();
        let sc = t.path().join("steamapps/shadercache/100/fozpipelinesv6");
        std::fs::create_dir_all(&sc).unwrap();
        std::fs::write(sc.join("a.foz"), b"x").unwrap();
        let (tx, rx) = mpsc::channel::<bool>();
        let mut w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(e) = res {
                let _ = tx.send(is_relevant(&e));
            }
        })
        .unwrap();
        w.watch(
            &t.path().join("steamapps/shadercache"),
            RecursiveMode::Recursive,
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(100));
        while rx.try_recv().is_ok() {}
        let _ = scan::dir_stats(&t.path().join("steamapps/shadercache"));
        std::thread::sleep(Duration::from_millis(300));
        let evs: Vec<bool> = rx.try_iter().collect();
        assert!(
            !evs.is_empty(),
            "expected raw OPEN/CLOSE events from the scan"
        );
        assert!(
            evs.iter().all(|r| !r),
            "scan produced a relevant event: {evs:?}"
        );
        std::fs::remove_dir_all(t.path().join("steamapps/shadercache/100")).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert!(rx.try_iter().any(|r| r), "deletion was not seen");
    }
}
