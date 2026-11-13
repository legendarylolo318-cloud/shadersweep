//! Steam install, library, app manifest and non-Steam shortcut discovery.

use crate::vdf::{self, Value};
use crate::CoreError;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SteamInstall {
    /// Canonical root of the Steam data dir.
    pub root: PathBuf,
    pub flatpak: bool,
}

/// Find every Steam install under `home`, deduplicated by canonical path.
pub fn find_installs(home: &Path) -> Vec<SteamInstall> {
    let candidates = [
        home.join(".local/share/Steam"),
        home.join(".steam/steam"),
        home.join(".steam/root"),
        home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam"),
        home.join(".var/app/com.valvesoftware.Steam/data/Steam"),
    ];
    let flatpak_prefix = home.join(".var/app/com.valvesoftware.Steam");
    let mut out: Vec<SteamInstall> = Vec::new();
    for c in candidates {
        let Ok(canon) = fs::canonicalize(&c) else {
            continue;
        };
        if !canon.join("steamapps").is_dir() {
            continue;
        }
        if out.iter().any(|i| i.root == canon) {
            continue;
        }
        let flatpak = canon.starts_with(&flatpak_prefix)
            || fs::canonicalize(&flatpak_prefix)
                .map(|p| canon.starts_with(p))
                .unwrap_or(false);
        out.push(SteamInstall {
            root: canon,
            flatpak,
        });
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Library {
    /// Canonical library root (contains `steamapps`).
    pub path: PathBuf,
    pub steamapps: PathBuf,
}

/// Parse `libraryfolders.vdf` and return every existing library (install root included).
pub fn find_libraries(install: &SteamInstall, warnings: &mut Vec<String>) -> Vec<Library> {
    let mut paths: Vec<PathBuf> = vec![install.root.clone()];
    let candidates = [
        install.root.join("steamapps/libraryfolders.vdf"),
        install.root.join("config/libraryfolders.vdf"),
    ];
    for f in candidates {
        let Ok(text) = fs::read_to_string(&f) else {
            continue;
        };
        match vdf::parse_text(&text) {
            Ok(v) => {
                if let Some(lf) = v.get("libraryfolders") {
                    for (_, entry) in lf.children() {
                        // New format: object with "path". Old format: plain string path.
                        let p = match entry {
                            Value::Obj(_) => entry.get("path").and_then(Value::as_str),
                            Value::Str(s) => Some(s.as_str()),
                            Value::Int(_) => None,
                        };
                        if let Some(p) = p {
                            paths.push(PathBuf::from(p));
                        }
                    }
                }
                break;
            }
            Err(e) => warnings.push(format!("{}: {e}", f.display())),
        }
    }
    let mut libs: Vec<Library> = Vec::new();
    for p in paths {
        let steamapps = p.join("steamapps");
        match (fs::canonicalize(&p), fs::canonicalize(&steamapps)) {
            (Ok(path), Ok(steamapps)) => {
                if !libs.iter().any(|l| l.path == path) {
                    libs.push(Library { path, steamapps });
                }
            }
            _ => warnings.push(format!(
                "library {} is listed but not accessible (drive unmounted?)",
                p.display()
            )),
        }
    }
    libs
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub appid: u32,
    pub name: String,
    pub installdir: String,
}

/// Read all `appmanifest_<appid>.acf` in a library's steamapps dir.
pub fn read_manifests(lib: &Library, warnings: &mut Vec<String>) -> Vec<Manifest> {
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir(&lib.steamapps) else {
        return out;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(id) = name
            .strip_prefix("appmanifest_")
            .and_then(|s| s.strip_suffix(".acf"))
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let parsed = fs::read_to_string(e.path())
            .map_err(|e| e.to_string())
            .and_then(|t| vdf::parse_text(&t).map_err(|e| e.to_string()));
        match parsed {
            Ok(v) => {
                let st = v.get("AppState");
                let nm = st
                    .and_then(|s| s.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let dir = st
                    .and_then(|s| s.get("installdir"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                out.push(Manifest {
                    appid: id,
                    name: if nm.is_empty() { dir.clone() } else { nm },
                    installdir: dir,
                });
            }
            Err(msg) => warnings.push(format!("{}: {msg}", e.path().display())),
        }
    }
    out
}

/// Steam's shortcut app id for a non-Steam game: crc32(exe + name) | 0x80000000.
pub fn shortcut_appid(exe: &str, appname: &str) -> u32 {
    let mut h = crc32fast::Hasher::new();
    h.update(exe.as_bytes());
    h.update(appname.as_bytes());
    h.finalize() | 0x8000_0000
}

/// Parse a binary `shortcuts.vdf` into appid → name.
pub fn parse_shortcuts(bytes: &[u8]) -> Result<BTreeMap<u32, String>, vdf::VdfError> {
    let v = vdf::parse_binary(bytes)?;
    let mut out = BTreeMap::new();
    if let Some(sc) = v.get("shortcuts") {
        for (_, e) in sc.children() {
            let name = e.get("appname").and_then(Value::as_str).unwrap_or("");
            let exe = e.get("exe").and_then(Value::as_str).unwrap_or("");
            if name.is_empty() && exe.is_empty() {
                continue;
            }
            let id = match e.get("appid").and_then(Value::as_u64) {
                Some(n) => n as u32,
                None => shortcut_appid(exe, name),
            };
            out.insert(id, name.to_string());
        }
    }
    Ok(out)
}

/// All shortcuts across every `userdata/*/config/shortcuts.vdf` of an install.
pub fn read_shortcuts(install: &SteamInstall, warnings: &mut Vec<String>) -> HashMap<u32, String> {
    let mut out = HashMap::new();
    let Ok(rd) = fs::read_dir(install.root.join("userdata")) else {
        return out;
    };
    for u in rd.flatten() {
        let f = u.path().join("config/shortcuts.vdf");
        let Ok(bytes) = fs::read(&f) else { continue };
        match parse_shortcuts(&bytes) {
            Ok(m) => out.extend(m),
            Err(e) => warnings.push(format!("{}: {e}", f.display())),
        }
    }
    out
}

pub fn read_file_vdf(path: &Path) -> Result<Value, CoreError> {
    let t = fs::read_to_string(path).map_err(|e| CoreError::io(path, e))?;
    vdf::parse_text(&t).map_err(|source| CoreError::Vdf {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcut_id_is_high_bit() {
        let id = shortcut_appid("\"/usr/bin/foo\"", "Foo");
        assert!(id & 0x8000_0000 != 0);
        assert_eq!(id, shortcut_appid("\"/usr/bin/foo\"", "Foo"));
    }

    #[test]
    fn shortcuts_without_appid_field() {
        let mut d = Vec::new();
        d.extend(
            b"\x00shortcuts\x00\x000\x00\x01AppName\x00Foo\x00\x01Exe\x00\"/x\"\x00\x08\x08\x08",
        );
        let m = parse_shortcuts(&d).unwrap();
        assert_eq!(
            m.get(&shortcut_appid("\"/x\"", "Foo")).map(String::as_str),
            Some("Foo")
        );
    }
}
