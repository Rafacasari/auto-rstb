# Auto RSTB

A Skyline plugin for Animal Crossing: New Horizons 3.0.3. It rebuilds the resource size
table (`System/Resource/ResourceSizeTable.srsizetable`) for your mods every time the game starts.
The table goes in its config folder. Hooks on two SDK functions (`nn::fs::OpenFile` and
`nn::fs::GetEntryType`) then send the game's reads of its table there, so no game code is
patched.

The sizes come from the `rstb` crate, a port of Heaven Tool's traced sizing. On the vanilla
romfs, it matches Heaven Tool on every file.

## Install

Build it with `cargo skyline build --release`. Then copy
`target/aarch64-skyline-switch/release/libauto_rstb.nro` to
`sd:/atmosphere/contents/01006F8002326000/romfs/skyline/plugins/`.

## Development

| Command | What it does |
| --- | --- |
| `cargo test -p rstb` | Runs the format tests (Yaz0, Zstd, SARC and the table). |
| `cargo run --release -p rstb --example check -- <romfs>` | Sizes every file of an unpacked 3.0.3 romfs, compares each size with the game's own table, and prints the time spent per file type. |
| `cargo clippy -p rstb --all-targets` | Lints the sizing crate. |
| `cargo skyline clippy` | Lints the plugin. |
| `cargo fmt --all` | Formats the code (settings in `rustfmt.toml`). |

The sizing crate uses `ruzstd` for Zstd and `crc32fast` for hashes. It reads Yaz0 and SARC
itself: the crates for those formats panic on malformed files, and a panic would crash the game.

## What it does at launch

