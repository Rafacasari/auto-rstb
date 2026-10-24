//! The resource ExpHeap allocation of model archives with geometry, for the ACNH shared-shader
//! profile. Reads allocation metadata without decoding meshes, textures or shader binaries.

use std::collections::{HashMap, HashSet};

use crate::bfres::{animation_bytes, is_bfres, runtime_pool_bytes, single_bfres, texture_count};
use crate::heap::{gpu_pool_parent_requests, high_water_mark, request, Request};
use crate::physics;
use crate::reader::{align_up, fail, Error, Reader, Result};
use crate::sarc::{self, Member};
use crate::shader::{ProgramLayout, ShaderArchive, ShadingModel};

#[rustfmt::skip]
const PASSES: [&str; 16] = [
    "material", "zonly", "zprepass", "gbuffer", "depth_silhouette", "dilate", "reflection", "sssss",
    "cubemap", "depthshadow", "xlu_zprepass", "visualize", "user0", "user1", "user2", "user3",
];
// ACNH 3.0.3 0x71040B4070; 16 is the terminal (no fallback) sentinel.
const PARENT_PASSES: [usize; 16] = [16, 0, 1, 1, 1, 0, 8, 0, 0, 1, 0, 0, 0, 0, 0, 0];
#[rustfmt::skip]
const UNIFORMS: [&str; 15] = [
    "invalid", "context", "skeleton", "skeleton_ex", "shape", "shape_ex", "environment", "res_material",
    "material", "shader_option", "scene_material", "user0", "user1", "user2", "material_ffl",
];
const STORAGE: [&str; 8] = ["invalid", "environment", "material", "scene_material", "user0", "user1", "user2", "analyze_buffer"];

const ARCHIVE_HEADER_BYTES: i64 = 0x2000;
// Every resource ExpHeap starts with one 16 KiB GPU pool already created; it and
// its pool metadata form the fixed region after the page-aligned BFRES/runtime pool.
const RESIDENT_REGION_BYTES: i64 = 0x8000;
// Fixed loader allocations around the page-aligned region and after the render objects.
const POOL_REGION_OVERHEAD_BYTES: i64 = 952;
const TRAILING_LOADER_BYTES: i64 = 1960;
// The first ModelObj per FMDL requests 512 bytes more than the second one.
const FIRST_MODEL_BLOCK_EXTRA_BYTES: i64 = 512;
// ExpHeap bytes of the render objects: payload plus the 32-byte block header.
const BINDING_DESCRIPTOR_BYTES: i64 = 0x410 + 0x20;
const VERTEX_STATE_HEADER_BYTES: i64 = 0x20;
const COMMAND_WRAPPER_BYTES: i64 = 0x260 + 0x20;

/// Totals of one model archive's geometry.
#[derive(Default)]
struct Analysis {
    archive_bytes: i64,
    physics_bytes: i64,
    materials: i64,
    runtime_pool_bytes: i64,
    material_work_bytes: i64,
    object_work_bytes: i64,
    selector_work_bytes: i64,
    uniform_requested_bytes: i64,
    model_blocks: Vec<i64>,
    bindings: i64,
    vertex_states: i64,
    vertex_state_bytes: i64,
    commands: i64,
    command_bytes: i64,
    mesh_draw_bytes: i64,
}

impl Analysis {
    /// ExpHeap high-water mark before any GPU pool beyond the resident one is created.
    /// The phive allocations precede the BFRES setup (0x71006E1098), so they share the
    /// page-aligned region.
    fn cpu_exp_heap_bytes(&self) -> i64 {
        // Three 8-aligned allocations per shader instance: wrapper, object work, selector
        // work. The resource creates two such sets.
        let shader_set = self.materials * 0x228 + self.object_work_bytes + self.selector_work_bytes + self.materials * (3 * 0x20 + 4);
        align_up(
            align_up(self.archive_bytes, 32) - ARCHIVE_HEADER_BYTES
                + self.physics_bytes
                + self.runtime_pool_bytes
                + POOL_REGION_OVERHEAD_BYTES,
            4096,
        ) + RESIDENT_REGION_BYTES
            + 2 * shader_set
            + self.material_work_bytes
            + self.bindings * BINDING_DESCRIPTOR_BYTES
            + self.vertex_state_bytes
            + self.vertex_states * VERTEX_STATE_HEADER_BYTES
            + self.commands * COMMAND_WRAPPER_BYTES
            + TRAILING_LOADER_BYTES
    }

