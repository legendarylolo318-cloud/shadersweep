//! Cache discovery + parallel sizing.

use crate::steam::{self, Library, SteamInstall};
use crate::CoreError;
use rayon::prelude::*;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Instant, UNIX_EPOCH};
use walkdir::WalkDir;

#[derive(Debug, Clone, Serialize)]
pub struct ChildStat {
    pub name: String,
    pub size: u64,
    pub files: u64,
}

#[derive(Debug, Clone, Default)]
pub struct DirStats {
    pub size: u64,
    pub files: u64,
    pub modified: Option<u64>,
    pub children: Vec<ChildStat>,
}

fn mtime(md: &fs::Metadata) -> Option<u64> {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
}

/// Allocated bytes (like `du`).
fn alloc(md: &fs::Metadata) -> u64 {
    md.blocks() * 512
}

fn walk(path: &Path) -> (u64, u64, Option<u64>) {
    let (mut size, mut files, mut m) = (0u64, 0u64, None::<u64>);
    for e in WalkDir::new(path).follow_links(false).into_iter().flatten() {
        if let Ok(md) = e.metadata() {
            size += alloc(&md);
            if !md.is_dir() {
                files += 1;
            }
            m = m.max(mtime(&md));
        }
    }
    (size, files, m)
}

/// Size a directory without following symlinks; `children` is a first-level breakdown.
pub fn dir_stats(path: &Path) -> DirStats {
    let own = fs::symlink_metadata(path).ok();
    let Ok(rd) = fs::read_dir(path) else {
        return DirStats::default();
    };
    let kids: Vec<_> = rd.flatten().collect();
    let parts: Vec<(ChildStat, Option<u64>)> = kids
        .par_iter()
        .map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            let (size, files, m) = if is_dir {
                walk(&e.path())
            } else {
                match fs::symlink_metadata(e.path()) {
                    Ok(md) => (alloc(&md), 1, mtime(&md)),
                    Err(_) => (0, 0, None),
                }
            };
            (ChildStat { name, size, files }, m)
        })
        .collect();
    let mut st = DirStats {
        size: own.as_ref().map(alloc).unwrap_or(0),
        modified: own.as_ref().and_then(mtime),
        ..Default::default()
    };
    // The directory's own mtime changes on any entry add/remove; prefer file mtimes.
    let mut latest = None;
    for (c, m) in parts {
        st.size += c.size;
        st.files += c.files;
        latest = latest.max(m);
        st.children.push(c);
    }
    if latest.is_some() {
        st.modified = latest;
    }
    st.children.sort_by_key(|c| std::cmp::Reverse(c.size));
    st
}

#[derive(Debug, Clone, Serialize)]
pub struct CacheEntry {
    pub appid: u32,
    pub name: String,
    pub library: PathBuf,
    pub library_index: usize,
    pub path: PathBuf,
    pub size: u64,
    pub files: u64,
    pub modified: Option<u64>,
    pub installed: bool,
    pub is_shortcut: bool,
    pub compat_path: Option<PathBuf>,
    pub children: Vec<ChildStat>,
    pub dev: u64,
}

