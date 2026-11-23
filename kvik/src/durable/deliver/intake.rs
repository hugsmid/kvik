//! The deliverer's intake of the log: refreshes, the losses they find (Q-930), and the batches taken once
//! acknowledged.

use super::*;

impl Deliverer {
    pub(super) fn take_events(&mut self, events: Vec<ReadEvent>, durable: LogPos) {
        let found = self.finder.apply(&events, durable);
        if !found.is_empty() {
            self.latch.note(&found);
        }
        for e in events {
            match e {
                ReadEvent::Batch(b) => self.pending.push_back(b),
                ReadEvent::Void { above, .. } => self.void(above),
                // A region below the mark: the writer's next start records the loss; positions in it are never
                // delivered.
                ReadEvent::Corruption { file_id, region } => {
                    self.stats.corruptions += 1;
                    self.damaged.insert(file_id, region.start, region.end);
                }
            }
        }
    }

    /// Takes the batches acknowledged since the last call.
    pub(crate) fn pump(&mut self, ctx: &Ctx<'_>) {
        // Ranges found lost before the batches are taken apply to them, wherever and whenever they are read back.
        self.forget_latched();
        let Some(r) = self.reader.get_mut().as_mut() else {
            return;
        };
        // A refresh that fails changes nothing the reader holds; the next one reads it again. It is counted.
        match read_new(r, ctx.watch.map(|w| w.stats().durable)) {
            Ok(Some(u)) => self.take_events(u.events, u.state.durable),
            Ok(None) => {}
            Err(_) => self.stats.refresh_errors += 1,
        }
        self.take_pending(ctx);
        self.forget_latched();
    }

    /// Takes in the ranges the scanning readers of the process found lost since the last call: one the reader had read
    /// leaves the summaries to be rebuilt (Q-930).
    pub(super) fn forget_latched(&mut self) {
        let read = self.reader.get_mut().as_ref().map(|r| r.state().durable);
        for (first, last) in self.latch.closed_since(&mut self.closed_seen) {
            if read.is_some_and(|r| first <= r) {
                self.index.forget(first, last);
            }
        }
    }

    /// Damaged regions a consumer's read met that no scanning reader of the process has reported (Q-929).
    pub(crate) fn regions_unreported(&self) -> u64 {
        self.unread
            .borrow()
            .spans()
            .filter(|&(f, a, z)| !self.latch.covers(f, a, z))
            .count() as u64
    }

    /// Keeps the instants of acknowledgements noted since the last call ([`super::hub::Hub::acked`]).
    pub(crate) fn note_acked(&mut self, acked: Vec<(LogPos, Stamp)>) {
        // An acknowledgement noted after its batch was taken has nothing left to time (Q-1119).
        for (p, s) in acked.into_iter().filter(|(p, _)| *p > self.head) {
            let e = self.stamps.entry(p).or_insert(s);
            *e = (*e).min(s);
        }
    }

    /// Takes the batches read that are acknowledged.
    pub(super) fn take_pending(&mut self, ctx: &Ctx<'_>) {
        let stats = ctx.watch.map(Watch::stats);
        let mut read: HashMap<
            crate::durable::history::Key,
            (Vec<kvik_log::reader::FoundFact>, bool),
        > = HashMap::new();
        let mut took = false;
        while let Some(b) = self.pending.front() {
            if let Some(st) = &stats {
                let entry = b.content.items.iter().any(|i| matches!(i, Item::Entry(_)));
                if b.last > st.durable || (entry && b.last > st.mark_synced) {
                    break;
                }
            }
            let b = self.pending.pop_front().expect("looked at above");
            // As of its acknowledgement, as when the emit read its batch back itself; never before an instant
            // already taken as of (a condition runner created later would otherwise fire early).
            let kept = self.stamps.split_off(&next(b.last));
            let at = std::mem::replace(&mut self.stamps, kept)
                .into_iter()
                .filter(|(p, _)| *p >= b.first)
                .map(|(_, s)| s)
                .min()
                .unwrap_or(Stamp {
                    now: ctx.now,
                    wall: ctx.wall,
                    mono: ctx.mono,
                })
                .max(self.stamped);
            self.stamped = at;
            let c = Ctx {
                now: at.now,
                wall: at.wall,
                mono: at.mono,
                ..*ctx
            };
            self.take_batch(b, &c, &mut read);
            took = true;
        }
        if took {
            self.prune();
        }
    }

    /// Drops the records no consumer can read again: at or below the lowest cursor (a consumer without one starts
    /// at the last position).
    pub(super) fn prune(&mut self) {
        let floor = self
            .consumers
            .values()
            .filter(|rt| rt.has_cursor)
            .map(|rt| rt.cursor)
            .chain(
                self.consumers
                    .values()
                    .filter_map(|rt| rt.behind.map(before)),
            )
            .min()
            .unwrap_or(self.head);
        let keep = self.items.split_off(&next(floor));
        self.items = keep;
        self.items_from = self.items_from.max(next(floor));
    }

