//! The game gives malloc only 32 MiB (nninitStartup, 0x71005BF120) and puts the rest of its
//! memory in sead's heaps before plugins run: Skyline runs them from the game's first
//! nn::vi::CreateLayer call, on the game's thread, after sead::HeapMgr is set up. The update
//! borrows one block from the sead ExpHeap with the most free space, through the heap's own
//! tryAlloc and free, and returns it before the game continues. Allocations made during the
//! update come from that block (malloc when it is full); everything else uses malloc.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::UnsafeCell;
use std::fmt::Write;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};

use talc::{ErrOnOom, Span, Talc};

/// Offsets in 3.0.3's main module, and sead::Heap's layout there.
mod sead {
    /// sead::HeapMgr::sRootHeaps, a PtrArray: count (i32), capacity (i32), items.
    pub const ROOT_HEAPS: usize = 0x59E_7550;
    /// sead::ExpHeap's vtable, as its constructor (0x71037C6FFC) stores it.
    pub const EXP_HEAP_VTABLE: usize = 0x4A6_4448;

    /// INamable's name string.
    pub const NAME: usize = 0x28;
    pub const SIZE: usize = 0x38;
    /// OffsetList of child heaps: list head (prev, next), count, then the offset of each
    /// child's list node.
    pub const CHILDREN: usize = 0x48;
    pub const CHILDREN_NODE_OFFSET: usize = 0x5C;

    // Virtual functions, as ExpHeap::create (0x71037C6D48) and getFreeSize use them
    pub const TRY_ALLOC: usize = 6;
    pub const FREE: usize = 7;
    pub const MAX_ALLOCATABLE_SIZE: usize = 16;
}

/// Free space left in the lending heap, for the game's other threads.
const HEAP_RESERVE: usize = 64 << 20;
const MAX_LOAN: usize = 1 << 30;
/// Room for the decompressed shader archive (41 MiB in 3.0.3) next to a large model.
const MIN_LOAN: usize = 128 << 20;
const LOAN_ALIGNMENT: i32 = 0x1000;
/// Guards the heap walk against a corrupt list.
const MAX_HEAPS: usize = 4096;

struct Block {
    talc: Talc<ErrOnOom>,
    base: usize,
    size: usize,
    live: usize,
}

pub struct Allocator {
    locked: AtomicBool,
    block: UnsafeCell<Option<Block>>,
}

// The block is only touched with `locked` held
unsafe impl Sync for Allocator {}

#[global_allocator]
static ALLOCATOR: Allocator = Allocator { locked: AtomicBool::new(false), block: UnsafeCell::new(None) };

impl Allocator {
    fn with_block<T>(&self, f: impl FnOnce(&mut Option<Block>) -> T) -> T {
        while self.locked.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
            std::hint::spin_loop();
        }
        let result = f(unsafe { &mut *self.block.get() });
        self.locked.store(false, Ordering::Release);
        result
    }
}

unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let borrowed = self.with_block(|block| {
            let block = block.as_mut()?;
            let pointer = unsafe { block.talc.malloc(layout) }.ok()?;
            block.live += 1;
            Some(pointer.as_ptr())
        });
        borrowed.unwrap_or_else(|| unsafe { System.alloc(layout) })
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        let borrowed = self.with_block(|block| match block {
            Some(block) if (block.base..block.base + block.size).contains(&(pointer as usize)) => {
                unsafe { block.talc.free(NonNull::new_unchecked(pointer), layout) };
                block.live -= 1;
                true
            }
            _ => false,
        });
        if !borrowed {
            unsafe { System.dealloc(pointer, layout) };
        }
    }
}

unsafe fn read<T: Copy>(address: usize) -> T {
    unsafe { *(address as *const T) }
}

unsafe fn virtual_function(heap: usize, slot: usize) -> usize {
    unsafe { read(read::<usize>(heap) + slot * 8) }
}

unsafe fn max_allocatable_size(heap: usize) -> usize {
    let f: unsafe extern "C" fn(usize, i32) -> usize = unsafe { std::mem::transmute(virtual_function(heap, sead::MAX_ALLOCATABLE_SIZE)) };
    unsafe { f(heap, LOAN_ALIGNMENT) }
}

/// Allocates from the heap's end, away from where the game allocates.
unsafe fn try_alloc(heap: usize, size: usize) -> usize {
    let f: unsafe extern "C" fn(usize, usize, i32) -> usize = unsafe { std::mem::transmute(virtual_function(heap, sead::TRY_ALLOC)) };
    unsafe { f(heap, size, -LOAN_ALIGNMENT) }
}

