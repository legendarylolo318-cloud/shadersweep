//! Attack-style tests for the delete path: everything here must end with protected data intact.

use shadersweep_core::guard::{Allowlist, GuardError, ProcessProbe};
use shadersweep_core::ops::{self, DeleteMode, Selection};
use shadersweep_core::scan::{self, Inventory, ScanOptions};
use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

type R = Result<(), Box<dyn std::error::Error>>;

struct Idle;
impl ProcessProbe for Idle {
    fn steam_running(&self) -> bool {
        false
    }
    fn running_appids(&self) -> HashSet<u32> {
        HashSet::new()
    }
}

fn write(p: &Path, content: &[u8]) -> R {
    if let Some(d) = p.parent() {
        fs::create_dir_all(d)?;
    }
    fs::write(p, content)?;
    Ok(())
}

struct Fx {
    _t: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    steam: PathBuf,
}

impl Fx {
    fn sa(&self) -> PathBuf {
        self.steam.join("steamapps")
    }
    fn opts(&self) -> ScanOptions {
        ScanOptions {
            home: self.home.clone(),
            cache_home: self.home.join(".cache"),
            env: vec![],
        }
    }
    fn scan(&self) -> Inventory {
        scan::scan(&self.opts(), &|_| {})
    }
}

/// One native library with: an installed game (100) with cache, Proton prefix and game files;
/// an orphaned cache (300); Steam config and userdata.
fn fixture() -> Result<Fx, Box<dyn std::error::Error>> {
    let t = tempfile::tempdir()?;
    let root = fs::canonicalize(t.path())?;
    let home = root.join("home");
    let steam = home.join(".local/share/Steam");
    let sa = steam.join("steamapps");
    write(
        &sa.join("appmanifest_100.acf"),
        b"\"AppState\"\n{\n\t\"appid\"\t\"100\"\n\t\"name\"\t\"Game\"\n\t\"installdir\"\t\"Game\"\n}\n",
    )?;
    write(&sa.join("shadercache/100/fozpipelinesv6/a.foz"), &[1; 1000])?;
    write(&sa.join("shadercache/300/DXVK_state_cache/b"), &[2; 500])?;
    write(
        &sa.join("compatdata/100/pfx/drive_c/users/steamuser/save.dat"),
        b"SAVE",
    )?;
    write(&sa.join("compatdata/300/pfx/save.dat"), b"SAVE")?;
    write(&sa.join("common/Game/game.bin"), b"GAME")?;
    write(
        &steam.join("config/config.vdf"),
        b"\"InstallConfigStore\"\n{\n}\n",
    )?;
    write(
        &steam.join("userdata/1/config/localconfig.vdf"),
        b"\"x\"\n{\n}\n",
    )?;
    write(&home.join("Documents/important.txt"), b"DOC")?;
    Ok(Fx {
        _t: t,
        root,
        home,
        steam,
    })
}

fn protected_intact(f: &Fx) {
    let sa = f.sa();
    for p in [
        sa.join("compatdata/100/pfx/drive_c/users/steamuser/save.dat"),
        sa.join("compatdata/300/pfx/save.dat"),
        sa.join("common/Game/game.bin"),
        f.steam.join("config/config.vdf"),
        f.steam.join("userdata/1/config/localconfig.vdf"),
        f.home.join("Documents/important.txt"),
    ] {
        assert!(p.exists(), "protected file was removed: {}", p.display());
    }
}

fn clear_all(inv: &Inventory, allow: &Allowlist) -> ops::ClearReport {
    let sel = Selection {
        appids: inv.entries.iter().map(|e| e.appid).collect(),
        orphaned: true,
        globals: (0..inv.globals.len()).collect(),
        ..Default::default()
    };
    let plan = ops::plan_clear(inv, allow, &Idle, &sel);
    ops::execute(&plan, DeleteMode::Permanent, allow, &Idle)
}

