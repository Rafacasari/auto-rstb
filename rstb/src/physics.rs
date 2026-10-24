//! Resource ExpHeap requests ACNH 3.0.3 makes for a model archive's "output.phive"
//! (0x710070FD2C, 0x71007104E4), in allocation order. Havok's own tagfile memory comes
//! from the phive system heap and is not charged to the resource.

use crate::heap::{high_water_mark, request, Request};
use crate::reader::{fail, to_int, Reader, Result};
use crate::{crc32, sarc};

// The phive holder directly follows the outer archive's SARC resource object
// (0x58 bytes, 0x20-aligned), so its free block starts at 24 mod 32.
const START_ADDRESS: i64 = 0x58 % 32;

const CLOTH_OPERATORS: [&str; 7] = [
    "hclBoneSpaceSkinPOperator",
    "hclCopyVerticesOperator",
    "hclGatherAllVerticesOperator",
    "hclMoveParticlesOperator",
    "hclObjectSpaceSkinPOperator",
    "hclSimpleMeshBoneDeformOperator",
    "hclSimulateOperator",
];

/// Bytes the phive allocations add to the resource ExpHeap, including headers and padding.
pub(crate) fn exp_heap_bytes(phive: &[u8]) -> Result<i64> {
    Ok(high_water_mark(&requests(phive)?, START_ADDRESS)? - START_ADDRESS)
}

/// The phive requests in allocation order; fails for members that are not modelled.
pub(crate) fn requests(phive: &[u8]) -> Result<Vec<Request>> {
    const NAMES: [&str; 4] = ["output.bphhb", "output_sub.bphhb", "output.bphcl", "output.bphfbc"];
    let mut files: [Option<&[u8]>; 4] = [None; 4];
    for member in sarc::read(phive)? {
        let index = NAMES.iter().position(|&name| member.is(name));
        match index {
            Some(i) if files[i].is_none() => files[i] = Some(member.data(phive)),
            _ => return fail(format!("Unsupported physics member {}", member.name.as_deref().unwrap_or(""))),
        }
    }

    let mut requests = vec![request(0x28, 8), request(0x58, 0x20)];
    for helper_bones in files[..2].iter().flatten() {
        add_helper_bones(&mut requests, Reader::new(helper_bones))?;
    }
    if let Some(cloth) = files[2] {
        add_cloth(&mut requests, Reader::new(cloth))?;
    }
    if let Some(fish_bones) = files[3] {
        add_fish_bones(&mut requests, Reader::new(fish_bones))?;
    }
    Ok(requests)
}

/// One array with an 8-byte count cookie.
fn array_request(count: i64, stride: i64) -> Request {
    request(count * stride + 8, 8)
}

// 0x7103DECFC0 creates the 0x4D0-byte object; 0x7103DE9C30 allocates its
// arrays, each with an 8-byte count cookie, and stops at the first empty one
// of the curve keys, driver bones, driven bones and aim constraints.
fn add_helper_bones(requests: &mut Vec<Request>, file: Reader) -> Result<()> {
    requests.push(request(0x4d0, 0x20));
    let aamp = Aamp::new(file)?;
    let root = aamp.list(aamp.root)?;
    if root.objects == 0 || root.lists == 0 {
        return fail("Helper bone parameters are missing.");
    }
    let header = aamp.object(&root, 0);
    let int = |name| aamp.int(header, name, 0);
    let (bones, curves, outputs) = (int("bone_num")?, int("connection_curve_num")?, int("output_num")?);
    let (drivers, driven, pose) = (int("driver_bone_num")?, int("driven_bone_num")?, int("pose_driven_num")?);
    let (position, rotate, aim) = (int("position_driven_num")?, int("rotate_driven_num")?, int("aim_driven_num")?);

    if bones < 1 {
        return Ok(());
    }
    if curves >= 1 {
        requests.push(array_request(curves, 0xa0));
        let settings = aamp.list(aamp.child_list(&root, 0))?;
        if settings.lists < 2 {
            return fail("Helper bone curves are missing.");
        }
        let curve_list = aamp.list(aamp.child_list(&settings, 1))?;
        if curve_list.objects < curves {
            return fail("Helper bone curve count exceeds its parameters.");
        }
        for i in 0..curves {
            let keys = aamp.int(aamp.object(&curve_list, i), "key_num", 0)?;
            if keys == 0 {
                return Ok(());
            }
            if keys > 0 {
                requests.push(array_request(keys, 0x28));
            }
        }
    }
    requests.push(array_request(bones, 0x58));
    if outputs > 0 {
        requests.push(array_request(outputs, 0xb0));
    }
    if drivers < 1 {
        return Ok(());
    }
    requests.push(array_request(drivers, 0x130));
    if driven < 1 {
        return Ok(());
    }
    requests.push(array_request(driven, 0xd0));
    for (count, stride) in [(pose, 0x290), (position, 0x30), (rotate, 0x30)] {
        if count > 0 {
            requests.push(array_request(count, stride));
        }
    }
    if aim >= 1 {
        requests.push(array_request(aim, 0x30));
    }
    Ok(())
}

