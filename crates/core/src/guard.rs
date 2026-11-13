//! Safety guards: canonical-path allowlist and running-process detection.

use crate::scan::Inventory;
use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::MetadataExt;
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
    #[error("path is inside or contains protected data (Proton prefixes, game files, Steam config or home): {0}")]
    Protected(PathBuf),
    #[error("directory changed since the scan (replaced or moved); rescan first: {0}")]
    Changed(PathBuf),
}

/// `(st_dev, st_ino)` of a directory, without following symlinks.
pub fn identity(path: &Path) -> Option<(u64, u64)> {
    fs::symlink_metadata(path).ok().map(|m| (m.dev(), m.ino()))
}

/// Paths that may ever be deleted. Built only from discovery results.
#[derive(Debug, Clone, Default)]
pub struct Allowlist {
    /// Canonical `shadercache` dirs: only their *direct* children may be deleted.
    containers: Vec<PathBuf>,
    /// Canonical directories that may be deleted as a whole (global driver caches).
    exact: Vec<PathBuf>,
    /// Canonical directories that must never be deleted, entered, or contained by a deleted
    /// path: every library's `compatdata` and `common`, every install's `userdata` and `config`.
    protected: Vec<PathBuf>,
    /// Canonical `$HOME`: it and its ancestors may never be deleted (caches live inside it).
    home: Option<PathBuf>,
}

fn canon_all(paths: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
    paths
        .into_iter()
        .filter_map(|p| fs::canonicalize(&p).ok().or(p.is_absolute().then_some(p)))
        .collect()
}

impl Allowlist {
    pub fn new(
        containers: Vec<PathBuf>,
        exact: Vec<PathBuf>,
        protected: Vec<PathBuf>,
        home: Option<PathBuf>,
    ) -> Self {
        let mut a = Self {
            containers: Vec::new(),
            exact: Vec::new(),
            protected: canon_all(protected),
            home: home.and_then(|h| canon_all([h]).pop()),
        };
        // A container is only trusted if, after resolving symlinks, it still is a directory
        // named `shadercache` that does not overlap protected data. This stops a symlinked
        // `steamapps/shadercache -> compatdata` (or -> $HOME) from exposing its children.
        a.containers = canon_all(containers)
            .into_iter()
            .filter(|c| {
                c.file_name().is_some_and(|n| n == "shadercache") && !a.touches_protected(c)
            })
            .collect();
        a.exact = canon_all(exact)
            .into_iter()
            .filter(|e| !a.touches_protected(e))
            .collect();
        a
    }

    pub fn from_inventory(inv: &Inventory) -> Self {
        let containers = inv
            .libraries
            .iter()
            .map(|l| l.steamapps.join("shadercache"))
            .collect();
        let exact = inv.globals.iter().map(|g| g.path.clone()).collect();
        let mut protected: Vec<PathBuf> = Vec::new();
        for l in &inv.libraries {
            protected.push(l.steamapps.join("compatdata"));
            protected.push(l.steamapps.join("common"));
        }
        for i in &inv.installs {
            protected.push(i.root.join("userdata"));
            protected.push(i.root.join("config"));
        }
        Self::new(containers, exact, protected, inv.home.clone())
    }

    /// True if `canon` equals, lies inside, or is an ancestor of a protected directory,
    /// or is `$HOME` or one of its ancestors.
    pub fn touches_protected(&self, canon: &Path) -> bool {
        self.protected
            .iter()
            .any(|p| p.starts_with(canon) || canon.starts_with(p))
            || self.home.as_ref().is_some_and(|h| h.starts_with(canon))
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
        if self.touches_protected(&canon) {
            return Err(GuardError::Protected(path.to_path_buf()));
        }
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

    /// `check`, plus: the canonical path still is the very directory seen at scan time
    /// (same device + inode) and still resolves to itself (no ancestor swapped for a symlink).
    pub fn check_identity(
        &self,
        path: &Path,
        expected: Option<(u64, u64)>,
    ) -> Result<PathBuf, GuardError> {
        let canon = self.check(path)?;
        if let Some(want) = expected {
            if identity(&canon) != Some(want) {
                return Err(GuardError::Changed(path.to_path_buf()));
            }
        }
        if fs::canonicalize(&canon).ok().as_deref() != Some(canon.as_path()) {
            return Err(GuardError::Changed(path.to_path_buf()));
        }
        Ok(canon)
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
                .map(|c| is_steam_comm(c.trim()))
                .unwrap_or(false)
        })
    }

    /// Games are detected via Steam's `reaper SteamLaunch AppId=N` wrapper
    /// and the `SteamAppId` / `SteamGameId` environment variables Steam sets for launched games.
    /// Verified against Steam's own log (`Game process added : AppID N "... reaper SteamLaunch
    /// AppId=N -- ... proton waitforexitandrun ..."`): the same reaper wrapper is used for
    /// native and Proton games, so Wine process names (`*.exe`, `wine64-preloader`) don't matter.
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
                            .filter(|&n| n != 0)
                        {
                            out.insert(n);
                        }
                    }
                }
            }
            if let Ok(env) = fs::read(p.join("environ")) {
                for kv in env.split(|&b| b == 0) {
                    if let Some(n) = env_appid(kv) {
                        out.insert(n);
                    }
                }
            }
        }
        out
    }
}

/// `comm` (max 15 chars) of Steam client processes: the native/Flatpak client binary is
/// `steam`, launched by `steam.sh`; the UI runs in `steamwebhelper`.
fn is_steam_comm(c: &str) -> bool {
    matches!(c, "steam" | "steam.sh" | "steamwebhelper")
}

/// App id from one `KEY=VALUE` environment entry of a game process.
/// `SteamAppId=N` is set for Steam games (native and Proton); for non-Steam shortcuts it is
/// `0`, but `SteamGameId` is the 64-bit game id whose high 32 bits are the shortcut app id.
fn env_appid(kv: &[u8]) -> Option<u32> {
    let num = |prefix: &[u8]| {
        kv.strip_prefix(prefix)
            .and_then(|s| std::str::from_utf8(s).ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
    };
    if let Some(n) = num(b"SteamAppId=") {
        return u32::try_from(n).ok().filter(|&n| n != 0);
    }
    if let Some(g) = num(b"SteamGameId=") {
        let id = if g > u64::from(u32::MAX) {
            (g >> 32) as u32
        } else {
            g as u32
        };
        return (id != 0).then_some(id);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_appid_variants() {
        assert_eq!(env_appid(b"SteamAppId=1422450"), Some(1422450));
        assert_eq!(env_appid(b"SteamAppId=0"), None);
        assert_eq!(env_appid(b"SteamAppId=junk"), None);
        assert_eq!(env_appid(b"SteamGameId=322170"), Some(322170));
        // non-Steam shortcut: (appid << 32) | 0x02000000
        let sc: u32 = 0x8123_4567;
        let gid = (u64::from(sc) << 32) | 0x0200_0000;
        assert_eq!(env_appid(format!("SteamGameId={gid}").as_bytes()), Some(sc));
        assert_eq!(env_appid(b"HOME=/x"), None);
    }

    #[test]
    fn steam_comm_names() {
        assert!(is_steam_comm("steam"));
        assert!(is_steam_comm("steamwebhelper"));
        assert!(is_steam_comm("steam.sh"));
        assert!(!is_steam_comm("steamos-session"));
    }
}
