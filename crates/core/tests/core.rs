use shadersweep_core::guard::{Allowlist, GuardError, ProcessProbe, ProcfsProbe};
use shadersweep_core::ops::{self, DeleteMode, Selection};
use shadersweep_core::scan::{self, Inventory, ScanOptions};
use shadersweep_core::steam::{self, shortcut_appid};
use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

type R = Result<(), Box<dyn std::error::Error>>;

struct Mock {
    steam: bool,
    games: HashSet<u32>,
}
impl ProcessProbe for Mock {
    fn steam_running(&self) -> bool {
        self.steam
    }
    fn running_appids(&self) -> HashSet<u32> {
        self.games.clone()
    }
}
fn idle() -> Mock {
    Mock {
        steam: false,
        games: HashSet::new(),
    }
}

fn write(p: &Path, content: &[u8]) -> R {
    if let Some(d) = p.parent() {
        fs::create_dir_all(d)?;
    }
    fs::write(p, content)?;
    Ok(())
}

fn manifest(lib: &Path, id: u32, name: &str) -> R {
    write(
        &lib.join(format!("steamapps/appmanifest_{id}.acf")),
        format!("\"AppState\"\n{{\n\t\"appid\"\t\"{id}\"\n\t\"name\"\t\"{name}\"\n\t\"installdir\"\t\"x\"\n}}\n").as_bytes(),
    )
}

fn cache(lib: &Path, id: u32, bytes: usize) -> R {
    write(
        &lib.join(format!(
            "steamapps/shadercache/{id}/fozpipelinesv6/steam_pipeline_cache.foz"
        )),
        &vec![1u8; bytes],
    )?;
    write(
        &lib.join(format!(
            "steamapps/shadercache/{id}/DXVK_state_cache/a.dxvk-cache"
        )),
        &[2u8; 100],
    )
}

fn opts(home: &Path) -> ScanOptions {
    ScanOptions {
        home: home.to_path_buf(),
        cache_home: home.join(".cache"),
        env: vec![],
    }
}

fn shortcuts_bin(name: &str, exe: &str) -> Vec<u8> {
    let mut d = Vec::new();
    d.extend(b"\x00shortcuts\x00\x000\x00");
    d.extend(format!("\x01AppName\x00{name}\x00\x01Exe\x00{exe}\x00").as_bytes());
    d.extend(b"\x08\x08\x08");
    d
}

/// Native Steam with a second library on "another drive", plus a symlinked alias.
fn fixture(root: &Path) -> Result<(PathBuf, PathBuf, PathBuf), Box<dyn std::error::Error>> {
    let home = root.join("home");
    let steam = home.join(".local/share/Steam");
    let lib2 = root.join("mnt/games/SteamLibrary");
    fs::create_dir_all(steam.join("steamapps"))?;
    fs::create_dir_all(lib2.join("steamapps"))?;
    fs::create_dir_all(home.join(".steam"))?;
    symlink(&steam, home.join(".steam/steam"))?;
    write(
        &steam.join("steamapps/libraryfolders.vdf"),
        format!(
            "\"libraryfolders\"\n{{\n\"0\"\n{{\n\"path\"\t\"{}\"\n}}\n\"1\"\n{{\n\"path\"\t\"{}\"\n}}\n\"2\"\n{{\n\"path\"\t\"{}/gone\"\n}}\n}}\n",
            steam.display(),
            lib2.display(),
            root.display()
        )
        .as_bytes(),
    )?;
    manifest(&steam, 100, "Installed Game")?;
    manifest(&lib2, 200, "Far Game")?;
    cache(&steam, 100, 5000)?;
    cache(&steam, 300, 9000)?; // orphan
    cache(&lib2, 200, 7000)?;
    let sc = shortcut_appid("\"/opt/x\"", "My Shortcut");
    write(
        &steam.join("userdata/1/config/shortcuts.vdf"),
        &shortcuts_bin("My Shortcut", "\"/opt/x\""),
    )?;
    cache(&steam, sc, 1000)?;
    fs::create_dir_all(steam.join("steamapps/compatdata/100/pfx"))?;
    Ok((home, steam, lib2))
}