    /// GPU buffers (memory flags 3) in loader order.
    fn gpu_buffer_requests(&self) -> Vec<Request> {
        // 0x71038D3D40 requests one g3d ModelObj block buffer per FMDL; every FMDL gets
        // two ModelObjs, all first ones before all second ones. An empty block requests nothing.
        let blocks: Vec<i64> = self.model_blocks.iter().copied().filter(|&bytes| bytes > 0).collect();
        let mut requests: Vec<Request> = blocks.iter().map(|&bytes| request(bytes + FIRST_MODEL_BLOCK_EXTRA_BYTES, 256)).collect();
        requests.extend(blocks.iter().map(|&bytes| request(bytes, 256)));
        if self.uniform_requested_bytes > 0 {
            requests.push(request(self.uniform_requested_bytes, 256));
        }
        let commands = self.command_bytes + self.mesh_draw_bytes;
        if commands > 0 {
            requests.push(request((commands + 0x203) & !3, 4));
        }
        requests
    }

    /// The RSTC value: the resource ExpHeap's high-water mark. Requests that do not fit the
    /// resident pool create new pools whose metadata and page-aligned backing are appended
    /// to the same ExpHeap, in creation order.
    fn allocation_bytes(&self) -> Result<i64> {
        high_water_mark(&gpu_pool_parent_requests(&self.gpu_buffer_requests())?, self.cpu_exp_heap_bytes())
    }
}

struct Material<'s> {
    model: &'s ShadingModel,
    model_index: usize,
    candidates: Vec<i64>,
    options: HashMap<String, String>,
    attributes: HashMap<String, String>,
    samplers: HashMap<String, String>,
    sampler_names: Vec<String>,
}

/// Sizes geometry model archives against the game's shared BFSHA archive.
pub(crate) struct GeometryAnalyzer {
    shaders: ShaderArchive,
    layouts: Layouts,
}

impl GeometryAnalyzer {
    pub fn new(shaders: ShaderArchive) -> Self {
        GeometryAnalyzer { shaders, layouts: HashMap::new() }
    }

    /// The archive's allocation; unsupported or malformed inputs fail, never giving a partial result.
    pub fn allocation_bytes(&mut self, archive: &[u8]) -> Result<i64> {
        let Some((bfres, phive)) = read_archive(archive) else {
            return fail("Expected a canonical BFRES v9 model archive, optionally with output.phive.");
        };
        let physics_bytes = match phive {
            Some(phive) => physics::exp_heap_bytes(phive.data(archive))?,
            None => 0,
        };
        let mut analysis = analyze(&self.shaders, &mut self.layouts, Reader::new(bfres.data(archive)))?;
        analysis.archive_bytes = archive.len() as i64;
        analysis.physics_bytes = physics_bytes;
        analysis.allocation_bytes()
    }
}

type Layouts = HashMap<(usize, i64), ProgramLayout>;

