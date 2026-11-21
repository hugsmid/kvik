//! A writer that finds a higher generation claimed stops uploading for good (`spec.md` §8 Generations; Q-265).

mod upload_support;

use std::time::Duration;

use kvik_backup::fake::block_on;
use kvik_backup::keys::{ClaimMarker, Keys};
use kvik_backup::upload::{BackupDriver, Step, UploadError};
use upload_support::*;

fn record_puts(h: &Harness) -> usize {
    let prefix = Keys::new(ID).records_prefix(0);
    h.events()
        .iter()
        .filter(|e| matches!(e, Event::Put(k) if k.starts_with(&prefix)))
        .count()
}

fn claim(h: &Harness, generation: u32) {
    let marker = ClaimMarker {
        generation,
        nonce: 5,
    }
    .encode();
    h.sink.mem.set(&Keys::new(ID).claim(generation), &marker);
}

fn three() -> Harness {
    let mut h = Harness::new();
    for len in [10, 70_000, 3] {
        h.seal(len);
    }
    h
}

#[test]
fn a_claim_above_the_stores_generation_stops_uploads_at_the_first_record() {
    let h = three();
    claim(&h, 1);
    let mut d = h.driver();
    let s = run_to_fault(&mut d, Duration::ZERO);
    assert!(
        matches!(s, Step::Failed(UploadError::Superseded { by: 1 })),
        "{s:?}"
    );
    let puts: Vec<Event> = h
        .events()
        .into_iter()
        .filter(|e| matches!(e, Event::Put(_)))
        .collect();
    assert_eq!(puts, vec![Event::Put(Keys::new(ID).store())]);
    let before = h.events().len();
    // A claim is never withdrawn; a bucket that stops reporting it does not restart uploads.
    h.sink.mem.remove(&Keys::new(ID).claim(1));
    for _ in 0..3 {
        let s = block_on(d.step(Duration::ZERO));
        assert!(matches!(s, Step::Failed(UploadError::Superseded { by: 1 })));
    }
    assert_eq!(h.events().len(), before, "nothing more after it stops");
}

#[test]
fn a_claim_made_while_uploading_stops_the_next_record_and_names_the_highest() {
    let h = three();
    let mut d = h.driver();
    for _ in 0..10_000 {
        assert!(matches!(block_on(d.step(Duration::ZERO)), Step::Progress));
        if record_puts(&h) > 0 {
            break;
        }
    }
    let uploaded = record_puts(&h);
    assert!(uploaded > 0);
    claim(&h, 1);
    claim(&h, 2);
    let s = run_to_fault(&mut d, Duration::ZERO);
    assert!(
        matches!(s, Step::Failed(UploadError::Superseded { by: 2 })),
        "{s:?}"
    );
    assert_eq!(record_puts(&h), uploaded);
    assert!(d.progress().last_error.is_some());
}
