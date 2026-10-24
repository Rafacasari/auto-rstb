use std::fmt;
use std::fs::File;
use std::io::Read;

/// Why a file could not be sized. Callers keep the game's own sizing for it (no table entry).
#[derive(Debug)]
pub struct Error(pub String);

pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Error(error.to_string())
    }
}

pub(crate) fn fail<T>(message: impl Into<String>) -> Result<T> {
    Err(Error(message.into()))
}

/// A file's bytes, failing instead of aborting when they do not fit in memory.
pub(crate) fn read_file(path: &str) -> Result<Vec<u8>> {
    let mut file = File::open(path)?;
    let mut data = Vec::new();
    data.try_reserve_exact(file.metadata()?.len() as usize).map_err(|_| Error(format!("Out of memory reading {path}.")))?;
    file.read_to_end(&mut data)?;
    Ok(data)
}

/// Rounds up to a power-of-two alignment.
pub(crate) fn align_up(value: i64, alignment: i64) -> i64 {
    (value + alignment - 1) & -alignment
}

/// A C# `checked((int)value)` conversion.
pub(crate) fn to_int(value: i64) -> Result<i64> {
    if value < i32::MIN as i64 || value > i32::MAX as i64 {
        return fail("Arithmetic operation resulted in an overflow.");
    }
    Ok(value)
}

/// Bounds-checked little-endian reads over a resource's bytes. Reads outside the data fail,
/// so malformed files never produce a partial size.
#[derive(Clone, Copy)]
pub(crate) struct Reader<'a> {
    data: &'a [u8],
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Reader { data }
    }

    pub fn len(&self) -> i64 {
        self.data.len() as i64
    }

    pub fn range(&self, at: i64, count: i64) -> Result<&'a [u8]> {
        if at < 0 || count < 0 || at > self.len() - count {
            return fail("Resource data exceeds its bounds.");
        }
        Ok(&self.data[at as usize..(at + count) as usize])
    }

    pub fn slice(&self, at: i64, count: i64) -> Result<Reader<'a>> {
        Ok(Reader::new(self.range(at, count)?))
    }

    pub fn byte(&self, at: i64) -> Result<u8> {
        Ok(self.range(at, 1)?[0])
    }

    pub fn u16(&self, at: i64) -> Result<u16> {
        Ok(u16::from_le_bytes(self.range(at, 2)?.try_into().unwrap()))
    }

    pub fn u32(&self, at: i64) -> Result<u32> {
        Ok(u32::from_le_bytes(self.range(at, 4)?.try_into().unwrap()))
    }

    pub fn u64(&self, at: i64) -> Result<u64> {
        Ok(u64::from_le_bytes(self.range(at, 8)?.try_into().unwrap()))
    }

    pub fn u32_be(&self, at: i64) -> Result<u32> {
        Ok(u32::from_be_bytes(self.range(at, 4)?.try_into().unwrap()))
    }

    pub fn magic(&self, at: i64, magic: &[u8]) -> Result<bool> {
        Ok(self.range(at, magic.len() as i64)? == magic)
    }

    pub fn require_magic(&self, at: i64, magic: &[u8]) -> Result<()> {
        if !self.magic(at, magic)? {
            return fail(format!("Expected {}", String::from_utf8_lossy(magic)));
        }
        Ok(())
    }

    /// A relocated 64-bit offset into this data (BFRES, BNTX); zero is a null pointer.
    pub fn pointer(&self, at: i64) -> Result<i64> {
        let value = self.u64(at)?;
        if value > self.len() as u64 {
            return fail("Resource pointer exceeds its bounds.");
        }
        Ok(value as i64)
    }

    /// A BFRES string: a 16-bit length, strict UTF-8 bytes and a terminator. A null pointer reads as "".
    pub fn name(&self, at: i64) -> Result<&'a str> {
        if at == 0 {
            return Ok("");
        }
        let length = self.u16(at)? as i64;
        if self.byte(at + 2 + length)? != 0 {
            return fail("Unterminated BFRES string.");
        }
        let bytes = self.range(at + 2, length)?;
        if bytes.contains(&0) {
            return fail("BFRES string contains an embedded terminator.");
        }
        std::str::from_utf8(bytes).or_else(|_| fail("BFRES string is not valid UTF-8."))
    }
}