impl CacheEntry {
    pub fn orphaned(&self) -> bool {
        !self.installed
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum GlobalSource {
    Default,
    Env,
}

#[derive(Debug, Clone, Serialize)]
pub struct GlobalCache {
    pub label: String,
    pub path: PathBuf,
    pub source: GlobalSource,
    pub size: u64,
    pub files: u64,
    pub modified: Option<u64>,
    pub children: Vec<ChildStat>,
}

#[derive(Debug, Clone)]
pub struct ScanOptions {
    pub home: PathBuf,
    pub cache_home: PathBuf,
    /// (variable name, value) for the cache env overrides that are set.
    pub env: Vec<(String, String)>,
}

pub const ENV_OVERRIDES: [&str; 3] = [
    "DXVK_STATE_CACHE_PATH",
    "__GL_SHADER_DISK_CACHE_PATH",
    "MESA_SHADER_CACHE_DIR",
];

impl ScanOptions {
    pub fn from_env() -> Result<Self, CoreError> {
        let home = dirs::home_dir().ok_or(CoreError::NoHome)?;
        let cache_home = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| home.join(".cache"));
        let env = ENV_OVERRIDES
            .iter()
            .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
            .filter(|(_, v)| !v.is_empty())
            .collect();
        Ok(Self {
            home,
            cache_home,
            env,
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct Inventory {
    pub installs: Vec<SteamInstall>,
    pub libraries: Vec<Library>,
    pub entries: Vec<CacheEntry>,
    pub globals: Vec<GlobalCache>,
    pub warnings: Vec<String>,
    pub scan_ms: u128,
}

#[derive(Debug, Clone)]
pub struct Progress {
    pub done: usize,
    pub total: usize,
    pub current: String,
}

/// An env override is only honoured if it can't plausibly be `/` or `$HOME` or a parent of it.
fn env_override_ok(canon: &Path, home: &Path) -> bool {
    let home = fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    canon.components().count() >= 4 && !home.starts_with(canon)
}

fn discover_globals(
    opts: &ScanOptions,
    warnings: &mut Vec<String>,
) -> Vec<(String, PathBuf, GlobalSource)> {
    let mut cands: Vec<(String, PathBuf, GlobalSource)> = vec![
        (
            "Mesa shader cache".into(),
            opts.cache_home.join("mesa_shader_cache"),
            GlobalSource::Default,
        ),
        (
            "Mesa shader cache (DB)".into(),
            opts.cache_home.join("mesa_shader_cache_db"),
            GlobalSource::Default,
        ),
        (
            "NVIDIA GL cache".into(),
            opts.cache_home.join("nvidia/GLCache"),
            GlobalSource::Default,
        ),
    ];
    if let Ok(rd) = fs::read_dir(&opts.cache_home) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.starts_with("radv_builtin_shaders") {
                cands.push((
                    format!("RADV built-in shaders ({n})"),
                    e.path(),
                    GlobalSource::Default,
                ));
            }
        }
    }
    for (k, v) in &opts.env {
        cands.push((format!("${k}"), PathBuf::from(v), GlobalSource::Env));
    }
    let mut out: Vec<(String, PathBuf, GlobalSource)> = Vec::new();
    for (label, p, src) in cands {
        match fs::symlink_metadata(&p) {
            Ok(md) if md.file_type().is_symlink() => {
                warnings.push(format!("{} is a symlink; ignored for safety", p.display()));
                continue;
            }
            Ok(md) if md.is_dir() => {}
            _ => continue,
        }
        let Ok(canon) = fs::canonicalize(&p) else {
            continue;
        };
        if src == GlobalSource::Env && (!p.is_absolute() || !env_override_ok(&canon, &opts.home)) {
            warnings.push(format!(
                "{label}={} rejected (too broad or relative)",
                p.display()
            ));
            continue;
        }
        if out.iter().any(|(_, c, _)| *c == canon) {
            continue;
        }
        out.push((label, canon, src));
    }
    out
}

/// Full scan. `progress` may be called from worker threads.
pub fn scan(opts: &ScanOptions, progress: &(dyn Fn(Progress) + Sync)) -> Inventory {
    let t0 = Instant::now();
    let mut inv = Inventory {
        installs: steam::find_installs(&opts.home),
        ..Default::default()
    };
    let mut warnings = Vec::new();

    let mut shortcuts: HashMap<u32, String> = HashMap::new();
    for i in &inv.installs {
        let libs = steam::find_libraries(i, &mut warnings);
        for l in libs {
            if !inv.libraries.iter().any(|x| x.path == l.path) {
                inv.libraries.push(l);
            }
        }
        shortcuts.extend(steam::read_shortcuts(i, &mut warnings));
    }

    let mut manifests: HashMap<u32, String> = HashMap::new();
    for l in &inv.libraries {
        for m in steam::read_manifests(l, &mut warnings) {
            manifests.insert(m.appid, m.name);
        }
    }

    // Collect cache dirs: (library idx, appid, path)
    let mut todo: Vec<(usize, u32, PathBuf)> = Vec::new();
    for (li, l) in inv.libraries.iter().enumerate() {
        let sc = l.steamapps.join("shadercache");
        let Ok(rd) = fs::read_dir(&sc) else { continue };
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            let name = e.file_name().to_string_lossy().into_owned();
            let Ok(appid) = name.parse::<u32>() else {
                continue;
            };
            if ft.is_symlink() {
                warnings.push(format!(
                    "{} is a symlink; ignored for safety",
                    e.path().display()
                ));
                continue;
            }
            if ft.is_dir() {
                todo.push((li, appid, e.path()));
            }
        }
    }

    let globals = discover_globals(opts, &mut warnings);
    let total = todo.len() + globals.len();
    let done = AtomicUsize::new(0);
    let tick = |name: String| {
        let d = done.fetch_add(1, Ordering::Relaxed) + 1;
        progress(Progress {
            done: d,
            total,
            current: name,
        });
    };

    let libs = &inv.libraries;
    let mut entries: Vec<CacheEntry> = todo
        .par_iter()
        .map(|(li, appid, path)| {
            let st = dir_stats(path);
            let lib = &libs[*li];
            let compat = lib.steamapps.join("compatdata").join(appid.to_string());
            let shortcut = shortcuts.get(appid);
            let installed = manifests.contains_key(appid) || shortcut.is_some();
            let name = manifests
                .get(appid)
                .or(shortcut)
                .cloned()
                .unwrap_or_else(|| format!("Unknown app {appid}"));
            tick(name.clone());
            CacheEntry {
                appid: *appid,
                name,
                library: lib.path.clone(),
                library_index: *li,
                path: path.clone(),
                size: st.size,
                files: st.files,
                modified: st.modified,
                installed,
                is_shortcut: shortcut.is_some() && !manifests.contains_key(appid),
                compat_path: compat.is_dir().then_some(compat),
                children: st.children,
                dev: fs::metadata(path).map(|m| m.dev()).unwrap_or(0),
            }
        })
        .collect();
    entries.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then(a.appid.cmp(&b.appid))
    });
    inv.entries = entries;

