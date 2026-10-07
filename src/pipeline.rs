//! The engine as three threads joined by rings (D46): gateway → matching → output.
//!
//! The gateway decodes commands from their 32-byte journal encoding and stamps them; the
//! matching thread applies them to the fast book; the output thread encodes and hashes the
//! event stream (D17), publishes market data (D40) and records end-to-end latency. Only
//! the gateway and output threads read a clock (D23). `run_single` does the same work in one
//! loop, and the two must agree byte for byte (D47).

use std::sync::mpsc;
use std::time::{Duration, Instant};

use hdrhistogram::Histogram;

use crate::book::OrderBook;
use crate::command::{Command, Event};
use crate::fast::FastBook;
use crate::feed::{self, Publisher};
use crate::journal::{decode_command, encode_command};
use crate::latency::histogram;
use crate::replay::{encode_event, Fnv64};
use crate::ring;

/// One command as it arrives: its journal encoding and length.
pub type Frame = ([u8; 32], usize);

pub fn frames(commands: &[Command]) -> Vec<Frame> {
    commands
        .iter()
        .map(|cmd| {
            let mut buf = [0; 32];
            let len = encode_command(cmd, &mut buf);
            (buf, len)
        })
        .collect()
}

/// A bounded channel the pipeline can run over, so the ring and `std::sync::mpsc` can be
/// compared on the same work (D45).
pub trait Channel {
    type Tx<T: Copy + Send>: Sender<T>;
    type Rx<T: Copy + Send>: Receiver<T>;
    fn pair<T: Copy + Send>(capacity: usize) -> (Self::Tx<T>, Self::Rx<T>);
}

pub trait Sender<T>: Send {
    /// Send `v`, waiting while the channel is full.
    fn send(&mut self, v: T);
}

pub trait Receiver<T>: Send {
    /// The next item, waiting for one; `None` once the sender is gone and all is received.
    fn recv(&mut self) -> Option<T>;
}

/// The hand-written SPSC ring (D45).
pub struct Ring;

impl Channel for Ring {
    type Tx<T: Copy + Send> = ring::Producer<T>;
    type Rx<T: Copy + Send> = ring::Consumer<T>;
    fn pair<T: Copy + Send>(capacity: usize) -> (Self::Tx<T>, Self::Rx<T>) {
        ring::ring(capacity)
    }
}

impl<T: Copy + Send> Sender<T> for ring::Producer<T> {
    fn send(&mut self, v: T) {
        self.push(v)
    }
}

impl<T: Copy + Send> Receiver<T> for ring::Consumer<T> {
    fn recv(&mut self) -> Option<T> {
        self.pop()
    }
}

/// `std::sync::mpsc::sync_channel`: bounded, blocking, general-purpose.
pub struct Mpsc;

impl Channel for Mpsc {
    type Tx<T: Copy + Send> = mpsc::SyncSender<T>;
    type Rx<T: Copy + Send> = mpsc::Receiver<T>;
    fn pair<T: Copy + Send>(capacity: usize) -> (Self::Tx<T>, Self::Rx<T>) {
        mpsc::sync_channel(capacity)
    }
}

impl<T: Copy + Send> Sender<T> for mpsc::SyncSender<T> {
    fn send(&mut self, v: T) {
        mpsc::SyncSender::send(self, v).expect("the receiver outlives the sender");
    }
}

impl<T: Copy + Send> Receiver<T> for mpsc::Receiver<T> {
    fn recv(&mut self) -> Option<T> {
        mpsc::Receiver::recv(self).ok()
    }
}

/// Gateway → matching.
#[derive(Clone, Copy)]
struct Inbound {
    cmd: Command,
    stamp: Instant,
}

/// Matching → output: a command's events one by one, then `Done` (D46).
#[derive(Clone, Copy)]
enum Outbound {
    Event(Event),
    Done { cmd: Command, stamp: Instant },
}

/// What a run produced. Two runs with equal `Digests` produced the same bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Digests {
    pub commands: u64,
    /// Frames the gateway couldn't decode, dropped there.
    pub bad_frames: u64,
    pub events: u64,
    pub feed_msgs: u64,
    /// FNV-1a over the event stream, as `replay` encodes it (D17).
    pub events_digest: u64,
    /// FNV-1a over the encoded feed (D43).
    pub feed_digest: u64,
}

/// The output stage: everything after matching. Shared by both runs.
struct Output {
    digests: Digests,
    events_hash: Fnv64,
    feed_hash: Fnv64,
    publisher: Publisher,
    msgs: Vec<feed::Msg>,
    buf: Vec<u8>,
}

impl Output {
    fn new() -> Self {
        Output {
            digests: Digests::default(),
            events_hash: Fnv64::default(),
            feed_hash: Fnv64::default(),
            publisher: Publisher::new(),
            msgs: Vec::with_capacity(64),
            buf: Vec::with_capacity(1024),
        }
    }

