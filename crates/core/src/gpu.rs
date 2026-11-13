//! GPU / driver fingerprint, used to suggest clearing driver caches after a driver update.

use crate::CoreError;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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

/// Run a command, giving up (and killing it) after `timeout`. `None` if it is missing,
/// fails to start, exits non-zero, or times out.
fn output_with_timeout(cmd: &mut Command, timeout: Duration) -> Option<String> {
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut s = Vec::new();
        let _ = stdout.read_to_end(&mut s);
        s
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) if start.elapsed() < timeout => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let out = reader.join().ok()?;
    status
        .filter(|s| s.success())
        .map(|_| String::from_utf8_lossy(&out).into_owned())
}

/// Fallback driver fingerprint when `vulkaninfo` is unavailable: each Vulkan ICD manifest's
/// driver library with its size and mtime (changes whenever Mesa/RADV or another ICD is updated).
pub fn icd_fingerprints(icd_dirs: &[&Path]) -> Vec<String> {
    let mut out = Vec::new();
    for dir in icd_dirs {
        let Ok(rd) = fs::read_dir(dir) else { continue };
        let mut files: Vec<_> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect();
        files.sort();
        for f in files {
            let Some(lib) = fs::read_to_string(&f)
                .ok()
                .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
                .and_then(|v| v.pointer("/ICD/library_path")?.as_str().map(str::to_owned))
            else {
                continue;
            };
            let lp = Path::new(&lib);
            let resolved = if lp.is_absolute() {
                Some(lp.to_path_buf())
            } else {
                ["/usr/lib", "/usr/lib64", "/usr/lib/x86_64-linux-gnu"]
                    .iter()
                    .map(|d| Path::new(d).join(lp))
                    .find(|p| p.exists())
            };
            let Some(md) = resolved.as_ref().and_then(|p| fs::metadata(p).ok()) else {
                continue;
            };
            let mt = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let name = f
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            out.push(format!("{name}: {lib} ({} bytes, mtime {mt})", md.len()));
        }
    }
    out
}

/// Where GPU information is read from (overridable for tests).
pub struct GpuSources<'a> {
    pub drm: &'a Path,
    pub nvidia_version: &'a Path,
    pub icd_dirs: &'a [&'a Path],
    /// Program to run as `vulkaninfo --summary`; `None` to skip.
    pub vulkaninfo: Option<&'a str>,
}

/// Gather GPU info from sysfs, the NVIDIA proc file, and `vulkaninfo` (falling back to Vulkan ICD
/// library fingerprints). Every source is optional: missing files or tools are simply skipped.
pub fn detect(src: &GpuSources) -> GpuInfo {
    let mut info = GpuInfo::default();
    if let Ok(rd) = fs::read_dir(src.drm) {
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
    if let Some(v) = read_trim(src.nvidia_version) {
        if let Some(l) = v.lines().next() {
            info.drivers.push(format!("nvidia {}", l.trim()));
        }
    }
    let mut vk_drivers = false;
    if let Some(prog) = src.vulkaninfo {
        let out = output_with_timeout(Command::new(prog).arg("--summary"), Duration::from_secs(5));
        if let Some(text) = out {
            let (g, d) = parse_vulkaninfo(&text);
            // Prefer readable names from Vulkan over raw PCI ids.
            if !g.is_empty() {
                info.gpus = g;
            }
            vk_drivers = !d.is_empty();
            info.drivers.extend(d);
        }
    }
    if !vk_drivers {
        info.drivers.extend(icd_fingerprints(src.icd_dirs));
    }
    info
}

pub fn detect_system(use_vulkaninfo: bool) -> GpuInfo {
    detect(&GpuSources {
        drm: Path::new("/sys/class/drm"),
        nvidia_version: Path::new("/proc/driver/nvidia/version"),
        icd_dirs: &[
            Path::new("/usr/share/vulkan/icd.d"),
            Path::new("/etc/vulkan/icd.d"),
        ],
        vulkaninfo: use_vulkaninfo.then_some("vulkaninfo"),
    })
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
    fn amd_without_nvidia_or_vulkaninfo() {
        let d = tempfile::tempdir().unwrap();
        let drm = d.path().join("drm");
        let dev = drm.join("card1/device");
        fs::create_dir_all(&dev).unwrap();
        fs::write(dev.join("vendor"), "0x1002\n").unwrap();
        fs::write(dev.join("device"), "0x7550\n").unwrap();
        fs::create_dir_all(drm.join("card1-DP-1")).unwrap();
        let icd = d.path().join("icd.d");
        fs::create_dir_all(&icd).unwrap();
        let lib = d.path().join("libvulkan_radeon.so");
        fs::write(&lib, b"elf").unwrap();
        fs::write(
            icd.join("radeon_icd.x86_64.json"),
            format!("{{\"ICD\":{{\"library_path\":\"{}\"}}}}", lib.display()),
        )
        .unwrap();
        fs::write(icd.join("broken.json"), "{not json").unwrap();
        let info = detect(&GpuSources {
            drm: &drm,
            nvidia_version: &d.path().join("no/nvidia/version"),
            icd_dirs: &[&icd, &d.path().join("missing")],
            vulkaninfo: Some("/nonexistent/vulkaninfo"),
        });
        assert_eq!(info.gpus, vec!["0x1002:0x7550 ()"]);
        assert_eq!(info.drivers.len(), 1);
        assert!(info.drivers[0].starts_with("radeon_icd.x86_64.json: "));
        // A Mesa update (library rewritten) changes the fingerprint.
        fs::write(&lib, b"elf-new-version").unwrap();
        let info2 = detect(&GpuSources {
            drm: &drm,
            nvidia_version: Path::new("/nonexistent"),
            icd_dirs: &[&icd],
            vulkaninfo: None,
        });
        assert_ne!(info.drivers, info2.drivers);
        // Nothing at all: no panic, empty info.
        let none = detect(&GpuSources {
            drm: Path::new("/nonexistent"),
            nvidia_version: Path::new("/nonexistent"),
            icd_dirs: &[],
            vulkaninfo: Some("/nonexistent/vulkaninfo"),
        });
        assert_eq!(none, GpuInfo::default());
    }

    #[test]
    fn hung_tool_times_out() {
        let t0 = Instant::now();
        let out = output_with_timeout(Command::new("sleep").arg("30"), Duration::from_millis(200));
        assert!(out.is_none());
        assert!(t0.elapsed() < Duration::from_secs(5));
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
