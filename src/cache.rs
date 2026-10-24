//! Sizes from earlier launches, so only new or changed files are sized again. A file is
//! unchanged when its length and stamp (see `platform::stamp`) match. Every size depends on
//! the sizing code and the shader archive, which together form the cache's epoch.

use std::collections::BTreeMap;

use crate::platform;

#[derive(PartialEq)]
struct Sized {
    length: u64,
    stamp: String,
    size: i64,
}

pub struct Cache {
    epoch: String,
    earlier: BTreeMap<String, Sized>,
    current: BTreeMap<String, Sized>,
}

impl Cache {
    /// Loads the cache, or starts an empty one when it is missing, unreadable or from another epoch.
    pub fn load(path: &str, epoch: String) -> Cache {
        let mut earlier = BTreeMap::new();
        let text = platform::read_optional(path).ok().flatten().and_then(|bytes| String::from_utf8(bytes).ok()).unwrap_or_default();
        let mut lines = text.lines();
        if lines.next() == Some(epoch.as_str()) {
            for line in lines {
                let mut fields = line.splitn(4, '\t');
                let (Some(length), Some(stamp), Some(size), Some(relative)) = (fields.next(), fields.next(), fields.next(), fields.next())
                else {
                    continue;
                };
                let (Ok(length), Ok(size)) = (length.parse(), size.parse()) else { continue };
                earlier.insert(relative.to_string(), Sized { length, stamp: stamp.to_string(), size });
            }
        }
        Cache { epoch, earlier, current: BTreeMap::new() }
    }

    pub fn get(&self, relative: &str, length: u64, stamp: &str) -> Option<i64> {
        self.earlier.get(relative).filter(|s| s.length == length && s.stamp == stamp).map(|s| s.size)
    }

    /// Records a current file's size; files not recorded are dropped on save.
    pub fn insert(&mut self, relative: &str, length: u64, stamp: String, size: i64) {
        self.current.insert(relative.to_string(), Sized { length, stamp, size });
    }

    /// Writes the cache when it changed.
    pub fn save(&self, path: &str) -> Result<(), String> {
        if self.current == self.earlier && platform::file_exists(path) {
            return Ok(());
        }
        let mut text = format!("{}\n", self.epoch);
        for (relative, s) in &self.current {
            text += &format!("{}\t{}\t{}\t{relative}\n", s.length, s.stamp, s.size);
        }
        platform::write(path, text.as_bytes())
    }
}
