//! File access on the Switch: Skyline's std for SD card files, nn::fs for what it lacks
//! (the game's romfs, entry checks and time stamps).

use std::ffi::CString;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};

use skyline::nn;

const ENTRY_FILE: u32 = 1;

/// The game's romfs as the game sees it: vanilla files, replaced by those of the loaded mods.
pub struct RomMount {
    name: CString,
    // nn::fs keeps using the cache until the romfs is unmounted
    _cache: Vec<u8>,
}

impl RomMount {
    pub fn new(name: &str) -> Result<RomMount, String> {
        let name = CString::new(name).map_err(|e| e.to_string())?;
        let mut size = 0u64;
        let rc = unsafe { nn::fs::QueryMountRomCacheSize(&mut size) };
        if rc != 0 {
            return Err(format!("Querying the romfs cache size failed ({rc:#x})."));
        }
        let mut cache = Vec::new();
        cache.try_reserve_exact(size as usize).map_err(|_| "Out of memory for the romfs cache.".to_string())?;
        cache.resize(size as usize, 0);
        let rc = unsafe { nn::fs::MountRom(name.as_ptr() as _, cache.as_mut_ptr(), size) };
        if rc != 0 {
            return Err(format!("Mounting the romfs failed ({rc:#x})."));
        }
        Ok(RomMount { name, _cache: cache })
    }

    /// The mounted path of a romfs file.
    pub fn path(&self, relative: &str) -> String {
        format!("{}:/{relative}", self.name.to_string_lossy())
    }
}

impl Drop for RomMount {
    fn drop(&mut self) {
        unsafe { nn::fs::Unmount(self.name.as_ptr() as _) };
    }
}

fn c_path(path: &str) -> Option<CString> {
    CString::new(path).ok()
}

pub fn file_exists(path: &str) -> bool {
    let Some(path) = c_path(path) else { return false };
    let mut entry_type = 0;
    unsafe { nn::fs::GetEntryType(&mut entry_type, path.as_ptr() as _) == 0 && entry_type == ENTRY_FILE }
}

pub fn read(path: &str) -> Result<Vec<u8>, String> {
    fs::read(path).map_err(|e| format!("Reading {path} failed: {e}"))
}

/// The file's bytes, or `None` when there is no such file.
pub fn read_optional(path: &str) -> Result<Option<Vec<u8>>, String> {
    if !file_exists(path) {
        return Ok(None);
    }
    read(path).map(Some)
}

/// Creates or overwrites a file. The file is sized first: Skyline's std cannot write past
/// a file's end.
pub fn write(path: &str, data: &[u8]) -> Result<(), String> {
    let result = (|| {
        let mut file = OpenOptions::new().write(true).create(true).truncate(false).open(path)?;
        file.set_len(data.len() as u64)?;
        file.write_all(data)?;
        file.flush()
    })();
    result.map_err(|e| format!("Writing {path} failed: {e}"))
}

/// Replaces a file with one written completely at `temporary` first, so an interrupted write
/// never leaves a partial file at `path`.
pub fn replace(path: &str, temporary: &str, data: &[u8]) -> Result<(), String> {
    write(temporary, data)?;
    rename(temporary, path)
}

/// Moves a file, replacing any file at `to`.
pub fn rename(from: &str, to: &str) -> Result<(), String> {
    remove(to)?;
    fs::rename(from, to).map_err(|e| format!("Moving {from} to {to} failed: {e}"))
}

/// Deletes a file if it exists.
pub fn remove(path: &str) -> Result<(), String> {
    if !file_exists(path) {
        return Ok(());
    }
    fs::remove_file(path).map_err(|e| format!("Deleting {path} failed: {e}"))
}

/// Creates a directory and its parents ("mount:/a/b").
pub fn create_dirs(path: &str) -> Result<(), String> {
    let root = path.find(":/").map_or(0, |at| at + 2);
    for (at, _) in path.match_indices('/').filter(|&(at, _)| at > root).chain([(path.len(), "")]) {
        // Existing directories fail to be created; the final check covers real failures
        let _ = fs::create_dir(&path[..at]);
    }
    if fs::read_dir(path).is_err() {
        return Err(format!("Creating {path} failed."));
    }
    Ok(())
}

pub fn dir_exists(path: &str) -> bool {
    fs::read_dir(path).is_ok()
}

/// The names of the directories in `root`, sorted, or none when `root` does not exist.
pub fn dirs(root: &str) -> Result<Vec<String>, String> {
    let Ok(entries) = fs::read_dir(root) else { return Ok(Vec::new()) };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("Listing {root} failed: {e}"))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.metadata().map_err(|e| format!("Reading {root}/{name} failed: {e}"))?.is_dir() {
            out.push(name);
        }
    }
    out.sort();
    Ok(out)
}

/// Every file under `root` as (path relative to `root`, length), or none when `root` does not exist.
pub fn files(root: &str) -> Result<Vec<(String, u64)>, String> {
    fn walk(root: &str, relative: &str, out: &mut Vec<(String, u64)>) -> Result<(), String> {
        let dir = if relative.is_empty() { root.to_string() } else { format!("{root}/{relative}") };
        let entries = fs::read_dir(&dir).map_err(|e| format!("Listing {dir} failed: {e}"))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("Listing {dir} failed: {e}"))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = if relative.is_empty() { name } else { format!("{relative}/{name}") };
            let metadata = entry.metadata().map_err(|e| format!("Reading {dir}/{path} failed: {e}"))?;
            if metadata.is_dir() {
                walk(root, &path, out)?;
            } else {
                out.push((path, metadata.len()));
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    if fs::read_dir(root).is_ok() {
        walk(root, "", &mut out)?;
    }
    out.sort();
    Ok(out)
}

/// Identifies a file's contents for the size cache: its creation and modification times,
/// or a hash of its bytes when the file system has no time stamps.
pub fn stamp(path: &str) -> Result<String, String> {
    let mut times = nn::fs::FileTimeStamp::new();
    if let Some(c) = c_path(path) {
        if unsafe { nn::fs::GetFileTimeStampForDebug(&mut times, c.as_ptr() as _) } == 0 && times.modify.time != 0 {
            return Ok(format!("t{:x}-{:x}", times.create.time, times.modify.time));
        }
    }
    let mut file = fs::File::open(path).map_err(|e| format!("Reading {path} failed: {e}"))?;
    let mut hasher = crc32fast::Hasher::new();
    let mut buffer = vec![0u8; 0x10000];
    loop {
        let read = file.read(&mut buffer).map_err(|e| format!("Reading {path} failed: {e}"))?;
        if read == 0 {
            return Ok(format!("c{:08x}", hasher.finalize()));
        }
        hasher.update(&buffer[..read]);
    }
}