// 0x7103DE4B10 creates the 0x3B8-byte object; 0x7103DE4D40 then allocates a 0x30-byte bone
// list and one 0x28-byte entry per bone, with an 8-byte count cookie. The loader rejects
// files whose root list has fewer than three objects.
fn add_fish_bones(requests: &mut Vec<Request>, file: Reader) -> Result<()> {
    requests.push(request(0x3b8, 8));
    let aamp = Aamp::new(file)?;
    let root = aamp.list(aamp.root)?;
    if root.objects < 3 {
        return fail("Fish bone parameters are missing.");
    }
    requests.push(request(0x30, 8));
    let bones = aamp.int(aamp.object(&root, 0), "mBoneNum", 2)?;
    if bones > 0 {
        requests.push(array_request(bones, 0x28));
    }
    Ok(())
}

// 0x7103E05A64 creates the 0xF8-byte cloth resource; 0x7103E05C30 loads the
// Havok tagfile (0x7103E06650) and then sizes its mesh and collidable lists.
fn add_cloth(requests: &mut Vec<Request>, file: Reader) -> Result<()> {
    requests.push(request(0xf8, 0x20));
    if !file.magic(0, b"Phive\0")? || file.u16(8)? != 0xfeff {
        return fail("Cloth must be a little-endian Phive file.");
    }
    add_cloth_havok(requests, &Tagfile::new(file.slice(file.u32(0xc)? as i64, file.u32(0x18)? as i64)?)?)?;

    let aamp = Aamp::new(file.slice(file.u32(0x10)? as i64, file.u32(0x1c)? as i64)?)?;
    let root = aamp.list(aamp.root)?;
    let (mut meshes, mut collidables) = (0, 0);
    for i in 0..root.lists {
        let list = aamp.list(aamp.child_list(&root, i))?;
        if list.crc == crc32("cloth_mesh_list") {
            meshes = list.objects;
        } else if list.crc == crc32("collidable_list") {
            collidables = list.objects;
        }
    }
    if meshes > 0 {
        requests.push(array_request(meshes, 0x1a0));
    }
    if collidables > 0 {
        requests.push(array_request(collidables, 0x78));
    }
    Ok(())
}

// The tagfile stores objects in their native layout with item indices in pointer
// and array slots; an array's length is its item's count.
fn add_cloth_havok(requests: &mut Vec<Request>, tag: &Tagfile) -> Result<()> {
    let container = tag.single("hclClothContainer")?;
    requests.push(request(0x18, 8));
    let (cloth_datas, cloth_datas_at) = tag.array(tag.u32(container + 0x28)?)?;
    if cloth_datas < 1 {
        return Ok(());
    }
    requests.push(array_request(cloth_datas, 32));
    for i in 0..cloth_datas {
        let cloth_data = tag.object(tag.u32(cloth_datas_at + i * 8)?, "hclClothData")?;
        let (sim_cloth_datas, sim_cloth_datas_at) = tag.array(tag.u32(cloth_data + 0x20)?)?;
        if sim_cloth_datas < 1 {
            return Ok(());
        }
        requests.push(array_request(sim_cloth_datas, 32));
        let (operators, operators_at) = tag.array(tag.u32(cloth_data + 0x50)?)?;
        let mut simulation = 0;
        for j in 0..operators {
            let item = tag.u32(operators_at + j * 8)?;
            if item == 0 {
                continue;
            }
            let kind = tag.type_name(item)?;
            if !CLOTH_OPERATORS.contains(&kind) {
                return fail(format!("Unverified cloth operator {kind}"));
            }
            if kind != "hclSimulateOperator" {
                continue;
            }
            // Operator type 1: one 2-byte entry per constraint set in execution order.
            let (configs, configs_at) = tag.array(tag.u32(tag.object(item, kind)? + 0x50)?)?;
            if configs < 1 {
                continue;
            }
            let mut entries = tag.array(tag.u32(configs_at + 8)?)?.0;
            if entries < 1 {
                if simulation >= sim_cloth_datas {
                    return fail("Simulate operator has no simulation data.");
                }
                let sim_cloth_data = tag.object(tag.u32(sim_cloth_datas_at + simulation * 8)?, "hclSimClothData")?;
                entries = tag.array(tag.u32(sim_cloth_data + 0x88)?)?.0 + 1;
            }
            requests.push(request(entries * 2, 8));
            simulation += 1;
        }
    }
    Ok(())
}

