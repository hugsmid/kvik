//! The pass above a seed gives way to the whole log on a void and on a region at or below the seed (Q-1111). Neither
//! reaches the pass through [`Seed::find`]: a reset below a directory's `through` makes it no seed, and recovery
//! truncates what a reboot leaves above the mark; so the pass is fed the events directly.

use kvik_log::reader::{ReadEvent, VoidCause};
use kvik_log::registry::RegistryFile;
use kvik_log::scan::{InvalidRegion, LossCause};
use kvik_log::writer::Position as LogPos;

use super::super::Seed;
use crate::durable::loss::Latch;
use crate::durable::start::{GiveWay, Tail};

const S: u64 = 10;

fn pos(sequence: u64) -> LogPos {
    LogPos {
        generation: 0,
        sequence,
    }
}

fn tail() -> Tail {
    let seed = Seed {
        through: pos(S),
        series: Vec::new(),
        judged_at: 0,
        registry: RegistryFile {
            generation: 0,
            through: S,
            max_series_id: 0,
            entries: Vec::new(),
        },
    };
    Tail::new(&seed, 1 << 20)
}

fn region(before: Option<u64>, after: Option<u64>) -> ReadEvent {
    ReadEvent::Corruption {
        file_id: 1,
        region: InvalidRegion {
            start: 64,
            end: 128,
            last_sequence_before: before,
            first_sequence_after: after,
            cause: LossCause::Corruption,
        },
    }
}

#[test]
fn a_void_above_the_seed_gives_way() {
    let void = ReadEvent::Void {
        above: pos(S + 1),
        cause: VoidCause::Recovery { entry: pos(S + 3) },
    };
    assert_eq!(
        tail().take(&[void], pos(S + 3), &Latch::default()),
        Err(GiveWay::Void)
    );
}

#[test]
fn a_region_holding_the_seed_gives_way_and_one_above_it_does_not() {
    let latch = Latch::default();
    assert_eq!(
        tail().take(&[region(Some(S - 2), Some(S + 2))], pos(S + 4), &latch),
        Err(GiveWay::Below)
    );
    assert_eq!(
        tail().take(&[region(Some(S), Some(S + 2))], pos(S + 4), &latch),
        Ok(())
    );
}