    inv.globals = globals
        .par_iter()
        .map(|(label, path, src)| {
            let st = dir_stats(path);
            tick(label.clone());
            GlobalCache {
                label: label.clone(),
                path: path.clone(),
                source: *src,
                size: st.size,
                files: st.files,
                modified: st.modified,
                children: st.children,
            }
        })
        .collect();

    inv.warnings = warnings;
    inv.scan_ms = t0.elapsed().as_millis();
    inv
}

impl Inventory {
    /// Directories that should be watched for changes (shadercache roots).
    pub fn watch_dirs(&self) -> Vec<PathBuf> {
        self.libraries
            .iter()
            .map(|l| l.steamapps.join("shadercache"))
            .filter(|p| p.is_dir())
            .collect()
    }

    /// Display name of the drive a library lives on (mount point when determinable).
    pub fn drive_name(&self, library_index: usize) -> String {
        let Some(l) = self.libraries.get(library_index) else {
            return "?".into();
        };
        mount_point(&l.path).unwrap_or_else(|| l.path.display().to_string())
    }
}

fn mount_point(path: &Path) -> Option<String> {
    let dev = fs::metadata(path).ok()?.dev();
    let (maj, min) = (
        ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff),
        (dev & 0xff) | ((dev >> 12) & !0xff),
    );
    let want = format!("{maj}:{min}");
    let mi = fs::read_to_string("/proc/self/mountinfo").ok()?;
    let mut best: Option<String> = None;
    for line in mi.lines() {
        let mut f = line.split(' ');
        let (_, _, mm, _, mp) = (f.next()?, f.next()?, f.next()?, f.next()?, f.next()?);
        if mm == want {
            let mp = mp.replace("\\040", " ");
            if path.starts_with(&mp) && best.as_ref().is_none_or(|b| mp.len() > b.len()) {
                best = Some(mp);
            }
        }
    }
    best
}

/// Names that exist in several libraries produce duplicates; helper for unique appids.
pub fn unique_appids(entries: &[CacheEntry]) -> HashSet<u32> {
    entries.iter().map(|e| e.appid).collect()
}