1. **Checks the game version.** On any version other than 3.0.3, it does nothing.
2. **Finds the files to size.** It scans every romfs folder that replaces game files (see [Mod folders](#mod-folders)). Each file is read from the game's romfs, which holds the copy the game loads when several folders replace it. Every file the game sizes through the table is calculated:
   - Files the game sizes itself get no entry.
   - File types with no traced size keep their entry as it is.
3. **Builds the table.** It always starts from the game's own table, then applies every calculated size on top.
4. **Writes the table to the config folder, only if it changed.**
5. **Sends the game's reads of `System/Resource/ResourceSizeTable.srsizetable` to that file.** The game opens it through the romfs mount, and the hooks swap the path for the config file's. LayeredFS never sees the read, so a new or longer table works in the same launch.

If the update fails, the table from an earlier launch is still used.

### Mod folders

| Folder | Used by |
| --- | --- |
| `sd:/atmosphere/contents/01006F8002326000/romfs` | Atmosphere's LayeredFS, and mod managers that merge mods there. |
| `sd:/load/01006F8002326000/<mod>/romfs` | Emulators such as Eden and Ryujinx, which load each mod from its own folder. |

The log lists each folder found and its number of files. A log line of `0 files` means the
plugin found no modded files, so the game reads its own sizes.

A file that only a mod the emulator doesn't load adds is skipped, and the log says so.

### Emulators

Emulators keep their mod folder outside the emulated SD card, where the plugin can't see it.
Link it into the SD card once: a junction on Windows, a symbolic link on Linux and macOS. Your
mods stay where they are, and the emulator keeps loading them as before.

The link must be named `load`, sit directly in the SD card folder, and point at the folder that
holds `01006F8002326000`. Nothing may exist at `load` yet: start the emulator once so the SD
card folder exists, and move any mods already in `<SD card>/load` into the mod folder first.

| Emulator | System | Mod folder | SD card folder |
| --- | --- | --- | --- |
| Eden | Windows | `%APPDATA%\eden\load` | `%APPDATA%\eden\sdmc` |
| Eden | Linux, macOS | `~/.local/share/eden/load` | `~/.local/share/eden/sdmc` |
| Ryujinx | Windows | `%APPDATA%\Ryujinx\mods\contents` | `%APPDATA%\Ryujinx\sdcard` |
| Ryujinx | Linux | `~/.config/Ryujinx/mods/contents` | `~/.config/Ryujinx/sdcard` |
| Ryujinx | macOS | `~/Library/Application Support/Ryujinx/mods/contents` | `~/Library/Application Support/Ryujinx/sdcard` |

Eden's Linux and macOS folders follow `XDG_DATA_HOME`, and Ryujinx's Linux folders follow
`XDG_CONFIG_HOME`; the commands below do too. A portable install (a `user` folder next to Eden,
or a `portable` folder next to Ryujinx), a Flatpak, or custom folders in the emulator's settings
use other folders: put those in the commands instead. Ryujinx's **File > Open Ryujinx Folder**
opens its data folder.

#### Windows

PowerShell:

```powershell
# Eden
New-Item -ItemType Junction -Path "$env:APPDATA\eden\sdmc\load" -Target "$env:APPDATA\eden\load"
# Ryujinx
New-Item -ItemType Junction -Path "$env:APPDATA\Ryujinx\sdcard\load" -Target "$env:APPDATA\Ryujinx\mods\contents"
```

Command Prompt:

```bat
:: Eden
mklink /J "%APPDATA%\eden\sdmc\load" "%APPDATA%\eden\load"
:: Ryujinx
mklink /J "%APPDATA%\Ryujinx\sdcard\load" "%APPDATA%\Ryujinx\mods\contents"
```

To remove the link, delete the `load` folder in File Explorer, or run `rmdir` on it in Command
Prompt. Both remove only the junction, not your mods. Don't use `Remove-Item -Recurse` on it:
Windows PowerShell 5.1 follows the junction and deletes the mods.

#### Linux

```bash
# Eden
eden="${XDG_DATA_HOME:-$HOME/.local/share}/eden"
mkdir -p "$eden/load" && ln -s "$eden/load" "$eden/sdmc/load"
# Ryujinx
ryujinx="${XDG_CONFIG_HOME:-$HOME/.config}/Ryujinx"
mkdir -p "$ryujinx/mods/contents" && ln -s "$ryujinx/mods/contents" "$ryujinx/sdcard/load"
```

#### macOS

```bash
# Eden
eden="${XDG_DATA_HOME:-$HOME/.local/share}/eden"
mkdir -p "$eden/load" && ln -s "$eden/load" "$eden/sdmc/load"
# Ryujinx
ryujinx="$HOME/Library/Application Support/Ryujinx"
mkdir -p "$ryujinx/mods/contents" && ln -s "$ryujinx/mods/contents" "$ryujinx/sdcard/load"
```

On Linux and macOS, remove the link with `rm` and no trailing slash, for example
`rm "$eden/sdmc/load"`. With a trailing slash, `rm -r` deletes the mods it points at.

### A table in a romfs folder

A table in `System/Resource/` of any of those folders hides the game's own table, so the plugin
can't read the vanilla one.

- **If the plugin already has a copy of the game's table,** it ignores the other tables.
- **If it has no copy yet,** it moves each one to `moved-from-<folder>.srsizetable` (`romfs` for Atmosphere's folder, `mod-<name>` for a mod's). It then asks you to start the game again, since the game's romfs keeps showing the moved files for that launch. This is the only restart.

A mod's table is never used as the base: its entries for the mod's files are sized again anyway.

## Files in `sd:/config/auto-rstb/`

| File | Contents |
| --- | --- |
| `ResourceSizeTable.srsizetable` | The table the game reads. |
| `vanilla.srsizetable` | A copy of the game's own table, read once from the romfs. |
| `moved-from-<folder>.srsizetable` | A table that was found in a romfs folder, kept in case you want it back. |
| `cache.tsv` | Sizes from earlier launches, keyed by the length and time stamp (or content hash) of each file's copies. It is rebuilt when the plugin or the shader archive changes. |
| `log.txt` | What happened at the last launch: the folders scanned, errors, files with no traced size, and the number of changed entries. |

To stop using the plugin's table, remove the plugin and delete `ResourceSizeTable.srsizetable`.

## Known limits

- **The plugin runs before the game reads the table.** Skyline runs plugins from the game's first `nn::vi::CreateLayer` call. The game reads the table when its resource manager starts (`0x71006C0FD0`), and that function's first step creates `AppStaticHeap`. That heap is not yet in the plugin's heap list (see the log), so the table is read after the plugin has written it and installed the hooks.
- **The hooks match the table by its romfs path.** Any mount name is accepted, so `rom:/System/Resource/ResourceSizeTable.srsizetable` and the same path on another mount are both redirected. The log line "The game reads its table from …" (printed to Skyline's output) confirms the redirect ran.
- **Memory is borrowed from the game.** malloc only has 32 MiB, so the update borrows up to 1 GiB from the sead heap with the most free space. It keeps 64 MiB free there, and returns the block before the game continues. If no heap can lend at least 128 MiB, the table is left as it is.
- **49 vanilla files are under-reported.** They only matter if a mod replaces one of them.
  - 22 `.bars`, Message, Layout and EventFlow files, plus `CloudVR` and one `_pbc` archive, are short by less than 400 bytes.
  - 21 geometry models are short by exactly 4096 bytes.
  - 6 geometry models are short by 16–40 KB: `FldUnitFall`, `FtrDiveFishFujitsubo`, `IdrAirPort00`, `IdrMarket02`, `IdrTailor00` and `SysFocus`. Their GPU command buffer is larger than the calculated one, so it needs another 16 KiB pool. The extra command bytes are not traced yet.
- **The game's own entries vary for identical files.** 42 model archives are byte-identical in 3.0.0 and 3.0.3, but their table entries differ by 4096 or 4456 bytes between the two versions. Page-sized differences from the game's entry can therefore come from how the entry was measured, not from the file. This is why no rule derived from the file can match every entry.
- **Some file types have no traced size:** `.bwav`, `.szs`, `.bnvib` and a few others. Their entry stays as it is, so a modded file that is larger than the original can still crash the game. The log lists every such file.
- **A unique hash is assumed to name the modded file.** Only hashes the base table already marks as collided are told apart by name.
