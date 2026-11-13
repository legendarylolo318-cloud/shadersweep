//! The default delete mode moves caches to the freedesktop Trash (undoable).
//! Lives in its own test binary because it points XDG_DATA_HOME at a temp dir.

use shadersweep_core::guard::Allowlist;
use shadersweep_core::ops::{self, DeleteMode};
use std::fs;
use std::path::PathBuf;

#[test]
fn trash_mode_moves_to_trash_and_keeps_data() -> Result<(), Box<dyn std::error::Error>> {
    let t = tempfile::tempdir()?;
    let root = fs::canonicalize(t.path())?;
    let data = root.join("data");
    fs::create_dir_all(&data)?;
    // Test binaries are single-purpose processes; nothing else reads this variable here.
    std::env::set_var("XDG_DATA_HOME", &data);
    let sc = root.join("lib/steamapps/shadercache");
    fs::create_dir_all(sc.join("100/fozpipelinesv6"))?;
    fs::write(sc.join("100/fozpipelinesv6/a.foz"), b"cache")?;
    let allow = Allowlist::new(vec![sc.clone()], vec![], vec![], None);
    let id = shadersweep_core::guard::identity(&sc.join("100"));
    ops::remove_checked(&allow, &sc.join("100"), id, DeleteMode::Trash)?;
    assert!(!sc.join("100").exists());
    let trashed: PathBuf = data.join("Trash/files/100/fozpipelinesv6/a.foz");
    assert_eq!(fs::read(trashed)?, b"cache");
    assert!(data.join("Trash/info/100.trashinfo").exists());
    Ok(())
}
