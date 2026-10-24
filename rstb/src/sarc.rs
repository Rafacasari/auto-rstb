use crate::reader::{fail, Reader, Result};

/// A SARC member: its name and absolute data range.
pub(crate) struct Member {
    pub name: Option<String>,
    pub offset: i64,
    pub length: i64,
}

impl Member {
    pub fn data<'a>(&self, archive: &'a [u8]) -> &'a [u8] {
        &archive[self.offset as usize..(self.offset + self.length) as usize]
    }

    pub fn is(&self, name: &str) -> bool {
        self.name.as_deref() == Some(name)
    }
}

/// Reads a little-endian SARC's member ranges in node order.
pub(crate) fn read(archive: &[u8]) -> Result<Vec<Member>> {
    let sarc = Reader::new(archive);
    let length = sarc.len();
    if !sarc.magic(0, b"SARC")?
        || sarc.u16(4)? != 0x14
        || sarc.u16(6)? != 0xfeff
        || sarc.u32(8)? as i64 != length
        || sarc.u16(16)? != 0x100
        || !sarc.magic(20, b"SFAT")?
        || sarc.u16(24)? != 12
    {
        return fail("Expected a little-endian SARC archive.");
    }

    let count = sarc.u16(26)? as i64;
    let names = 32 + count * 16;
    let data = sarc.u32(12)? as i64;
    if names > length || data > length {
        return fail("SARC nodes exceed the archive.");
    }
    let has_names = names <= length - 8 && sarc.magic(names, b"SFNT")?;

    let mut members = Vec::with_capacity(count as usize);
    for i in 0..count {
        let node = 32 + i * 16;
        let attributes = sarc.u32(node + 4)?;
        let start = data + sarc.u32(node + 8)? as i64;
        let end = data + sarc.u32(node + 12)? as i64;
        if end < start || end > length {
            return fail("SARC member exceeds the archive.");
        }
        let mut name = None;
        if attributes >> 24 != 0 {
            let at = names + 8 + (attributes & 0xffffff) as i64 * 4;
            let terminator = match has_names && at < data {
                true => sarc.range(at, data - at)?.iter().position(|&b| b == 0),
                false => None,
            };
            let Some(terminator) = terminator else { return fail("SARC member name is invalid.") };
            // ASCII decoding, as .NET's Encoding.ASCII: other bytes become '?'.
            let bytes = sarc.range(at, terminator as i64)?;
            name = Some(bytes.iter().map(|&b| if b < 0x80 { b as char } else { '?' }).collect());
        }
        members.push(Member { name, offset: start, length: end - start });
    }
    Ok(members)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A little-endian SARC holding named members.
    fn archive(members: &[(&str, &[u8])]) -> Vec<u8> {
        let mut names: Vec<u8> = Vec::new();
        let mut data: Vec<u8> = Vec::new();
        let mut nodes = Vec::new();
        for (name, bytes) in members {
            nodes.extend(0u32.to_le_bytes()); // Name hash, unused by the reader
            nodes.extend((0x0100_0000 | (names.len() as u32 / 4)).to_le_bytes());
            nodes.extend((data.len() as u32).to_le_bytes());
            nodes.extend(((data.len() + bytes.len()) as u32).to_le_bytes());
            names.extend(name.as_bytes());
            names.resize((names.len() + 4) & !3, 0);
            data.extend(*bytes);
        }
        let data_offset = 0x14 + 12 + nodes.len() + 8 + names.len();
        let mut out = b"SARC".to_vec();
        out.extend(0x14u16.to_le_bytes());
        out.extend(0xfeffu16.to_le_bytes());
        out.extend(((data_offset + data.len()) as u32).to_le_bytes());
        out.extend((data_offset as u32).to_le_bytes());
        out.extend(0x100u32.to_le_bytes());
        out.extend(b"SFAT");
        out.extend(12u16.to_le_bytes());
        out.extend((members.len() as u16).to_le_bytes());
        out.extend(0x65u32.to_le_bytes());
        out.extend(nodes);
        out.extend(b"SFNT\x08\0\0\0");
        out.extend(names);
        out.extend(data);
        out
    }

    #[test]
    fn reads_members() {
        let sarc = archive(&[("output.bfres", b"model"), ("a/b.txt", b"")]);
        let members = read(&sarc).unwrap();
        assert_eq!(members.len(), 2);
        assert!(members[0].is("output.bfres"));
        assert_eq!(members[0].data(&sarc), b"model");
        assert!(members[1].is("a/b.txt"));
        assert_eq!(members[1].data(&sarc), b"");
    }

    #[test]
    fn rejects_invalid_archives() {
        let sarc = archive(&[("output.bfres", b"model")]);
        assert!(read(&sarc[..sarc.len() - 1]).is_err());
        assert!(read(b"Yaz0").is_err());
    }
}
