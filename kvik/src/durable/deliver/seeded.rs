//! The deliverer's part of a seeded start (`spec.md` §3 Order of a start): the state at the seed from the registry
//! file and the series directory, the log above it taken in the writer's pass, and what lies below it read from the
//! log only when a call needs it.

use super::*;
use crate::durable::history::span_of;
use crate::durable::keys::LocalFacts;
use crate::durable::seed::Seed;

impl Deliverer {
    /// Takes the state at the seed: the registry entries, routes and reset points up to it, each series' recent
    /// window empty above its latest `t`, and its last arrival where the series directory decides it.
    pub(crate) fn seed(&mut self, seed: &Seed) {
        let fold = self.index.take_fold();
        self.index = Index::seeded(seed.entries(), &seed.series);
        if let Some(f) = fold {
            self.index.set_fold(f);
        }
        for (p, _, e) in seed.entries() {
            self.view.apply(p, e);
            if let Entry::Recovery(r) = e {
                let after = LogPos {
                    generation: p.generation,
                    sequence: r.after,
                };
                self.resets.insert(p, after);
                for c in &r.consumers {
                    let l = self.lost.entry(c.clone()).or_insert(after);
                    *l = (*l).min(after);
                }
            }
            self.entries.insert(p, e.clone());
        }
        self.finder.seed_recoveries(seed.recoveries());
        let mut unknown = self.arrival_unknown.borrow_mut();
        for e in &seed.series {
            let (Some(latest), Some(c)) = (e.latest_t, e.counts) else {
                continue;
            };
            self.recent.seed(
                e.series_id,
                latest,
                span_of(&self.view.registry, e.series_id),
            );
            if c.last_revisable {
                unknown.insert(e.series_id, None);
            } else {
                self.last_arrival.insert(e.series_id, c.last_received_at);
            }
        }
        drop(unknown);
        self.head = seed.through;
        // Nothing at or below the seed is held: a consumer behind reads it from the log.
        self.items_from = next(seed.through);
        self.seeded = Some(seed.through);
    }

    /// Takes the batch at the seed, which the writer's pass reads first, for its commit time alone.
    pub(crate) fn seed_commit(&mut self, at_seed: &ReadEvent) {
        let _ = self.finder.apply(std::slice::from_ref(at_seed), self.head);
    }

    /// Takes a part of the log above the seed, as the writer's pass read it.
    pub(crate) fn take_shared(&mut self, events: Vec<ReadEvent>, durable: LogPos, ctx: &Ctx<'_>) {
        self.take_events(events, durable);
        self.take_pending(ctx);
    }

    /// Starts the deliverer's own reader after what the writer's pass gave it; `false` if it cannot start there.
    pub(crate) fn resume_after_pass(&mut self) -> bool {
        let head = self.head;
        self.reader
            .get_mut()
            .as_mut()
            .is_some_and(|r| r.start_after(head).unwrap_or(false))
    }

    /// The receive time of a series' last fact that was not outranked (`spec.md` §6 Restart and resume): for a series
    /// whose last fact at the seed may have been outranked, read from the log once.
    pub(super) fn last_arrival_of(&self, s: u32) -> Option<i64> {
        if let Some(r) = self.last_arrival.get(s) {
            return Some(*r);
        }
        let mut unknown = self.arrival_unknown.borrow_mut();
        let slot = unknown.get_mut(&s)?;
        if slot.is_none() {
            *slot = Some(self.read_last_arrival(s));
        }
        slot.flatten()
    }

    /// The last arrival of series `s` among its facts at or below the seed, read from the log: the receive time of the
    /// last one by position that no earlier fact of its key outranks (`spec.md` §4 Outranked facts).
    fn read_last_arrival(&self, s: u32) -> Option<i64> {
        let through = self.seeded?;
        self.slow.set(self.slow.get() + 1);
        let mut r = self.local()?;
        let found = crate::durable::keys::OnLog {
            reader: &mut r,
            lost: &self.damaged,
            latch: &self.latch,
        }
        .between(s, i64::MIN, i64::MAX)?;
        let mut facts: Vec<_> = found
            .into_iter()
            .filter(|f| f.position <= through)
            .collect();
        facts.sort_by_key(|f| f.position);
        let mut best: HashMap<i64, u64> = HashMap::new();
        let mut last = None;
        for f in facts {
            let outranked = f
                .row
                .changed_at
                .is_some_and(|c| best.get(&f.row.t).is_some_and(|b| *b > c));
            if let Some(c) = f.row.changed_at {
                let b = best.entry(f.row.t).or_insert(c);
                *b = (*b).max(c);
            }
            if !outranked {
                last = Some(f.received_at);
            }
        }
        last
    }

    /// The last position committed at or before `at` (`spec.md` §2.3 As-of time): among the batches taken, else, below
    /// a seeded start's seed, read from the log.
    pub(super) fn commit_at(&self, at: i64, ctx: &Ctx<'_>) -> LogPos {
        if let Some(p) = self.index.commit_at(at) {
            return p;
        }
        let Some(through) = self.seeded else {
            return LogPos::default();
        };
        self.slow.set(self.slow.get() + 1);
        read_commit_at(ctx, through, at).unwrap_or_default()
    }
}

/// The last position up to `through` whose batch was committed at or before `at`, in log order after its voids.
fn read_commit_at(ctx: &Ctx<'_>, through: LogPos, at: i64) -> Option<LogPos> {
    let mut reader = crate::durable::start::log_reader(ctx.dir, ctx.env).ok()?;
    let mut commits: Vec<(LogPos, i64)> = Vec::new();
    let read = loop {
        let Ok((u, more)) = reader.refresh_upto(LOAD_PART, through) else {
            break None;
        };
        for e in &u.events {
            match e {
                ReadEvent::Batch(b) if b.last <= through => commits.push((b.last, b.commit_time)),
                ReadEvent::Void { above, .. } => commits.retain(|(p, _)| p <= above),
                _ => {}
            }
        }
        if !more {
            break Some(());
        }
    };
    let _ = reader.close();
    read?;
    commits
        .iter()
        .rev()
        .find(|(_, t)| *t <= at)
        .map(|(p, _)| *p)
}
