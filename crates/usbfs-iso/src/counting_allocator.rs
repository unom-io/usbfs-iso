//! A thread-local allocation counter, so "no allocation after `start()`" is a test rather than a
//! claim (design rule 2).
//!
//! Thread-local on purpose: `cargo test` runs tests in parallel on one process, so a global
//! counter would see every other test's allocations and the assertion would be noise. Counting per
//! thread makes the measurement belong to the test that took it.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
}

/// Wraps the system allocator and tallies allocations on the calling thread.
pub struct Counting;

// SAFETY: every method forwards directly to `System`, which is a correct `GlobalAlloc`, and the
// counter is a thread-local `Cell<usize>` that neither aliases nor reinterprets the allocation.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.with(|c| c.set(c.get() + 1));
        // SAFETY: `layout` is forwarded unchanged from our caller, who upholds `alloc`'s contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr`/`layout` are forwarded unchanged from our caller.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCS.with(|c| c.set(c.get() + 1));
        // SAFETY: as `dealloc`, plus `new_size` which the caller guarantees is valid for `layout`.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCS.with(|c| c.set(c.get() + 1));
        // SAFETY: as `alloc`.
        unsafe { System.alloc_zeroed(layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Run `f` and report how many allocations it made on this thread.
pub fn allocations_during<R>(f: impl FnOnce() -> R) -> (R, usize) {
    let before = ALLOCS.with(|c| c.get());
    let out = f();
    (out, ALLOCS.with(|c| c.get()) - before)
}
