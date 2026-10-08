//! Tick-indexed price levels for one side of the fast book (D33).
//!
//! A window of `WINDOW` consecutive ticks maps each price straight to a slot holding a
//! level index, so finding a level is an array index instead of a tree search, and adding
//! or removing a level never allocates. A two-level occupancy bitmap finds the next
//! non-empty slot in a handful of word operations, however sparse the side is. Prices
//! outside the window (a far-away order, or a market that drifted) fall back to a
//! `BTreeMap`. That path is correct but allocates, like the M4 book.
//!
//! The window is allocated on the first insert and centred on that price, and it never
//! moves. Re-centring, or rejecting prices outside a band the way real venues' price
//! collars do, would be the next step if the overflow tree ever got busy.

use std::collections::BTreeMap;

use crate::types::Price;

/// Ticks covered by the window: plus or minus 32,768 ticks around the first price.
/// That's 256 KiB of slots plus 8 KiB of bitmap per side; only the slots near the
/// touch are ever hot.
pub const WINDOW: usize = 1 << 16;
const WORDS: usize = WINDOW / 64;
const SUMMARY: usize = WORDS / 64;

/// "No level" in a slot (same convention as the fast book's `NIL`).
const EMPTY: u32 = u32::MAX;

/// Bits 0..=b.
fn up_to(b: usize) -> u64 {
    if b == 63 {
        u64::MAX
    } else {
        (1 << (b + 1)) - 1
    }
}

/// Bits b..=63.
fn from(b: usize) -> u64 {
    u64::MAX << b
}

fn highest(word: u64) -> usize {
    63 - word.leading_zeros() as usize
}

fn lowest(word: u64) -> usize {
    word.trailing_zeros() as usize
}

/// A bitmap over `WINDOW` slots with a summary bit per word ("this word is non-zero"),
/// so a search skips 64 empty words with one summary word.
struct Occupancy {
    words: Vec<u64>,
    summary: Vec<u64>,
}

impl Occupancy {
    fn new() -> Self {
        Occupancy {
            words: vec![0; WORDS],
            summary: vec![0; SUMMARY],
        }
    }

    fn set(&mut self, i: usize) {
        let w = i / 64;
        self.words[w] |= 1 << (i % 64);
        self.summary[w / 64] |= 1 << (w % 64);
    }

    fn clear(&mut self, i: usize) {
        let w = i / 64;
        self.words[w] &= !(1 << (i % 64));
        if self.words[w] == 0 {
            self.summary[w / 64] &= !(1 << (w % 64));
        }
    }

    fn get(&self, i: usize) -> bool {
        self.words[i / 64] >> (i % 64) & 1 == 1
    }

    /// The highest set index `<= i`.
    fn highest_at_or_below(&self, i: usize) -> Option<usize> {
        let w = i / 64;
        let here = self.words[w] & up_to(i % 64);
        if here != 0 {
            return Some(w * 64 + highest(here));
        }
        let w = self.highest_word_below(w)?;
        Some(w * 64 + highest(self.words[w]))
    }

    /// The lowest set index `>= i`.
    fn lowest_at_or_above(&self, i: usize) -> Option<usize> {
        let w = i / 64;
        let here = self.words[w] & from(i % 64);
        if here != 0 {
            return Some(w * 64 + lowest(here));
        }
        let w = self.lowest_word_above(w)?;
        Some(w * 64 + lowest(self.words[w]))
    }

    /// The highest non-empty word with index `< w`.
    fn highest_word_below(&self, w: usize) -> Option<usize> {
        let w = w.checked_sub(1)?;
        let s = w / 64;
        let here = self.summary[s] & up_to(w % 64);
        if here != 0 {
            return Some(s * 64 + highest(here));
        }
        (0..s)
            .rev()
            .find(|&s| self.summary[s] != 0)
            .map(|s| s * 64 + highest(self.summary[s]))
    }

    /// The lowest non-empty word with index `> w`.
    fn lowest_word_above(&self, w: usize) -> Option<usize> {
        let w = w + 1;
        if w >= WORDS {
            return None;
        }
        let s = w / 64;
        let here = self.summary[s] & from(w % 64);
        if here != 0 {
            return Some(s * 64 + lowest(here));
        }
        (s + 1..SUMMARY)
            .find(|&s| self.summary[s] != 0)
            .map(|s| s * 64 + lowest(self.summary[s]))
    }
}

