//! The shared Switch BFSHA (Park_UBER) reflection used by the resource loader's allocations.
//! Only the reflection metadata is read; shader binaries are never decoded.

use std::cell::OnceCell;
use std::collections::HashMap;

use crate::reader::{fail, read_file, to_int, Reader, Result};
use crate::{sarc, yaz0};

/// A shader string: a 16-bit length, bytes and a terminator. A null pointer reads as "".
fn string(data: Reader, at: i64) -> Result<String> {
    if at == 0 {
        return Ok(String::new());
    }
    let length = data.u16(at)? as i64;
    if data.byte(at + 2 + length)? != 0 {
        return fail("Unterminated shader string.");
    }
    Ok(String::from_utf8_lossy(data.range(at + 2, length)?).into_owned())
}

/// The key names of a dictionary in entry order, skipping its root node.
fn dictionary(data: Reader, at: i64) -> Result<Vec<String>> {
    if at == 0 {
        return Ok(Vec::new());
    }
    let count = to_int(data.u32(at + 4)? as i64)?;
    if count > 65535 {
        return fail("Shader dictionary is too large.");
    }
    data.range(at + 8, (count + 1) * 16)?;
    (0..count).map(|i| string(data, data.pointer(at + 32 + i * 16)?)).collect()
}

fn signature(data: Reader, at: i64, expected: &[u8]) -> Result<()> {
    if !data.magic(at, expected)? {
        return fail(format!("Expected {} shader section.", String::from_utf8_lossy(expected)));
    }
    Ok(())
}

pub(crate) struct ShaderOption {
    pub name: String,
    pub choices: Vec<String>,
    default_choice: i64,
    word: i64,
    shift: u32,
    mask: u32,
}

pub(crate) struct UniformBlock {
    name: String,
    kind: u8,
    size: i64,
}

pub(crate) struct ShadingModel {
    pub name: String,
    offset: i64,
    program_array: i64,
    key_table: i64,
    program_count: i64,
    key_words: i64,
    static_key_words: i64,
    pub options: Vec<ShaderOption>,
    attributes: Vec<(String, i64)>,
    sampler_names: Vec<String>,
    uniform_blocks: Vec<UniformBlock>,
    static_programs: OnceCell<HashMap<Vec<u32>, Vec<i64>>>,
}

/// A compiled program's resource reflection.
pub(crate) struct ProgramLayout {
    /// Vertex attribute names and locations.
    pub attributes: Vec<(String, i64)>,
    /// Sampler locations in the model's sampler order.
    pub samplers: Vec<(String, u32)>,
    pub uniforms: HashMap<String, u32>,
    pub storage: HashMap<String, u32>,
}

pub(crate) struct ShaderArchive {
    bytes: Vec<u8>,
    pub name: String,
    pub models: Vec<ShadingModel>,
}

impl ShaderArchive {
    pub fn parse(bytes: Vec<u8>) -> Result<Self> {
        let data = Reader::new(&bytes);
        signature(data, 0, b"FSHA")?;
        if data.u32(8)? != 0x00080000 || data.u16(12)? != 0xfeff {
            return fail("Only the ACNH Switch BFSHA v8 layout is supported.");
        }
        if data.u32(28)? as i64 != data.len() {
            return fail("Shader archive size mismatch.");
        }
        let name = string(data, data.pointer(56)?)?;
        let array = data.pointer(72)?;
        let count = data.u16(120)? as i64;
        data.range(array, count * 256)?;
        let models = (0..count).map(|i| ShadingModel::parse(data, array + i * 256)).collect::<Result<_>>()?;
        Ok(ShaderArchive { bytes, name, models })
    }

