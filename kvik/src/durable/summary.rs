//! The summaries an index without facts keeps per series (roadmap 11.0), and their rebuild from the kept log after a
//! void or a loss removed facts they counted (Q-930).

use kvik_log::content::Item;
use kvik_log::facts::FactRow;
use kvik_log::reader::ReadEvent;
use kvik_log::writer::Position as LogPos;

use super::by_id::ById;

/// What an index without facts keeps of a series' facts: the first position, the latest `t` and the lateness
/// reference (spec §2.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Summary {
    pub(super) first: LogPos,
    pub(super) latest: i64,
    pub(super) reference: Option<i64>,
}

impl Summary {
    pub(super) fn note(s: &mut ById<Summary>, p: LogPos, row: &FactRow) {
        let x = s.get_or_insert_with(row.series_id, || Summary {
            first: p,
            latest: row.t,
            reference: None,
        });
        x.latest = x.latest.max(row.t);
        if !row.flags.future && !row.flags.unsynced_clock {
            x.reference = x.reference.max(Some(row.t));
        }
    }
}

/// The second pass of [`read_summary`]: each fact counted unless a later void of the stream removes it, which the
/// first pass found.
struct SummaryPass {
    /// Per void, in stream order, the least `above` of it and every later one.
    bounds: Vec<LogPos>,
    seen: usize,
    summary: ById<Summary>,
}

impl SummaryPass {
    fn new(mut voids: Vec<LogPos>) -> Self {
        for k in (0..voids.len().saturating_sub(1)).rev() {
            voids[k] = voids[k].min(voids[k + 1]);
        }
        SummaryPass {
            bounds: voids,
            seen: 0,
            summary: ById::default(),
        }
    }

    fn apply(&mut self, events: &[ReadEvent]) {
        for e in events {
            match e {
                ReadEvent::Batch(b) => {
                    let bound = self.bounds.get(self.seen).copied();
                    let mut sequence = b.first.sequence;
                    for item in &b.content.items {
                        let p = LogPos {
                            generation: b.first.generation,
                            sequence,
                        };
                        if let Item::Fact(row) = item
                            && bound.is_none_or(|a| p <= a)
                        {
                            Summary::note(&mut self.summary, p, row);
                        }
                        sequence += item.sequences();
                    }
                }
                ReadEvent::Void { .. } => self.seen += 1,
                ReadEvent::Corruption { .. } => {}
            }
        }
    }
}

fn voids_of(events: &[ReadEvent], out: &mut Vec<LogPos>) {
    out.extend(events.iter().filter_map(|e| match e {
        ReadEvent::Void { above, .. } => Some(*above),
        _ => None,
    }));
}

/// The summaries of the kept log, read in parts of `part` items with two fresh readers: the first pass finds where
/// voids fall, the second counts each fact no later void removes, so that memory holds one part at a time.
pub(crate) fn read_summary(
    open: impl Fn() -> Result<kvik_log::reader::LogReader, kvik_log::reader::ReaderError>,
    part: usize,
) -> Result<ById<Summary>, kvik_log::reader::ReaderError> {
    let mut voids = Vec::new();
    let mut r = open()?;
    loop {
        let (u, more) = r.refresh_some(part)?;
        voids_of(&u.events, &mut voids);
        if !more {
            break;
        }
    }
    let _ = r.close();
    let mut pass = SummaryPass::new(voids);
    let mut r = open()?;
    loop {
        let (u, more) = r.refresh_some(part)?;
        pass.apply(&u.events);
        if !more {
            break;
        }
    }
    let _ = r.close();
    Ok(pass.summary)
}

#[cfg(test)]
mod tests {
    use kvik_log::content::BatchContent;
    use kvik_log::facts::{FactFlags as LogFlags, Quality as LogQuality, Value as LogValue};
    use kvik_log::reader::{ReadBatch, VoidCause};

    use super::*;
    use crate::durable::index::Index;

    fn batch(first: u64, items: Vec<Item>) -> ReadEvent {
        let n: u64 = items.iter().map(Item::sequences).sum();
        let p = |sequence| LogPos {
            generation: 0,
            sequence,
        };
        ReadEvent::Batch(ReadBatch {
            file_id: 1,
            offset: 0,
            first: p(first),
            last: p(first + n - 1),
            commit_time: first as i64,
            content: BatchContent {
                first_sequence: first,
                received_at: first as i64,
                items,
            },
        })
    }

