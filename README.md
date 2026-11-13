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
- Global driver caches as separate entries: `mesa_shader_cache`, `mesa_shader_cache_sf`, `mesa_shader_cache_db`, `radv_builtin_shaders*`, `nvidia/GLCache`. With `MESA_SHADER_CACHE_DIR` / `__GL_SHADER_DISK_CACHE_PATH` set, only the cache subdirectories the drivers create there (`mesa_shader_cache*`, `GLCache`) are offered, never the directory you pointed the variable at. `DXVK_STATE_CACHE_PATH` is reported but not managed (that directory may hold other files).
- Sizes are apparent bytes of the files, the same number `du -sb` prints (`scripts/compare-du.sh` checks this).
- Clear = move to the freedesktop Trash (undoable) by default; "permanent" is a separate, unchecked-by-default checkbox in the confirmation dialog, which lists exact paths and total size.
- Safety: refuses running games (native, Proton and non-Steam shortcuts), warns when Steam runs, never follows symlinks, canonical-path allowlist and inode identity re-checked right before every delete.
- GPU/driver change banner, read-only Shader Pre-Caching indicator, stats (per drive, top 10, orphaned, driver total), search, sortable columns, auto-rescan (inotify).
- CLI: `shadersweep list [--orphaned] [--json]`, `shadersweep clear <appid>... [--orphaned] [--yes]`, `shadersweep clear <appid>... --permanent --yes`, `shadersweep stats [--json]`. Permanent deletion is never interactive: it requires both `--permanent` and `--yes`.

## Install (AppImage)
Download `shadersweep-<version>-x86_64.AppImage` from the [releases](https://github.com/legendarylolo318-cloud/shadersweep/releases), then:
```sh
sha256sum -c shadersweep-0.1.0-x86_64.AppImage.sha256
chmod +x shadersweep-0.1.0-x86_64.AppImage
./shadersweep-0.1.0-x86_64.AppImage          # GUI
./shadersweep-0.1.0-x86_64.AppImage list     # any arguments run the CLI
```
Needs glibc 2.34 or newer, fontconfig, and a Wayland or X11 session (libwayland-client / libX11 and libxkbcommon from the system). If FUSE is unavailable, add `--appimage-extract-and-run`.

## Build
```sh
# Arch: sudo pacman -S rust fontconfig freetype2 libxkbcommon wayland
cargo build --release          # target/release/shadersweep (CLI) and shadersweep-gui
cargo test --workspace
```
Running `shadersweep` with no subcommand starts `shadersweep-gui`. `./build-appimage.sh` builds `dist/shadersweep-<version>-x86_64.AppImage` and its `.sha256`
(it downloads pinned, hash-verified copies of appimagetool and the AppImage runtime). Set `SHADERSWEEP_LOG=1` to make the GUI print scan results and warnings to stderr.

## What "Rebuild" actually does
Rebuild is **clear + explain**. shadersweep moves the game's `shadercache/<appid>` to the Trash; Steam then
re-downloads its pre-cached shaders (if "Allow background processing of Vulkan shaders" / Shader Pre-Caching
is on) or recompiles them the next time you launch the game (the first launch may stutter).

shadersweep does **not** run `fossilize_replay` itself. Steam ships it (`ubuntu12_64/fossilize_replay`, also under
`steamrt*/`) and its `--help` shows flags such as `--num-threads`, but the exact invocation Steam uses
(master/slave processes, whitelist and on-disk pipeline cache files) is undocumented, so a replay could not be made reliable and was
deliberately not implemented.

## Safety model
All deletion goes through one function (`ops::remove_checked`). Right before acting it canonicalizes the path, refuses symlinks, checks the
canonical path against an allowlist built from discovery, and verifies the directory is still the same inode the scan saw.
- Only direct children of discovered `steamapps/shadercache` directories (which must still be named `shadercache` after resolving symlinks)
  and the discovered global driver cache directories can ever be deleted.
- Anything inside, or containing, a library's `steamapps/compatdata` (Proton prefixes, save games) or `steamapps/common` (game files), an
  install's `userdata` or `config`, and `$HOME` or any of its ancestors is refused, even if the allowlist were wrong. Proton prefixes are shown as
  read-only info only.
- Permanent deletion uses `remove_dir_all`, which does not follow symlinks while recursing.
- Steam config files are only ever read (tests check that their contents and mtimes are unchanged).

## Known limits
- Names for uninstalled games are unknown (`Unknown app <id>`): Steam's binary `appinfo.vdf` is not parsed.
- Shader Pre-Caching on/off is not stored in any readable config file on the reference machine, so the GUI reports "not readable".
- Game detection relies on `reaper ... SteamLaunch AppId=N` command lines (the wrapper Steam uses for native and Proton games, checked against
  Steam's own launch log) and the `SteamAppId` / `SteamGameId` environment variables. A game started outside Steam is not detected.
- Flatpak Steam discovery is covered by tests with a fixture tree, but has not been tried against a real Flatpak install.
- Only Linux x86_64 AppImages are published.

## License
MIT