#[test]
fn compatdata_and_common_can_never_be_deleted() -> R {
    let f = fixture()?;
    let inv = f.scan();
    let allow = Allowlist::from_inventory(&inv);
    let sa = f.sa();
    for p in [
        sa.join("compatdata"),
        sa.join("compatdata/100"),
        sa.join("compatdata/100/pfx"),
        sa.join("common"),
        sa.join("common/Game"),
        sa.clone(),
        f.steam.clone(),
        f.steam.join("config"),
        f.steam.join("userdata/1"),
    ] {
        assert!(
            matches!(
                allow.check(&p),
                Err(GuardError::Protected(_) | GuardError::Outside(_))
            ),
            "{} was allowed",
            p.display()
        );
    }
    // A plan tampered to point at a prefix is refused at execution time.
    let mut plan = ops::plan_clear(
        &inv,
        &allow,
        &Idle,
        &Selection {
            appids: vec![100],
            ..Default::default()
        },
    );
    assert_eq!(plan.items.len(), 1);
    plan.items[0].path = sa.join("compatdata/100");
    plan.items[0].identity = None;
    let rep = ops::execute(&plan, DeleteMode::Permanent, &allow, &Idle);
    assert_eq!(rep.done.len(), 0);
    assert!(rep.failed[0].1.contains("protected"), "{:?}", rep.failed);
    // Even an allowlist built by hand with compatdata/common as "caches" refuses them.
    let evil = Allowlist::new(
        vec![sa.join("compatdata"), sa.join("common")],
        vec![sa.join("compatdata/100"), sa.join("common/Game")],
        vec![sa.join("compatdata"), sa.join("common")],
        Some(f.home.clone()),
    );
    for p in [sa.join("compatdata/100"), sa.join("common/Game")] {
        assert!(evil.check(&p).is_err());
    }
    // Clearing literally everything that was discovered leaves protected data alone.
    let rep = clear_all(&inv, &allow);
    assert_eq!(rep.done.len(), 2);
    protected_intact(&f);
    Ok(())
}

#[test]
fn shadercache_symlinked_to_compatdata_exposes_nothing() -> R {
    let f = fixture()?;
    let sa = f.sa();
    fs::remove_dir_all(sa.join("shadercache"))?;
    symlink(sa.join("compatdata"), sa.join("shadercache"))?;
    let inv = f.scan();
    assert!(inv.entries.is_empty(), "prefixes listed as caches");
    assert!(inv.warnings.iter().any(|w| w.contains("not a shadercache")));
    let allow = Allowlist::from_inventory(&inv);
    assert!(allow.check(&sa.join("shadercache/100")).is_err());
    assert!(allow.check(&sa.join("compatdata/100")).is_err());
    protected_intact(&f);
    Ok(())
}

#[test]
fn shadercache_symlinked_to_home_exposes_nothing() -> R {
    let f = fixture()?;
    let sa = f.sa();
    fs::remove_dir_all(sa.join("shadercache"))?;
    write(&f.home.join("12345/keep.txt"), b"x")?;
    symlink(&f.home, sa.join("shadercache"))?;
    let inv = f.scan();
    assert!(inv.entries.is_empty());
    let allow = Allowlist::from_inventory(&inv);
    assert!(allow.check(&f.home.join("12345")).is_err());
    assert!(f.home.join("12345/keep.txt").exists());
    Ok(())
}

#[test]
fn symlink_inside_cache_pointing_to_home() -> R {
    let f = fixture()?;
    let sc = f.sa().join("shadercache");
    symlink(&f.home, sc.join("100/fozpipelinesv6/escape"))?;
    symlink(&f.home, sc.join("4242"))?;
    let inv = f.scan();
    assert!(!inv.entries.iter().any(|e| e.appid == 4242));
    let allow = Allowlist::from_inventory(&inv);
    assert!(matches!(
        allow.check(&sc.join("4242")),
        Err(GuardError::Symlink(_))
    ));
    let rep = clear_all(&inv, &allow);
    assert!(rep.failed.is_empty(), "{:?}", rep.failed);
    assert!(!sc.join("100").exists());
    protected_intact(&f);
    Ok(())
}

#[test]
fn dotdot_and_non_numeric_names() -> R {
    let f = fixture()?;
    let sc = f.sa().join("shadercache");
    fs::create_dir_all(sc.join("..2"))?;
    fs::create_dir_all(sc.join("12abc"))?;
    let inv = f.scan();
    let ids: Vec<u32> = inv.entries.iter().map(|e| e.appid).collect();
    assert_eq!(ids.len(), 2, "{ids:?}");
    let allow = Allowlist::from_inventory(&inv);
    for p in [
        sc.join("../.."),
        sc.join("100/../../compatdata/100"),
        sc.join("100/../.."),
    ] {
        assert!(matches!(
            allow.check(&p),
            Err(GuardError::ParentComponent(_))
        ));
    }
    Ok(())
}

