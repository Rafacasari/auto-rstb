use crate::heap::{request, Request};
use crate::reader::{align_up, fail, Reader, Result};
use crate::sarc::{self, Member};
use crate::sizing::model_archive_load_bytes;

/// The canonical single-member ACNH model archive: one BFRES at 0x2000. Other members
/// (physics, shaders, nested archives) have independent allocations.
pub(crate) fn single_bfres(archive: &[u8]) -> Option<Member> {
    let mut members = sarc::read(archive).ok()?;
    if members.len() != 1 || members[0].offset != 0x2000 || !is_bfres(members[0].data(archive)) {
        return None;
    }
    members.pop()
}

/// An unrelocated little-endian BFRES v9 with a relocation table inside its bounds.
pub(crate) fn is_bfres(data: &[u8]) -> bool {
    let bfres = Reader::new(data);
    let header = || -> Result<bool> {
        if bfres.len() < 0xf0
            || !bfres.magic(0, b"FRES    ")?
            || bfres.u32(8)? != 0x00090000
            || bfres.u16(12)? != 0xfeff
            || bfres.u16(20)? != 0
            || bfres.u32(28)? as i64 != bfres.len()
        {
            return Ok(false);
        }
        let relocation = bfres.u32(24)? as i64;
        Ok(relocation <= bfres.len() - 16 && bfres.magic(relocation, b"_RLT")?)
    };
    header().unwrap_or(false)
}

/// The single runtime pool that BFRES setup (0x71038FB8C0) requests for a resource's
/// models, materials, shapes, meshes, BNTX texture references and animations.
pub(crate) fn runtime_pool_bytes(models: i64, materials: i64, shapes: i64, meshes: i64, textures: i64, animations: i64) -> i64 {
    models * 0x430 + materials * 0x198 + shapes * (0x1120 + 0xb0) + meshes * 0x260 + textures * 0x10 + animations
}

/// Counts the textures of the external files, which must all be BNTX v4.1 archives
/// stored before the relocation table.
pub(crate) fn texture_count(bfres: Reader) -> Result<i64> {
    let files = bfres.u16(0xec)? as i64;
    let array = bfres.pointer(0xb8)?;
    if (files == 0) != (array == 0) || (files == 0 && bfres.u64(0xc0)? != 0) {
        return fail("External file arrays do not match their count.");
    }

    let relocation = bfres.u32(24)? as i64;
    let mut textures = 0;
    for i in 0..files {
        let at = bfres.pointer(array + i * 16)?;
        let length = bfres.pointer(array + i * 16 + 8)?;
        if at + length > relocation {
            return fail("External file overlaps the relocation table.");
        }
        let bntx = bfres.slice(at, length)?;
        if bntx.len() < 0x58
            || !bntx.magic(0, b"BNTX\0\0\0\0")?
            || bntx.u32(8)? != 0x00040100
            || bntx.u16(12)? != 0xfeff
            || bntx.u16(20)? != 0
            || bntx.u32(28)? as i64 != length
            || !bntx.magic(32, b"NX  ")?
        {
            return fail("External files must be BNTX v4.1 textures.");
        }

        let count = bntx.u32(36)? as i64;
        let infos = bntx.pointer(40)?;
        if count == 0 {
            return fail("BNTX has no textures.");
        }
        bntx.range(infos, count * 8)?;
        for texture in 0..count {
            let info = bntx.pointer(infos + texture * 8)?;
            bntx.range(info, 0xa0)?;
            bntx.require_magic(info, b"BRTI")?;
        }
        textures += count;
    }
    Ok(textures)
}

/// 0x18 bytes per skeletal, material, bone-visibility and shape animation, plus the
/// 8-aligned normalized names of material and bone-visibility animations. Scene
/// animations are not counted. Returns the total and the name bytes.
pub(crate) fn animation_bytes(bfres: Reader) -> Result<(i64, i64)> {
    let mut animations = 0;
    for kind in 0..4 {
        let count = bfres.u16(0xe2 + kind * 2)? as i64;
        let array = 0x58 + kind * 0x10;
        if (count == 0) != (bfres.u64(array)? == 0) || (count == 0) != (bfres.u64(array + 8)? == 0) {
            return fail("Animation arrays do not match their count.");
        }
        animations += count;
    }

    let mut name_bytes = 0;
    for (array, stride, count) in [(0x68, 0x70, bfres.u16(0xe4)?), (0x78, 0x60, bfres.u16(0xe6)?)] {
        let start = bfres.pointer(array)?;
        let count = count as i64;
        bfres.range(start, count * stride)?;
        for i in 0..count {
            let name = bfres.pointer(start + i * stride + 8)?;
            if name == 0 {
                return fail("Animation has no name.");
            }
            name_bytes += normalized_animation_name_bytes(bfres.name(name)?);
        }
    }
    let name_bytes = align_up(name_bytes, 8);
    Ok((animations * 0x18 + name_bytes, name_bytes))
}

