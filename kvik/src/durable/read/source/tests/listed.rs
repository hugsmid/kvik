//! One-series reads of sealed segments through their series batches (`format.md` A.8a, Q-1150).

use std::time::Duration;

use super::{all, collect, reader_with, sealed_segments, seals};
use crate::ValueType;
use crate::durable::read::cache::{LEAST_MEMORY, Sizes};
use crate::durable::read::latest::tests::Rig;
use crate::durable::reader::facts::facts;
use crate::durable::tests::block_on;
use crate::read::View;
use crate::series::{Kind, SeriesSpec};
use crate::time::Timestamp;
use crate::types::{Intent, Reading, Value};

#[test]
fn a_one_series_read_of_sealed_segments_reads_them_through_their_series_batches() {
    let mut rig = Rig::with_log("listed", seals(Duration::from_millis(20)));
    let mut ids = vec![rig.ids[0], rig.ids[1]];
    for name in ["h/g/two", "h/g/three"] {
        let spec = SeriesSpec::new(
            name,
            Kind::Gauge {
                value: ValueType::F64,
            },
            "W",
        );
        let store = rig.store.as_ref().expect("an open store");
        let id = block_on(store.define_series(spec.clone())).unwrap();
        assert_eq!(block_on(rig.model.define_series(spec)).unwrap(), id);
        ids.push(id);
    }
    let (a, b) = (ids[0], ids[1]);
    let now = rig.clock.wall().0;
    for k in 0..80i64 {
        let id = ids[k as usize % ids.len()];
        let t = Timestamp(now - 10_000 + k);
        rig.emit_readings(&[Reading::new(id, t, Value::F64(k as f64))], Intent::Normal);
        if k % 20 == 19 {
            std::thread::sleep(Duration::from_millis(30));
        }
    }
    let marker = Timestamp(now - 5_000);
    rig.emit_readings(&[Reading::new(a, marker, Value::F64(-1.0))], Intent::Normal);
    std::thread::sleep(Duration::from_millis(30));
    rig.emit_readings(&[Reading::new(b, marker, Value::F64(-1.0))], Intent::Normal);
    assert!(sealed_segments(&rig) >= 2);
    let reader = reader_with(&rig.env, &rig.dir, Sizes::of(LEAST_MEMORY));
    let got = collect(facts(reader.clone(), a, all(), View::History).unwrap()).unwrap();
    let want = reader
        .with(|i, _| i.facts(a, all(), View::History))
        .unwrap();
    assert_eq!(got, want);
    assert!(
        reader.listed() > 0,
        "no sealed segment read through its series batches"
    );
    rig.shut();
}