    const MARK: LogPos = LogPos {
        generation: 0,
        sequence: 100,
    };

    fn fact_of(series_id: u32, t: i64, future: bool) -> Item {
        Item::Fact(FactRow {
            series_id,
            t,
            quality: LogQuality::Good,
            flags: LogFlags {
                future,
                ..LogFlags::default()
            },
            value: LogValue::F64(1),
            changed_at: None,
        })
    }

    #[test]
    fn summaries_held_and_read_in_two_passes_equal_those_of_every_fact() {
        let void = |above| ReadEvent::Void {
            above: LogPos {
                generation: 0,
                sequence: above,
            },
            cause: VoidCause::Rewritten,
        };
        // Facts of series 1 and 2; a void above 3 removes the later ones, which are taken again lower; then a second
        // void above 5 and more facts.
        let stream = vec![
            batch(1, vec![fact_of(1, 10, false), fact_of(2, 50, true)]),
            batch(3, vec![fact_of(1, 30, false), fact_of(2, 40, false)]),
            batch(5, vec![fact_of(1, 99, false)]),
            void(3),
            batch(4, vec![fact_of(1, 20, false), fact_of(2, 45, false)]),
            batch(6, vec![fact_of(2, 70, true)]),
            void(5),
            batch(6, vec![fact_of(3, 5, false)]),
        ];
        let mut full = Index::default();
        full.apply(&stream, MARK);
        let mut held = Index::without_facts();
        let mut voids = Vec::new();
        for (k, e) in stream.iter().enumerate() {
            held.apply(std::slice::from_ref(e), MARK);
            voids_of(std::slice::from_ref(e), &mut voids);
            assert_eq!(held.summary_stale(), k >= 3);
        }
        let mut pass = SummaryPass::new(voids);
        for e in &stream {
            pass.apply(std::slice::from_ref(e));
        }
        held.set_summary(pass.summary);
        assert!(!held.summary_stale());
        for id in 1..=4 {
            assert_eq!(held.latest_t(id), full.latest_t(id), "series {id}");
            assert_eq!(
                held.lateness_reference(id),
                full.lateness_reference(id),
                "series {id}"
            );
        }
        assert_eq!(
            (held.latest_t(2), held.lateness_reference(2)),
            (Some(50), Some(45))
        );
        assert_eq!(held.lateness_reference(1), Some(30));
        // Without a void the summaries are kept exactly as facts are taken.
        let mut plain = Index::without_facts();
        plain.apply(&stream[..3], MARK);
        assert!(!plain.summary_stale());
        assert_eq!(
            (plain.latest_t(1), plain.lateness_reference(2)),
            (Some(99), Some(40))
        );
    }

    #[test]
    fn a_later_lower_void_bounds_the_facts_an_earlier_higher_one_kept() {
        let void = |above| ReadEvent::Void {
            above: LogPos {
                generation: 0,
                sequence: above,
            },
            cause: VoidCause::Rewritten,
        };
        // Positions 4 and 5 survive the void above 5 and not the later one above 3.
        let stream = vec![
            batch(1, vec![fact_of(1, 10, false), fact_of(1, 20, false)]),
            batch(3, vec![fact_of(2, 30, false)]),
            batch(4, vec![fact_of(1, 40, false), fact_of(2, 50, false)]),
            batch(6, vec![fact_of(1, 60, false)]),
            void(5),
            batch(6, vec![fact_of(3, 1, false)]),
            void(3),
            batch(4, vec![fact_of(3, 2, false)]),
        ];
        let mut full = Index::default();
        full.apply(&stream, MARK);
        let mut voids = Vec::new();
        voids_of(&stream, &mut voids);
        let mut pass = SummaryPass::new(voids);
        pass.apply(&stream);
        let mut held = Index::without_facts();
        held.apply(&stream, MARK);
        held.set_summary(pass.summary);
        for id in 1..=3 {
            assert_eq!(held.latest_t(id), full.latest_t(id), "series {id}");
            assert_eq!(
                held.lateness_reference(id),
                full.lateness_reference(id),
                "series {id}"
            );
        }
        assert_eq!(
            (held.latest_t(1), held.latest_t(2), held.latest_t(3)),
            (Some(20), Some(30), Some(2))
        );
    }
}