fn analyze(shaders: &ShaderArchive, layouts: &mut Layouts, d: Reader) -> Result<Analysis> {
    let model_count = d.u16(0xdc)? as i64;
    if model_count == 0 {
        return fail("No geometry models to analyze.");
    }
    let textures = texture_count(d)?;
    let mut a = Analysis { model_blocks: vec![0; model_count as usize], ..Default::default() };
    let (mut shapes, mut meshes) = (0, 0);
    for (model_index, model) in entries(d, 0x28, model_count, 0x78)?.into_iter().enumerate() {
        d.require_magic(model, b"FMDL")?;
        let material_offsets = entries(d, model + 0x38, d.u16(model + 0x6c)? as i64, 0xa8)?;
        let vertex_offsets = entries(d, model + 0x20, d.u16(model + 0x68)? as i64, 0x58)?;
        let mut materials = Vec::with_capacity(material_offsets.len());
        for &material in &material_offsets {
            let parsed = read_material(shaders, d, material)?;
            if parsed.candidates.is_empty() {
                return fail(format!("No compiled static candidate for material {}", d.name(d.pointer(material + 8)?)?));
            }
            if shaders.has_shader_option_bindings(parsed.model)? {
                return fail("Additional shader-option command bindings are unresolved.");
            }
            if parsed.model.options.iter().any(|o| o.name == "gsys_assign_variation") {
                return fail("Assign-variation shader profiles are unresolved.");
            }
            let material_uniform = shaders.material_uniform_bytes(parsed.model)?;
            if material_uniform != 0 {
                a.material_work_bytes +=
                    material_work(d.u16(material + 0x9e)? as i64, d.u16(material + 0xa2)? as i64, d.byte(material + 0x9d)? as i64);
            }
            a.object_work_bytes += shaders.shader_object_work_bytes(parsed.model)?;
            a.selector_work_bytes += shaders.selector_work_bytes(parsed.model);
            // 0x71002B0E1C queries uniform alignment, then 0x71038FB8C0
            // rounds each material's GPU work separately to 256 bytes.
            let uniform = align_up(material_uniform, 256);
            a.uniform_requested_bytes += uniform;
            // The ModelObj block (0x71002ACF30) holds each material's block too.
            a.model_blocks[model_index] += uniform;
            materials.push(parsed);
        }
        a.materials += material_offsets.len() as i64;

        for shape in entries(d, model + 0x28, d.u16(model + 0x6a)? as i64, 0x60)? {
            d.require_magic(shape, b"FSHP")?;
            let (material_index, vertex_index) = (d.u16(shape + 0x52)? as usize, d.u16(shape + 0x56)? as usize);
            if material_index >= materials.len()
                || vertex_index >= vertex_offsets.len()
                || d.pointer(shape + 0x10)? != vertex_offsets[vertex_index]
            {
                return fail("Shape material or vertex reference is invalid.");
            }
            let material = &materials[material_index];
            let vertex = vertex_offsets[vertex_index];
            d.require_magic(vertex, b"FVTX")?;
            shape_passes(shaders, layouts, d, shape, vertex, material, &mut a)?;
            for mesh in entries(d, shape + 0x18, d.byte(shape + 0x5b)? as i64, 0x38)? {
                // The loader aggregates all submeshes into one indexed draw.
                if d.u16(mesh + 0x34)? == 0 {
                    return fail("Mesh has no submeshes.");
                }
                a.mesh_draw_bytes += if d.u32(mesh + 0x30)? == 0 { 44 } else { 48 };
                meshes += 1;
            }
            // 0x71002AE3E0: one 256-byte shape block (no view-dependent shapes or key shapes here).
            a.model_blocks[model_index] += 0x100;
            shapes += 1;
        }
    }
    let (animations, _) = animation_bytes(d)?;
    a.runtime_pool_bytes = runtime_pool_bytes(model_count, a.materials, shapes, meshes, textures, animations);
    Ok(a)
}

/// The render objects one shape creates over the 16 render passes.
fn shape_passes(
    shaders: &ShaderArchive,
    layouts: &mut Layouts,
    d: Reader,
    shape: i64,
    vertex: i64,
    material: &Material,
    a: &mut Analysis,
) -> Result<()> {
    let mut bindings = HashSet::new();
    let mut states = HashMap::new();
    let mut commands = HashMap::new();
    let mut options = material.options.clone();
    options.insert("gsys_weight".into(), d.byte(shape + 0x5a)?.to_string());
    options.insert("gsys_index_stream_format".into(), "0".into());
    let mut pass_options = material.model.options.iter().filter(|o| o.name == "gsys_assign_type");
    let (Some(pass_option), None) = (pass_options.next(), pass_options.next()) else {
        return fail("Missing render-pass shader option.");
    };
    for requested in 0..PASSES.len() {
        let mut pass = requested;
        while pass < PASSES.len() && !pass_option.choices.iter().any(|c| *c == format!("gsys_assign_{}", PASSES[pass])) {
            pass = PARENT_PASSES[pass];
        }
        if pass == PASSES.len() {
            return fail("No render-pass fallback.");
        }
        options.insert("gsys_assign_type".into(), format!("gsys_assign_{}", PASSES[pass]));
        let Some(index) = shaders.find_program(material.model, &material.candidates, &options)? else {
            return fail("No compiled dynamic shader program.");
        };
        let layout = match layouts.entry((material.model_index, index)) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => entry.insert(shaders.program_layout(material.model, index)?),
        };
        bindings.insert(binding_key(layout, material));
        let vertex_key = vertex_key(d, vertex, material, layout)?;
        states.insert(vertex_key.key.clone(), vertex_key.bytes);
        let command = (location_key(&layout.uniforms, &UNIFORMS), location_key(&layout.storage, &STORAGE), vertex_key.key);
        commands.insert(command, vertex_command_bytes(vertex_key.streams, vertex_key.attributes)?);
    }
    a.bindings += bindings.len() as i64;
    a.vertex_states += states.len() as i64;
    a.commands += commands.len() as i64;
    a.vertex_state_bytes += states.values().sum::<i64>();
    a.command_bytes += commands.values().sum::<i64>();
    Ok(())
}

