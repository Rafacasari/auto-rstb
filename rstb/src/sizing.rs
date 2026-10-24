//! The resource-heap allocation ACNH 3.0.3 records in the table for a file. See Heaven Tool's
//! docs/resource-load-sizing.md and docs/model-resource-sizing.md for the traced load chains.

use std::fs::File;
use std::io::Read;

use crate::bfres::texture_animation_archive_size;
use crate::geometry::GeometryAnalyzer;
use crate::heap::{high_water_mark, request, Request};
use crate::reader::{align_up, fail, read_file, Result};
use crate::{physics, sarc, shader, zstd};

/// The game sizes the file itself: the table must have no entry for it.
pub const GAME_SIZED: i64 = -1;
/// The file type has no traced load; its table entry is left as it is.
pub const UNSUPPORTED: i64 = -2;

/// The shared shader archive, relative to the romfs root.
pub const SHADER_ARCHIVE: &str = "Shader/Park_UBER_All.Nin_NX_NVN.szs";

// Zstd files are only modelled for these loads; other compressed resources keep the game's fallback.
const ZSTD_LOAD_FOLDERS: [&str; 4] = ["Layout/", "Font/", "Message/", "Model/Layout_"];

// A resource heap (0x71037C6D48) starts with its 0x108-byte ExpHeap object; the first free block follows it.
const RESOURCE_HEAP_FREE_BLOCK: i64 = 0x108;

/// The table name of a romfs file (relative, '/'-separated), or `None` when the file is not
/// a table resource: Skyline plugins, the table itself (compressed or not) and BYML files other than the one
/// the game loads as a resource.
pub fn table_name(relative: &str) -> Option<&str> {
    if relative.starts_with("skyline")
        || relative.starts_with("System/Resource/ResourceSizeTable.")
        || (relative.ends_with(".byml") && relative != "EventFlow/Info/EventFlowInfoProduct.byml")
    {
        return None;
    }
    Some(relative.strip_suffix(".zs").unwrap_or(relative))
}

/// Calculates file allocations. The shader archive needed by geometry models is loaded once,
/// on first use.
pub struct Sizer {
    shader_path: String,
    geometry: Option<std::result::Result<GeometryAnalyzer, String>>,
}

impl Sizer {
    /// `shader_path` is the game's [`SHADER_ARCHIVE`] as the game would load it.
    pub fn new(shader_path: impl Into<String>) -> Self {
        Sizer { shader_path: shader_path.into(), geometry: None }
    }

    /// The allocation the game records for the file at `path` (the file on disk, optionally
    /// Zstd-compressed with ".zs"), named `name` in the table. Returns [`GAME_SIZED`] or
    /// [`UNSUPPORTED`] when there is no traced size.
    pub fn file_size(&mut self, path: &str, name: &str) -> Result<i64> {
        let size = if name.starts_with("Model/") && !name.starts_with("Model/Layout_") && name.ends_with(".Nin_NX_NVN") {
            self.model_archive_size(path)?
        } else if path.ends_with(".zs") && !ZSTD_LOAD_FOLDERS.iter().any(|folder| name.starts_with(folder)) {
            GAME_SIZED
        } else {
            match load_bytes_for(path, name) {
                // Loaded data is always allocated in 32-byte multiples
                Some(load) => load(align_up(resource_length(path)?, 32))?,
                None => UNSUPPORTED,
            }
        };
        if size > u32::MAX as i64 {
            return fail(format!("{path} is too big!"));
        }
        Ok(size)
    }

    fn model_archive_size(&mut self, path: &str) -> Result<i64> {
        let archive = read_resource(path)?;
        if let Some(allocation) = texture_animation_archive_size(&archive) {
            return Ok(allocation as i64);
        }
        if let Ok(members) = sarc::read(&archive) {
            let loaded = align_up(archive.len() as i64, 32);
            // Collision archives: PBC members are read in place, so only the archive load itself allocates.
            if !members.is_empty() && members.iter().all(|m| m.name.as_deref().is_some_and(|n| n.ends_with(".pbc"))) {
                return model_archive_load_bytes(loaded, &[], &[]);
            }
            // Physics-only archives: the member hook adds the helper-bone, cloth or fish-bone requests.
            // Other members (such as cloth wind) are not modelled, so the game's fallback is kept.
            if let [phive] = members.as_slice() {
                if phive.is("output.phive") {
                    return Ok(physics::requests(phive.data(&archive))
                        .and_then(|requests| model_archive_load_bytes(loaded, &requests, &[]))
                        .unwrap_or(GAME_SIZED));
                }
            }
        }
        // Geometry needs the shared shader archive; without it the game's fallback is kept.
        let Some(analyzer) = self.geometry() else { return Ok(GAME_SIZED) };
        Ok(analyzer.allocation_bytes(&archive).unwrap_or(GAME_SIZED))
    }