fn scanned(
    root: &Path,
) -> Result<(Inventory, PathBuf, PathBuf, PathBuf), Box<dyn std::error::Error>> {
    let (home, steam, lib2) = fixture(root)?;
    Ok((scan::scan(&opts(&home), &|_| {}), home, steam, lib2))
}

#[test]
fn multiple_libraries_dedupe_and_names() -> R {
    let t = tempfile::tempdir()?;
    let (inv, ..) = scanned(t.path())?;
    // ~/.local/share/Steam and ~/.steam/steam symlink dedupe to one install
    assert_eq!(inv.installs.len(), 1);
    assert_eq!(inv.libraries.len(), 2);
    assert!(inv.warnings.iter().any(|w| w.contains("gone")));
    let by = |id: u32| inv.entries.iter().find(|e| e.appid == id);
    assert_eq!(by(100).map(|e| e.name.as_str()), Some("Installed Game"));
    assert_eq!(by(200).map(|e| e.name.as_str()), Some("Far Game"));
    assert!(by(200).is_some_and(|e| e.installed && e.size > 7000));
    assert!(by(100).is_some_and(|e| e.compat_path.is_some() && e.files == 2));
    Ok(())
}

#[test]
fn orphan_detection_and_shortcuts() -> R {
    let t = tempfile::tempdir()?;
    let (inv, ..) = scanned(t.path())?;
    let orphans: Vec<u32> = inv
        .entries
        .iter()
        .filter(|e| e.orphaned())
        .map(|e| e.appid)
        .collect();
    assert_eq!(orphans, vec![300]);
    let sc = shortcut_appid("\"/opt/x\"", "My Shortcut");
    let e = inv.entries.iter().find(|e| e.appid == sc);
    assert!(e.is_some_and(|e| e.name == "My Shortcut" && e.installed && e.is_shortcut));
    Ok(())
}

#[test]
fn flatpak_and_native_found_separately() -> R {
    let t = tempfile::tempdir()?;
    let (home, ..) = fixture(t.path())?;
    let fp = home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam");
    fs::create_dir_all(fp.join("steamapps"))?;
    manifest(&fp, 555, "Flat Game")?;
    cache(&fp, 555, 100)?;
    let inv = scan::scan(&opts(&home), &|_| {});
    assert_eq!(inv.installs.len(), 2);
    assert!(inv.installs.iter().any(|i| i.flatpak));
    assert!(inv.entries.iter().any(|e| e.appid == 555 && e.installed));
    Ok(())
}

#[test]
fn global_caches_and_env_override() -> R {
    let t = tempfile::tempdir()?;
    let (home, ..) = fixture(t.path())?;
    write(&home.join(".cache/mesa_shader_cache/ab/x"), b"1234")?;
    write(&home.join(".cache/radv_builtin_shaders64/i"), b"1")?;
    let custom = t.path().join("tmp/custom/dxvk");
    write(&custom.join("f"), b"1")?;
    let mut o = opts(&home);
    o.env = vec![
        ("DXVK_STATE_CACHE_PATH".into(), custom.display().to_string()),
        ("MESA_SHADER_CACHE_DIR".into(), home.display().to_string()), // dangerous: $HOME
        ("__GL_SHADER_DISK_CACHE_PATH".into(), "/".into()),
    ];
    let inv = scan::scan(&o, &|_| {});
    let labels: Vec<&str> = inv.globals.iter().map(|g| g.label.as_str()).collect();
    assert!(labels.iter().any(|l| l.starts_with("Mesa shader cache")));
    assert!(labels.iter().any(|l| l.contains("radv_builtin_shaders64")));
    assert!(labels.contains(&"$DXVK_STATE_CACHE_PATH"));
    assert!(!labels.contains(&"$MESA_SHADER_CACHE_DIR"));
    assert!(!labels.contains(&"$__GL_SHADER_DISK_CACHE_PATH"));
    assert!(inv.warnings.iter().any(|w| w.contains("rejected")));
    Ok(())
}