struct AampList {
    crc: u32,
    lists_at: i64,
    lists: i64,
    objects_at: i64,
    objects: i64,
}

struct Aamp<'a> {
    data: Reader<'a>,
    root: i64,
}

impl<'a> Aamp<'a> {
    fn new(data: Reader<'a>) -> Result<Self> {
        if !data.magic(0, b"AAMP")? || data.u32(4)? != 2 {
            return fail("Expected an AAMP v2 parameter file.");
        }
        Ok(Aamp { data, root: to_int(0x30 + to_int(data.u32(0x14)? as i64)?)? })
    }

    fn list(&self, at: i64) -> Result<AampList> {
        let d = self.data;
        Ok(AampList {
            crc: d.u32(at)?,
            lists_at: at + d.u16(at + 4)? as i64 * 4,
            lists: d.u16(at + 6)? as i64,
            objects_at: at + d.u16(at + 8)? as i64 * 4,
            objects: d.u16(at + 10)? as i64,
        })
    }

    fn child_list(&self, list: &AampList, index: i64) -> i64 {
        list.lists_at + index * 12
    }

    fn object(&self, list: &AampList, index: i64) -> i64 {
        list.objects_at + index * 8
    }

    /// A missing parameter keeps the game's default.
    fn int(&self, object: i64, name: &str, fallback: i64) -> Result<i64> {
        let d = self.data;
        let crc = crc32(name);
        let parameters = object + d.u16(object + 4)? as i64 * 4;
        for i in 0..d.u16(object + 6)? as i64 {
            let at = parameters + i * 8;
            if d.u32(at)? != crc {
                continue;
            }
            let info = d.u32(at + 4)?;
            if info >> 24 != 2 {
                return fail(format!("Physics parameter {name} is not an int."));
            }
            return Ok(d.u32(at + (info & 0xffffff) as i64 * 4)? as i32 as i64);
        }
        Ok(fallback)
    }
}

struct Item {
    kind: usize,
    offset: i64,
    count: i64,
}

struct Tagfile<'a> {
    data: Reader<'a>,
    items: Vec<Item>,
    /// Type names by index; type 0 is null.
    types: Vec<Option<String>>,
}

struct Section {
    tag: [u8; 4],
    start: i64,
    end: i64,
}