unsafe fn free(heap: usize, address: usize) {
    let f: unsafe extern "C" fn(usize, usize) = unsafe { std::mem::transmute(virtual_function(heap, sead::FREE)) };
    unsafe { f(heap, address) }
}

unsafe fn heap_name(heap: usize) -> String {
    let name = unsafe { read::<*const std::ffi::c_char>(heap + sead::NAME) };
    if name.is_null() {
        return String::from("?");
    }
    unsafe { std::ffi::CStr::from_ptr(name) }.to_string_lossy().into_owned()
}

/// Every sead heap, root heaps first, each followed by its descendants.
unsafe fn heaps(module: usize) -> Vec<usize> {
    let roots = module + sead::ROOT_HEAPS;
    let count = unsafe { read::<i32>(roots) }.max(0) as usize;
    let items = unsafe { read::<usize>(roots + 8) };
    let mut pending: Vec<usize> = (0..count).rev().map(|i| unsafe { read::<usize>(items + i * 8) }).filter(|&heap| heap != 0).collect();
    let mut heaps = Vec::new();
    while let Some(heap) = pending.pop() {
        if heaps.len() == MAX_HEAPS {
            break;
        }
        heaps.push(heap);
        let head = heap + sead::CHILDREN;
        let node_offset = unsafe { read::<i32>(heap + sead::CHILDREN_NODE_OFFSET) } as isize;
        let mut children = Vec::new();
        let mut node = unsafe { read::<usize>(head + 8) };
        while node != head && node != 0 && children.len() < MAX_HEAPS {
            children.push(node.wrapping_add_signed(-node_offset));
            node = unsafe { read::<usize>(node + 8) };
        }
        pending.extend(children.into_iter().rev());
    }
    heaps
}

/// Memory lent by a sead heap for the update.
pub struct Loan {
    pub size: usize,
    /// Each heap's name, size and largest free block, with the lending heap marked.
    pub report: String,
}

/// Borrows from the ExpHeap with the largest free block. Only ExpHeaps are used: their free
/// returns the block, unlike frame heaps.
fn borrow(report: &mut String) -> Option<(usize, usize, usize)> {
    let module = unsafe { skyline::hooks::getRegionAddress(skyline::hooks::Region::Text) } as usize;
    let mut lender = None;
    for heap in unsafe { heaps(module) } {
        let exp_heap = unsafe { read::<usize>(heap) } == module + sead::EXP_HEAP_VTABLE;
        let free = if exp_heap { unsafe { max_allocatable_size(heap) } } else { 0 };
        let _ = writeln!(
            report,
            "  {} ({} MiB, {} MiB free{})",
            unsafe { heap_name(heap) },
            unsafe { read::<usize>(heap + sead::SIZE) } >> 20,
            free >> 20,
            if exp_heap { "" } else { ", not an ExpHeap" }
        );
        if exp_heap && lender.is_none_or(|(_, most)| free > most) {
            lender = Some((heap, free));
        }
    }
    let (heap, free) = lender?;
    let size = free.checked_sub(HEAP_RESERVE)?.min(MAX_LOAN) & !(LOAN_ALIGNMENT as usize - 1);
    if size < MIN_LOAN {
        return None;
    }
    let base = unsafe { try_alloc(heap, size) };
    let _ = writeln!(
        report,
        "Borrowing {} MiB from {}: {}.",
        size >> 20,
        unsafe { heap_name(heap) },
        if base == 0 { "refused" } else { "done" }
    );
    (base != 0).then_some((heap, base, size))
}

/// Runs `f` with allocations from memory borrowed from the game's heaps. `f` must free
/// everything it allocates: its result cannot own heap memory.
pub fn with_borrowed_memory<T: Copy>(f: impl FnOnce(&Loan) -> T) -> T {
    let mut report = String::from("sead heaps:\n");
    let Some((heap, base, size)) = borrow(&mut report) else { return f(&Loan { size: 0, report }) };
    ALLOCATOR.with_block(|block| {
        let mut talc = Talc::new(ErrOnOom);
        // Claiming only fails for spans too small to hold the allocator's metadata
        let _ = unsafe { talc.claim(Span::new(base as *mut u8, (base + size) as *mut u8)) };
        *block = Some(Block { talc, base, size, live: 0 });
    });
    let result = f(&Loan { size, report });
    let live = ALLOCATOR.with_block(|block| {
        let live = block.as_ref().map_or(0, |b| b.live);
        if live == 0 {
            *block = None;
        }
        live
    });
    if live == 0 {
        unsafe { free(heap, base) };
    } else {
        // Freeing the block would corrupt what still lives in it; the game loses this memory
        println!("[auto-rstb] {live} allocations outlived the update; {} MiB stays borrowed.", size >> 20);
    }
    result
}
