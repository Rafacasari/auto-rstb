//! The RSTC resource size table: the allocation budget the game reserves for each file.

use std::collections::BTreeMap;

use crate::crc32;
use crate::reader::{fail, Reader, Result};

/// The table's path in the romfs.
pub const PATH: &str = "System/Resource/ResourceSizeTable.srsizetable";

const COLLIDED_NAME_BYTES: usize = 128;
/// Size and DLC number.
const ENTRY_BYTES: usize = 8;

/// A table entry's recorded size and DLC number.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Entry {
    pub size: u32,
    pub dlc: u32,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Table {
    /// Entries whose name hash is unique, by hash.
    pub unique: BTreeMap<u32, Entry>,
    /// Entries whose name hash collides with another entry's, by name (ordinal order).
    pub collided: BTreeMap<String, Entry>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Update {
    None,
    Changed,
    Added,
    Removed,
}

impl Table {
    /// Parses a decompressed table.
    pub fn parse(data: &[u8]) -> Result<Table> {
        let d = Reader::new(data);
        if !d.magic(0, b"RSTC")? {
            return fail("Not a valid RSTC table.");
        }
        let unique_count = d.u32(4)? as i64;
        let collided_count = d.u32(8)? as i64;
        let mut table = Table { unique: BTreeMap::new(), collided: BTreeMap::new() };
        let mut at = 12;
        let entry = |at: i64| -> Result<Entry> { Ok(Entry { size: d.u32(at)?, dlc: d.u32(at + 4)? }) };
        for _ in 0..unique_count {
            let crc = d.u32(at)?;
            if table.unique.insert(crc, entry(at + 4)?).is_some() {
                return fail(format!("Duplicate table entry {crc:08X}."));
            }
            at += 4 + ENTRY_BYTES as i64;
        }
        for _ in 0..collided_count {
            let bytes = d.range(at, COLLIDED_NAME_BYTES as i64)?;
            let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
            let Ok(name) = std::str::from_utf8(&bytes[..end]) else { return fail("Collided table name is not text.") };
            if !name.is_ascii() || table.collided.insert(name.to_owned(), entry(at + COLLIDED_NAME_BYTES as i64)?).is_some() {
                return fail(format!("Invalid or duplicate table entry {name}."));
            }
            at += (COLLIDED_NAME_BYTES + ENTRY_BYTES) as i64;
        }
        table.normalize();
        Ok(table)
    }

    /// The decompressed table: unique entries by hash, then collided entries by name.
    pub fn serialize(&self) -> Vec<u8> {
        let mut out =
            Vec::with_capacity(12 + self.unique.len() * (4 + ENTRY_BYTES) + self.collided.len() * (COLLIDED_NAME_BYTES + ENTRY_BYTES));
        out.extend_from_slice(b"RSTC");
        out.extend_from_slice(&(self.unique.len() as u32).to_le_bytes());
        out.extend_from_slice(&(self.collided.len() as u32).to_le_bytes());
        let write = |out: &mut Vec<u8>, entry: &Entry| {
            out.extend_from_slice(&entry.size.to_le_bytes());
            out.extend_from_slice(&entry.dlc.to_le_bytes());
        };
        for (crc, entry) in &self.unique {
            out.extend_from_slice(&crc.to_le_bytes());
            write(&mut out, entry);
        }
        for (name, entry) in &self.collided {
            let start = out.len();
            out.extend_from_slice(name.as_bytes());
            out.resize(start + COLLIDED_NAME_BYTES, 0);
            write(&mut out, entry);
        }
        out
    }

    /// The entry the game finds for a file, if any.
    pub fn get(&self, name: &str) -> Option<Entry> {
        self.collided.get(name).or_else(|| self.unique.get(&crc32(name))).copied()
    }

    /// Stores a calculated size. Negative or oversized values remove the entry, so the game
    /// sizes the file itself. A unique hash is assumed to belong to the named file; only
    /// collided entries store names to tell files apart.
    pub fn set(&mut self, name: &str, size: i64) -> Result<Update> {
        let crc = crc32(name);
        let Ok(size) = u32::try_from(size) else {
            let removed = self.collided.remove(name).is_some() || self.unique.remove(&crc).is_some();
            self.normalize();
            return Ok(if removed { Update::Removed } else { Update::None });
        };
        let existing = match self.collided.get_mut(name) {
            Some(entry) => Some(entry),
            None => self.unique.get_mut(&crc),
        };
        if let Some(entry) = existing {
            if entry.size == size {
                return Ok(Update::None);
            }
            entry.size = size;
            return Ok(Update::Changed);
        }
        if self.collided.keys().any(|other| crc32(other) == crc) {
            if !name.is_ascii() || name.len() > COLLIDED_NAME_BYTES {
                return fail(format!("{name} cannot be stored as a collided table name."));
            }
            self.collided.insert(name.to_owned(), Entry { size, dlc: 0 });
        } else {
            self.unique.insert(crc, Entry { size, dlc: 0 });
        }
        Ok(Update::Added)
    }

    /// Collided entries left without a partner become unique again.
    fn normalize(&mut self) {
        let mut counts: BTreeMap<u32, usize> = BTreeMap::new();
        for name in self.collided.keys() {
            *counts.entry(crc32(name)).or_default() += 1;
        }
        let single: Vec<String> = self.collided.keys().filter(|name| counts[&crc32(name)] == 1).cloned().collect();
        for name in single {
            let entry = self.collided.remove(&name).unwrap();
            self.unique.entry(crc32(&name)).or_insert(entry);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Two names with the same CRC32
    const FIRST: &str = "Test/29685295.bin";
    const SECOND: &str = "Test/32060020.bin";

    fn entry(size: u32) -> Entry {
        Entry { size, dlc: 0 }
    }

    fn collided_table() -> Table {
        Table {
            unique: BTreeMap::from([(crc32("Other.bin"), entry(1))]),
            collided: BTreeMap::from([(FIRST.to_owned(), entry(2)), (SECOND.to_owned(), entry(3))]),
        }
    }

    #[test]
    fn round_trip() {
        let table = collided_table();
        assert_eq!(Table::parse(&table.serialize()).unwrap(), table);
        assert!(Table::parse(b"RSTB\0\0\0\0\0\0\0\0").is_err());
    }

    #[test]
    fn updates_unique_entries() {
        let mut table = Table { unique: BTreeMap::new(), collided: BTreeMap::new() };
        assert_eq!(table.set("A.bin", 10).unwrap(), Update::Added);
        assert_eq!(table.set("A.bin", 10).unwrap(), Update::None);
        assert_eq!(table.set("A.bin", 20).unwrap(), Update::Changed);
        assert_eq!(table.get("A.bin"), Some(entry(20)));
        assert_eq!(table.set("A.bin", -1).unwrap(), Update::Removed);
        assert_eq!(table.get("A.bin"), None);
    }

    #[test]
    fn collided_entries_are_told_apart_by_name() {
        let mut table = collided_table();
        assert_eq!(table.get(FIRST), Some(entry(2)));
        assert_eq!(table.get(SECOND), Some(entry(3)));
        // Without its partner, the remaining name becomes a unique entry again
        assert_eq!(table.set(FIRST, -1).unwrap(), Update::Removed);
        assert!(table.collided.is_empty());
        assert_eq!(table.get(SECOND), Some(entry(3)));
    }
}
