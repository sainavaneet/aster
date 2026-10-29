//! A counting allocator for the tests, to prove the per-frame draw paths of the
//! modern chrome never allocate. Counting is per thread and off by default.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

pub(super) struct Counting;

thread_local! {
    static ENABLED: Cell<bool> = const { Cell::new(false) };
    static COUNT: Cell<usize> = const { Cell::new(0) };
}

// SAFETY: forwards every call to the system allocator; the counters are
// const-initialised thread locals with no destructor, so touching them from
// inside the allocator never allocates.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ENABLED.with(Cell::get) {
            COUNT.with(|count| count.set(count.get() + 1));
        }
        System.alloc(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if ENABLED.with(Cell::get) {
            COUNT.with(|count| count.set(count.get() + 1));
        }
        System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Runs `f` and returns how many allocations this thread made during it.
pub(super) fn allocations(f: impl FnOnce()) -> usize {
    COUNT.with(|count| count.set(0));
    ENABLED.with(|enabled| enabled.set(true));
    f();
    ENABLED.with(|enabled| enabled.set(false));
    COUNT.with(Cell::get)
}
