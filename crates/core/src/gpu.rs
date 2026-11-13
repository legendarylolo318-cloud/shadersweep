//! GPU / driver fingerprint, used to suggest clearing driver caches after a driver update.

use crate::CoreError;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuInfo {
    pub gpus: Vec<String>,
    pub drivers: Vec<String>,
}

impl GpuInfo {
    pub fn summary(&self) -> String {
        format!("{} | {}", self.gpus.join(", "), self.drivers.join(", "))
    }
}

fn read_trim(p: &Path) -> Option<String> {
    fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}

/// Parse `vulkaninfo --summary` output.
pub fn parse_vulkaninfo(text: &str) -> (Vec<String>, Vec<String>) {
    let (mut gpus, mut drivers) = (Vec::new(), Vec::new());
    let mut name = String::new();
    for l in text.lines() {
        let Some((k, v)) = l.split_once('=') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        match k {
            "deviceName" => {
                name = v.to_string();
                if !gpus.contains(&name) {
                    gpus.push(name.clone());
                }
            }
            "driverName" => name = v.to_string(),
            "driverInfo" => {
                let d = format!("{name} {v}");
                if !drivers.contains(&d) {
                    drivers.push(d);
                }
            }
            _ => {}
        }
    }
    (gpus, drivers)
}

/// Gather GPU info from sysfs, the NVIDIA proc file, and (optionally) `vulkaninfo`.
pub fn detect(drm: &Path, nvidia_version: &Path, use_vulkaninfo: bool) -> GpuInfo {
    let mut info = GpuInfo::default();
    if let Ok(rd) = fs::read_dir(drm) {
        let mut cards: Vec<_> = rd
            .flatten()
            .filter(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                n.strip_prefix("card")
                    .is_some_and(|r| !r.is_empty() && r.bytes().all(|b| b.is_ascii_digit()))
            })
            .collect();
        cards.sort_by_key(|e| e.file_name());
        for c in cards {
            let dev = c.path().join("device");
            let vendor = read_trim(&dev.join("vendor")).unwrap_or_default();
            let id = read_trim(&dev.join("device")).unwrap_or_default();
            let drv = fs::read_link(dev.join("driver"))
                .ok()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                .unwrap_or_default();
            info.gpus.push(format!("{vendor}:{id} ({drv})"));
        }
    }
    if let Some(v) = read_trim(nvidia_version) {
        if let Some(l) = v.lines().next() {
            info.drivers.push(format!("nvidia {}", l.trim()));
        }
    }
    if use_vulkaninfo {
        if let Ok(out) = Command::new("vulkaninfo").arg("--summary").output() {
            if out.status.success() {
                let (g, d) = parse_vulkaninfo(&String::from_utf8_lossy(&out.stdout));
                // Prefer readable names from Vulkan over raw PCI ids.
                if !g.is_empty() {
                    info.gpus = g;
                }
                info.drivers.extend(d);
            }
        }
    }
    info
}

pub fn detect_system(use_vulkaninfo: bool) -> GpuInfo {
    detect(
        Path::new("/sys/class/drm"),
        Path::new("/proc/driver/nvidia/version"),
        use_vulkaninfo,
    )
}

/// Compare with the previously recorded fingerprint, then record the current one.
/// Returns the previous info when it differs (first run counts as "no change").
pub fn check_and_record(state_file: &Path, cur: &GpuInfo) -> Result<Option<GpuInfo>, CoreError> {
    let prev: Option<GpuInfo> = fs::read_to_string(state_file)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok());
    if prev.as_ref() != Some(cur) {
        if let Some(dir) = state_file.parent() {
            fs::create_dir_all(dir).map_err(|e| CoreError::io(dir, e))?;
        }
        fs::write(state_file, serde_json::to_vec_pretty(cur)?)
            .map_err(|e| CoreError::io(state_file, e))?;
    }
    Ok(prev.filter(|p| p != cur))
}

pub fn default_state_file() -> Option<std::path::PathBuf> {
    dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .map(|d| d.join("shadersweep/gpu.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vulkan_summary() {
        let t = "\tdeviceName = AMD Radeon RX 9070 XT (RADV GFX1201)\n\tdriverName = radv\n\tdriverInfo = Mesa 26.2.7\n";
        let (g, d) = parse_vulkaninfo(t);
        assert_eq!(g, vec!["AMD Radeon RX 9070 XT (RADV GFX1201)"]);
        assert_eq!(d, vec!["radv Mesa 26.2.7"]);
    }

    #[test]
    fn change_detection() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("s/gpu.json");
        let a = GpuInfo {
            gpus: vec!["a".into()],
            drivers: vec!["1".into()],
        };
        let b = GpuInfo {
            gpus: vec!["a".into()],
            drivers: vec!["2".into()],
        };
        assert_eq!(check_and_record(&f, &a).unwrap(), None);
        assert_eq!(check_and_record(&f, &a).unwrap(), None);
        assert_eq!(check_and_record(&f, &b).unwrap(), Some(a));
        assert_eq!(check_and_record(&f, &b).unwrap(), None);
    }
}