#[test]
fn libraryfolders_pointing_at_root_and_home() -> R {
    let f = fixture()?;
    let sa = f.sa();
    // $HOME has a steamapps dir with a shadercache: it is a "library" but only its shadercache
    // children can be touched, never $HOME itself.
    write(&f.home.join("steamapps/shadercache/777/x"), b"x")?;
    write(
        &sa.join("libraryfolders.vdf"),
        format!(
            "\"libraryfolders\"\r\n{{\r\n\"0\"\r\n{{\r\n\"path\"\t\"/\"\r\n}}\r\n\"1\"\r\n{{\r\n\"path\"\t\"{}\"\r\n}}\r\n\"2\"\r\n{{\r\n\"path\"\t\"{}/../..\"\r\n}}\r\n}}\r\n",
            f.home.display(),
            f.home.display()
        )
        .as_bytes(),
    )?;
    let inv = f.scan();
    let allow = Allowlist::from_inventory(&inv);
    for p in [
        PathBuf::from("/"),
        f.home.clone(),
        f.root.clone(),
        f.home.join("steamapps"),
        f.home.join("Documents"),
    ] {
        assert!(allow.check(&p).is_err(), "{} allowed", p.display());
    }
    let rep = clear_all(&inv, &allow);
    assert!(rep.failed.is_empty(), "{:?}", rep.failed);
    assert!(!f.home.join("steamapps/shadercache/777").exists());
    protected_intact(&f);
    Ok(())
}

#[test]
fn race_dir_replaced_by_symlink_after_scan() -> R {
    let f = fixture()?;
    let inv = f.scan();
    let allow = Allowlist::from_inventory(&inv);
    let sc = f.sa().join("shadercache");
    let plan = ops::plan_clear(
        &inv,
        &allow,
        &Idle,
        &Selection {
            appids: vec![100],
            ..Default::default()
        },
    );
    fs::remove_dir_all(sc.join("100"))?;
    symlink(f.home.join("Documents"), sc.join("100"))?;
    let rep = ops::execute(&plan, DeleteMode::Permanent, &allow, &Idle);
    assert_eq!(rep.done.len(), 0);
    assert!(rep.failed[0].1.contains("symlink"), "{:?}", rep.failed);
    protected_intact(&f);
    Ok(())
}

#[test]
fn race_dir_swapped_for_another_dir_after_scan() -> R {
    let f = fixture()?;
    let inv = f.scan();
    let allow = Allowlist::from_inventory(&inv);
    let sc = f.sa().join("shadercache");
    let plan = ops::plan_clear(
        &inv,
        &allow,
        &Idle,
        &Selection {
            appids: vec![100],
            ..Default::default()
        },
    );
    fs::rename(sc.join("100"), f.root.join("moved"))?;
    fs::create_dir(sc.join("100"))?;
    let rep = ops::execute(&plan, DeleteMode::Permanent, &allow, &Idle);
    assert_eq!(rep.done.len(), 0);
    assert!(rep.failed[0].1.contains("changed"), "{:?}", rep.failed);
    assert!(f.root.join("moved/fozpipelinesv6/a.foz").exists());
    Ok(())
}

#[test]
fn race_container_replaced_by_symlink_after_scan() -> R {
    let f = fixture()?;
    let inv = f.scan();
    let allow = Allowlist::from_inventory(&inv);
    let sa = f.sa();
    let plan = ops::plan_clear(
        &inv,
        &allow,
        &Idle,
        &Selection {
            appids: vec![100],
            ..Default::default()
        },
    );
    // shadercache becomes a link to compatdata, which also has a "100" child.
    fs::rename(sa.join("shadercache"), f.root.join("old_sc"))?;
    symlink(sa.join("compatdata"), sa.join("shadercache"))?;
    let rep = ops::execute(&plan, DeleteMode::Permanent, &allow, &Idle);
    assert_eq!(rep.done.len(), 0, "{:?}", rep.done);
    protected_intact(&f);
    Ok(())
}