    /// Rebuilds the records from `at` up to where they are kept, from the entries and the log's facts. A read of the
    /// log that fails changes nothing and is counted (`refresh_errors`): the consumer stays behind and the next
    /// call that catches it up reads again. Returns whether the records from `at` are held.
    pub(super) fn restore_items(&mut self, at: LogPos, ctx: &Ctx<'_>) -> bool {
        if at >= self.items_from {
            return true;
        }
        let to = self.items_from;
        let Some(facts) = self.facts_between(at, to, ctx) else {
            self.stats.refresh_errors += 1;
            return false;
        };
        for (p, e) in self.entries.range(at..to) {
            self.items.insert(*p, Rec::Entry(e.clone()));
        }
        for (stored, outranked, unchecked) in facts {
            self.items.insert(
                stored.position,
                Rec::Fact(Arc::new(FactRec {
                    stored,
                    outranked,
                    unchecked,
                })),
            );
        }
        self.items_from = at;
        true
    }

    pub(super) fn void(&mut self, above: LogPos) {
        self.pending.retain(|b| b.last <= above);
        self.index.apply(
            &[ReadEvent::Void {
                above,
                cause: kvik_log::reader::VoidCause::Rewritten,
            }],
            above,
        );
        self.recent.void(above);
        self.items.retain(|p, _| *p <= above);
        self.items_from = self.items_from.min(next(above));
        self.entries.retain(|p, _| *p <= above);
        self.resets.retain(|p, _| *p <= above);
        self.head = self.head.min(above);
        let mut view = View::default();
        for (p, e) in &self.entries {
            view.apply(*p, e);
        }
        self.view = view;
        for rt in self.consumers.values_mut() {
            rt.stale_runners = true;
        }
    }

    pub(super) fn take_batch(
        &mut self,
        b: ReadBatch,
        ctx: &Ctx<'_>,
        read: &mut HashMap<crate::durable::history::Key, (Vec<kvik_log::reader::FoundFact>, bool)>,
    ) {
        self.index.batch(&b);
        // Versioned facts below the recent window: their keys' earlier facts are read from the log, once per pass.
        let mut keys: Vec<crate::durable::history::Key> = b
            .content
            .items
            .iter()
            .filter_map(|i| match i {
                Item::Fact(r) if r.changed_at.is_some() => Some((r.series_id, r.t)),
                _ => None,
            })
            .filter(|k| self.recent.key(*k).is_none() && !read.contains_key(k))
            .collect();
        keys.sort_unstable();
        keys.dedup();
        if !keys.is_empty() {
            let found = self.read_keys(&keys);
            for k in keys {
                let facts = found.facts.get(&k).cloned().unwrap_or_default();
                read.insert(k, (facts, found.unreadable.contains(&k)));
            }
        }
        let mut fed: Vec<u32> = Vec::new();
        let mut sequence = b.first.sequence;
        let names: Vec<String> = self.consumers.keys().cloned().collect();
        // The consumers with more to take after the batch; an entry may change what any of them takes.
        let mut more = vec![false; names.len()];
        for item in b.content.items {
            let p = LogPos {
                generation: b.first.generation,
                sequence,
            };
            sequence += item.sequences();
            match item {
                Item::Fact(row) => {
                    let key = (row.series_id, row.t);
                    // An earlier fact of this batch may have moved the window's floor past a key it held when the
                    // batch's keys were looked at.
                    if row.changed_at.is_some()
                        && self.recent.key(key).is_none()
                        && !read.contains_key(&key)
                    {
                        let found = self.read_keys(&[key]);
                        let facts = found.facts.get(&key).cloned().unwrap_or_default();
                        read.insert(key, (facts, found.unreadable.contains(&key)));
                    }
                    let (outranked, unchecked) = match row.changed_at {
                        Some(c) => self.outranked(key, p, c, read),
                        None => (false, false),
                    };
                    self.unchecked += u64::from(unchecked);
                    let rec = Arc::new(FactRec {
                        stored: Stored {
                            position: p,
                            received_at: b.content.received_at,
                            row,
                        },
                        outranked,
                        unchecked,
                    });
                    let series = rec.stored.row.series_id;
                    let registry = &self.view.registry;
                    self.recent.note(
                        &rec,
                        || crate::durable::history::span_of(registry, series),
                        ctx.now,
                    );
                    if !outranked {
                        fed.push(series);
                        self.last_arrival.insert(series, b.content.received_at);
                    }
                    if !self.loading {
                        for (name, more) in names.iter().zip(more.iter_mut()) {
                            *more |= self.deliver_live(name, &rec);
                        }
                        self.items.insert(p, Rec::Fact(rec));
                        #[cfg(test)]
                        {
                            self.records += 1;
                        }
                    }
                }
                Item::Entry(e) if matches!(*e, Entry::Ignored { .. }) => {}
                Item::Entry(e) => {
                    let e = *e;
                    self.view.apply(p, &e);
                    if let Entry::Recovery(r) = &e {
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
                    more.fill(true);
                    let routing = matches!(e, Entry::Routing(_));
                    self.entries.insert(p, e.clone());
                    if !self.loading {
                        self.items.insert(p, Rec::Entry(e));
                    }
                    if routing && !self.loading {
                        for name in &names {
                            self.sync_runners(name, ctx.mono);
                        }
                    }
                }
            }
        }
        self.head = self.head.max(b.last);
        if !self.loading {
            for (name, _) in names.iter().zip(&more).filter(|(_, m)| **m) {
                self.changed(name);
            }
            for name in &names {
                self.evaluate(name, Some(&fed), ctx);
            }
        }
    }
}
