//! ACNH 3.0.3 resource size table (RSTC) calculation, ported from Heaven Tool.
//! Every size is the resource heap the game's traced load path allocates for a file;
//! files whose loads are not modelled are left to the game's own sizing.

mod bfres;
mod geometry;
mod heap;
mod physics;
mod reader;
mod sarc;
mod shader;
pub mod sizing;
pub mod table;
pub mod yaz0;
mod zstd;

pub use reader::{Error, Result};

/// Changes whenever calculated sizes may change, so cached sizes are recalculated.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The table's file name hash.
pub(crate) fn crc32(name: &str) -> u32 {
    crc32fast::hash(name.as_bytes())
}
