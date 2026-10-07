//! Per-command latency measurement (M5, D23–D27).
//!
//! This is a harness around the engine, not part of it. It reads a clock around each
//! `apply` call; the books themselves never do (D4), so replay stays deterministic.
//!
//! Each command's latency goes into an HdrHistogram for its kind (D24, D26). A command's
//! kind depends on what it did, not only on what it was: a limit that trades is a
//! different code path from one that rests, so it gets its own histogram.

use std::time::Instant;

use hdrhistogram::Histogram;

use crate::book::{apply_all, OrderBook};
use crate::command::{Command, Event};

/// What a command turned out to do. Decided from its events, after `apply`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A limit that traded nothing and now rests (GTC or post-only).
    Rest,
    /// A limit that traded at least once.
    Cross,
    /// An IOC or FOK limit that traded nothing and was cancelled.
    Kill,
    /// A market order (filled, partly filled, or nothing to hit).
    Market,
    /// A cancel that removed a resting order.
    Cancel,
    /// A modify that was applied (including one that then traded).
    Modify,
    /// Any command the book refused.
    Reject,
}

impl Kind {
    pub const ALL: [Kind; 7] = [
        Kind::Rest,
        Kind::Cross,
        Kind::Kill,
        Kind::Market,
        Kind::Cancel,
        Kind::Modify,
        Kind::Reject,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Kind::Rest => "limit-rest",
            Kind::Cross => "limit-cross",
            Kind::Kill => "limit-kill",
            Kind::Market => "market",
            Kind::Cancel => "cancel",
            Kind::Modify => "modify",
            Kind::Reject => "reject",
        }
    }

    /// Classify one command from the events it produced.
    pub fn of(cmd: &Command, events: &[Event]) -> Kind {
        // A rejected command emits exactly one event, `rejected`, and changes nothing.
        if let [Event::Rejected { .. }] = events {
            return Kind::Reject;
        }
        match cmd {
            Command::Limit { .. } => {
                if events.iter().any(|e| matches!(e, Event::Trade { .. })) {
                    Kind::Cross
                } else if events.iter().any(|e| matches!(e, Event::Cancelled { .. })) {
                    Kind::Kill
                } else {
                    Kind::Rest
                }
            }
            Command::Market { .. } => Kind::Market,
            Command::Cancel { .. } => Kind::Cancel,
            Command::Modify { .. } => Kind::Modify,
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

/// Nanosecond histograms for one run: one per kind, one for everything, and one for
/// the cost of reading the clock itself.
pub struct Report {
    pub by_kind: Vec<Histogram<u64>>,
    pub all: Histogram<u64>,
    /// Two back-to-back clock reads with nothing between them: the measurement floor.
    pub clock: Histogram<u64>,
}

fn histogram() -> Histogram<u64> {
    // Up to 10 s at 3 significant digits: every value within 0.1% of what was recorded.
    // 0 is always recordable, for a call cheaper than the clock's resolution.
    Histogram::new_with_bounds(1, 10_000_000_000, 3).expect("valid histogram bounds")
}

impl Default for Report {
    fn default() -> Self {
        Report {
            by_kind: Kind::ALL.iter().map(|_| histogram()).collect(),
            all: histogram(),
            clock: histogram(),
        }
    }
}

impl Report {
    pub fn kind(&self, kind: Kind) -> &Histogram<u64> {
        &self.by_kind[kind.index()]
    }

    fn record(&mut self, kind: Kind, ns: u64) {
        self.by_kind[kind.index()].saturating_record(ns);
        self.all.saturating_record(ns);
    }
}

/// Elapsed nanoseconds as a `u64`. 584 years fits, so the cast can't truncate in practice.
fn nanos(start: Instant) -> u64 {
    start.elapsed().as_nanos() as u64
}

/// Apply `commands` to a fresh book, timing each `apply` call on its own (D25).
///
/// Only the `apply` call sits between the two clock reads. Clearing the event buffer,
/// classifying and recording happen outside the timed window.
pub fn measure<B: OrderBook>(commands: &[Command]) -> Report {
    let mut report = Report::default();
    let mut book = B::with_config(Default::default());
    let mut events = Vec::with_capacity(64);
    for cmd in commands {
        events.clear();
        let start = Instant::now();
        book.apply(cmd, &mut events);
        let ns = nanos(start);
        report.record(Kind::of(cmd, &events), ns);
    }
    // The same number of empty timed windows, so the floor is measured under the same
    // conditions (same core, same frequency) as the real samples.
    for _ in 0..commands.len() {
        let start = Instant::now();
        let ns = nanos(start);
        report.clock.saturating_record(ns);
    }
    report
}

/// One untimed pass over `commands` on a fresh book. It brings the CPU out of its idle
/// frequency and fills the caches and branch predictors, so the first timed run isn't
/// systematically slower than the rest (D27).
pub fn warm_up<B: OrderBook>(commands: &[Command]) {
    apply_all(&mut B::with_config(Default::default()), commands);
}

/// Measure books `A` and `B` alternately, `runs` times each, after warming up both (D27).
/// A laptop CPU changes frequency with load and heat, so measuring all of A and then all
/// of B can put the two books in different frequency phases. Alternating gives each pair of
/// runs the same conditions.
pub fn measure_interleaved<A: OrderBook, B: OrderBook>(
    commands: &[Command],
    runs: usize,
) -> (Vec<Report>, Vec<Report>) {
    warm_up::<A>(commands);
    warm_up::<B>(commands);
    (0..runs)
        .map(|_| (measure::<A>(commands), measure::<B>(commands)))
        .unzip()
}

/// The run whose overall p99 is the median of all runs (D27). Taking one whole run,
/// rather than the median of each column separately, keeps every number in a row from
/// the same run.
pub fn median_run(reports: &[Report]) -> &Report {
    assert!(!reports.is_empty(), "no runs");
    let mut order: Vec<usize> = (0..reports.len()).collect();
    order.sort_by_key(|&i| reports[i].all.value_at_quantile(0.99));
    &reports[order[order.len() / 2]]
}

/// One table row: count, then mean / p50 / p99 / p99.9 / max in nanoseconds.
pub fn row(name: &str, h: &Histogram<u64>) -> String {
    if h.is_empty() {
        return format!("{name:<12} {:>9}", 0);
    }
    format!(
        "{name:<12} {:>9} {:>7.0} {:>7} {:>7} {:>7} {:>9}",
        h.len(),
        h.mean(),
        h.value_at_quantile(0.50),
        h.value_at_quantile(0.99),
        h.value_at_quantile(0.999),
        h.max()
    )
}

pub const HEADER: &str = "kind             count    mean     p50     p99   p99.9       max   (ns)";

/// The full table for one report.
pub fn table(report: &Report) -> String {
    let mut out = String::from(HEADER);
    for kind in Kind::ALL {
        out.push('\n');
        out.push_str(&row(kind.name(), report.kind(kind)));
    }
    out.push('\n');
    out.push_str(&row("all", &report.all));
    out.push('\n');
    out.push_str(&row("clock floor", &report.clock));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::TimeInForce;
    use crate::gen::{GenConfig, Generator};
    use crate::reference::RefBook;
    use crate::types::{OrderId, Price, Qty, Side};
    use crate::FastBook;

    fn limit(id: u64, side: Side, qty: u64, price: i64, tif: TimeInForce) -> Command {
        Command::Limit {
            id: OrderId(id),
            side,
            qty: Qty(qty),
            price: Price(price),
            tif,
        }
    }

    /// Apply `cmds` in order and return the kind of each.
    fn kinds(cmds: &[Command]) -> Vec<Kind> {
        let mut book = RefBook::new();
        let mut events = Vec::new();
        cmds.iter()
            .map(|cmd| {
                events.clear();
                book.apply(cmd, &mut events);
                Kind::of(cmd, &events)
            })
            .collect()
    }

    #[test]
    fn classifies_every_kind() {
        use TimeInForce::*;
        let cmds = [
            limit(1, Side::Sell, 10, 100, Gtc),    // rests
            limit(2, Side::Buy, 4, 100, Gtc),      // trades, filled
            limit(3, Side::Buy, 10, 100, Ioc),     // trades 6, rest cancelled: still a cross
            limit(4, Side::Buy, 5, 100, Ioc),      // nothing to hit: killed
            limit(5, Side::Sell, 5, 101, Gtc),     // rests
            limit(6, Side::Buy, 9, 101, Fok),      // can't fill all 9: killed
            limit(7, Side::Buy, 5, 101, PostOnly), // would cross: rejected
            Command::Market {
                id: OrderId(8),
                side: Side::Sell,
                qty: Qty(3),
            }, // no bids: accepted, cancelled, still a market
            Command::Modify {
                id: OrderId(5),
                qty: Qty(2),
                price: Price(101),
            },
            Command::Cancel { id: OrderId(5) },
            Command::Cancel { id: OrderId(5) }, // already gone: rejected
            limit(1, Side::Buy, 1, 90, Gtc),    // id not increasing: rejected
        ];
        use Kind::*;
        assert_eq!(
            kinds(&cmds),
            [
                Rest, Cross, Cross, Kill, Rest, Kill, Reject, Market, Modify, Cancel, Reject,
                Reject
            ]
        );
    }

    #[test]
    fn a_modify_that_trades_is_still_a_modify() {
        let cmds = [
            limit(1, Side::Sell, 5, 101, TimeInForce::Gtc),
            limit(2, Side::Buy, 5, 99, TimeInForce::Gtc),
            Command::Modify {
                id: OrderId(2),
                qty: Qty(5),
                price: Price(101),
            },
        ];
        assert_eq!(kinds(&cmds)[2], Kind::Modify);
    }

    /// Every command lands in exactly one histogram, the one its kind says.
    fn check_counts<B: OrderBook>() {
        let cmds: Vec<Command> = Generator::new(GenConfig::default()).take(20_000).collect();
        let report = measure::<B>(&cmds);
        let expected = kinds(&cmds);
        for kind in Kind::ALL {
            let n = expected.iter().filter(|&&k| k == kind).count() as u64;
            assert_eq!(report.kind(kind).len(), n, "{}", kind.name());
            assert!(n > 0, "generated flow has no {}", kind.name());
        }
        assert_eq!(report.all.len(), cmds.len() as u64);
        assert_eq!(report.clock.len(), cmds.len() as u64);
    }

    #[test]
    fn counts_match_classification_reference() {
        check_counts::<RefBook>();
    }

    #[test]
    fn counts_match_classification_fast() {
        check_counts::<FastBook>();
    }

    #[test]
    fn median_run_picks_the_middle_p99() {
        let mut reports: Vec<Report> = (0..5).map(|_| Report::default()).collect();
        for (r, p99) in reports.iter_mut().zip([500, 100, 900, 300, 200]) {
            // 100 samples: 98 fast ones and 2 at `p99`, so p99 is exactly `p99`.
            for _ in 0..98 {
                r.record(Kind::Rest, 10);
            }
            r.record(Kind::Rest, p99);
            r.record(Kind::Rest, p99);
        }
        // Sorted by p99 the runs go 100, 200, 300, 500, 900: the median is the 4th run,
        // not the 3rd, so picking "the middle index" without sorting would be caught.
        let median = median_run(&reports);
        assert_eq!(median.all.value_at_quantile(0.99), 300);
    }

    #[test]
    fn interleaved_gives_each_book_every_run() {
        let cmds: Vec<Command> = Generator::new(GenConfig::default()).take(2_000).collect();
        let (a, b) = measure_interleaved::<RefBook, FastBook>(&cmds, 3);
        assert_eq!((a.len(), b.len()), (3, 3));
        for r in a.iter().chain(&b) {
            assert_eq!(r.all.len(), cmds.len() as u64);
        }
    }

    #[test]
    fn table_has_a_row_per_kind_plus_totals() {
        let cmds: Vec<Command> = Generator::new(GenConfig::default()).take(1_000).collect();
        let t = table(&measure::<FastBook>(&cmds));
        assert_eq!(t.lines().count(), 1 + Kind::ALL.len() + 2);
        assert!(t.lines().any(|l| l.starts_with("limit-cross")));
        assert!(t.lines().last().unwrap().starts_with("clock floor"));
    }
}
