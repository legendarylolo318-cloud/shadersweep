# shadersweep

A native Linux Steam shader cache manager (Rust + Slint). It shows how much disk space each game's
shader cache uses, highlights caches left behind by uninstalled games, and clears them safely.

> **shadersweep is an independent project and is not affiliated with, endorsed by, or sponsored by Valve Corporation.**
> "Steam" is a trademark of Valve. This project ships no Valve logos or branding.

*Screenshots: (placeholder)*

## Features
- Finds native and Flatpak Steam installs (deduplicated by canonical path) and every library folder from `libraryfolders.vdf`, including other drives.
- Per-game cache size / file count / last modified / installed-or-orphaned; names from `appmanifest_*.acf` and non-Steam shortcuts (`shortcuts.vdf`, binary VDF).
- Orphaned caches (no matching manifest) highlighted, with one-click "Clear orphaned".
- Global driver caches as separate entries: `mesa_shader_cache`, `mesa_shader_cache_db`, `radv_builtin_shaders*`, `nvidia/GLCache`, and the `DXVK_STATE_CACHE_PATH`, `__GL_SHADER_DISK_CACHE_PATH`, `MESA_SHADER_CACHE_DIR` overrides.
- Clear = move to the freedesktop Trash (undoable) by default; "permanent" is a separate checkbox. The confirmation dialog lists exact paths and total size.
- Safety: refuses running games, warns when Steam runs, never follows symlinks, canonical-path allowlist re-checked right before every delete.
- GPU/driver change banner, read-only Shader Pre-Caching indicator, stats (per drive, top 10, orphaned, driver total), search, sortable columns, auto-rescan (inotify).
- CLI: `shadersweep list [--orphaned] [--json]`, `shadersweep clear <appid>... [--orphaned] [--permanent] [--yes]`, `shadersweep stats [--json]`.

## Build
```sh
# Arch: sudo pacman -S rust fontconfig freetype2 libxkbcommon wayland
cargo build --release          # target/release/shadersweep (CLI) and shadersweep-gui
cargo test --workspace
```
Running `shadersweep` with no subcommand starts `shadersweep-gui`.

## What "Rebuild" actually does
Rebuild is **clear + explain**. shadersweep moves the game's `shadercache/<appid>` to the Trash; Steam then
re-downloads its pre-cached shaders (if "Allow background processing of Vulkan shaders" / Shader Pre-Caching
is on) or recompiles them the next time you launch the game (the first launch may stutter).

shadersweep does **not** run `fossilize_replay` itself. Steam ships it (`ubuntu12_64/fossilize_replay`, also under
`steamrt*/`) and its `--help` shows flags such as `--num-threads`, but the exact invocation Steam uses
(master/slave processes, whitelist and on-disk pipeline cache files) is undocumented, so a replay could not be made reliable and was
deliberately not implemented.

## Safety model
Only direct children of discovered `steamapps/shadercache` directories and the discovered global cache directories can ever
be deleted. Proton prefixes (`steamapps/compatdata/<appid>`) are shown as read-only info; they hold save games and are never offered for deletion.
Env-override cache paths that are `/`, `$HOME`, an ancestor of it, or very shallow are ignored.
Steam config is only ever read.

## Known limits
- Names for uninstalled games are unknown (`Unknown app <id>`): Steam's binary `appinfo.vdf` is not parsed.
- Shader Pre-Caching on/off is not stored in any readable config file on the reference machine, so the GUI reports "not readable".
- Game detection relies on `reaper ... SteamLaunch AppId=N` command lines and the `SteamAppId` environment variable.

## License
MIT
