//! A bounded single-producer single-consumer ring (D45).
//!
//! `tail` counts pushes and is written only by the producer; `head` counts pops and is
//! written only by the consumer. Both only increase (wrapping at `usize::MAX`, which no
//! run reaches), so `tail - head` is the number of items in the ring, and slot `i % cap`
//! holds item `i`.
//!
//! Orderings: the producer writes a slot, then stores `tail` with `Release`. The consumer
//! loads `tail` with `Acquire`, so every slot below that `tail` is fully written before it
//! reads one. The same pair in the other direction (`head`: consumer `Release`, producer
//! `Acquire`) means the producer reuses a slot only after the consumer has finished reading it.

use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

/// Keeps an index on its own cache line, so a write to `head` doesn't invalidate the
/// line holding `tail` on the other core (false sharing).
#[repr(align(64))]
struct Padded<T>(T);

struct Ring<T> {
    slots: Box<[UnsafeCell<MaybeUninit<T>>]>,
    mask: usize,
    head: Padded<AtomicUsize>,
    tail: Padded<AtomicUsize>,
    /// Set when the producer is dropped: nothing more will be pushed.
    closed: AtomicBool,
}

// SAFETY: a slot is only ever accessed by one thread at a time. The producer writes slots
// in [head + cap, ...) that the consumer has released, and the consumer reads slots in
// [head, tail) that the producer has published, as the module comment explains.
// `T: Send` because items move from one thread to the other.
unsafe impl<T: Send> Sync for Ring<T> {}

pub struct Producer<T> {
    ring: Arc<Ring<T>>,
    /// This side's own index; the shared copy is only ever written from here.
    tail: usize,
    /// The consumer's `head` when last read. It can only be behind the real one, so
    /// "full" by the cache may be stale, never wrong the other way.
    head_cache: usize,
}

pub struct Consumer<T> {
    ring: Arc<Ring<T>>,
    head: usize,
    tail_cache: usize,
}

/// A ring holding up to `capacity` items, which must be a power of two (a mask then
/// replaces a division on every access).
pub fn ring<T: Copy + Send>(capacity: usize) -> (Producer<T>, Consumer<T>) {
    assert!(
        capacity.is_power_of_two(),
        "capacity {capacity} isn't a power of two"
    );
    let ring = Arc::new(Ring {
        slots: (0..capacity)
            .map(|_| UnsafeCell::new(MaybeUninit::uninit()))
            .collect(),
        mask: capacity - 1,
        head: Padded(AtomicUsize::new(0)),
        tail: Padded(AtomicUsize::new(0)),
        closed: AtomicBool::new(false),
    });
    let producer = Producer {
        ring: Arc::clone(&ring),
        tail: 0,
        head_cache: 0,
    };
    let consumer = Consumer {
        ring,
        head: 0,
        tail_cache: 0,
    };
    (producer, consumer)
}

impl<T: Copy + Send> Producer<T> {
    /// Push `v`, or hand it back if the ring is full.
    pub fn try_push(&mut self, v: T) -> Result<(), T> {
        let cap = self.ring.slots.len();
        if self.tail.wrapping_sub(self.head_cache) == cap {
            self.head_cache = self.ring.head.0.load(Ordering::Acquire);
            if self.tail.wrapping_sub(self.head_cache) == cap {
                return Err(v);
            }
        }
        let slot = &self.ring.slots[self.tail & self.ring.mask];
        // SAFETY: the slot is below head + cap, so the consumer is done with it (Acquire
        // above), and it's at or above tail, so the consumer won't read it until the
        // Release below.
        unsafe { (*slot.get()).write(v) };
        self.tail = self.tail.wrapping_add(1);
        self.ring.tail.0.store(self.tail, Ordering::Release);
        Ok(())
    }

    /// Push `v`, waiting while the ring is full.
    pub fn push(&mut self, mut v: T) {
        let mut backoff = Backoff::default();
        while let Err(back) = self.try_push(v) {
            v = back;
            backoff.wait();
        }
    }
}

impl<T> Drop for Producer<T> {
    fn drop(&mut self) {
        // Release: a consumer that sees `closed` also sees every push before it.
        self.ring.closed.store(true, Ordering::Release);
    }
}

