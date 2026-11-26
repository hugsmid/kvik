//! Walking the rounds a batch takes, and bringing each read to the batch's cut.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use super::*;

impl Session {
    /// The walk over rounds of facts alone, where each opened `Tail` takes, below its row limit, the facts of the
    /// series it selects: those come shared from the hub, one decode per set of series a round's sessions take, and
    /// the round's series are what the batch touched. `None` where the whole walk is needed.
    pub(super) fn walk_shared(
        &self,
        rounds: &[Arc<super::super::hub::Round>],
        reads: &BTreeMap<LiveId, Read>,
        lagged: bool,
    ) -> Option<Walk> {
        #[cfg(test)]
        if self.every {
            return None;
        }
        let partial = |r: &super::super::hub::Round| r.seq == self.at.0 + 1 && self.at.1 > 0;
        if lagged || rounds.iter().any(|r| !r.facts_only || partial(r)) {
            return None;
        }
        let hub = self.reader.hub();
        let mut w = Walk::default();
        for (k, x) in reads {
            let Read::Tail(t) = x else { continue };
            if !t.opened {
                continue;
            }
            let mut taken = 0usize;
            let mut chunks = Vec::new();
            for r in rounds {
                let mine: BTreeSet<u32> = r
                    .series
                    .iter()
                    .copied()
                    .filter(|id| selects(&t.sel, &t.names, *id))
                    .collect();
                if mine.is_empty() {
                    continue;
                }
                let f = hub.facts_of(r, &mine);
                taken = taken.saturating_add(f.len());
                if taken > t.opts.max_rows {
                    return None;
                }
                chunks.push(f);
            }
            if !chunks.is_empty() {
                w.shared.insert(*k, chunks);
            }
        }
        for r in rounds {
            w.touched.extend(&r.series);
        }
        Some(w)
    }

    /// Walks the pieces of the rounds after the session's cursor: the facts each opened `Tail` takes, up to the
    /// first row limit met, and what the `Latest`s must judge again. Every gap taken is named, past the stop too, so
    /// no batch carries a fact the reader knows lost nor leaves a loss below its cut unnamed (Q-1011); a fact a
    /// later void or gap drops is never taken, so the stop's cut lies below every void after it, which waits.
    pub(super) fn walk(
        &self,
        v: &View,
        rounds: &[Arc<super::super::hub::Round>],
        reads: &mut BTreeMap<LiveId, Read>,
        lagged: bool,
    ) -> Walk {
        let pieces: Vec<(u64, usize, &Piece)> = rounds
            .iter()
            .flat_map(|r| r.pieces().map(move |(i, p)| (r.seq, i, p)))
            .filter(|(seq, i, _)| *seq != self.at.0 + 1 || *i >= self.at.1)
            .collect();
        let mut later_void = vec![None; pieces.len()];
        let mut least: Option<LogPos> = None;
        for (k, (_, _, p)) in pieces.iter().enumerate().rev() {
            later_void[k] = least;
            if let Piece::Void(above) = p {
                least = Some(least.map_or(*above, |l| l.min(*above)));
            }
        }
        let gaps: Vec<(usize, LogPos, LogPos)> = pieces
            .iter()
            .enumerate()
            .filter_map(|(k, (_, _, p))| match p {
                Piece::Gap(a, b) => Some((k, *a, *b)),
                _ => None,
            })
            .collect();
        let dropped = |k: usize, at: LogPos| {
            later_void[k].is_some_and(|above| at > above)
                || gaps.iter().any(|(j, a, b)| *j > k && *a <= at && at <= *b)
        };
        let mut w = Walk::default();
        for (k, (seq, i, piece)) in pieces.iter().enumerate() {
            if w.stop.is_some() {
                if let Piece::Gap(a, b) = piece {
                    w.named.push((*a, *b));
                    self.name(&mut w, (*a, *b));
                }
                continue;
            }
            match piece {
                Piece::Fact(s) => {
                    let id = s.row.series_id;
                    w.touched.insert(id);
                    if lagged || dropped(k, s.position) {
                        continue;
                    }
                    let wants: Vec<LiveId> = reads
                        .iter()
                        .filter_map(|(k, x)| match x {
                            Read::Tail(t) if t.opened && selects(&t.sel, &t.names, id) => Some(*k),
                            _ => None,
                        })
                        .collect();
                    let full = wants.iter().any(|k| {
                        let max = match &reads[k] {
                            Read::Tail(t) => t.opts.max_rows,
                            Read::Latest(_) => usize::MAX,
                        };
                        w.facts.get(k).map_or(0, Vec::len) >= max
                    });
                    if full {
                        let cut = LogPos {
                            generation: s.position.generation,
                            sequence: s.position.sequence.saturating_sub(1),
                        };
                        w.stop = Some((*seq, *i, cut));
                        continue;
                    }
                    for k in wants {
                        w.facts.entry(k).or_default().push(s.clone());
                    }
                }
                Piece::Entry(e) => {
                    w.entries = true;
                    w.touched.extend(entry_series(e));
                    for x in reads.values_mut() {
                        if let Read::Tail(t) = x
                            && t.opened
                        {
                            t.names.apply(e);
                        }
                    }
                }
                Piece::Void(above) => {
                    w.reset = Some(w.reset.map_or(*above, |r| r.min(*above)));
                    for x in reads.values_mut() {
                        if let Read::Tail(t) = x
                            && t.opened
                        {
                            t.from = t.from.min(*above);
                            t.names = v.latest().registry_at(*above);
                        }
                    }
                }
                Piece::Gap(a, b) => self.name(&mut w, (*a, *b)),
            }
        }
        w
    }

