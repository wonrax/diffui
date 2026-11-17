//! The engine's heap, capped.
//!
//! QuickJS has its own memory limit, but passing it only shows up as an
//! exception the script can swallow (or as a thrown `null` when even the
//! error object didn't fit). The host wants to know for certain, so the cap
//! lives in the allocator and trips a flag.

use std::cell::Cell;
use std::ptr;
use std::rc::Rc;

use rquickjs::allocator::{Allocator, RustAllocator};

pub(crate) struct Capped {
    limit: usize,
    used: usize,
    /// Set once an allocation was refused.
    hit: Rc<Cell<bool>>,
}

impl Capped {
    pub(crate) fn new(limit: usize, hit: Rc<Cell<bool>>) -> Self {
        Self {
            limit,
            used: 0,
            hit,
        }
    }

    fn admits(&mut self, more: usize) -> bool {
        let fits = self
            .used
            .checked_add(more)
            .is_some_and(|total| total <= self.limit);
        if !fits {
            self.hit.set(true);
        }
        fits
    }

    /// Count a block the inner allocator handed out.
    fn took(&mut self, block: *mut u8) -> *mut u8 {
        if !block.is_null() {
            // SAFETY: `block` was just returned by `RustAllocator`.
            self.used += unsafe { RustAllocator::usable_size(block) };
        }
        block
    }
}

// SAFETY: every block comes from `RustAllocator`, which upholds the trait's
// contract, or is null; this only decides whether to ask it.
unsafe impl Allocator for Capped {
    fn alloc(&mut self, size: usize) -> *mut u8 {
        if !self.admits(size) {
            return ptr::null_mut();
        }
        let block = RustAllocator.alloc(size);
        self.took(block)
    }

    fn calloc(&mut self, count: usize, size: usize) -> *mut u8 {
        if !count
            .checked_mul(size)
            .is_some_and(|total| self.admits(total))
        {
            return ptr::null_mut();
        }
        let block = RustAllocator.calloc(count, size);
        self.took(block)
    }

    unsafe fn dealloc(&mut self, block: *mut u8) {
        // SAFETY: the caller passes a live block from this allocator.
        unsafe {
            self.used -= RustAllocator::usable_size(block);
            RustAllocator.dealloc(block);
        }
    }

    unsafe fn realloc(&mut self, block: *mut u8, new_size: usize) -> *mut u8 {
        // SAFETY: the caller passes a live block from this allocator.
        let old = unsafe { RustAllocator::usable_size(block) };
        if !self.admits(new_size.saturating_sub(old)) {
            return ptr::null_mut();
        }
        // SAFETY: as above; on failure the old block stays valid and counted.
        let moved = unsafe { RustAllocator.realloc(block, new_size) };
        if !moved.is_null() {
            self.used -= old;
            self.took(moved);
        }
        moved
    }

    unsafe fn usable_size(block: *mut u8) -> usize {
        // SAFETY: the caller passes a live block from this allocator.
        unsafe { RustAllocator::usable_size(block) }
    }
}
