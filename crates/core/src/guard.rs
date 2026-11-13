//! Safety guards: canonical-path allowlist and running-process detection.

use crate::scan::Inventory;
use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum GuardError {
    #[error("path is not absolute: {0}")]
    NotAbsolute(PathBuf),
    #[error("path contains '..': {0}")]
    ParentComponent(PathBuf),
    #[error("path does not exist or cannot be inspected: {0}")]
    Missing(PathBuf),
    #[error("refusing to delete a symlink: {0}")]
    Symlink(PathBuf),
    #[error("not a directory: {0}")]
    NotDir(PathBuf),
    #[error("path is outside the discovered cache locations: {0}")]
    Outside(PathBuf),
}

/// Paths that may ever be deleted. Built only from discovery results.
#[derive(Debug, Clone, Default)]
pub struct Allowlist {
    /// Canonical `shadercache` dirs: only their *direct* children may be deleted.
    containers: Vec<PathBuf>,
    /// Canonical directories that may be deleted as a whole (global driver caches).
    exact: Vec<PathBuf>,
}

impl Allowlist {
    pub fn new(containers: Vec<PathBuf>, exact: Vec<PathBuf>) -> Self {
        Self { containers, exact }
    }

    pub fn from_inventory(inv: &Inventory) -> Self {
        let containers = inv
            .libraries
            .iter()
            .filter_map(|l| fs::canonicalize(l.steamapps.join("shadercache")).ok())
            .collect();
        let exact = inv.globals.iter().map(|g| g.path.clone()).collect();
        Self { containers, exact }
    }

    /// Validate `path` right before deletion. Returns the canonical path.
    pub fn check(&self, path: &Path) -> Result<PathBuf, GuardError> {
        if !path.is_absolute() {
            return Err(GuardError::NotAbsolute(path.to_path_buf()));
        }
        if path.components().any(|c| c == Component::ParentDir) {
            return Err(GuardError::ParentComponent(path.to_path_buf()));
        }
        let md = fs::symlink_metadata(path).map_err(|_| GuardError::Missing(path.to_path_buf()))?;
        if md.file_type().is_symlink() {
            return Err(GuardError::Symlink(path.to_path_buf()));
        }
        if !md.is_dir() {
            return Err(GuardError::NotDir(path.to_path_buf()));
        }
        let canon = fs::canonicalize(path).map_err(|_| GuardError::Missing(path.to_path_buf()))?;
        let ok = self.exact.contains(&canon)
            || self
                .containers
                .iter()
                .any(|c| canon.parent() == Some(c.as_path()) && canon != *c);
        if ok {
            Ok(canon)
        } else {
            Err(GuardError::Outside(path.to_path_buf()))
        }
    }
}

/// Process detection, abstracted so it can be mocked in tests.
pub trait ProcessProbe {
    fn steam_running(&self) -> bool;
    fn running_appids(&self) -> HashSet<u32>;
}

/// Reads a procfs tree (normally `/proc`).
pub struct ProcfsProbe {
    pub proc_root: PathBuf,
}

impl Default for ProcfsProbe {
    fn default() -> Self {
        Self {
            proc_root: PathBuf::from("/proc"),
        }
    }
}

impl ProcfsProbe {
    fn pids(&self) -> Vec<PathBuf> {
        fs::read_dir(&self.proc_root)
            .map(|rd| {
                rd.flatten()
                    .filter(|e| {
                        e.file_name()
                            .to_string_lossy()
                            .bytes()
                            .all(|b| b.is_ascii_digit())
                    })
                    .map(|e| e.path())
                    .collect()
            })
            .unwrap_or_default()
    }
}

impl ProcessProbe for ProcfsProbe {
    fn steam_running(&self) -> bool {
        self.pids().iter().any(|p| {
            fs::read_to_string(p.join("comm"))
                .map(|c| matches!(c.trim(), "steam" | "steamwebhelper"))
                .unwrap_or(false)
        })
    }

    /// Games are detected via Steam's `reaper SteamLaunch AppId=N` wrapper
    /// and the `SteamAppId=N` environment variable Steam sets for launched games.
    fn running_appids(&self) -> HashSet<u32> {
        let mut out = HashSet::new();
        for p in self.pids() {
            if let Ok(cmd) = fs::read(p.join("cmdline")) {
                let args: Vec<&[u8]> = cmd.split(|&b| b == 0).collect();
                if args.iter().any(|a| *a == b"SteamLaunch") {
                    for a in &args {
                        if let Some(n) = a
                            .strip_prefix(b"AppId=")
                            .and_then(|s| std::str::from_utf8(s).ok())
                            .and_then(|s| s.parse::<u32>().ok())
                        {
                            out.insert(n);
                        }
                    }
                }
            }
            if let Ok(env) = fs::read(p.join("environ")) {
                for kv in env.split(|&b| b == 0) {
                    if let Some(n) = kv
                        .strip_prefix(b"SteamAppId=")
                        .and_then(|s| std::str::from_utf8(s).ok())
                        .and_then(|s| s.parse::<u32>().ok())
                    {
                        if n != 0 {
                            out.insert(n);
                        }
                    }
                }
            }
        }
        out
    }
}