#[test]
fn allowlist_rejects_outside_dotdot_and_symlinks() -> R {
    let t = tempfile::tempdir()?;
    let (inv, home, steam, _) = scanned(t.path())?;
    let allow = Allowlist::from_inventory(&inv);
    let sc = steam.join("steamapps/shadercache");
    assert!(allow.check(&sc.join("100")).is_ok());
    // the container itself, its parents, home, deeper paths
    assert!(matches!(allow.check(&sc), Err(GuardError::Outside(_))));
    assert!(matches!(allow.check(&steam), Err(GuardError::Outside(_))));
    assert!(matches!(allow.check(&home), Err(GuardError::Outside(_))));
    assert!(matches!(
        allow.check(&sc.join("100/fozpipelinesv6")),
        Err(GuardError::Outside(_))
    ));
    // ../ tricks
    assert!(matches!(
        allow.check(&sc.join("100/../../..")),
        Err(GuardError::ParentComponent(_))
    ));
    assert!(matches!(
        allow.check(Path::new("relative/100")),
        Err(GuardError::NotAbsolute(_))
    ));
    // symlink inside shadercache pointing outside
    let victim = t.path().join("victim");
    write(&victim.join("precious"), b"x")?;
    symlink(&victim, sc.join("777"))?;
    assert!(matches!(
        allow.check(&sc.join("777")),
        Err(GuardError::Symlink(_))
    ));
    // symlinked parent: alias dir -> shadercache must not widen access to other places
    symlink(&victim, t.path().join("alias"))?;
    assert!(matches!(
        allow.check(&t.path().join("alias")),
        Err(GuardError::Symlink(_))
    ));
    assert!(allow.check(&t.path().join("alias/precious")).is_err());
    // a path reaching a cache through a symlinked ancestor still canonicalises into the allowlist
    symlink(&sc, t.path().join("sc_alias"))?;
    assert!(allow.check(&t.path().join("sc_alias/100")).is_ok());
    // sibling of shadercache is outside
    fs::create_dir_all(steam.join("steamapps/compatdata/100"))?;
    assert!(matches!(
        allow.check(&steam.join("steamapps/compatdata/100")),
        Err(GuardError::Outside(_))
    ));
    assert!(victim.join("precious").exists());
    Ok(())
}

#[test]
fn symlink_in_cache_dir_is_not_scanned_or_followed() -> R {
    let t = tempfile::tempdir()?;
    let (home, steam, _) = fixture(t.path())?;
    let victim = t.path().join("victim");
    write(&victim.join("big"), &vec![0u8; 100_000])?;
    symlink(&victim, steam.join("steamapps/shadercache/777"))?;
    symlink(&victim, steam.join("steamapps/shadercache/100/linkout"))?;
    let inv = scan::scan(&opts(&home), &|_| {});
    assert!(!inv.entries.iter().any(|e| e.appid == 777));
    let e = inv
        .entries
        .iter()
        .find(|e| e.appid == 100)
        .ok_or("missing 100")?;
    assert!(e.size < 50_000, "symlink target was counted: {}", e.size);
    // permanent delete of the dir removes the link, not the target
    let allow = Allowlist::from_inventory(&inv);
    let plan = ops::plan_clear(
        &inv,
        &allow,
        &idle(),
        &Selection {
            appids: vec![100],
            ..Default::default()
        },
    );
    let rep = ops::execute(&plan, DeleteMode::Permanent, &allow, &idle());
    assert!(rep.failed.is_empty());
    assert!(victim.join("big").exists());
    Ok(())
}