impl<T: Copy + Send> Consumer<T> {
    /// Pop the oldest item, if there is one.
    pub fn try_pop(&mut self) -> Option<T> {
        if self.head == self.tail_cache {
            self.tail_cache = self.ring.tail.0.load(Ordering::Acquire);
            if self.head == self.tail_cache {
                return None;
            }
        }
        let slot = &self.ring.slots[self.head & self.ring.mask];
        // SAFETY: head < tail, so the producer has written this slot and published it
        // (the Acquire above), and won't touch it again until the Release below.
        // `T: Copy`, so reading it out leaves nothing that needs dropping.
        let v = unsafe { (*slot.get()).assume_init_read() };
        self.head = self.head.wrapping_add(1);
        self.ring.head.0.store(self.head, Ordering::Release);
        Some(v)
    }

    /// Pop the oldest item, waiting while the ring is empty. `None` once the producer
    /// is gone and every item it pushed has been popped.
    pub fn pop(&mut self) -> Option<T> {
        let mut backoff = Backoff::default();
        loop {
            if let Some(v) = self.try_pop() {
                return Some(v);
            }
            if self.ring.closed.load(Ordering::Acquire) {
                // Every push happened before `closed` was set, so one more look is final.
                return self.try_pop();
            }
            backoff.wait();
        }
    }
}

/// Spin briefly, then give the core away. Pure spinning is lowest-latency on a dedicated
/// core, but on a shared laptop a waiting thread would steal time from the one it waits for.
#[derive(Default)]
pub struct Backoff(u32);

impl Backoff {
    pub fn wait(&mut self) {
        if self.0 < 100 {
            self.0 += 1;
            std::hint::spin_loop();
        } else {
            std::thread::yield_now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fills_empties_and_wraps() {
        let (mut tx, mut rx) = ring::<u32>(4);
        assert_eq!(rx.try_pop(), None);
        for round in 0..3 {
            for i in 0..4 {
                tx.try_push(round * 10 + i).unwrap();
            }
            assert_eq!(tx.try_push(99), Err(99), "full at 4");
            for i in 0..4 {
                assert_eq!(rx.try_pop(), Some(round * 10 + i));
            }
            assert_eq!(rx.try_pop(), None);
        }
        // Interleaved: never more than 2 in flight, so the indices pass the slot count many times.
        for i in 0..100 {
            tx.try_push(i).unwrap();
            tx.try_push(i + 1000).unwrap();
            assert_eq!(rx.try_pop(), Some(i));
            assert_eq!(rx.try_pop(), Some(i + 1000));
        }
    }

    #[test]
    fn capacity_one_works() {
        let (mut tx, mut rx) = ring::<u8>(1);
        tx.try_push(1).unwrap();
        assert_eq!(tx.try_push(2), Err(2));
        assert_eq!(rx.try_pop(), Some(1));
        tx.try_push(3).unwrap();
        assert_eq!(rx.try_pop(), Some(3));
    }

    #[test]
    #[should_panic(expected = "isn't a power of two")]
    fn capacity_must_be_a_power_of_two() {
        ring::<u8>(3);
    }

    #[test]
    fn closing_drains_before_none() {
        let (mut tx, mut rx) = ring::<u8>(4);
        tx.push(1);
        tx.push(2);
        drop(tx);
        assert_eq!(rx.pop(), Some(1));
        assert_eq!(rx.pop(), Some(2));
        assert_eq!(rx.pop(), None);
    }

    /// The race `pop` re-checks for: the consumer finds the ring empty, then the producer
    /// pushes its last item and closes, then the consumer sees `closed`. That item must still
    /// come out. The window is a few ns wide, so this repeats a tiny hand-off many times.
    #[test]
    fn the_last_item_before_closing_is_never_lost() {
        for _ in 0..20_000 {
            let (mut tx, mut rx) = ring::<u8>(1);
            std::thread::scope(|s| {
                s.spawn(move || tx.push(7));
                assert_eq!(rx.pop(), Some(7));
            });
        }
    }

    /// Two threads, a small ring so it's constantly full and empty: every item arrives
    /// exactly once, in order. Items carry a check value, so a torn or stale slot shows.
    #[test]
    fn threads_see_every_item_in_order() {
        for cap in [1, 2, 8] {
            let n = 200_000u64;
            let (mut tx, mut rx) = ring::<(u64, u64)>(cap);
            let producer = std::thread::spawn(move || {
                for i in 0..n {
                    tx.push((i, !i));
                }
            });
            for i in 0..n {
                assert_eq!(rx.pop(), Some((i, !i)), "capacity {cap}");
            }
            assert_eq!(rx.pop(), None);
            producer.join().unwrap();
        }
    }
}