fn read_material<'s>(shaders: &'s ShaderArchive, d: Reader, at: i64) -> Result<Material<'s>> {
    d.require_magic(at, b"FMAT")?;
    let assign = d.pointer(at + 0x20)?;
    if d.name(d.pointer(assign)?)? != shaders.name {
        return fail("Material references a different shader archive.");
    }
    let model_name = d.name(d.pointer(assign + 8)?)?;
    let mut found = shaders.models.iter().enumerate().filter(|(_, m)| m.name == model_name);
    let (Some((model_index, model)), None) = (found.next(), found.next()) else {
        return fail(format!("Missing shading model {model_name}"));
    };
    let mut options = assignments(d, assign, 0x30, 0x38, d.u16(assign + 0x46)? as i64)?;
    let mut info = HashMap::new();
    for entry in entries(d, at + 0x10, d.u16(at + 0x9a)? as i64, 0x18)? {
        let name = d.name(d.pointer(entry)?)?;
        if d.byte(entry + 0x12)? == 2 && d.u16(entry + 0x10)? > 0 {
            let value = d.name(d.pointer(d.pointer(entry + 8)?)?)?;
            if info.insert(name, value).is_some() {
                return fail("Duplicate material render info.");
            }
        }
    }
    let value = |name: &str| info.get(name).copied().unwrap_or("");
    let render_state = match value("gsys_render_state_mode") {
        "opaque" => "0",
        "mask" => "1",
        "translucent" => "2",
        "custom" => "3",
        _ => return fail("Unknown material render mode."),
    };
    let display_face = match value("gsys_render_state_display_face") {
        "back" => "2",
        "front" => "1",
        "both" => "0",
        "none" => "3",
        _ => return fail("Unknown material culling mode."),
    };
    const FUNCTIONS: [&str; 8] = ["never", "less", "equal", "lequal", "greater", "nequal", "gequal", "always"];
    let alpha_test_func = FUNCTIONS.iter().position(|&f| f == value("gsys_alpha_test_func")).unwrap_or(0);
    let alpha_test_enable = if value("gsys_alpha_test_enable") == "true" { "1" } else { "0" };
    let pass = match value("gsys_pass") {
        "seal" => "1",
        "xlu_water" => "2",
        "reduced_buffer" => "3",
        _ => "0",
    };
    options.insert("gsys_renderstate".into(), render_state.into());
    options.insert("gsys_display_face_type".into(), display_face.into());
    options.insert("gsys_alpha_test_func".into(), alpha_test_func.to_string());
    options.insert("gsys_alpha_test_enable".into(), alpha_test_enable.into());
    options.insert("gsys_pass".into(), pass.into());
    Ok(Material {
        model,
        model_index,
        candidates: shaders.static_candidates(model, &options)?,
        options,
        attributes: assignments(d, assign, 0x10, 0x18, d.byte(assign + 0x44)? as i64)?,
        samplers: assignments(d, assign, 0x20, 0x28, d.byte(assign + 0x45)? as i64)?,
        sampler_names: dictionary_names(d, d.pointer(at + 0x48)?, d.byte(at + 0x9d)? as i64)?,
    })
}

/// 3.0.3 model archives: a single BFRES at 0x2000, or "output.phive" then "output.bfres"
/// (0x710070FD2C). Shader archives and cloth wind (.bphcw) members are not supported.
fn read_archive(archive: &[u8]) -> Option<(Member, Option<Member>)> {
    if let Some(bfres) = single_bfres(archive) {
        return Some((bfres, None));
    }
    let mut members = sarc::read(archive).ok()?;
    if members.len() != 2 || !members[0].is("output.phive") || members[0].offset != 0x2000 || !members[1].is("output.bfres") {
        return None;
    }
    let bfres = members.pop()?;
    let phive = members.pop()?;
    is_bfres(bfres.data(archive)).then_some((bfres, Some(phive)))
}

fn material_work(parameters: i64, source: i64, samplers: i64) -> i64 {
    [(parameters + 31) / 32 * 4, (source + 7) & !7, samplers * 8, 0x48, samplers * 8]
        .into_iter()
        .filter(|&size| size != 0)
        .fold(0, |end, size| align_up(end, 8) + size)
}

