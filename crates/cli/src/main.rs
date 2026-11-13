//! `shadersweep` command line. With no subcommand it launches `shadersweep-gui`.

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use shadersweep_core::guard::{Allowlist, ProcessProbe, ProcfsProbe};
use shadersweep_core::ops::{self, DeleteMode, Selection};
use shadersweep_core::scan::{self, Inventory, ScanOptions};
use shadersweep_core::stats;
use shadersweep_core::util::{format_date, human_size};
use std::io::{BufRead, IsTerminal, Write};
use std::os::unix::process::CommandExt;

#[derive(Parser)]
#[command(
    name = "shadersweep",
    version,
    about = "Steam shader cache manager (not affiliated with Valve)"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// List per-game shader caches
    List {
        /// Only caches of uninstalled games
        #[arg(long)]
        orphaned: bool,
        #[arg(long)]
        json: bool,
    },
    /// Clear shader caches (moves to Trash unless --permanent)
    Clear {
        appids: Vec<u32>,
        /// Also clear every orphaned cache
        #[arg(long)]
        orphaned: bool,
        /// Delete permanently instead of moving to the Trash
        #[arg(long)]
        permanent: bool,
        /// Do not ask for confirmation
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Usage statistics
    Stats {
        #[arg(long)]
        json: bool,
    },
    /// Launch the graphical interface
    Gui,
}

fn load() -> Result<Inventory> {
    let opts = ScanOptions::from_env().context("locating home directory")?;
    let inv = scan::scan(&opts, &|_| {});
    for w in &inv.warnings {
        eprintln!("warning: {w}");
    }
    Ok(inv)
}

fn confirm(prompt: &str) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        bail!("confirmation needed but stdin is not a terminal; pass --yes");
    }
    print!("{prompt} [y/N] ");
    std::io::stdout().flush()?;
    let mut s = String::new();
    std::io::stdin().lock().read_line(&mut s)?;
    Ok(matches!(s.trim().to_lowercase().as_str(), "y" | "yes"))
}

fn launch_gui() -> Result<()> {
    let sibling = std::env::current_exe()?.with_file_name("shadersweep-gui");
    let prog = if sibling.exists() {
        sibling
    } else {
        "shadersweep-gui".into()
    };
    let err = std::process::Command::new(prog).exec();
    bail!("cannot start shadersweep-gui: {err}")
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        None | Some(Cmd::Gui) => launch_gui(),
        Some(Cmd::List { orphaned, json }) => {
            let inv = load()?;
            let rows: Vec<_> = inv
                .entries
                .iter()
                .filter(|e| !orphaned || e.orphaned())
                .collect();
            if json {
                let v: Vec<_> = rows
                    .iter()
                    .map(|e| {
                        serde_json::json!({
                            "appid": e.appid, "name": e.name, "library": e.library,
                            "path": e.path, "size_bytes": e.size, "files": e.files,
                            "modified": e.modified, "installed": e.installed,
                            "orphaned": e.orphaned(),
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                println!(
                    "{:>10}  {:<40} {:>10} {:>10}  {:<9} LIBRARY",
                    "APPID", "NAME", "SIZE", "MODIFIED", "INSTALLED"
                );
                for e in &rows {
                    let name: String = e.name.chars().take(40).collect();
                    println!(
                        "{:>10}  {:<40} {:>10} {:>10}  {:<9} {}",
                        e.appid,
                        name,
                        human_size(e.size),
                        format_date(e.modified),
                        if e.installed { "yes" } else { "no" },
                        e.library.display()
                    );
                }
                println!(
                    "total: {} in {} caches",
                    human_size(rows.iter().map(|e| e.size).sum()),
                    rows.len()
                );
            }
            Ok(())
        }
        Some(Cmd::Stats { json }) => {
            let inv = load()?;
            let s = stats::compute(&inv);
            if json {
                println!("{}", serde_json::to_string_pretty(&s)?);
            } else {
                println!(
                    "Game shader caches: {} ({} caches)",
                    human_size(s.total_game_size),
                    inv.entries.len()
                );
                println!(
                    "Orphaned:           {} ({} caches)",
                    human_size(s.orphaned_size),
                    s.orphaned_count
                );
                println!("Global driver:      {}", human_size(s.global_size));
                println!("Per drive:");
                for d in &s.per_drive {
                    println!(
                        "  {:<30} {:>10} ({} caches)",
                        d.drive,
                        human_size(d.size),
                        d.caches
                    );
                }
                println!("Top 10:");
                for (n, sz) in &s.top10 {
                    println!("  {:>10}  {n}", human_size(*sz));
                }
                for g in &inv.globals {
                    println!(
                        "Global: {:>10}  {} ({})",
                        human_size(g.size),
                        g.label,
                        g.path.display()
                    );
                }
            }
            Ok(())
        }
        Some(Cmd::Clear {
            appids,
            orphaned,
            permanent,
            yes,
        }) => {
            if appids.is_empty() && !orphaned {
                bail!("give at least one appid or --orphaned");
            }
            let inv = load()?;
            let allow = Allowlist::from_inventory(&inv);
            let probe = ProcfsProbe::default();
            let sel = Selection {
                appids,
                orphaned,
                globals: vec![],
            };
            let plan = ops::plan_clear(&inv, &allow, &probe, &sel);
            for r in &plan.refused {
                eprintln!("refused: {}: {}", r.label, r.reason);
            }
            if plan.items.is_empty() {
                eprintln!("nothing to clear");
                std::process::exit(if plan.refused.is_empty() { 0 } else { 2 });
            }
            if plan.steam_running {
                eprintln!("warning: Steam is running; it may recreate or lock caches. Closing Steam first is safest.");
            }
            for i in &plan.items {
                println!(
                    "{:>10}  {}  [{}]",
                    human_size(i.size),
                    i.path.display(),
                    i.label
                );
            }
            let how = if permanent {
                "PERMANENTLY DELETE"
            } else {
                "move to Trash"
            };
            println!(
                "Will {how} {} item(s), {}",
                plan.items.len(),
                human_size(plan.total_size)
            );
            if !yes && !confirm("Proceed?")? {
                println!("aborted");
                return Ok(());
            }
            let mode = if permanent {
                DeleteMode::Permanent
            } else {
                DeleteMode::Trash
            };
            let rep = ops::execute(&plan, mode, &allow, &probe);
            for (i, e) in &rep.failed {
                eprintln!("failed: {}: {e}", i.label);
            }
            println!(
                "cleared {} item(s), freed {}",
                rep.done.len(),
                human_size(rep.freed)
            );
            let _: &dyn ProcessProbe = &probe;
            if rep.failed.is_empty() {
                Ok(())
            } else {
                std::process::exit(1)
            }
        }
    }
}
