//! Shared by the integration tests.
#![allow(dead_code)]

use std::num::NonZeroU16;

use lob::command::TimeInForce;
use lob::rng::Rng;
use lob::{Command, OrderId, Price, Qty, Side, Stp, StpAction};

/// Mostly a fresh id, sometimes an old one (to hit id-not-increasing rejects).
pub fn new_order_id(rng: &mut Rng, next_id: &mut u64) -> OrderId {
    if *next_id > 1 && rng.chance(10) {
        OrderId(1 + rng.below(*next_id - 1))
    } else {
        *next_id += 1;
        OrderId(*next_id - 1)
    }
}

/// A group in `1..=groups`, or none, with equal odds; never one when `groups` is 0, and
/// then no random number is drawn, so ungrouped flows stay what they were.
pub fn random_stp(rng: &mut Rng, groups: u16) -> Option<Stp> {
    if groups == 0 {
        return None;
    }
    let group = NonZeroU16::new(rng.below(groups as u64 + 1) as u16)?;
    let action = match rng.below(3) {
        0 => StpAction::CancelNewest,
        1 => StpAction::CancelOldest,
        _ => StpAction::CancelBoth,
    };
    Some(Stp { group, action })
}

/// Edge-case flow: narrow prices, small quantities, some zero, oversized (max 12) and
/// off-tick values, and reused ids. Crossing, partial fills, sweeps and FOK edge cases
/// are all common. New orders carry STP groups from `1..=groups` (none if 0).
pub fn random_command(rng: &mut Rng, next_id: &mut u64, tick: i64, groups: u16) -> Command {
    let side = if rng.chance(50) {
        Side::Buy
    } else {
        Side::Sell
    };
    // Some quantities are 0 or above the max (12), some prices off the tick grid.
    let qty = Qty(rng.below(14));
    let price = Price(if rng.chance(5) {
        rng.range(90, 110)
    } else {
        rng.range(95, 105) * tick
    });
    match rng.below(100) {
        0..=44 => {
            let tif = match rng.below(10) {
                0 => TimeInForce::Ioc,
                1 => TimeInForce::Fok,
                2 => TimeInForce::PostOnly,
                _ => TimeInForce::Gtc,
            };
            Command::Limit {
                id: new_order_id(rng, next_id),
                side,
                qty,
                price,
                tif,
                stp: random_stp(rng, groups),
            }
        }
        45..=54 => Command::Market {
            id: new_order_id(rng, next_id),
            side,
            qty,
            stp: random_stp(rng, groups),
        },
        55..=74 => Command::Modify {
            id: OrderId(1 + rng.below(*next_id)),
            qty,
            price,
        },
        _ => Command::Cancel {
            id: OrderId(1 + rng.below(*next_id)),
        },
    }
}