    fn data(&self) -> Reader<'_> {
        Reader::new(&self.bytes)
    }

    pub fn material_uniform_bytes(&self, model: &ShadingModel) -> Result<i64> {
        let index = self.data().byte(model.offset + 0xf0)? as usize;
        if index == 255 {
            return Ok(0);
        }
        match model.uniform_blocks.get(index) {
            Some(block) => Ok(block.size),
            None => fail("Invalid material uniform block index."),
        }
    }

    pub fn has_shader_option_bindings(&self, model: &ShadingModel) -> Result<bool> {
        let array = self.data().pointer(model.offset + 8)?;
        let count = self.data().u16(model.offset + 0xe4)? as i64;
        self.data().range(array, count * 0x28)?;
        for i in 0..count {
            if self.data().byte(array + i * 0x28 + 0x1c)? & 1 != 0 {
                return Ok(true);
            }
        }
        Ok(model.uniform_blocks.iter().any(|b| b.kind == 4 && b.size != 0))
    }

    pub fn shader_object_work_bytes(&self, model: &ShadingModel) -> Result<i64> {
        let statics = model.static_key_words * 4;
        let options = (self.data().u16(model.offset + 0xe4)? as i64 + 31) / 32 * 4;
        Ok(aligned_blocks(&[statics, statics, options, 0x48]))
    }

    pub fn selector_work_bytes(&self, model: &ShadingModel) -> i64 {
        let dynamics = (model.key_words - model.static_key_words) * 4;
        aligned_blocks(&[dynamics, dynamics, dynamics])
    }

    /// Programs whose static key matches the choices.
    pub fn static_candidates(&self, model: &ShadingModel, choices: &HashMap<String, String>) -> Result<Vec<i64>> {
        if model.static_programs.get().is_none() {
            let mut statics: HashMap<Vec<u32>, Vec<i64>> = HashMap::new();
            for i in 0..model.program_count {
                let words = (0..model.static_key_words)
                    .map(|j| self.data().u32(model.key_table + (i * model.key_words + j) * 4))
                    .collect::<Result<Vec<_>>>()?;
                statics.entry(words).or_default().push(i);
            }
            let _ = model.static_programs.set(statics);
        }
        let words = self.key(model, choices, false)?;
        Ok(model.static_programs.get().unwrap().get(&words).cloned().unwrap_or_default())
    }

    /// The candidate whose dynamic key matches the choices, or `None`.
    pub fn find_program(&self, model: &ShadingModel, candidates: &[i64], choices: &HashMap<String, String>) -> Result<Option<i64>> {
        let words = self.key(model, choices, true)?;
        let mut found = None;
        for &index in candidates {
            let mut matches = true;
            for (j, &word) in words.iter().enumerate() {
                let at = model.key_table + (index * model.key_words + model.static_key_words + j as i64) * 4;
                if self.data().u32(at)? != word {
                    matches = false;
                    break;
                }
            }
            if !matches {
                continue;
            }
            if found.is_some() {
                return fail("Duplicate compiled shader key.");
            }
            found = Some(index);
        }
        Ok(found)
    }

    fn key(&self, model: &ShadingModel, choices: &HashMap<String, String>, dynamic: bool) -> Result<Vec<u32>> {
        let static_count = self.data().u16(model.offset + 0xe4)? as usize;
        let length = if dynamic { model.key_words - model.static_key_words } else { model.static_key_words };
        let mut words = vec![0u32; length as usize];
        let options = if dynamic { static_count..model.options.len() } else { 0..static_count };
        for i in options {
            let Some(option) = model.options.get(i) else { return fail("Shader option index is out of range.") };
            let word = option.word - if dynamic { model.static_key_words } else { 0 };
            if word < 0 || word >= length {
                return fail("Shader option references the wrong key region.");
            }
            let mut selected = option.default_choice;
            if let Some(value) = choices.get(&option.name) {
                match option.choices.iter().position(|c| c == value) {
                    Some(found) => selected = found as i64,
                    // The dynamic setter leaves its default for an unknown choice.
                    // An unknown static value cannot establish a compiled candidate.
                    None if !dynamic => return fail(format!("Unknown shader choice {}={value}.", option.name)),
                    None => {}
                }
            }
            let bits = (selected as u32).wrapping_shl(option.shift);
            if bits & !option.mask != 0 {
                return fail("Shader choice exceeds its mask.");
            }
            let word = &mut words[word as usize];
            *word = (*word & !option.mask) | bits;
        }
        Ok(words)
    }

    pub fn program_layout(&self, model: &ShadingModel, index: i64) -> Result<ProgramLayout> {
        if index < 0 || index >= model.program_count {
            return fail("Shader program index is out of range.");
        }
        let program = model.program_array + index * 0x40;
        let mask = self.data().u32(program + 0x30)?;
        let attributes = model
            .attributes
            .iter()
            .enumerate()
            // C# `1u << i` masks the shift count.
            .filter(|(i, (_, location))| mask & 1u32.wrapping_shl(*i as u32) != 0 && *location >= 0)
            .map(|(_, attribute)| attribute.clone())
            .collect();
        let block_names: Vec<String> = model.uniform_blocks.iter().map(|b| b.name.clone()).collect();
        let storage_names = dictionary(self.data(), self.data().pointer(model.offset + 0x78)?)?;
        Ok(ProgramLayout {
            attributes,
            samplers: self.locations(self.data().pointer(program)?, &model.sampler_names)?,
            uniforms: self.locations(self.data().pointer(program + 0x10)?, &block_names)?.into_iter().collect(),
            storage: self.locations(self.data().pointer(program + 0x18)?, &storage_names)?.into_iter().collect(),
        })
    }

    fn locations(&self, at: i64, names: &[String]) -> Result<Vec<(String, u32)>> {
        let mut locations: Vec<(String, u32)> = Vec::with_capacity(names.len());
        if names.is_empty() {
            return Ok(locations);
        }
        if at == 0 {
            return fail("Missing compiled resource locations.");
        }
        self.data().range(at, names.len() as i64 * 24)?;
        for (i, name) in names.iter().enumerate() {
            let entry = at + i as i64 * 24;
            // The game packs source stages 0, 2, 1, and an absent compute slot.
            let packed = (self.data().u32(entry)? & 255)
                | (self.data().u32(entry + 8)? & 255) << 8
                | (self.data().u32(entry + 4)? & 255) << 16
                | 0xff000000;
            if locations.iter().any(|(existing, _)| existing == name) {
                return fail("Duplicate shader resource name.");
            }
            locations.push((name.clone(), packed));
        }
        Ok(locations)
    }
}