/// The sampler bindings of a program: (material sampler index or 255, location), deduplicated in order.
fn binding_key(layout: &ProgramLayout, material: &Material) -> Vec<(usize, u32)> {
    let mut entries = Vec::new();
    for (name, location) in &layout.samplers {
        if *location == u32::MAX {
            continue;
        }
        let Some(target) = material.samplers.get(name) else { continue };
        let source = material.sampler_names.iter().position(|n| n == target).unwrap_or(255);
        if !entries.contains(&(source, *location)) {
            entries.push((source, *location));
        }
    }
    entries
}

struct VertexKey {
    /// (format, offset, location, compressed buffer index) per bound attribute.
    key: Vec<(u32, u16, i64, usize)>,
    bytes: i64,
    streams: i64,
    attributes: i64,
}

fn vertex_key(d: Reader, vertex: i64, material: &Material, layout: &ProgramLayout) -> Result<VertexKey> {
    let mut attributes = HashMap::new();
    for at in entries(d, vertex + 8, d.byte(vertex + 0x4c)? as i64, 16)? {
        if attributes.insert(d.name(d.pointer(at)?)?, at).is_some() {
            return fail("Duplicate vertex attribute.");
        }
    }
    let mut buffers: Vec<u8> = Vec::new();
    let mut key = Vec::new();
    let mut max_location = -1;
    for (name, location) in &layout.attributes {
        let Some(&at) = material.attributes.get(name).and_then(|target| attributes.get(target.as_str())) else { continue };
        let source = d.byte(at + 0xe)?;
        if source >= d.byte(vertex + 0x4d)? || *location >= 16 {
            return fail("Vertex stream or location is out of range.");
        }
        let compressed = match buffers.iter().position(|&b| b == source) {
            Some(index) => index,
            None => {
                buffers.push(source);
                buffers.len() - 1
            }
        };
        key.push((d.u32(at + 8)?, d.u16(at + 0xc)?, *location, compressed));
        max_location = max_location.max(*location);
    }
    let work = buffers.len() as i64 * 8 + (max_location + 1) * 4;
    Ok(VertexKey { bytes: 0x230 + align_up(work, 64), streams: buffers.len() as i64, attributes: key.len() as i64, key })
}

fn location_key(locations: &HashMap<String, u32>, names: &[&str]) -> Vec<u32> {
    names.iter().map(|name| locations.get(&format!("gsys_{name}")).copied().unwrap_or(u32::MAX)).collect()
}

// SDK instructions validated by a separate 237-case ARM64 harness.
fn vertex_command_bytes(streams: i64, attributes: i64) -> Result<i64> {
    if streams > 16 {
        return fail("Too many resource vertex streams.");
    }
    Ok(if attributes == 0 { 0 } else { 132 + 28 * streams })
}

/// The offsets of a BFRES array's entries, whose pointer is stored at the given field.
fn entries(d: Reader, field: i64, count: i64, stride: i64) -> Result<Vec<i64>> {
    let start = d.pointer(field)?;
    if start == 0 && count != 0 {
        return fail("Missing allocation metadata array.");
    }
    d.range(start, count * stride)?;
    Ok((0..count).map(|i| start + i * stride).collect())
}

/// The key names of a BFRES dictionary in entry order, skipping its root node.
fn dictionary_names(d: Reader, at: i64, count: i64) -> Result<Vec<String>> {
    if at == 0 && count == 0 {
        return Ok(Vec::new());
    }
    if at == 0 || d.u32(at + 4)? as i64 != count {
        return fail("Allocation dictionary count mismatch.");
    }
    d.range(at + 8, (count + 1) * 16)?;
    (0..count).map(|i| Ok(d.name(d.pointer(at + 32 + i * 16)?)?.to_owned())).collect()
}

/// A shader assignment table: a name dictionary paired with an array of value strings.
fn assignments(d: Reader, at: i64, array: i64, dictionary: i64, count: i64) -> Result<HashMap<String, String>> {
    let names = dictionary_names(d, d.pointer(at + dictionary)?, count)?;
    let values = entries(d, at + array, count, 8)?;
    let mut result = HashMap::with_capacity(names.len());
    for (name, value) in names.into_iter().zip(values) {
        if result.insert(name, d.name(d.pointer(value)?)?.to_owned()).is_some() {
            return Err(Error("Duplicate shader assignment.".into()));
        }
    }
    Ok(result)
}