    /// Names gap `g` in the batch unless a batch that stopped before it named it already.
    fn name(&self, w: &mut Walk, g: (LogPos, LogPos)) {
        if !self.named.contains(&g) && !w.gaps.contains(&g) {
            w.gaps.push(g);
        }
    }
}

/// Brings `Tail` `t` to the batch's cut.
pub(super) fn tail(
    at: &At<'_>,
    (id, t): (LiveId, &mut TailRead),
    w: &Walk,
    lagged: bool,
    out: &mut Vec<LiveEvent>,
) {
    if !t.opened {
        t.opened = true;
        t.from = at.cut;
        t.names = at.registry().into_owned();
        out.push(LiveEvent::Opened(Answer::Tail));
        return;
    }
    if let Some(above) = w.reset {
        out.push(LiveEvent::Reset { above: pos(above) });
        out.push(LiveEvent::Opened(Answer::Tail));
    }
    for (a, b) in &w.gaps {
        out.push(LiveEvent::Gap {
            first: pos(*a),
            last: pos(*b),
        });
    }
    if lagged {
        if t.from < at.cut {
            out.push(LiveEvent::Lagged {
                from: pos(next_pos(t.from)),
                to: pos(at.cut),
            });
        }
        t.names = at.registry().into_owned();
    } else {
        let facts: Vec<Fact> = match w.shared.get(&id) {
            Some(chunks) => chunks.iter().flat_map(|c| c.iter().cloned()).collect(),
            None => w
                .facts
                .get(&id)
                .map_or_else(Vec::new, |f| f.iter().map(fact_out).collect()),
        };
        if !facts.is_empty() {
            out.push(LiveEvent::Appended(Appended::Facts(facts)));
        }
    }
    t.from = at.cut;
}

/// Brings `Latest` `l` to the batch's cut: opened again after a reset or a loss, else its membership when entries
/// came and the members the batch touched, due by time, or `refreshed` (their live staleness).
pub(super) fn latest_read(
    at: &At<'_>,
    registry: &Registry,
    l: &mut LatestRead,
    w: &Walk,
    (lagged, refreshed): (bool, &BTreeSet<u32>),
    out: &mut Vec<LiveEvent>,
) -> Result<(), QueryError> {
    let reopen = !l.opened || w.reset.is_some() || !w.gaps.is_empty();
    if l.opened {
        if let Some(above) = w.reset {
            out.push(LiveEvent::Reset { above: pos(above) });
        }
        for (a, b) in &w.gaps {
            out.push(LiveEvent::Gap {
                first: pos(*a),
                last: pos(*b),
            });
        }
    }
    if reopen {
        l.opened = true;
        l.members = at.resolve(&l.sel, registry);
        l.shown.clear();
        l.due.clear();
        let mut answer = Vec::new();
        for id in &l.members {
            let live = at.live(*id)?;
            if let Some(d) = at.due(registry, *id, &live) {
                l.due.insert(*id, d);
            }
            l.shown.insert(*id, live.clone());
            answer.push((*id, live));
        }
        out.push(LiveEvent::Opened(Answer::Latest(answer)));
        return Ok(());
    }
    let mut judge: BTreeSet<SeriesId> = BTreeSet::new();
    if w.entries || lagged {
        let members = at.resolve(&l.sel, registry);
        for id in l.members.iter().filter(|id| !members.contains(id)) {
            out.push(LiveEvent::Left { series: *id });
            l.shown.remove(id);
            l.due.remove(id);
        }
        for (index, id) in members.iter().enumerate() {
            if l.shown.contains_key(id) {
                continue;
            }
            let live = at.live(*id)?;
            let info = latest::series(at.v, &SeriesKey::Id(*id))?;
            if let Some(d) = at.due(registry, *id, &live) {
                l.due.insert(*id, d);
            }
            l.shown.insert(*id, live.clone());
            out.push(LiveEvent::Joined {
                series: *id,
                info: Box::new(info),
                index,
                part: crate::read::Part::Live(live),
            });
        }
        l.members = members;
        if lagged {
            judge.extend(l.members.iter().copied());
        }
    }
    judge.extend(
        l.members
            .iter()
            .filter(|id| {
                w.touched.contains(&id.0)
                    || refreshed.contains(&id.0)
                    || l.due.get(id).is_some_and(|d| *d <= at.now)
            })
            .copied(),
    );
    for id in judge {
        let Some(old) = l.shown.get(&id) else {
            continue;
        };
        let live = at.live(id)?;
        match at.due(registry, id, &live) {
            Some(d) => l.due.insert(id, d),
            None => l.due.remove(&id),
        };
        if *old != live {
            l.shown.insert(id, live.clone());
            out.push(LiveEvent::Updated(Updated::Live { series: id, live }));
        }
    }
    Ok(())
}