impl ShadingModel {
    fn parse(data: Reader, at: i64) -> Result<Self> {
        let name = string(data, data.pointer(at)?)?;
        let program_array = data.pointer(at + 136)?;
        let key_table = data.pointer(at + 144)?;
        let bnsh_base = data.pointer(at + 168)?;
        let program_count = data.u16(at + 232)? as i64;
        let static_words = data.byte(at + 234)? as i64;
        let key_words = static_words + data.byte(at + 235)? as i64;
        data.range(program_array, program_count * 64)?;
        data.range(key_table, program_count * key_words * 4)?;
        signature(data, bnsh_base, b"BNSH")?;
        let bnsh_size = to_int(data.u32(bnsh_base + 28)? as i64)?;
        data.range(bnsh_base, bnsh_size)?;
        if bnsh_size < 32 {
            return fail("Invalid BNSH file size.");
        }
        let mut options = read_options(data, data.pointer(at + 8)?, data.pointer(at + 16)?, 0, key_words)?;
        options.extend(read_options(data, data.pointer(at + 24)?, data.pointer(at + 32)?, static_words, key_words)?);

        let attribute_array = data.pointer(at + 40)?;
        let attributes = dictionary(data, data.pointer(at + 48)?)?
            .into_iter()
            .enumerate()
            .map(|(i, name)| Ok((name, data.byte(attribute_array + i as i64 * 2 + 1)? as i8 as i64)))
            .collect::<Result<_>>()?;
        let sampler_names = dictionary(data, data.pointer(at + 64)?)?;

        let block_array = data.pointer(at + 88)?;
        let mut uniform_blocks = Vec::new();
        for name in dictionary(data, data.pointer(at + 96)?)? {
            let b = block_array + uniform_blocks.len() as i64 * 32;
            let size = data.u16(b + 26)? as i64;
            let defaults = data.pointer(b + 16)?;
            let uniform_array = data.pointer(b)?;
            // Uniform offsets are not needed, but the game's reflection reads them.
            for i in 0..dictionary(data, data.pointer(b + 8)?)?.len() as i64 {
                data.u16(uniform_array + i * 16 + 12)?;
            }
            let kind = data.byte(b + 25)?;
            if defaults != 0 {
                data.range(defaults, size)?;
            }
            uniform_blocks.push(UniformBlock { name, kind, size });
        }

        Ok(ShadingModel {
            name,
            offset: at,
            program_array,
            key_table,
            program_count,
            key_words,
            static_key_words: static_words,
            options,
            attributes,
            sampler_names,
            uniform_blocks,
            static_programs: OnceCell::new(),
        })
    }
}

fn read_options(data: Reader, array: i64, names: i64, dynamic_base: i64, key_words: i64) -> Result<Vec<ShaderOption>> {
    let names = dictionary(data, names)?;
    data.range(array, names.len() as i64 * 40)?;
    let mut options = Vec::with_capacity(names.len());
    for (i, name) in names.into_iter().enumerate() {
        let at = array + i as i64 * 40;
        let choices = dictionary(data, data.pointer(at + 8)?)?;
        let default_choice = data.byte(at + 25)? as i64;
        let mut word = data.byte(at + 30)? as i64;
        let shift = data.byte(at + 31)? as u32;
        if dynamic_base > 0 {
            word = word - data.byte(at + 29)? as i64 + dynamic_base;
        }
        let mask = data.u32(at + 32)?;
        if choices.len() != data.byte(at + 24)? as usize
            || default_choice >= choices.len() as i64
            || word < 0
            || word >= key_words
            || shift > 31
            || mask == 0
        {
            return fail("Invalid shader option bit packing.");
        }
        options.push(ShaderOption { name, choices, default_choice, word, shift, mask });
    }
    Ok(options)
}

fn aligned_blocks(sizes: &[i64]) -> i64 {
    sizes.iter().filter(|&&size| size != 0).fold(0, |end, size| ((end + 7) & !7) + size)
}

/// Loads the shader archive file: a BFSHA, optionally inside a SARC, optionally Yaz0-compressed.
pub(crate) fn load(path: &str) -> Result<ShaderArchive> {
    let mut data = read_file(path)?;
    if yaz0::is_yaz0(&data) {
        data = yaz0::decompress(&data)?;
    }
    if !data.starts_with(b"FSHA") {
        let members = sarc::read(&data)?;
        let mut archives = members.iter().filter(|m| m.data(&data).starts_with(b"FSHA"));
        let (Some(archive), None) = (archives.next(), archives.next()) else {
            return fail("Expected one BFSHA file in the shader archive.");
        };
        let (start, end) = (archive.offset as usize, (archive.offset + archive.length) as usize);
        data.truncate(end);
        data.drain(..start);
    }
    ShaderArchive::parse(data)
}
