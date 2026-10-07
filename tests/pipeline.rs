//! The three-thread pipeline must produce exactly what one thread does (D47), over the
//! ring at capacities that force constant hand-offs, and over `std::sync::mpsc`.

mod common;

use common::random_command;
use lob::gen::Generator;
use lob::pipeline::{frames, run, run_single, Mpsc, Ring};
use lob::rng::Rng;
use lob::Command;

fn random(seed: u64, n: usize) -> Vec<Command> {
    let mut rng = Rng::new(seed);
    let mut next_id = 1;
    (0..n)
        .map(|_| random_command(&mut rng, &mut next_id, 1))
        .collect()
}

#[test]
fn the_pipeline_equals_one_thread() {
    for seed in 0..5 {
        let frames = frames(&random(seed, 5_000));
        let want = run_single(&frames);
        assert!(want.events > 5_000 && want.feed_msgs > 1_000, "{want:?}");
        for cap in [1, 2, 64] {
            assert_eq!(
                run::<Ring>(&frames, cap, 0).digests,
                want,
                "seed {seed}, ring {cap}"
            );
        }
        assert_eq!(
            run::<Mpsc>(&frames, 16, 0).digests,
            want,
            "seed {seed}, mpsc"
        );
    }
}

#[test]
fn the_pipeline_matches_the_pinned_replay_and_feed_digests() {
    let frames = frames(&Generator::seeded(1).take(20_000).collect::<Vec<_>>());
    let report = run::<Ring>(&frames, 64, 0);
    // The README's replay digest (D17) and the feed digest pinned in tests/feed.rs (D43).
    assert_eq!(report.digests.events_digest, 0xf0cd_0c4b_e21b_0c27);
    assert_eq!(report.digests.feed_digest, 0x1bef_8ebc_21b7_ca92);
    assert_eq!(report.latency.len(), 20_000);
}

#[test]
fn a_paced_run_keeps_its_schedule() {
    let frames = frames(&random(7, 2_000));
    // 200k commands/s: 2,000 commands take at least 10 ms.
    let report = run::<Ring>(&frames, 64, 200_000);
    assert!(report.elapsed.as_millis() >= 10, "{:?}", report.elapsed);
    assert_eq!(report.digests, run_single(&frames));
    assert_eq!(report.latency.len(), 2_000);
}
