//! A batch's entries, moved out of its calls and back, never cloned (`spec.md` §3 Group commit: one call is one
//! batch unit, its entries in order).

use super::Call;
use crate::content::Item;

/// Moves every call's entries, in call order, onto the end of `items`; returns each call's count.
pub(super) fn gather(calls: &mut [Call], items: &mut Vec<Item>) -> Vec<usize> {
    let mut counts = Vec::with_capacity(calls.len());
    for c in calls {
        counts.push(c.items.len());
        items.append(&mut c.items);
    }
    counts
}

/// Gives `calls` back their entries from the end of `items`, which ends with exactly those entries, in call order.
pub(super) fn give_back(calls: &mut [Call], counts: &[usize], items: &mut Vec<Item>) {
    debug_assert_eq!(calls.len(), counts.len());
    for (c, &n) in calls.iter_mut().zip(counts).rev() {
        c.items = items.split_off(items.len() - n);
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use crate::facts::{FactFlags, FactRow, Quality, Value};
    use crate::fs::MemFs;
    use crate::store::{NewStore, StoreDir};
    use crate::writer::{IngestQueue, LogWriter, WriterConfig, create_segment};

    fn text(series: u32, t: i64) -> Item {
        Item::Fact(FactRow {
            series_id: series,
            t,
            quality: Quality::Good,
            flags: FactFlags::default(),
            value: Value::Text(format!("v{series}-{t}")),
            changed_at: None,
        })
    }

    fn call(series: u32, n: i64) -> Call {
        Call::new((0..n).map(|t| text(series, t)).collect(), 1_024).unwrap()
    }

    /// The address of each text value's bytes: equal addresses mean the value was moved, not cloned.
    fn addresses(items: &[Item]) -> Vec<*const u8> {
        items
            .iter()
            .map(|i| match i {
                Item::Fact(FactRow {
                    value: Value::Text(s),
                    ..
                }) => s.as_ptr(),
                _ => unreachable!("text facts only"),
            })
            .collect()
    }

    #[test]
    fn gather_moves_entries_in_call_order_and_give_back_restores_each_call() {
        let mut calls = vec![call(1, 3), call(2, 1), call(3, 4)];
        let before: Vec<Call> = calls.clone();
        let at: Vec<_> = calls.iter().flat_map(|c| addresses(&c.items)).collect();
        let mut items = Vec::new();
        let counts = gather(&mut calls, &mut items);
        assert_eq!(counts, [3, 1, 4]);
        assert!(calls.iter().all(|c| c.items.is_empty()));
        let flat: Vec<Item> = before.iter().flat_map(|c| c.items.clone()).collect();
        assert_eq!(items, flat);
        assert_eq!(addresses(&items), at);
        give_back(&mut calls[1..], &counts[1..], &mut items);
        assert_eq!(items, before[0].items);
        assert_eq!(calls[1..], before[1..]);
        let at_rest: Vec<_> = calls[1..]
            .iter()
            .flat_map(|c| addresses(&c.items))
            .collect();
        assert_eq!(at_rest, at[3..]);
    }

    #[test]
    fn a_committed_batch_holds_the_calls_entries_moved_not_cloned() {
        let new = NewStore {
            store_id: [1; 16],
            created_at: 0,
            site_zone: "Etc/UTC".into(),
            tzdata: "2026a".into(),
            software: "test".into(),
        };
        let fs = MemFs::new();
        let store = StoreDir::open(Arc::new(fs.clone()), Path::new("s"), Some(&new)).unwrap();
        let cfg = WriterConfig {
            segment_size: 64 << 10,
            min_sync_interval: Duration::ZERO,
            disk_critical: 0,
            disk_low_bytes: 0,
            relaxed_limits: true,
            ..WriterConfig::default()
        };
        let mut w = LogWriter::open(store, cfg, Some([2; 16]), Duration::ZERO, 0).unwrap();
        let req = w.segment_request(0).unwrap();
        let r = create_segment(&fs, &req, &w.halt());
        w.segment_created(r);
        let mut q = IngestQueue::new(1_024);
        let calls = [call(1, 5), call(2, 2), call(3, 7)];
        let want: Vec<Item> = calls.iter().flat_map(|c| c.items.clone()).collect();
        let at: Vec<_> = calls.iter().flat_map(|c| addresses(&c.items)).collect();
        for c in calls {
            q.push(c).unwrap();
        }
        let step = w.step(Duration::from_millis(1), 1, &mut q);
        assert_eq!(step.done.len(), 3);
        assert_eq!(w.batch, want);
        assert_eq!(addresses(&w.batch), at);
        assert_eq!(w.batch_calls.len(), 3);
        w.step(Duration::from_millis(2), 2, &mut q);
        assert_eq!(w.batch_held(), 0, "the next step frees the batch");
    }
}