// 0x71038B0F60 recognizes these three-byte suffixes, removes an optional
// separator, and copies into a 1024-byte temporary. 0x71038FB8C0 reserves
// the normalized name plus NUL even when its copy-name flag is disabled.
fn normalized_animation_name_bytes(name: &str) -> i64 {
    let bytes = name.as_bytes();
    if !["fsp", "fcl", "fts", "ftp", "fvb", "fvm"].iter().any(|suffix| name.ends_with(suffix)) {
        return 0;
    }
    let mut end = bytes.len() - 3;
    if end > 0 && matches!(bytes[end - 1], b'_' | b'.') {
        end -= 1;
    }
    end.min(1023) as i64 + 1
}

/// Calculates ACNH model archives whose single BFRES has no geometry: textures, animations or both.
/// Geometry, scene animations and unfamiliar layouts deliberately return `None`.
pub(crate) fn texture_animation_archive_size(archive: &[u8]) -> Option<u32> {
    let member = single_bfres(archive)?;
    let pool = runtime_pool_without_models(Reader::new(member.data(archive))).ok()??;
    let allocation = model_archive_load_bytes(align_up(archive.len() as i64, 32), &[], &resource_requests(pool)).ok()?;
    // No generic 32-byte rounding: ACNH's recorded allocations may be 16 mod 32.
    u32::try_from(allocation).ok()
}

// After the archive holder, 0x7103B26680 creates the BFRES resource (0x71038FB680). Its
// constructor (0x71038FD400) lists one pointer per shader archive: the member hook's
// static-model shader (0x710070FD2C passes it as a load argument when the archive has no
// Park_UBER.bfsha) and the global registry's Park_UBER (gsys.bgapkginfo shader_num = 1).
// Setup (0x71038FB8C0) then allocates the runtime pool; without models nothing else is charged.
fn resource_requests(runtime_pool: i64) -> [Request; 3] {
    [request(0x308, 8), request(2 * 8, 8), request(runtime_pool, 8)]
}

// The runtime pool of a BFRES without models: texture references plus the animation share.
fn runtime_pool_without_models(bfres: Reader) -> Result<Option<i64>> {
    // Version 9's model count and two reserved counts, then the scene animations (not counted
    // by the pool and otherwise untraced), at most one external file and no external GPU flags.
    if bfres.u16(0xdc)? != 0
        || bfres.u16(0xde)? != 0
        || bfres.u16(0xe0)? != 0
        || bfres.u16(0xea)? != 0
        || bfres.u16(0xec)? > 1
        || bfres.byte(0xee)? != 0
        || bfres.byte(0xef)? > 1
    {
        return Ok(None);
    }
    // The model and reserved arrays/dictionaries, then the scene animation ones.
    for offset in (0x28..=0x50).step_by(8) {
        if bfres.u64(offset)? != 0 {
            return Ok(None);
        }
    }
    if bfres.u64(0x98)? != 0 || bfres.u64(0xa0)? != 0 || !has_no_gpu_data(bfres)? {
        return Ok(None);
    }

    let pool = runtime_pool_bytes(0, 0, 0, 0, texture_count(bfres)?, animation_bytes(bfres)?.0);
    // A BFRES with neither would request an empty pool, which has not been traced.
    Ok((pool != 0).then_some(pool))
}

// Some exporters emit an empty geometry pool even for texture-only files.
// Its serialized storage is already part of the loaded archive.
// Accept only an uninitialized pool with no GPU buffer bytes; actual geometry
// still fails the count/array checks and cannot enter this calculation.
fn has_no_gpu_data(bfres: Reader) -> Result<bool> {
    let memory_pool = bfres.pointer(0xa8)?;
    let buffer_info = bfres.pointer(0xb0)?;
    if memory_pool == 0 && buffer_info == 0 {
        return Ok(true);
    }
    Ok(memory_pool != 0
        && buffer_info != 0
        && bfres.u32(buffer_info)? == 0x24
        && bfres.u32(buffer_info + 4)? == 0
        && bfres.u64(buffer_info + 8)? == memory_pool as u64
        && bfres.range(memory_pool, 0x120)?.iter().all(|&b| b == 0))
}
