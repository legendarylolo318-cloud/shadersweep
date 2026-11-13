//! Read-only look at Steam's shader pre-caching setting. Never writes Steam config.

use crate::steam::{self, SteamInstall};
use crate::vdf::Value;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreCaching {
    Enabled,
    Disabled,
    /// The setting is not stored in any file we can read (it is not on the reference machine).
    Unknown,
}

/// Key names searched for (case-insensitive). The real on-disk key is undocumented;
/// none of these exist in the config files inspected during development.
const KEYS: [&str; 4] = [
    "EnableShaderPreCaching",
    "ShaderPreCaching",
    "ShaderPrecachingEnabled",
    "DisableShaderPreCaching",
];

pub fn shader_precaching(install: &SteamInstall) -> PreCaching {
    let mut files: Vec<PathBuf> = vec![install.root.join("config/config.vdf")];
    if let Ok(rd) = std::fs::read_dir(install.root.join("userdata")) {
        for u in rd.flatten() {
            files.push(u.path().join("config/localconfig.vdf"));
        }
    }
    for f in files {
        let Ok(v) = steam::read_file_vdf(&f) else {
            continue;
        };
        for k in KEYS {
            let mut hits: Vec<&Value> = Vec::new();
            v.find_all(k, &mut hits);
            if let Some(n) = hits.iter().find_map(|h| h.as_u64()) {
                let inverted = k.starts_with("Disable");
                return if (n != 0) != inverted {
                    PreCaching::Enabled
                } else {
                    PreCaching::Disabled
                };
            }
        }
    }
    PreCaching::Unknown
}