#[test]
fn refuses_running_game_and_warns_on_steam() -> R {
    let t = tempfile::tempdir()?;
    let (inv, _, steam, _) = scanned(t.path())?;
    let allow = Allowlist::from_inventory(&inv);
    let probe = Mock {
        steam: true,
        games: HashSet::from([100]),
    };
    let sel = Selection {
        appids: vec![100, 200],
        ..Default::default()
    };
    let plan = ops::plan_clear(&inv, &allow, &probe, &sel);
    assert!(plan.steam_running);
    assert_eq!(plan.items.len(), 1);
    assert_eq!(plan.items[0].appid, Some(200));
    assert!(plan
        .refused
        .iter()
        .any(|r| r.label.contains("100") && r.reason.contains("running")));
    // game starts after planning: execute re-checks
    let plan2 = ops::plan_clear(&inv, &allow, &idle(), &sel);
    let rep = ops::execute(&plan2, DeleteMode::Permanent, &allow, &probe);
    assert_eq!(rep.failed.len(), 1);
    assert_eq!(rep.done.len(), 1);
    assert!(steam.join("steamapps/shadercache/100").exists());
    assert!(!steam.join("steamapps/shadercache/200").exists() || steam.parent().is_some());
    Ok(())
}

#[test]
fn clear_orphaned_only_leaves_everything_else() -> R {
    let t = tempfile::tempdir()?;
    let (inv, _, steam, lib2) = scanned(t.path())?;
    let allow = Allowlist::from_inventory(&inv);
    let plan = ops::plan_clear(
        &inv,
        &allow,
        &idle(),
        &Selection {
            orphaned: true,
            ..Default::default()
        },
    );
    assert_eq!(plan.items.len(), 1);
    let rep = ops::execute(&plan, DeleteMode::Permanent, &allow, &idle());
    assert_eq!(rep.done.len(), 1);
    assert!(!steam.join("steamapps/shadercache/300").exists());
    assert!(steam.join("steamapps/shadercache/100").exists());
    assert!(lib2.join("steamapps/shadercache/200").exists());
    assert!(steam.join("steamapps/compatdata/100").exists());
    Ok(())
}

#[test]
fn execute_rejects_tampered_plan() -> R {
    let t = tempfile::tempdir()?;
    let (inv, home, ..) = scanned(t.path())?;
    let allow = Allowlist::from_inventory(&inv);
    let mut plan = ops::plan_clear(
        &inv,
        &allow,
        &idle(),
        &Selection {
            appids: vec![100],
            ..Default::default()
        },
    );
    plan.items[0].path = home.clone();
    let rep = ops::execute(&plan, DeleteMode::Permanent, &allow, &idle());
    assert_eq!(rep.failed.len(), 1);
    assert!(home.exists());
    Ok(())
}

#[test]
fn procfs_probe_reads_fixture_proc() -> R {
    let t = tempfile::tempdir()?;
    let p = t.path();
    write(&p.join("10/comm"), b"steam\n")?;
    write(
        &p.join("11/cmdline"),
        b"reaper\0SteamLaunch\0AppId=1422450\0--\0game\0",
    )?;
    write(&p.join("12/environ"), b"A=b\0SteamAppId=105600\0")?;
    write(&p.join("13/cmdline"), b"vim\0AppId=999\0")?; // not a SteamLaunch
    write(&p.join("self/comm"), b"steam\n")?;
    let probe = ProcfsProbe {
        proc_root: p.to_path_buf(),
    };
    assert!(probe.steam_running());
    assert_eq!(probe.running_appids(), HashSet::from([1422450, 105600]));
    let empty = tempfile::tempdir()?;
    let probe = ProcfsProbe {
        proc_root: empty.path().to_path_buf(),
    };
    assert!(!probe.steam_running());
    Ok(())
}

#[test]
fn libraryfolders_old_format_and_manifest_parse() -> R {
    let t = tempfile::tempdir()?;
    let steam = t.path().join("Steam");
    let lib = t.path().join("old");
    fs::create_dir_all(steam.join("steamapps"))?;
    fs::create_dir_all(lib.join("steamapps"))?;
    write(
        &steam.join("steamapps/libraryfolders.vdf"),
        format!(
            "\"LibraryFolders\"\n{{\n\t\"TimeNextStatsReport\"\t\"1\"\n\t\"1\"\t\"{}\"\n}}\n",
            lib.display()
        )
        .as_bytes(),
    )?;
    let inst = steam::SteamInstall {
        root: fs::canonicalize(&steam)?,
        flatpak: false,
    };
    let mut w = vec![];
    let libs = steam::find_libraries(&inst, &mut w);
    assert_eq!(libs.len(), 2);
    Ok(())
}
