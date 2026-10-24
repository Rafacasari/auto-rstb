//! Auto RSTB: at launch, rebuilds Animal Crossing: New Horizons' resource size table
//! for the modded files (see `mods`), starting from the game's own table. The table is written
//! to the config folder, and the game's reads of its table are sent there (see `redirect`), so
//! a changed table works in the same launch.

mod arena;
mod cache;
mod mods;
mod platform;
mod redirect;

use std::fmt::Display;

use rstb::sizing::{table_name, Sizer, GAME_SIZED, SHADER_ARCHIVE, UNSUPPORTED};
use rstb::table::{Table, Update, PATH as TABLE};
use rstb::yaz0;

use cache::Cache;
use mods::{Copies, Folder};
use platform::RomMount;

/// The game version the sizes were traced on.
const TRACED_VERSION: &str = "3.0.3";

const CONFIG: &str = "sd:/config/auto-rstb";
/// The table the game reads instead of its own.
const MODDED_TABLE: &str = "sd:/config/auto-rstb/ResourceSizeTable.srsizetable";
const CACHE: &str = "sd:/config/auto-rstb/cache.tsv";
const LOG: &str = "sd:/config/auto-rstb/log.txt";
const TEMPORARY: &str = "sd:/config/auto-rstb/table.tmp";

const ROM_MOUNT: &str = "autorstb";

const ERROR_CODE: u32 = 2;

#[derive(Default)]
struct Log(String);

impl Log {
    fn line(&mut self, text: impl Display) {
        let text = text.to_string();
        println!("[auto-rstb] {text}");
        self.0 += &text;
        self.0.push('\n');
    }

    fn save(&self) {
        if let Err(error) = platform::write(LOG, self.0.as_bytes()) {
            println!("[auto-rstb] {error}");
        }
    }
}