    fn geometry(&mut self) -> Option<&mut GeometryAnalyzer> {
        let path = &self.shader_path;
        let loaded = self.geometry.get_or_insert_with(|| shader::load(path).map(GeometryAnalyzer::new).map_err(|e| e.to_string()));
        loaded.as_mut().ok()
    }

    /// Why the shader archive could not be used, once a geometry model needed it.
    pub fn shader_error(&self) -> Option<&str> {
        self.geometry.as_ref()?.as_ref().err().map(String::as_str)
    }
}

/// The allocations of a file's resource load for its loaded data size, or `None` when unsupported.
fn load_bytes_for(path: &str, name: &str) -> Option<fn(i64) -> Result<i64>> {
    // Layout and Font archives are both requested with alignment 0x2000 (0x7103284690)
    if name.starts_with("Layout/") || name.starts_with("Font/") {
        return Some(|data| archive_load_bytes(data, 0x2000));
    }
    if name.starts_with("Model/Layout_") {
        return Some(|data| Ok(data + 9264));
    }
    if name.starts_with("Message/") {
        return Some(|data| archive_load_bytes(data, 0x100));
    }
    if path.ends_with(".bars") {
        return Some(|data| load_bytes(&[request(0x30, 32), request(data, 0x80), request(0xa8, 8)]));
    }
    // Traced for bfevfl (factory 0x71048837E8); bcsv, byml and asb use the same budget in every vanilla entry.
    if [".bcsv", ".bfevfl", ".byml", ".asb"].iter().any(|extension| path.ends_with(extension)) {
        return Some(|data| load_bytes(&[request(0x38, 32), request(data, 32)]));
    }
    None
}

/// Resource-heap bytes used by a load's allocations, measured from a heap start aligned to
/// every request. The game adds the load alignment and 8 bytes on top (0x7103E36E30), which
/// covers any other heap start, so this is the value the table stores.
fn load_bytes(requests: &[Request]) -> Result<i64> {
    high_water_mark(requests, RESOURCE_HEAP_FREE_BLOCK)
}

// Zstd archives: the 0x58-byte archive resource, then the decompressed data at the caller's load alignment.
fn archive_load_bytes(data: i64, load_alignment: i64) -> Result<i64> {
    load_bytes(&[request(0x58, 32), request(data, load_alignment)])
}

/// Model archives: after the archive load, the model resource opens the data as a second SARC
/// resource (0x71006E0EC0), runs the member hook (0x710070FD2C) and keeps the archive in a
/// 0x10-byte holder (0x71006E1098). Without shader members the hook only allocates for a phive
/// member. An output.bfres member is then created after the holder (`resource_requests`).
pub(crate) fn model_archive_load_bytes(data: i64, member_requests: &[Request], resource_requests: &[Request]) -> Result<i64> {
    let mut requests = vec![request(0x58, 32), request(data, 0x2000), request(0x58, 32)];
    requests.extend_from_slice(member_requests);
    requests.push(request(0x10, 8));
    requests.extend_from_slice(resource_requests);
    load_bytes(&requests)
}

/// The file's bytes, decompressed when it is Zstd data.
fn read_resource(path: &str) -> Result<Vec<u8>> {
    let data = read_file(path)?;
    if zstd::is_zstd(&data) {
        return zstd::decompress(&data);
    }
    Ok(data)
}

/// The file's length, decompressed when it is Zstd data.
fn resource_length(path: &str) -> Result<i64> {
    let file = File::open(path)?;
    let length = file.metadata()?.len() as i64;
    let mut magic = Vec::with_capacity(4);
    file.take(4).read_to_end(&mut magic)?;
    if !zstd::is_zstd(&magic) {
        return Ok(length);
    }
    Ok(zstd::decompressed_length(&read_file(path)?)? as i64)
}
