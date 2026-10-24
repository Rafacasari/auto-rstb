use crate::reader::{align_up, fail, Result};

/// One allocation request: payload bytes and a power-of-two alignment.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Request {
    pub bytes: i64,
    pub alignment: i64,
}

pub(crate) const fn request(bytes: i64, alignment: i64) -> Request {
    Request { bytes, alignment }
}

const HEADER_BYTES: i64 = 32;

/// ACNH 3.0.3's forward ExpHeap allocation path (0x71037C77A0), with sufficient space and no
/// frees: the end of the last payload, or the starting address when nothing is requested. The
/// starting address is the free block's header address, not the first payload address.
pub(crate) fn high_water_mark(requests: &[Request], initial_free_block: i64) -> Result<i64> {
    if initial_free_block < 0 || initial_free_block & 7 != 0 {
        return fail("The initial free block must be 8-aligned.");
    }
    let mut front = initial_free_block;
    for request in requests {
        if request.bytes < 0 {
            return fail("Heap requests cannot be negative.");
        }
        let alignment = request.alignment;
        if !(8..=8192).contains(&alignment) || alignment & (alignment - 1) != 0 {
            return fail("Only the native-verified positive alignments from 8 through 8192 are supported.");
        }
        let address = align_up(front + HEADER_BYTES, alignment);
        front = address + align_up(request.bytes.max(8), 8);
    }
    Ok(front)
}

// 3.0.3 startup GPU-pool policy (0x71005C9B44 → 0x71037FEBE0): managed reuse with
// 16 KiB minimum pools. Every resource ExpHeap already holds one empty minimum pool.
const MINIMUM_POOL_BYTES: i64 = 0x4000;

/// Parent ExpHeap requests from 3.0.3's GPU-pool manager (0x71037F8590) for GPU buffers
/// (memory flags 3) in loader order. Requests that fit an existing pool add nothing; others
/// create a pool whose metadata and page-aligned backing follow on the same heap.
pub(crate) fn gpu_pool_parent_requests(requests: &[Request]) -> Result<Vec<Request>> {
    let mut pools = vec![Pool::new(MINIMUM_POOL_BYTES, entries(MINIMUM_POOL_BYTES, 0))];
    let mut parent = Vec::new();
    for wanted in requests {
        let alignment = wanted.alignment;
        if wanted.bytes <= 0 {
            return fail("GPU requests must be nonempty.");
        }
        if alignment <= 0 || alignment & (alignment - 1) != 0 {
            return fail("GPU alignment must be a positive power of two.");
        }
        // Virtual heaps start at 0x100000; larger alignments need base-aware placement.
        if alignment > 0x100000 {
            return fail("GPU alignments above the native virtual base are unresolved.");
        }
        let mut best: Option<(usize, i64)> = None;
        for (i, pool) in pools.iter().enumerate() {
            let Some(candidate) = pool.try_place(wanted) else { continue };
            // Native selection maximizes request/free-size (single-precision).
            let better = match best {
                None => true,
                Some((b, _)) => wanted.bytes as f32 / pool.free as f32 > wanted.bytes as f32 / pools[b].free as f32,
            };
            if better {
                best = Some((i, candidate));
            }
        }
        let (index, offset) = match best {
            Some(found) => found,
            None => {
                let backing = align_up(wanted.bytes, 4096).max(MINIMUM_POOL_BYTES);
                let entries = entries(backing, wanted.bytes);
                // 0x138 pool record + 0x108 virtual heap + 32 bytes per entry.
                parent.push(request(0x240 + entries * 0x20, 8));
                parent.push(request(backing, align_up(alignment, 4096)));
                pools.push(Pool::new(backing, entries));
                let Some(offset) = pools.last().unwrap().try_place(wanted) else {
                    return fail("New GPU pool cannot satisfy its initial request.");
                };
                (pools.len() - 1, offset)
            }
        };
        pools[index].add(offset, wanted.bytes);
    }
    Ok(parent)
}

fn entries(backing: i64, initial_request: i64) -> i64 {
    if backing - initial_request <= 0x1ff {
        1
    } else {
        (backing >> 9).min(128)
    }
}

struct Pool {
    length: i64,
    capacity: usize,
    allocated: Vec<(i64, i64)>,
    free: i64,
}

impl Pool {
    fn new(length: i64, capacity: i64) -> Self {
        Pool { length, capacity: capacity as usize, allocated: Vec::new(), free: length }
    }

    /// First fit over the sorted blocks.
    fn try_place(&self, request: &Request) -> Option<i64> {
        if self.allocated.len() >= self.capacity {
            return None;
        }
        let size = request.bytes.max(8);
        let alignment = request.alignment.max(8);
        let mut offset = 0;
        for &(start, bytes) in &self.allocated {
            offset = align_up(offset, alignment);
            if size <= start - offset {
                return Some(offset);
            }
            offset = start + bytes;
        }
        offset = align_up(offset, alignment);
        (size <= self.length - offset).then_some(offset)
    }

    fn add(&mut self, offset: i64, size: i64) {
        let size = size.max(8);
        self.allocated.push((offset, size));
        self.allocated.sort_by_key(|&(start, _)| start);
        self.free -= size;
    }
}
