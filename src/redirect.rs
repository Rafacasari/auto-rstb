//! Sends the game's reads of its resource size table to the table built for the mods, through
//! hooks on nn::fs::OpenFile and nn::fs::GetEntryType (SDK functions, not game code). The game
//! reads the table through those as "<romfs mount>:/System/Resource/...", so the table never
//! goes through LayeredFS, which keeps each file's length from when the game started.

use std::ffi::{CStr, CString};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use skyline::nn::fs::{DirectoryEntryType, FileHandle};

struct Redirect {
    /// The table's path in the romfs.
    table: &'static str,
    target: CString,
}

static REDIRECT: OnceLock<Redirect> = OnceLock::new();
static USED: AtomicBool = AtomicBool::new(false);

/// From now on, opening `table` (a romfs path) on any mount opens `target` instead.
pub fn install(table: &'static str, target: &str) {
    let Ok(target) = CString::new(target) else { return };
    if REDIRECT.set(Redirect { table, target }).is_ok() {
        skyline::install_hooks!(open_file, get_entry_type);
    }
}

/// The path to use instead of `path`: the target for the table, else `path` itself.
fn redirect(path: *const u8) -> *const u8 {
    let Some(redirect) = REDIRECT.get() else { return path };
    if path.is_null() {
        return path;
    }
    let bytes = unsafe { CStr::from_ptr(path as _) }.to_bytes();
    let Some(mount_end) = bytes.windows(2).position(|w| w == b":/") else { return path };
    let relative = &bytes[mount_end + 2..];
    let relative = &relative[relative.iter().take_while(|&&b| b == b'/').count()..];
    if relative != redirect.table.as_bytes() {
        return path;
    }
    if !USED.swap(true, Ordering::Relaxed) {
        println!("[auto-rstb] The game reads its table from {}.", redirect.target.to_string_lossy());
    }
    redirect.target.as_ptr() as _
}

#[skyline::hook(replace = skyline::nn::fs::OpenFile)]
fn open_file(handle: *mut FileHandle, path: *const u8, mode: i32) -> u32 {
    original!()(handle, redirect(path), mode)
}

#[skyline::hook(replace = skyline::nn::fs::GetEntryType)]
fn get_entry_type(entry_type: *mut DirectoryEntryType, path: *const u8) -> u32 {
    original!()(entry_type, redirect(path))
}