enum Outcome {
    /// The table the game reads this launch is the one built.
    Ready,
    /// The game would read a file that is gone this launch, so it must start again.
    Restart(&'static str),
}

#[skyline::main(name = "auto-rstb")]
pub fn main() {
    // Sets up std's output before memory is borrowed, as it lives for the whole game
    println!("[auto-rstb] Updating the resource size table.");
    let version = skyline::nn::oe::get_display_version();
    if arena::with_borrowed_memory(|loan| update(loan, &version)) {
        skyline::nn::oe::exit_application();
    }
    // A table from an earlier launch still beats the game's own when this update failed
    if platform::file_exists(MODDED_TABLE) {
        redirect::install(TABLE, MODDED_TABLE);
    }
}

/// Updates the table, returning whether the game must close.
fn update(loan: &arena::Loan, version: &str) -> bool {
    let mut log = Log::default();
    log.line(format!("Game version {version}."));
    if version != TRACED_VERSION {
        log.line(format!("The sizes were traced on {TRACED_VERSION}; file types this version sizes differently may be off."));
    }
    log.line(loan.report.trim_end());
    let outcome = match loan.size {
        // malloc's 32 MiB, which the game shares, is too little for model and shader data
        0 => Err("No game heap could lend enough memory to size files.".to_string()),
        _ => run(&mut log, version),
    };
    match outcome {
        Ok(Outcome::Ready) => {
            log.save();
            false
        }
        Ok(Outcome::Restart(reason)) => {
            log.line(reason);
            log.save();
            skyline::error::show_error(ERROR_CODE, "Auto RSTB: please start the game again.", reason);
            true
        }
        Err(error) => {
            log.line(format!("Failed: {error}"));
            log.save();
            skyline::error::show_error(
                ERROR_CODE,
                "Auto RSTB could not update the resource size table.",
                &format!(
                    "{error}

Modded files may crash the game. Details are in {LOG}."
                ),
            );
            false
        }
    }
}

fn run(log: &mut Log, version: &str) -> Result<Outcome, String> {
    platform::create_dirs(CONFIG)?;
    let folders = mods::folders()?;
    let rom = RomMount::new(ROM_MOUNT).map_err(|error| log.line(format!("{error} Vanilla files are unavailable."))).ok();
    let Some(vanilla) = vanilla_table(log, version, &folders, rom.as_ref())? else {
        return Ok(Outcome::Restart("A resource size table in a romfs folder hid the game's own, so it was moved to the config folder. The game's table can be read from the next launch."));
    };

    let mut table = vanilla;
    size_files(log, version, &mut table, &folders, rom.as_ref())?;
    drop(rom);

    if platform::read_optional(MODDED_TABLE)?.is_some_and(|bytes| parse_table(&bytes).is_ok_and(|current| current == table)) {
        log.line("The table is up to date.");
        return Ok(Outcome::Ready);
    }
    let bytes = yaz0::compress(&table.serialize());
    platform::replace(MODDED_TABLE, TEMPORARY, &bytes)?;
    log.line(format!("Wrote the table ({} bytes).", bytes.len()));
    Ok(Outcome::Ready)
}

/// The game's own table: the copy saved for this game version, else the romfs's table, which
/// is the game's only while no romfs folder has a table. Tables found there are moved away,
/// returning `None`: the game's romfs keeps showing them for this launch.
fn vanilla_table(log: &mut Log, version: &str, folders: &[Folder], rom: Option<&RomMount>) -> Result<Option<Table>, String> {
    let saved = format!("{CONFIG}/vanilla-{version}.srsizetable");
    let modded: Vec<_> =
        folders.iter().map(|folder| (folder, format!("{}/{TABLE}", folder.root))).filter(|(_, path)| platform::file_exists(path)).collect();
    if let Some(bytes) = platform::read_optional(&saved)? {
        match parse_table(&bytes) {
            Ok(table) => {
                for (_, path) in &modded {
                    log.line(format!("{path} is ignored: the game reads {MODDED_TABLE}, built from its own table."));
                }
                return Ok(Some(table));
            }
            Err(error) => log.line(format!("The saved copy of the game's table is invalid ({error}), so it is read again.")),
        }
    }
    if !modded.is_empty() {
        for (folder, path) in &modded {
            let moved = format!("{CONFIG}/moved-from-{}.srsizetable", folder.name);
            platform::rename(path, &moved)?;
            log.line(format!("Moved {path} to {moved}."));
        }
        return Ok(None);
    }
    let rom = rom.ok_or("The game's resource size table cannot be read without the romfs.")?;
    let bytes = platform::read(&rom.path(TABLE))?;
    let table = parse_table(&bytes)?;
    platform::write(&saved, &bytes)?;
    Ok(Some(table))
}

fn parse_table(bytes: &[u8]) -> Result<Table, String> {
    yaz0::decompress(bytes).and_then(|data| Table::parse(&data)).map_err(|error| error.0)
}

/// The path a modded file is read from. The game's romfs holds the copy the game loads, as
/// it resolves files that several folders replace. Without it, the first copy is read.
fn source(log: &mut Log, rom: Option<&RomMount>, relative: &str, copies: &Copies) -> Option<String> {
    match rom {
        Some(rom) => {
            let path = rom.path(relative);
            if platform::file_exists(&path) {
                return Some(path);
            }
            // A new file of a mod the emulator does not load
            log.line(format!("{relative}: not in the game's romfs, so its mod is not loaded; skipped."));
            None
        }
        None => {
            if copies.len() > 1 {
                log.line(format!("{relative}: {} copies, and without the game's romfs the first is sized.", copies.len()));
            }
            Some(copies[0].0.clone())
        }
    }
}

/// Sizes every table resource in the romfs folders into `table`.
fn size_files(log: &mut Log, version: &str, table: &mut Table, folders: &[Folder], rom: Option<&RomMount>) -> Result<(), String> {
    let (files, counts) = mods::files(folders)?;
    let listed: Vec<_> = folders.iter().zip(&counts).map(|(folder, count)| format!("{} ({count})", folder.root)).collect();
    log.line(format!("Modded files: {}.", listed.join(", ")));

    let shader_copies = files.get(SHADER_ARCHIVE);
    let shader_path = match (rom, shader_copies) {
        (Some(rom), _) => rom.path(SHADER_ARCHIVE),
        (None, Some(copies)) => copies[0].0.clone(),
        (None, None) => format!("{}/{SHADER_ARCHIVE}", folders[0].root),
    };
    let shader = match shader_copies {
        Some(copies) => format!("sd shader {} {}", mods::length(copies), mods::stamp(copies)?),
        None => "game shader".to_string(),
    };
    let mut cache = Cache::load(CACHE, format!("auto-rstb {} rstb {} game {version} {shader}", env!("CARGO_PKG_VERSION"), rstb::VERSION));
    let mut sizer = Sizer::new(shader_path);
    let (mut sized, mut cached, mut changed, mut added, mut removed) = (0, 0, 0, 0, 0);
    for (relative, copies) in &files {
        let Some(name) = table_name(relative) else { continue };
        let Some(path) = source(log, rom, relative, copies) else { continue };
        let length = mods::length(copies);
        let stamp = mods::stamp(copies);
        let size = match stamp.as_deref().ok().and_then(|stamp| cache.get(relative, length, stamp)) {
            Some(size) => {
                cached += 1;
                size
            }
            None => sizer.file_size(&path, name).unwrap_or_else(|error| {
                log.line(format!("{relative}: {error} The game sizes it itself."));
                GAME_SIZED
            }),
        };
        if let Ok(stamp) = stamp {
            cache.insert(relative, length, stamp, size);
        }
        if size == UNSUPPORTED {
            log.line(format!("{relative}: no traced size; its table entry was left as it is."));
            continue;
        }
        match table.set(name, size) {
            Ok(Update::Changed) => changed += 1,
            Ok(Update::Added) => added += 1,
            Ok(Update::Removed) => removed += 1,
            Ok(Update::None) => {}
            Err(error) => log.line(format!("{relative}: {error}")),
        }
        sized += 1;
    }
    if let Some(error) = sizer.shader_error() {
        log.line(format!("The shader archive could not be read ({error}); the game sizes geometry models itself."));
    }
    if let Err(error) = cache.save(CACHE) {
        log.line(error);
    }
    log.line(format!("{} files, {sized} sized ({cached} from the cache); against the game's table, {changed} entries changed, {added} added, {removed} removed.", files.len()));
    Ok(())
}