impl<'a> Tagfile<'a> {
    fn new(file: Reader<'a>) -> Result<Self> {
        // The Phive header's tagfile size includes zero padding to 16 bytes after TAG0.
        let roots = sections(file, 0, file.len().min((file.u32_be(0)? & 0x3fffffff) as i64))?;
        let [root] = roots.as_slice() else { return fail("Expected a Havok TAG0 tagfile.") };
        if &root.tag != b"TAG0" || file.range(root.end, file.len() - root.end)?.iter().any(|&b| b != 0) {
            return fail("Expected a Havok TAG0 tagfile.");
        }
        let (mut body, mut strings, mut names, mut index) = (None, None, None, None);
        for section in sections(file, root.start, root.end)? {
            if &section.tag == b"DATA" {
                body = Some(file.slice(section.start, section.end - section.start)?);
            }
            if &section.tag != b"TYPE" && &section.tag != b"INDX" {
                continue;
            }
            for child in sections(file, section.start, section.end)? {
                let content = file.range(child.start, child.end - child.start)?;
                match &child.tag {
                    b"TSTR" | b"TST1" => strings = Some(content),
                    b"TNAM" | b"TNA1" => names = Some(content),
                    b"ITEM" => index = Some(content),
                    _ => {}
                }
            }
        }
        let (Some(data), Some(strings), Some(names), Some(index)) = (body.filter(|b| b.len() != 0), strings, names, index) else {
            return fail("Havok tagfile is missing its data, types or items.");
        };

        // ASCII decoding, as .NET's Encoding.ASCII: other bytes become '?'.
        let text: Vec<String> =
            strings.split(|&b| b == 0).map(|s| s.iter().map(|&b| if b < 0x80 { b as char } else { '?' }).collect()).collect();
        let mut position = 0;
        let count = to_int(varint(names, &mut position)?)?;
        if count < 0 {
            return fail("Havok type count is negative.");
        }
        let mut types = Vec::new();
        if count > 0 {
            types.push(None);
        }
        for _ in 1..count {
            let name = varint(names, &mut position)?;
            match usize::try_from(name).ok().and_then(|n| text.get(n)) {
                Some(name) => types.push(Some(name.clone())),
                None => return fail("Havok type name is out of range."),
            }
            let mut templates = varint(names, &mut position)?;
            while templates > 0 {
                varint(names, &mut position)?;
                varint(names, &mut position)?;
                templates -= 1;
            }
        }

        let mut items = Vec::with_capacity(index.len() / 12);
        for record in index.as_chunks::<12>().0 {
            let word = |i: usize| u32::from_le_bytes(record[i..i + 4].try_into().unwrap());
            let (kind, offset, count) = (word(0) & 0xffffff, word(4), word(8));
            if kind as usize >= types.len() || offset as i64 > data.len() || count > i32::MAX as u32 {
                return fail("Havok item is out of range.");
            }
            items.push(Item { kind: kind as usize, offset: offset as i64, count: count as i64 });
        }
        Ok(Tagfile { data, items, types })
    }

    fn u32(&self, at: i64) -> Result<i64> {
        Ok(self.data.u32(at)? as i64)
    }

    fn item(&self, item: i64) -> Result<&Item> {
        match self.items.get(item as usize) {
            Some(found) => Ok(found),
            None => fail("Havok item index is out of range."),
        }
    }

    fn type_name(&self, item: i64) -> Result<&str> {
        Ok(self.types[self.item(item)?.kind].as_deref().unwrap_or(""))
    }

    /// An array's item count and offset.
    fn array(&self, item: i64) -> Result<(i64, i64)> {
        if item == 0 {
            return Ok((0, 0));
        }
        let found = self.item(item)?;
        Ok((found.count, found.offset))
    }

    fn object(&self, item: i64, kind: &str) -> Result<i64> {
        if item == 0 || self.type_name(item)? != kind {
            return fail(format!("Expected Havok object {kind}"));
        }
        Ok(self.item(item)?.offset)
    }

    fn single(&self, kind: &str) -> Result<i64> {
        let mut found = None;
        for item in self.items.iter().skip(1) {
            if self.types[item.kind].as_deref() == Some(kind) {
                if found.is_some() {
                    return fail(format!("Multiple {kind} objects."));
                }
                found = Some(item.offset);
            }
        }
        found.map_or_else(|| fail(format!("Missing Havok object {kind}")), Ok)
    }
}

fn sections(file: Reader, mut start: i64, end: i64) -> Result<Vec<Section>> {
    let mut sections = Vec::new();
    while start < end {
        let size = (file.u32_be(start)? & 0x3fffffff) as i64;
        if size < 8 || size > end - start {
            return fail("Invalid Havok section size.");
        }
        let tag = file.range(start + 4, 4)?.try_into().unwrap();
        sections.push(Section { tag, start: start + 8, end: start + size });
        start += size;
    }
    Ok(sections)
}

fn varint(data: &[u8], at: &mut usize) -> Result<i64> {
    let Some(&first) = data.get(*at) else { return fail("Truncated Havok varint.") };
    let extra = match first {
        0x00..=0x7f => 0,
        0x80..=0xbf => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xe7 => 3,
        0xe8..=0xef => 4,
        0xf0..=0xf7 => 7,
        0xf8..=0xff => 8,
    };
    if data.len() - *at - 1 < extra {
        return fail("Truncated Havok varint.");
    }
    let mut value: i64 = match extra {
        0 => first as i64,
        1 => (first & 0x3f) as i64,
        2 => (first & 0x1f) as i64,
        3 | 4 | 7 => (first & 7) as i64,
        _ => 0,
    };
    for i in 1..=extra {
        value = value.wrapping_shl(8) | data[*at + i] as i64;
    }
    *at += extra + 1;
    Ok(value)
}
