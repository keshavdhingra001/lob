//! Zero heap allocations per command in steady state (D32).
//!
//! A counting global allocator wraps the system one. Each test thread counts its own
//! allocations (a thread-local counter), so tests running in parallel don't mix counts.
//! After a warm-up, the fast book must apply every remaining command without a single
//! allocation, reallocation or zeroed allocation. The reference book is measured too, to
//! show what the fast book's design removed.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use lob::gen::{GenConfig, Generator};
use lob::{BookConfig, Command, Event, FastBook, OrderBook, RefBook};

struct Counting;

thread_local! {
    // `const` initialisation and no destructor: touching it can't itself allocate.
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

fn count() {
    // `try_with`: during thread teardown the slot may be gone; skip counting then.
    let _ = ALLOCATIONS.try_with(|c| c.set(c.get() + 1));
}

// SAFETY: every call is forwarded unchanged to `System`, which upholds the contract.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count();
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count();
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count();
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn allocations() -> u64 {
    ALLOCATIONS.with(Cell::get)
}

/// Allocations made while applying `commands[warm_up..]`, after applying the first
/// `warm_up` uncounted.
fn steady_state<B: OrderBook>(mut book: B, commands: &[Command], warm_up: usize) -> u64 {
    // Big enough for any single command's events in these flows; the caller owns it (D5).
    let mut events: Vec<Event> = Vec::with_capacity(4096);
    for cmd in &commands[..warm_up] {
        events.clear();
        book.apply(cmd, &mut events);
    }
    let before = allocations();
    for cmd in &commands[warm_up..] {
        events.clear();
        book.apply(cmd, &mut events);
    }
    allocations() - before
}

fn flow(seed: u64, n: usize, max_live: usize) -> Vec<Command> {
    Generator::new(GenConfig {
        seed,
        max_live,
        ..GenConfig::default()
    })
    .take(n)
    .collect()
}

#[test]
fn the_counter_counts() {
    let before = allocations();
    let v: Vec<u64> = Vec::with_capacity(10);
    drop(v);
    assert_eq!(allocations() - before, 1);
}

#[test]
fn fast_book_allocates_nothing_per_command() {
    for (seed, max_live) in [(1, 5_000), (2, 5_000), (3, 200_000)] {
        let commands = flow(seed, 1_000_000, max_live);
        let book = FastBook::with_capacity(BookConfig::default(), max_live * 2);
        let n = steady_state(book, &commands, 10_000);
        assert_eq!(n, 0, "seed {seed}, max-live {max_live}: {n} allocations");
    }
}

/// For contrast (and so the test above can't pass by accident because nothing is
/// counted): the reference book allocates for new price levels and growing queues.
#[test]
fn reference_book_does_allocate() {
    let commands = flow(1, 200_000, 5_000);
    let n = steady_state(RefBook::new(), &commands, 10_000);
    assert!(n > 1_000, "only {n} allocations");
}
