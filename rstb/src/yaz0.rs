//! Yaz0 compression: a 16-byte header ("Yaz0", big-endian decompressed size, 8 zero bytes),
//! then groups of a flag byte and eight literal bytes or back-references.

use crate::reader::{fail, Error, Result};

const HEADER_BYTES: usize = 16;
const WINDOW: usize = 0x1000;
const MIN_MATCH: usize = 3;
const MAX_MATCH: usize = 0xff + 0x12;

pub fn is_yaz0(data: &[u8]) -> bool {
    data.starts_with(b"Yaz0")
}

/// Decompresses a whole Yaz0 file.
pub fn decompress(data: &[u8]) -> Result<Vec<u8>> {
    if data.len() < HEADER_BYTES || !is_yaz0(data) {
        return fail("Expected Yaz0 data.");
    }
    let size = u32::from_be_bytes(data[4..8].try_into().unwrap()) as usize;
    let mut out = Vec::new();
    out.try_reserve_exact(size).map_err(|_| Error("Out of memory for Yaz0 data.".into()))?;
    let mut input = data[HEADER_BYTES..].iter().copied();
    let mut next = || input.next().ok_or_else(|| Error("Yaz0 data is truncated.".into()));
    let (mut flags, mut flag_bits) = (0u8, 0);
    while out.len() < size {
        if flag_bits == 0 {
            (flags, flag_bits) = (next()?, 8);
        }
        flag_bits -= 1;
        let literal = flags & 0x80 != 0;
        flags <<= 1;
        if literal {
            out.push(next()?);
            continue;
        }
        let (first, second) = (next()? as usize, next()? as usize);
        let distance = ((first & 0xf) << 8 | second) + 1;
        let length = match first >> 4 {
            0 => next()? as usize + 0x12,
            n => n + 2,
        };
        if distance > out.len() {
            return fail("Yaz0 back-reference before the data start.");
        }
        let start = out.len() - distance;
        // A reference may overlap the bytes it produces, so it is copied byte by byte
        for i in start..start + length.min(size - out.len()) {
            out.push(out[i]);
        }
    }
    Ok(out)
}

/// Compresses data with greedy longest matches.
pub fn compress(data: &[u8]) -> Vec<u8> {
    let mut encoder = Encoder::new(data.len());
    encoder.encode(data);
    encoder.finish(data.len())
}

struct Encoder {
    out: Vec<u8>,
    flag_at: usize,
    flag_bits: u32,
}

impl Encoder {
    fn new(capacity: usize) -> Self {
        let mut out = Vec::with_capacity(capacity + HEADER_BYTES);
        out.resize(HEADER_BYTES, 0);
        out[..4].copy_from_slice(b"Yaz0");
        Encoder { out, flag_at: 0, flag_bits: 8 }
    }

    fn chunk(&mut self, literal: bool) {
        if self.flag_bits == 8 {
            self.flag_at = self.out.len();
            self.out.push(0);
            self.flag_bits = 0;
        }
        if literal {
            self.out[self.flag_at] |= 0x80 >> self.flag_bits;
        }
        self.flag_bits += 1;
    }

    fn literal(&mut self, value: u8) {
        self.chunk(true);
        self.out.push(value);
    }

    fn reference(&mut self, distance: usize, length: usize) {
        self.chunk(false);
        let back = distance - 1;
        if length >= 0x12 {
            self.out.extend_from_slice(&[(back >> 8) as u8, back as u8, (length - 0x12) as u8]);
        } else {
            self.out.extend_from_slice(&[((length - 2) << 4 | back >> 8) as u8, back as u8]);
        }
    }

    fn encode(&mut self, data: &[u8]) {
        const HASH_BITS: u32 = 15;
        const MAX_CHAIN: usize = 128;
        let hash = |at: usize| {
            ((u32::from_le_bytes([data[at], data[at + 1], data[at + 2], 0]).wrapping_mul(2654435761)) >> (32 - HASH_BITS)) as usize
        };
        let mut head = vec![usize::MAX; 1 << HASH_BITS];
        let mut previous = vec![usize::MAX; WINDOW];
        let insert = |at: usize, head: &mut [usize], previous: &mut [usize]| {
            if at + MIN_MATCH <= data.len() {
                let h = hash(at);
                previous[at % WINDOW] = head[h];
                head[h] = at;
            }
        };
        let mut at = 0;
        while at < data.len() {
            let (mut best_length, mut best_distance) = (0, 0);
            if at + MIN_MATCH <= data.len() {
                let limit = (data.len() - at).min(MAX_MATCH);
                let mut candidate = head[hash(at)];
                let mut chain = 0;
                while candidate != usize::MAX && at - candidate <= WINDOW && chain < MAX_CHAIN {
                    let length = data[candidate..].iter().zip(&data[at..at + limit]).take_while(|(a, b)| a == b).count();
                    if length > best_length {
                        (best_length, best_distance) = (length, at - candidate);
                        if length == limit {
                            break;
                        }
                    }
                    let next = previous[candidate % WINDOW];
                    if next == usize::MAX || next >= candidate {
                        break;
                    }
                    candidate = next;
                    chain += 1;
                }
            }
            if best_length >= MIN_MATCH {
                self.reference(best_distance, best_length);
                for i in at..at + best_length {
                    insert(i, &mut head, &mut previous);
                }
                at += best_length;
            } else {
                self.literal(data[at]);
                insert(at, &mut head, &mut previous);
                at += 1;
            }
        }
    }

    fn finish(mut self, decompressed: usize) -> Vec<u8> {
        self.out[4..8].copy_from_slice(&(decompressed as u32).to_be_bytes());
        self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Literals, short repeats and a run longer than the longest back-reference.
    fn sample() -> Vec<u8> {
        let mut data: Vec<u8> = (0..=255).collect();
        data.extend(b"abc".repeat(40));
        data.extend([7; 1000]);
        data
    }

    #[test]
    fn round_trip() {
        for data in [Vec::new(), b"a".to_vec(), sample()] {
            assert_eq!(decompress(&compress(&data)).unwrap(), data);
        }
    }

    #[test]
    fn rejects_corrupt_data() {
        let compressed = compress(&sample());
        assert!(decompress(&compressed[..compressed.len() - 1]).is_err());
        // A first group made of a back-reference points before the data's start
        let mut reference = compressed[..HEADER_BYTES].to_vec();
        reference.extend([0x00, 0x10, 0x00]);
        assert!(decompress(&reference).is_err());
    }
}
