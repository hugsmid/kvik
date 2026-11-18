//! The last-uploaded marker invariants every upload run must leave (Q-614).

use std::collections::{BTreeMap, BTreeSet};

use kvik_backup::envelope::Envelope;
use kvik_backup::fake::{MemSink, TestEnvelope};
use kvik_backup::keys::{Keys, ObjectName};
use kvik_backup::marker::{Marker, Summary};
use kvik_files::record::Record;

use super::{Event, ID, KEY, in_bucket};

/// Every last-uploaded marker in `mem` names exactly the records of its generation in the bucket up to its epoch, and
/// a generation's sequences run from 1 with no gap and rising epochs (Q-614). With `idle`, each generation's latest
/// marker names its last record.
pub fn check_markers(mem: &MemSink, idle: bool) {
    let (keys, env) = (Keys::new(ID), TestEnvelope::new(KEY));
    let records = in_bucket(mem);
    let mut markers: BTreeMap<u32, Vec<Marker>> = BTreeMap::new();
    for (k, v) in mem.objects() {
        if let Some(ObjectName::Marker {
            generation,
            sequence,
        }) = keys.parse(&k)
        {
            let m = Marker::decode(&env.open(&k, &v).unwrap()).unwrap();
            assert_eq!((m.generation, m.sequence), (generation, sequence), "{k}");
            markers.entry(generation).or_default().push(m);
        }
    }
    for (g, ms) in &mut markers {
        ms.sort_by_key(|m| m.sequence);
        let mine: Vec<&Record> = records
            .iter()
            .map(|u| &u.record)
            .filter(|r| r.generation == *g)
            .collect();
        for (i, m) in ms.iter().enumerate() {
            assert_eq!(m.sequence, i as u64 + 1, "generation {g}");
            let upto = Summary::of(mine.iter().copied().filter(|r| r.epoch <= m.summary.epoch));
            assert_eq!(m.summary, upto, "generation {g} marker {}", m.sequence);
            if let Some(before) = i.checked_sub(1) {
                assert!(m.summary.epoch > ms[before].summary.epoch, "generation {g}");
            }
        }
        if idle {
            assert_eq!(
                ms.last().map(|m| m.summary.epoch),
                mine.last().map(|r| r.epoch),
                "generation {g}"
            );
        }
    }
    if idle {
        let marked: BTreeSet<u32> = markers.keys().copied().collect();
        let uploaded: BTreeSet<u32> = records.iter().map(|u| u.record.generation).collect();
        assert_eq!(marked, uploaded, "a generation with records and no marker");
    }
}

/// Each marker reaches the bucket after the record it names (Q-614); its last `put` is the one it holds, since a test
/// may remove one outside the sink's rules.
pub fn check_marker_order(events: &[Event], objects: &BTreeMap<String, Vec<u8>>) {
    let (keys, env) = (Keys::new(ID), TestEnvelope::new(KEY));
    let first_put = |key: &str| {
        events
            .iter()
            .position(|e| matches!(e, Event::Put(k) if k == key))
    };
    let last_put = |key: &str| {
        events
            .iter()
            .rposition(|e| matches!(e, Event::Put(k) if k == key))
    };
    for (k, v) in objects {
        if let Some(ObjectName::Marker { generation, .. }) = keys.parse(k) {
            let m = Marker::decode(&env.open(k, v).unwrap()).unwrap();
            let record = first_put(&keys.record(generation, m.summary.epoch));
            let marker = last_put(k);
            assert!(
                record.is_some() && record < marker,
                "{k} before record {}",
                m.summary.epoch
            );
        }
    }
}
