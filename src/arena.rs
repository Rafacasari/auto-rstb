//! The game gives malloc only 32 MiB (nninitStartup) and puts the rest of its memory in sead's
//! heaps. The update runs on the game's thread and borrows one block from the sead ExpHeap with
//! the most free space, through the heap's own tryAlloc and free. It returns the block before
//! the game continues. Allocations made during the update come from that block (malloc when it
//! is full); everything else uses malloc.
//!
//! The heaps are found at runtime from sead's layout (see `find_sead`), so no address of a
//! particular game version is used.

use std::alloc::{GlobalAlloc, Layout, System};
use std::arch::asm;
use std::cell::UnsafeCell;
use std::fmt::Write;
use std::ops::Range;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};

use talc::{ErrOnOom, Span, Talc};

/// sead's layout, as traced in 3.0.3. It belongs to the sead library, not to the game's code.
mod sead {
    /// HeapMgr::sRootHeaps is a FixedPtrArray: count (i32), capacity (i32), a pointer to the
    /// items, then the items themselves.
    pub const PTR_ARRAY_ITEMS: usize = 0x10;
    /// The root heap's name, given by HeapMgr::initialize.
    pub const ROOT_HEAP_NAME: &[u8] = b"RootHeap\0";

    /// INamable's name string.
    pub const NAME: usize = 0x28;
    pub const SIZE: usize = 0x38;
    /// OffsetList of child heaps: list head (prev, next), count, then the offset of each
    /// child's list node.
    pub const CHILDREN: usize = 0x48;
    pub const CHILDREN_NODE_OFFSET: usize = 0x5C;

    // Virtual functions, as ExpHeap::create and getFreeSize use them
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
/// Bounds the root heap array's capacity, which sead sets to 4.
const MAX_ROOT_HEAPS: i32 = 64;

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

/// svcQueryMemory's description of the mapping around an address.
#[repr(C)]
#[derive(Default)]
struct MemoryInfo {
    address: usize,
    size: usize,
    kind: u32,
    attributes: u32,
    permissions: u32,
    ipc_count: u32,
    device_count: u32,
    padding: u32,
}

const PERMISSION_READ: u32 = 1;

fn query_memory(address: usize) -> Option<MemoryInfo> {
    let mut info = MemoryInfo::default();
    let result: usize;
    // svcQueryMemory (0x06): x0 = the info to fill, x2 = the address; returns the result in w0
    // and the page info in w1
    unsafe {
        asm!("svc 0x6", inout("x0") &raw mut info as usize => result, out("x1") _, in("x2") address, options(nostack));
    }
    (result as u32 == 0).then_some(info)
}

/// Whether `length` bytes at `start` are mapped readable, so reading them cannot fault.
fn readable(start: usize, length: usize) -> bool {
    let Some(end) = start.checked_add(length) else { return false };
    let mut address = start;
    while address < end {
        let Some(info) = query_memory(address) else { return false };
        if info.permissions & PERMISSION_READ == 0 || info.size == 0 {
            return false;
        }
        address = info.address.saturating_add(info.size);
    }
    true
}

/// The main module's mapped range and its .bss, from the MOD0 header every Switch module has.
fn main_module() -> Option<(Range<usize>, Range<usize>)> {
    let text = unsafe { skyline::hooks::getRegionAddress(skyline::hooks::Region::Text) } as usize;
    let mod0 = text + unsafe { read::<u32>(text + 4) } as usize;
    if !readable(mod0, 16) || unsafe { read::<[u8; 4]>(mod0) } != *b"MOD0" {
        return None;
    }
    let field = |offset: usize| mod0.wrapping_add_signed(unsafe { read::<i32>(mod0 + offset) } as isize);
    let bss = field(8)..field(12);
    Some((text..bss.end, bss))
}

/// Where sead keeps the heaps.
struct Sead {
    module: usize,
    root_heaps: usize,
    /// The vtable that identifies an ExpHeap.
    exp_heap_vtable: usize,
}

/// Finds HeapMgr::sRootHeaps: the array in the main module's .bss whose items follow it and
/// whose first item is named "RootHeap". The root heap is an ExpHeap, so its vtable identifies
/// the others.
fn find_sead() -> Option<Sead> {
    let (module, bss) = main_module()?;
    let arrays = (bss.start.next_multiple_of(8)..bss.end.saturating_sub(sead::PTR_ARRAY_ITEMS)).step_by(8);
    arrays.into_iter().find_map(|array| {
        let (count, capacity) = unsafe { (read::<i32>(array), read::<i32>(array + 4)) };
        let items = array + sead::PTR_ARRAY_ITEMS;
        let fixed_array = unsafe { read::<usize>(array + 8) } == items
            && (1..=MAX_ROOT_HEAPS).contains(&capacity)
            && (1..=capacity).contains(&count)
            && items + capacity as usize * 8 <= bss.end;
        if !fixed_array {
            return None;
        }
        let root = unsafe { read::<usize>(items) };
        if !readable(root, sead::NAME + 8) {
            return None;
        }
        let name = unsafe { read::<usize>(root + sead::NAME) };
        let length = sead::ROOT_HEAP_NAME.len();
        let named_root = readable(name, length) && unsafe { std::slice::from_raw_parts(name as *const u8, length) } == sead::ROOT_HEAP_NAME;
        let vtable = unsafe { read::<usize>(root) };
        (named_root && module.contains(&vtable)).then_some(Sead { module: module.start, root_heaps: array, exp_heap_vtable: vtable })
    })
}

/// Every sead heap, root heaps first, each followed by its descendants.
unsafe fn heaps(root_heaps: usize) -> Vec<usize> {
    let count = unsafe { read::<i32>(root_heaps) }.max(0) as usize;
    let items = unsafe { read::<usize>(root_heaps + 8) };
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
    let Some(sead) = find_sead() else {
        report.push_str("sead's root heaps were not found.\n");
        return None;
    };
    let _ = writeln!(report, "sead heaps (sRootHeaps at main + {:#X}):", sead.root_heaps - sead.module);
    let mut lender = None;
    for heap in unsafe { heaps(sead.root_heaps) } {
        let exp_heap = unsafe { read::<usize>(heap) } == sead.exp_heap_vtable;
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
    let mut report = String::new();
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