#[test]
fn env_overrides_only_offer_driver_subdirs() -> R {
    let f = fixture()?;
    let cache = f.home.join(".cache");
    write(&cache.join("mesa_shader_cache/ab/x"), b"1")?;
    write(&cache.join("other_app/data"), b"keep")?;
    let custom = f.root.join("srv/shaders");
    write(&custom.join("mesa_shader_cache_db/part"), b"1")?;
    write(&custom.join("GLCache/n"), b"1")?;
    write(&custom.join("unrelated.txt"), b"keep")?;
    let dxvk = f.root.join("srv/dxvk");
    write(&dxvk.join("game.dxvk-cache"), b"1")?;
    let mut o = f.opts();
    o.env = vec![
        // ~/.cache itself is 4 components deep and used to pass the "too broad" check.
        ("MESA_SHADER_CACHE_DIR".into(), cache.display().to_string()),
        (
            "__GL_SHADER_DISK_CACHE_PATH".into(),
            custom.display().to_string(),
        ),
        ("DXVK_STATE_CACHE_PATH".into(), dxvk.display().to_string()),
    ];
    let inv = scan::scan(&o, &|_| {});
    let paths: Vec<&Path> = inv.globals.iter().map(|g| g.path.as_path()).collect();
    assert!(!paths.contains(&cache.as_path()));
    assert!(!paths.contains(&custom.as_path()));
    assert!(!paths.contains(&dxvk.as_path()));
    assert!(paths.contains(&cache.join("mesa_shader_cache").as_path()));
    assert!(paths.contains(&custom.join("GLCache").as_path()));
    assert!(inv
        .warnings
        .iter()
        .any(|w| w.contains("DXVK_STATE_CACHE_PATH")));
    let allow = Allowlist::from_inventory(&inv);
    let rep = clear_all(&inv, &allow);
    assert!(rep.failed.is_empty(), "{:?}", rep.failed);
    assert!(cache.join("other_app/data").exists());
    assert!(custom.join("unrelated.txt").exists());
    assert!(dxvk.join("game.dxvk-cache").exists());
    protected_intact(&f);
    Ok(())
}

#[test]
fn gui_style_path_selection_clears_only_that_library() -> R {
    let f = fixture()?;
    let lib2 = f.root.join("mnt/SteamLibrary");
    write(&lib2.join("steamapps/shadercache/100/x"), b"other drive")?;
    write(
        &f.sa().join("libraryfolders.vdf"),
        format!(
            "\"libraryfolders\"\n{{\n\"0\" {{ \"path\" \"{}\" }}\n\"1\" {{ \"path\" \"{}\" }}\n}}\n",
            f.steam.display(),
            lib2.display()
        )
        .as_bytes(),
    )?;
    let inv = f.scan();
    let both: Vec<_> = inv.entries.iter().filter(|e| e.appid == 100).collect();
    assert_eq!(both.len(), 2);
    let allow = Allowlist::from_inventory(&inv);
    let target = f.sa().join("shadercache/100");
    let plan = ops::plan_clear(
        &inv,
        &allow,
        &Idle,
        &Selection {
            paths: vec![target.clone()],
            ..Default::default()
        },
    );
    assert_eq!(plan.items.len(), 1);
    let rep = ops::execute(&plan, DeleteMode::Permanent, &allow, &Idle);
    assert_eq!(rep.done.len(), 1);
    assert!(!target.exists());
    assert!(lib2.join("steamapps/shadercache/100/x").exists());
    Ok(())
}

fn snapshot(dir: &Path) -> Vec<(PathBuf, u64, SystemTime)> {
    let mut v: Vec<_> = walkdir_files(dir)
        .into_iter()
        .filter_map(|p| {
            let m = fs::symlink_metadata(&p).ok()?;
            Some((p, m.len(), m.modified().ok()?))
        })
        .collect();
    v.sort();
    v
}

fn walkdir_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        if let Ok(rd) = fs::read_dir(&d) {
            for e in rd.flatten() {
                let p = e.path();
                if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    stack.push(p);
                } else {
                    out.push(p);
                }
            }
        }
    }
    out
}

#[test]
fn steam_config_is_never_written() -> R {
    let f = fixture()?;
    write(
        &f.steam.join("userdata/1/config/shortcuts.vdf"),
        b"\x00shortcuts\x00\x000\x00\x01AppName\x00X\x00\x01Exe\x00x\x00\x08\x08\x08",
    )?;
    let before_cfg = snapshot(&f.steam.join("config"));
    let before_ud = snapshot(&f.steam.join("userdata"));
    let before_manifest = snapshot(&f.sa().join("compatdata"));
    let inv = f.scan();
    let _ = shadersweep_core::settings::shader_precaching(&inv.installs[0]);
    let allow = Allowlist::from_inventory(&inv);
    clear_all(&inv, &allow);
    assert_eq!(before_cfg, snapshot(&f.steam.join("config")));
    assert_eq!(before_ud, snapshot(&f.steam.join("userdata")));
    assert_eq!(before_manifest, snapshot(&f.sa().join("compatdata")));
    Ok(())
}