/// The price levels of one side: price -> level index, ordered by price.
pub struct Ladder {
    tick: i64,
    /// Tick number (price / tick) of slot 0. Meaningful once `slots` is allocated.
    base: i64,
    slots: Vec<u32>,
    occupancy: Option<Occupancy>,
    overflow: BTreeMap<Price, u32>,
    len: usize,
}

impl Ladder {
    pub fn new(tick: i64) -> Self {
        Ladder {
            tick,
            base: 0,
            slots: Vec::new(),
            occupancy: None,
            overflow: BTreeMap::new(),
            len: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Levels outside the window, in the overflow tree.
    pub fn overflow_len(&self) -> usize {
        self.overflow.len()
    }

    /// Prices in the ladder are always on the tick grid (the book validates them first).
    fn ticks(&self, price: Price) -> i64 {
        price.0.div_euclid(self.tick)
    }

    /// How many ticks `price` is above the window's first slot. In `i128`: with prices
    /// anywhere in `i64`, the difference can need 65 bits.
    fn offset(&self, price: Price) -> i128 {
        i128::from(self.ticks(price)) - i128::from(self.base)
    }

    /// The window slot for `price`, if the window exists and covers it.
    fn slot(&self, price: Price) -> Option<usize> {
        if self.slots.is_empty() {
            return None;
        }
        let i = self.offset(price);
        (0..WINDOW as i128).contains(&i).then_some(i as usize)
    }

    fn price_at(&self, i: usize) -> Price {
        Price((self.base + i as i64) * self.tick)
    }

    pub fn get(&self, price: Price) -> Option<u32> {
        match self.slot(price) {
            Some(i) => (self.slots[i] != EMPTY).then(|| self.slots[i]),
            None => self.overflow.get(&price).copied(),
        }
    }

    /// Add a level at a price that has none.
    pub fn insert(&mut self, price: Price, level: u32) {
        if self.slots.is_empty() {
            // One-time allocation, centred on the first price this side ever sees.
            self.base = self.ticks(price).saturating_sub((WINDOW / 2) as i64);
            self.slots = vec![EMPTY; WINDOW];
            self.occupancy = Some(Occupancy::new());
        }
        match self.slot(price) {
            Some(i) => {
                debug_assert_eq!(self.slots[i], EMPTY, "level already at {price}");
                self.slots[i] = level;
                self.occupancy.as_mut().expect("allocated").set(i);
            }
            None => {
                let old = self.overflow.insert(price, level);
                debug_assert!(old.is_none(), "level already at {price}");
            }
        }
        self.len += 1;
    }

    pub fn remove(&mut self, price: Price) {
        match self.slot(price) {
            Some(i) => {
                debug_assert_ne!(self.slots[i], EMPTY, "no level at {price}");
                self.slots[i] = EMPTY;
                self.occupancy.as_mut().expect("allocated").clear(i);
            }
            None => {
                self.overflow
                    .remove(&price)
                    .expect("level is in the overflow");
            }
        }
        self.len -= 1;
    }

    /// The highest level with price `< below` (or the highest overall, for `None`).
    pub fn highest_below(&self, below: Option<Price>) -> Option<(Price, u32)> {
        let from_tree = match below {
            Some(p) => self.overflow.range(..p).next_back(),
            None => self.overflow.last_key_value(),
        }
        .map(|(&p, &l)| (p, l));
        let from_window = self.occupancy.as_ref().and_then(|occ| {
            let top = match below {
                // Slot index of the highest price strictly below `p`, clamped to the window.
                Some(p) => (self.offset(p) - 1).min(WINDOW as i128 - 1),
                None => WINDOW as i128 - 1,
            };
            let top = usize::try_from(top).ok()?;
            occ.highest_at_or_below(top)
                .map(|i| (self.price_at(i), self.slots[i]))
        });
        better(from_tree, from_window, |a, b| a > b)
    }

    /// The lowest level with price `> above` (or the lowest overall, for `None`).
    pub fn lowest_above(&self, above: Option<Price>) -> Option<(Price, u32)> {
        let from_tree = match above {
            Some(p) => self
                .overflow
                .range((std::ops::Bound::Excluded(p), std::ops::Bound::Unbounded))
                .next(),
            None => self.overflow.first_key_value(),
        }
        .map(|(&p, &l)| (p, l));
        let from_window = self.occupancy.as_ref().and_then(|occ| {
            let bottom = match above {
                Some(p) => (self.offset(p) + 1).max(0),
                None => 0,
            };
            let bottom = usize::try_from(bottom).ok().filter(|&b| b < WINDOW)?;
            occ.lowest_at_or_above(bottom)
                .map(|i| (self.price_at(i), self.slots[i]))
        });
        better(from_tree, from_window, |a, b| a < b)
    }

    /// Visit levels from the highest price down until `f` returns false. No allocation.
    pub fn visit_descending(&self, mut f: impl FnMut(Price, u32) -> bool) {
        let mut next = self.highest_below(None);
        while let Some((price, level)) = next {
            if !f(price, level) {
                return;
            }
            next = self.highest_below(Some(price));
            // Each step must move strictly down. A search bug that returned the same level
            // again would otherwise loop forever (and `depth` would grow its Vec without bound).
            debug_assert!(
                next.is_none_or(|(p, _)| p < price),
                "no progress at {price}"
            );
        }
    }

    /// Visit levels from the lowest price up until `f` returns false. No allocation.
    pub fn visit_ascending(&self, mut f: impl FnMut(Price, u32) -> bool) {
        let mut next = self.lowest_above(None);
        while let Some((price, level)) = next {
            if !f(price, level) {
                return;
            }
            next = self.lowest_above(Some(price));
            debug_assert!(
                next.is_none_or(|(p, _)| p > price),
                "no progress at {price}"
            );
        }
    }

    /// Internal consistency, for the book's `check_invariants`. O(levels + 1,024), so the
    /// tests can afford it after every command:
    /// - the summary agrees with the bitmap
    /// - every set bit has a level in its slot
    /// - overflow prices are outside the window, and `len` counts everything
    ///
    /// A stale slot with its bit clear isn't looked for here (that needs the full scan,
    /// `check_full`), but `get` reads slots directly, so it would change the book's events
    /// and fail the differential test.
    pub fn check(&self) -> Result<(), String> {
        let mut count = self.overflow.len();
        if let Some(occ) = &self.occupancy {
            for (w, &word) in occ.words.iter().enumerate() {
                if (occ.summary[w / 64] >> (w % 64) & 1 == 1) != (word != 0) {
                    return Err(format!("summary disagrees with word {w}"));
                }
                let mut bits = word;
                while bits != 0 {
                    let i = w * 64 + lowest(bits);
                    if self.slots[i] == EMPTY {
                        return Err(format!("bit {i} is set but its slot is empty"));
                    }
                    count += 1;
                    bits &= bits - 1;
                }
            }
        }
        if let Some(p) = self.overflow.keys().find(|&&p| self.slot(p).is_some()) {
            return Err(format!("overflow holds {p}, which is inside the window"));
        }
        if count != self.len {
            return Err(format!(
                "ladder holds {count} levels but len is {}",
                self.len
            ));
        }
        Ok(())
    }

    /// `check` plus a scan of every slot: no level sits in a slot whose bit is clear.
    pub fn check_full(&self) -> Result<(), String> {
        self.check()?;
        if let Some(occ) = &self.occupancy {
            if let Some(i) = (0..WINDOW).find(|&i| self.slots[i] != EMPTY && !occ.get(i)) {
                return Err(format!("slot {i} holds a level but its bit is clear"));
            }
        }
        Ok(())
    }
}

/// Whichever candidate wins `beats` on price.
fn better(
    a: Option<(Price, u32)>,
    b: Option<(Price, u32)>,
    beats: impl Fn(Price, Price) -> bool,
) -> Option<(Price, u32)> {
    match (a, b) {
        (Some(x), Some(y)) => Some(if beats(x.0, y.0) { x } else { y }),
        (x, None) => x,
        (None, y) => y,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rng::Rng;

    /// Everything the ladder answers, checked against a `BTreeMap` model.
    fn agree(ladder: &Ladder, model: &BTreeMap<Price, u32>, probes: &[Price]) {
        ladder.check_full().unwrap();
        assert_eq!(ladder.len(), model.len());
        assert_eq!(
            ladder.highest_below(None),
            model.last_key_value().map(|(&p, &l)| (p, l))
        );
        assert_eq!(
            ladder.lowest_above(None),
            model.first_key_value().map(|(&p, &l)| (p, l))
        );
        for &p in probes {
            assert_eq!(ladder.get(p), model.get(&p).copied(), "get {p}");
            assert_eq!(
                ladder.highest_below(Some(p)),
                model.range(..p).next_back().map(|(&p, &l)| (p, l)),
                "highest below {p}"
            );
            assert_eq!(
                ladder.lowest_above(Some(p)),
                model
                    .range((std::ops::Bound::Excluded(p), std::ops::Bound::Unbounded))
                    .next()
                    .map(|(&p, &l)| (p, l)),
                "lowest above {p}"
            );
        }
        let mut down = Vec::new();
        ladder.visit_descending(|p, l| {
            down.push((p, l));
            true
        });
        let expected: Vec<(Price, u32)> = model.iter().rev().map(|(&p, &l)| (p, l)).collect();
        assert_eq!(down, expected);
        let mut up = Vec::new();
        ladder.visit_ascending(|p, l| {
            up.push((p, l));
            true
        });
        assert_eq!(up, expected.into_iter().rev().collect::<Vec<_>>());
    }

    /// Random inserts and removes around a centre, with some prices far outside the
    /// window (overflow, above and below) and some exactly on the window's edges.
    fn random_session(seed: u64, tick: i64) {
        let mut rng = Rng::new(seed);
        let mut ladder = Ladder::new(tick);
        let mut model = BTreeMap::new();
        let centre = 10_000 * tick;
        let half = (WINDOW / 2) as i64 * tick;
        let mut next_level = 0;
        for step in 0..3_000 {
            let price = Price(match rng.below(10) {
                0 => centre + half * 3 + rng.range(-50, 50) * tick, // overflow above
                1 => centre - half * 3 + rng.range(-50, 50) * tick, // overflow below
                2 => centre - half + rng.range(-2, 2) * tick,       // the low edge
                3 => centre + half + rng.range(-2, 2) * tick,       // the high edge
                _ => centre + rng.range(-300, 300) * tick,          // near the touch
            });
            // The first insert fixes the window, so make it the centre.
            let price = if step == 0 { Price(centre) } else { price };
            if model.remove(&price).is_some() {
                ladder.remove(price);
            } else {
                ladder.insert(price, next_level);
                model.insert(price, next_level);
                next_level += 1;
            }
            if step % 50 == 0 {
                let probes: Vec<Price> = model
                    .keys()
                    .copied()
                    .chain([Price(centre), Price(centre - half), Price(centre + half)])
                    .collect();
                agree(&ladder, &model, &probes);
            }
        }
        agree(&ladder, &model, &[]);
    }

    #[test]
    fn matches_a_btreemap_tick_1() {
        for seed in 0..5 {
            random_session(seed, 1);
        }
    }

    #[test]
    fn matches_a_btreemap_tick_5_and_negative_prices() {
        random_session(9, 5);
        // Centred near zero so the window spans negative prices.
        let mut ladder = Ladder::new(5);
        let mut model = BTreeMap::new();
        for (i, p) in [0, -5, 5, -100, 250, -163_840, 163_835]
            .into_iter()
            .enumerate()
        {
            ladder.insert(Price(p), i as u32);
            model.insert(Price(p), i as u32);
        }
        agree(&ladder, &model, &[Price(-5), Price(0), Price(1_000_000)]);
    }

    #[test]
    fn bitmap_search_crosses_words_and_summary_words() {
        let mut occ = Occupancy::new();
        assert_eq!(occ.highest_at_or_below(WINDOW - 1), None);
        assert_eq!(occ.lowest_at_or_above(0), None);
        // 5 and 64 * 64 * 3 + 1 are in different summary words.
        for i in [5, 64, 64 * 64 * 3 + 1, WINDOW - 1] {
            occ.set(i);
        }
        assert_eq!(occ.highest_at_or_below(WINDOW - 2), Some(64 * 64 * 3 + 1));
        assert_eq!(occ.highest_at_or_below(64 * 64 * 3), Some(64));
        assert_eq!(occ.highest_at_or_below(63), Some(5));
        assert_eq!(occ.highest_at_or_below(4), None);
        assert_eq!(occ.lowest_at_or_above(6), Some(64));
        assert_eq!(occ.lowest_at_or_above(65), Some(64 * 64 * 3 + 1));
        assert_eq!(occ.lowest_at_or_above(64 * 64 * 3 + 2), Some(WINDOW - 1));
        occ.clear(64);
        assert_eq!(occ.lowest_at_or_above(6), Some(64 * 64 * 3 + 1));
        assert_eq!(occ.summary[0] & 2, 0, "word 1 is empty again");
    }

    #[test]
    fn visit_stops_when_asked() {
        let mut ladder = Ladder::new(1);
        for p in [100, 101, 102] {
            ladder.insert(Price(p), p as u32);
        }
        let mut seen = Vec::new();
        ladder.visit_descending(|p, _| {
            seen.push(p.0);
            p.0 > 101
        });
        assert_eq!(seen, [102, 101]);
    }
}