    fn on_command(&mut self, cmd: &Command, events: &[Event]) {
        let d = &mut self.digests;
        d.commands += 1;
        self.buf.clear();
        for event in events {
            d.events += 1;
            encode_event(d.events, event, &mut self.buf);
        }
        self.events_hash.update(&self.buf);

        self.msgs.clear();
        self.publisher
            .on_command(cmd, events, &mut self.msgs)
            .expect("the fast book's events are consistent");
        self.buf.clear();
        for msg in &self.msgs {
            feed::encode(msg, &mut self.buf);
        }
        d.feed_msgs += self.msgs.len() as u64;
        self.feed_hash.update(&self.buf);
    }

    fn finish(mut self, bad_frames: u64) -> Digests {
        self.digests.bad_frames = bad_frames;
        self.digests.events_digest = self.events_hash.finish();
        self.digests.feed_digest = self.feed_hash.finish();
        self.digests
    }
}

/// All three stages in one loop on one thread: the reference for `run` (D47) and the
/// baseline for its throughput.
pub fn run_single(frames: &[Frame]) -> Digests {
    let mut book = FastBook::new();
    let mut output = Output::new();
    let mut events = Vec::with_capacity(64);
    let mut bad_frames = 0;
    for (buf, len) in frames {
        let Ok(cmd) = decode_command(&buf[..*len]) else {
            bad_frames += 1;
            continue;
        };
        events.clear();
        book.apply(&cmd, &mut events);
        output.on_command(&cmd, &events);
    }
    output.finish(bad_frames)
}

pub struct Report {
    pub digests: Digests,
    /// Gateway stamp to the end of the output stage, per command, in ns.
    pub latency: Histogram<u64>,
    pub elapsed: Duration,
}

/// Run `frames` through the three-thread pipeline over channel `C`, with rings of
/// `capacity` items. `rate` 0 floods; otherwise the gateway sends `rate` commands per
/// second on a fixed schedule and stamps each with its scheduled time (D48).
pub fn run<C: Channel>(frames: &[Frame], capacity: usize, rate: u64) -> Report {
    let (mut to_matching, mut from_gateway) = C::pair::<Inbound>(capacity);
    let (mut to_output, mut from_matching) = C::pair::<Outbound>(capacity);
    let start = Instant::now();
    let (digests, latency) = std::thread::scope(|s| {
        let gateway = s.spawn(move || {
            let mut bad_frames = 0;
            for (i, (buf, len)) in frames.iter().enumerate() {
                // Rate 0 (no schedule) floods.
                let stamp = match (i as u64 * 1_000_000_000).checked_div(rate) {
                    None => Instant::now(),
                    Some(ns) => {
                        let due = start + Duration::from_nanos(ns);
                        while Instant::now() < due {
                            std::hint::spin_loop();
                        }
                        due
                    }
                };
                match decode_command(&buf[..*len]) {
                    Ok(cmd) => to_matching.send(Inbound { cmd, stamp }),
                    Err(_) => bad_frames += 1,
                }
            }
            bad_frames
            // Dropping `to_matching` here closes the ring.
        });
        s.spawn(move || {
            let mut book = FastBook::new();
            let mut events = Vec::with_capacity(64);
            while let Some(Inbound { cmd, stamp }) = from_gateway.recv() {
                events.clear();
                book.apply(&cmd, &mut events);
                for &event in &events {
                    to_output.send(Outbound::Event(event));
                }
                to_output.send(Outbound::Done { cmd, stamp });
            }
        });
        let output = s.spawn(move || {
            let mut output = Output::new();
            let mut latency = histogram();
            let mut events = Vec::with_capacity(64);
            while let Some(item) = from_matching.recv() {
                match item {
                    Outbound::Event(event) => events.push(event),
                    Outbound::Done { cmd, stamp } => {
                        output.on_command(&cmd, &events);
                        events.clear();
                        latency.saturating_record(stamp.elapsed().as_nanos() as u64);
                    }
                }
            }
            (output, latency)
        });
        let bad_frames = gateway.join().expect("gateway thread");
        let (output, latency) = output.join().expect("output thread");
        (output.finish(bad_frames), latency)
    });
    Report {
        digests,
        latency,
        elapsed: start.elapsed(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bad_frames_are_dropped_and_counted() {
        let commands: Vec<Command> = ["limit 1 sell 5 100", "limit 2 buy 5 100"]
            .iter()
            .map(|l| l.parse().unwrap())
            .collect();
        let mut frames = frames(&commands);
        frames.insert(1, ([9; 32], 3));
        let single = run_single(&frames);
        assert_eq!((single.commands, single.bad_frames), (2, 1));
        assert_eq!((single.events, single.feed_msgs), (3, 3));
        assert_eq!(run::<Ring>(&frames, 2, 0).digests, single);
    }
}
