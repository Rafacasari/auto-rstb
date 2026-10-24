//! Zstandard (".zs") resources. Frame headers are walked first so the output buffer is reserved
//! fallibly at its exact size; the decoder's own window is probed before it allocates it.

use std::io::Read;

use ruzstd::decoding::{BlockDecodingStrategy, FrameDecoder};

use crate::reader::{fail, Error, Reader, Result};

const MAGIC: u32 = 0xfd2fb528;
/// Larger windows are refused; ACNH's resources use at most a few MiB.
const MAX_WINDOW: u64 = 64 << 20;

pub fn is_zstd(data: &[u8]) -> bool {
    data.starts_with(&MAGIC.to_le_bytes())
}

/// One frame's byte range, back-reference window and declared decompressed size.
struct Frame {
    start: usize,
    end: usize,
    window: u64,
    content_size: Option<u64>,
}

/// Walks the frames of a Zstandard stream, skipping skippable frames.
fn frames(data: &[u8]) -> Result<Vec<Frame>> {
    let d = Reader::new(data);
    let mut frames = Vec::new();
    let mut at = 0i64;
    while at < d.len() {
        let magic = d.u32(at)?;
        if magic & 0xffff_fff0 == 0x184d_2a50 {
            at += 8 + d.u32(at + 4)? as i64;
            continue;
        }
        if magic != MAGIC {
            return fail("Expected a Zstandard frame.");
        }
        let start = at;
        let descriptor = d.byte(at + 4)?;
        at += 5;
        let single_segment = descriptor & 0x20 != 0;
        if descriptor & 0x08 != 0 {
            return fail("Reserved Zstandard frame bit is set.");
        }
        let mut window = 0;
        if !single_segment {
            let byte = d.byte(at)? as u64;
            at += 1;
            let base = 1u64 << (10 + (byte >> 3));
            window = base + base / 8 * (byte & 7);
        }
        if descriptor & 3 != 0 {
            return fail("Zstandard dictionaries are not supported.");
        }
        let content_size = match (descriptor >> 6, single_segment) {
            (0, false) => None,
            (0, true) => Some(d.byte(at)? as u64),
            (1, _) => Some(d.u16(at)? as u64 + 256),
            (2, _) => Some(d.u32(at)? as u64),
            _ => Some(d.u64(at)?),
        };
        at += match descriptor >> 6 {
            0 => single_segment as i64,
            1 => 2,
            2 => 4,
            _ => 8,
        };
        if single_segment {
            window = content_size.unwrap_or(0);
        }
        loop {
            let header = d.range(at, 3)?;
            let header = u32::from_le_bytes([header[0], header[1], header[2], 0]);
            at += 3 + match (header >> 1) & 3 {
                0 | 2 => (header >> 3) as i64,
                1 => 1,
                _ => return fail("Reserved Zstandard block type."),
            };
            if header & 1 != 0 {
                break;
            }
        }
        if descriptor & 0x04 != 0 {
            at += 4;
        }
        if at > d.len() {
            return fail("Truncated Zstandard frame.");
        }
        frames.push(Frame { start: start as usize, end: at as usize, window, content_size });
    }
    Ok(frames)
}

/// Fails, instead of aborting on allocation, when the decoder's window would not fit in memory.
fn new_decoder(window: u64) -> Result<FrameDecoder> {
    if window > MAX_WINDOW {
        return fail("Zstandard window is too large.");
    }
    let mut probe: Vec<u8> = Vec::new();
    probe.try_reserve_exact(2 * window as usize + (1 << 20)).map_err(|_| Error("Out of memory for the Zstandard window.".into()))?;
    drop(probe);
    let mut decoder = FrameDecoder::new();
    decoder.set_max_window_size(MAX_WINDOW);
    Ok(decoder)
}

fn decode_error(error: impl std::fmt::Display) -> Error {
    Error(format!("Zstandard data is invalid: {error}"))
}

/// Decompressed size of a frame without a declared size, by decoding it.
fn count_frame(frame: &[u8], window: u64) -> Result<u64> {
    let mut decoder = new_decoder(window)?;
    let mut input = frame;
    decoder.init(&mut input).map_err(decode_error)?;
    let mut scratch = vec![0u8; 0x10000];
    let mut total = 0u64;
    loop {
        decoder.decode_blocks(&mut input, BlockDecodingStrategy::UptoBytes(1 << 20)).map_err(decode_error)?;
        loop {
            let read = decoder.read(&mut scratch)?;
            if read == 0 {
                break;
            }
            total += read as u64;
        }
        if decoder.is_finished() && decoder.can_collect() == 0 {
            return Ok(total);
        }
    }
}

/// The decompressed length of a Zstandard stream.
pub fn decompressed_length(data: &[u8]) -> Result<u64> {
    total_length(data, &frames(data)?)
}

fn total_length(data: &[u8], frames: &[Frame]) -> Result<u64> {
    frames.iter().try_fold(0u64, |total, frame| {
        let size = match frame.content_size {
            Some(size) => size,
            None => count_frame(&data[frame.start..frame.end], frame.window)?,
        };
        total.checked_add(size).ok_or_else(|| Error("Zstandard data is too large.".into()))
    })
}

/// Decompresses a Zstandard stream.
pub fn decompress(data: &[u8]) -> Result<Vec<u8>> {
    let frames = frames(data)?;
    let length = usize::try_from(total_length(data, &frames)?).map_err(|_| Error("Zstandard data is too large.".into()))?;
    let mut out = Vec::new();
    out.try_reserve_exact(length).map_err(|_| Error("Out of memory for Zstandard data.".into()))?;
    out.resize(length, 0);
    let window = frames.iter().map(|f| f.window).max().unwrap_or(0);
    let written = new_decoder(window)?.decode_all(data, &mut out).map_err(decode_error)?;
    if written != length {
        return fail("Zstandard frames do not match their declared sizes.");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruzstd::encoding::{compress_to_vec, CompressionLevel};

    fn frame(data: &[u8]) -> Vec<u8> {
        compress_to_vec(data, CompressionLevel::Fastest)
    }

    #[test]
    fn decodes_every_frame() {
        let (first, second) = (b"first frame ".repeat(100), b"second".repeat(50));
        let mut stream = frame(&first);
        // A skippable frame with three bytes of user data
        stream.extend([0x50, 0x2a, 0x4d, 0x18, 3, 0, 0, 0, 1, 2, 3]);
        stream.extend(frame(&second));
        let expected = [first, second].concat();
        assert!(is_zstd(&stream));
        assert_eq!(decompressed_length(&stream).unwrap(), expected.len() as u64);
        assert_eq!(decompress(&stream).unwrap(), expected);
    }

    #[test]
    fn rejects_truncated_data() {
        let stream = frame(&b"data".repeat(100));
        let truncated = &stream[..stream.len() - 1];
        assert!(decompressed_length(truncated).is_err());
        assert!(decompress(truncated).is_err());
    }
}
