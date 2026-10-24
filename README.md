# Auto RSTB

Mods that replace game files can crash Animal Crossing: New Horizons when the new files are
bigger than the game expects. The game reads those expected sizes from its resource size table
(RSTB).

Auto RSTB fixes the table for you every time the game starts. There is no tool to run and no
table to copy around: add or remove mods, start the game, and the table matches them.

## What you need

- Animal Crossing: New Horizons. The sizes were made for version 3.0.3. Later versions work
  too, though a few sizes may be off if the game changed how it measures them.
- [Skyline for ACNH](https://github.com/acnhmodding/skyline-acnh/), on a Switch with Atmosphere or
  on an emulator (Eden, Ryujinx). Other Skyline builds can make the game hang at startup.

## Install

1. Get `libauto_rstb.nro`.
2. Put it in the `skyline/plugins` folder your other Skyline plugins use. On a Switch, that is:
   ```
   sd:/atmosphere/contents/01006F8002326000/romfs/skyline/plugins/
   ```
3. **On an emulator only:** do the one-time [emulator setup](#emulator-setup) below.
4. Start the game.

Your mods stay where they normally go:

- **Switch:** `sd:/atmosphere/contents/01006F8002326000/romfs/`
- **Emulators:** each mod in its own folder, as usual (for example `villager1/romfs/...`)

## Emulator setup

Emulators keep mods outside their virtual SD card, where the plugin can't see them. To fix that,
add a link (like a shortcut) named `load` inside the SD card folder that points to the
emulator's mod folder. You only do this once. Your mods don't move, and the emulator keeps
loading them as before.

**Before you start:** open the game in the emulator once, so its SD card folder exists. If that
SD card folder already has a `load` folder, move whatever is in it to the mod folder and delete
it.

### Windows

Open **PowerShell** and paste the line for your emulator:

```powershell
# Eden
New-Item -ItemType Junction -Path "$env:APPDATA\eden\sdmc\load" -Target "$env:APPDATA\eden\load"
# Ryujinx
New-Item -ItemType Junction -Path "$env:APPDATA\Ryujinx\sdcard\load" -Target "$env:APPDATA\Ryujinx\mods\contents"
```

Or, in **Command Prompt**:

```bat
:: Eden
mklink /J "%APPDATA%\eden\sdmc\load" "%APPDATA%\eden\load"
:: Ryujinx
mklink /J "%APPDATA%\Ryujinx\sdcard\load" "%APPDATA%\Ryujinx\mods\contents"
```

### Linux

```bash
# Eden
eden="${XDG_DATA_HOME:-$HOME/.local/share}/eden"
mkdir -p "$eden/load" && ln -s "$eden/load" "$eden/sdmc/load"
# Ryujinx
ryujinx="${XDG_CONFIG_HOME:-$HOME/.config}/Ryujinx"
mkdir -p "$ryujinx/mods/contents" && ln -s "$ryujinx/mods/contents" "$ryujinx/sdcard/load"
```

### macOS

```bash
# Eden
eden="${XDG_DATA_HOME:-$HOME/.local/share}/eden"
mkdir -p "$eden/load" && ln -s "$eden/load" "$eden/sdmc/load"
# Ryujinx
ryujinx="$HOME/Library/Application Support/Ryujinx"
mkdir -p "$ryujinx/mods/contents" && ln -s "$ryujinx/mods/contents" "$ryujinx/sdcard/load"
```

### Custom or portable installs

The commands above use each emulator's default folders:

| Emulator | System | Mod folder | SD card folder |
| --- | --- | --- | --- |
| Eden | Windows | `%APPDATA%\eden\load` | `%APPDATA%\eden\sdmc` |
| Eden | Linux, macOS | `~/.local/share/eden/load` | `~/.local/share/eden/sdmc` |
| Ryujinx | Windows | `%APPDATA%\Ryujinx\mods\contents` | `%APPDATA%\Ryujinx\sdcard` |
| Ryujinx | Linux | `~/.config/Ryujinx/mods/contents` | `~/.config/Ryujinx/sdcard` |
| Ryujinx | macOS | `~/Library/Application Support/Ryujinx/mods/contents` | `~/Library/Application Support/Ryujinx/sdcard` |

If you use a portable install, a Flatpak, or custom folders in the emulator's settings, put your
own folders in the commands instead. In Ryujinx, **File > Open Ryujinx Folder** shows where its
folders are.

### Removing the link safely

The link only points to your mods, but some ways of deleting it also delete what it points to.

- **Windows:** delete the `load` folder in File Explorer, or run `rmdir` on it in Command
  Prompt. Don't use `Remove-Item -Recurse` in PowerShell: it can delete your mods.
- **Linux and macOS:** use `rm` without a trailing slash, for example `rm "$eden/sdmc/load"`.

## Checking that it works

The game should start normally. The first launch takes a little longer while the plugin measures
your mods; later launches reuse what it measured and are faster.

To see what it did, open `sd:/config/auto-rstb/log.txt`. It lists the mod folders it found, how
many files each one has, and how many table entries changed.

**The log says `0` files?** The plugin didn't find your mods. On an emulator, check the
[emulator setup](#emulator-setup).

## Messages you may see

- **"Auto RSTB: please start the game again."** One of your mods came with its own RSTB file.
  The plugin moved it out of the way (to `sd:/config/auto-rstb/`) and needs one restart to read
  the game's original table. This only happens once.
- **"Auto RSTB could not update the resource size table."** Something went wrong; the reason is
  in `log.txt`. The table from your last successful launch is still used.

## Good to know

- **Mod authors don't need to include an RSTB.** If a mod has one, the plugin doesn't need it.
- **A few file types can't be measured yet**, such as `.bwav`, `.szs` and `.bnvib`. If a mod
  makes one of these bigger than the original, the game can still crash. The log lists every
  such file.

## Uninstall

Delete `libauto_rstb.nro`, then delete the `sd:/config/auto-rstb/` folder.
