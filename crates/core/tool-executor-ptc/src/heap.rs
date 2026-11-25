//! A metered heap for the interpreter.
//!
//! QuickJS enforces its own memory limit, but once the heap is full it often
//! cannot allocate even its "out of memory" error, and the program then ends
//! with a pending exception of `null`, indistinguishable from `throw null`.
//! This allocator enforces the limit itself and records that it refused an
//! allocation, so an exhausted heap is always reported as such.
//!
//! Every allocation is delegated to rquickjs's own `RustAllocator`; this type
//! only counts bytes and says no. The `unsafe` here is limited to forwarding
//! pointers that QuickJS obtained from this same allocator.

use rquickjs::allocator::{Allocator, RustAllocator};
use std::cell::Cell;
use std::ptr;
use std::rc::Rc;

pub(crate) struct MeteredHeap {
    limit: usize,
    used: usize,
    exhausted: Rc<Cell<bool>>,
}

impl MeteredHeap {
    /// The heap and the flag that turns true when it refuses an allocation.
    pub(crate) fn new(limit: usize) -> (Self, Rc<Cell<bool>>) {
        let exhausted = Rc::new(Cell::new(false));
        (Self { limit, used: 0, exhausted: exhausted.clone() }, exhausted)
    }

    /// Whether `extra` more bytes fit; records the refusal when they do not.
    fn admit(&mut self, extra: usize) -> bool {
        let fits = self.used.checked_add(extra).is_some_and(|total| total <= self.limit);
        if !fits {
            self.exhausted.set(true);
        }
        fits
    }

    fn track(&mut self, block: *mut u8) -> *mut u8 {
        if !block.is_null() {
            // SAFETY: `block` was just returned non-null by `RustAllocator`.
            self.used += unsafe { RustAllocator::usable_size(block) };
        }
        block
    }
}

// SAFETY: every pointer returned comes from `RustAllocator` (or is null on
// refusal), and every pointer received was returned by this allocator, so
// forwarding it to `RustAllocator` upholds that allocator's contract.
unsafe impl Allocator for MeteredHeap {
    fn alloc(&mut self, size: usize) -> *mut u8 {
        if !self.admit(size) {
            return ptr::null_mut();
        }
        let block = RustAllocator.alloc(size);
        self.track(block)
    }

    fn calloc(&mut self, count: usize, size: usize) -> *mut u8 {
        let Some(total) = count.checked_mul(size) else {
            self.exhausted.set(true);
            return ptr::null_mut();
        };
        if !self.admit(total) {
            return ptr::null_mut();
        }
        let block = RustAllocator.calloc(count, size);
        self.track(block)
    }

    unsafe fn dealloc(&mut self, block: *mut u8) {
        // SAFETY: `block` is non-null (the caller filters null) and came from
        // this allocator, i.e. from `RustAllocator`.
        unsafe {
            self.used = self.used.saturating_sub(RustAllocator::usable_size(block));
            RustAllocator.dealloc(block);
        }
    }

    unsafe fn realloc(&mut self, block: *mut u8, new_size: usize) -> *mut u8 {
        // C `realloc` semantics, which `RustAllocator` does not handle itself:
        // a null block is an allocation, a zero size is a free.
        if block.is_null() {
            return self.alloc(new_size);
        }
        if new_size == 0 {
            // SAFETY: as in `dealloc`.
            unsafe { self.dealloc(block) };
            return ptr::null_mut();
        }
        // SAFETY: `block` is non-null and came from `RustAllocator`.
        let old = unsafe { RustAllocator::usable_size(block) };
        if new_size > old && !self.admit(new_size - old) {
            return ptr::null_mut();
        }
        // SAFETY: as above; on failure the old block stays valid and counted.
        let moved = unsafe { RustAllocator.realloc(block, new_size) };
        if !moved.is_null() {
            // SAFETY: `moved` was just returned non-null by `RustAllocator`.
            let now = unsafe { RustAllocator::usable_size(moved) };
            self.used = self.used.saturating_sub(old) + now;
        }
        moved
    }

    unsafe fn usable_size(block: *mut u8) -> usize {
        // SAFETY: forwarded verbatim; `block` came from `RustAllocator`.
        unsafe { RustAllocator::usable_size(block) }
    }
}
