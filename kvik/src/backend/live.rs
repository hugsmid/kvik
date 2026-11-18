//! Live reads on the model backend, the oracle of the durable session (roadmap 9.0d): each batch computes everything
//! again at the model's head, every `Latest` member judged and every entry above a `Tail`'s position replayed in
//! order with the registry it builds (Q-895). The model has no resets and no losses, so it sends neither; it has no
//! timing, so a batch comes whenever something changed.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use km::engine::EntryBody;
use km::registry::Registry;
use kvik_model as km;

use super::convert::{fact_out, mpos, pos, sid};
use super::{Error, Shared, Slot};
use crate::error::QueryError;
use crate::read::{
    Answer, Appended, Live, LiveEvent, LiveId, Part, Query, Selection, UpdateBatch, Updated,
    event_rank,
};
use crate::series::SeriesKey;
use crate::types::{Position, SeriesId};

#[derive(Debug)]
enum Read {
    Latest {
        sel: Selection,
        shown: Option<Vec<(SeriesId, Live)>>,
    },
    Tail {
        sel: Selection,
        /// The position delivered through and the registry as of it, once opened.
        at: Option<(Position, Registry)>,
    },
}

/// See the module documentation.
#[derive(Debug)]
pub(crate) struct Session {
    shared: Arc<Shared>,
    reads: BTreeMap<LiveId, Read>,
    last_id: u64,
}

fn known(registry: &Registry) -> Vec<(SeriesId, Vec<String>, bool)> {
    registry
        .all()
        .map(|d| (sid(d.id), d.names.clone(), d.retired))
        .collect()
}

fn resolve(sel: &Selection, known: &[(SeriesId, Vec<String>, bool)]) -> Vec<SeriesId> {
    let known: Vec<(SeriesId, &[String], bool)> = known
        .iter()
        .map(|(id, n, r)| (*id, n.as_slice(), *r))
        .collect();
    sel.resolve(&known)
}

impl Session {
    pub(crate) fn new(shared: Arc<Shared>) -> Self {
        Session {
            shared,
            reads: BTreeMap::new(),
            last_id: 0,
        }
    }

    pub(crate) fn open(&mut self, q: Query) -> Result<LiveId, QueryError> {
        crate::store::live_check(&q)?;
        self.last_id += 1;
        let id = LiveId(self.last_id);
        let read = match q {
            Query::Latest(sel) => Read::Latest { sel, shown: None },
            Query::Tail { sel } => Read::Tail { sel, at: None },
        };
        self.reads.insert(id, read);
        Ok(id)
    }

    pub(crate) fn cancel(&mut self, id: LiveId) -> bool {
        self.reads.remove(&id).is_some()
    }

    pub(crate) fn next(&mut self, timeout: Duration) -> Result<Option<UpdateBatch>, QueryError> {
        let until = Instant::now() + timeout;
        loop {
            let shared = self.shared.clone();
            let b = shared.with(|s| self.batch(s)).map_err(Error::query)?;
            if b.is_some() || Instant::now() >= until {
                return Ok(b);
            }
            std::thread::sleep(Duration::from_millis(10).min(until - Instant::now()));
        }
    }

    fn batch(&mut self, s: &mut Slot) -> super::Result<Option<UpdateBatch>> {
        let durable = s.refresh()?.durable;
        let now = s.now();
        let registry = s.model.registry().map_err(super::convert::error)?.clone();
        let known = known(&registry);
        let entries = s.model.entries().map_err(super::convert::error)?;
        let mut events = Vec::new();
        for (id, read) in self.reads.iter_mut() {
            let mut mine = Vec::new();
            match read {
                Read::Latest { sel, shown } => {
                    let members = resolve(sel, &known);
                    let mut values = Vec::new();
                    for m in &members {
                        values.push((*m, s.live(*m)?));
                    }
                    match shown {
                        None => mine.push(LiveEvent::Opened(Answer::Latest(values.clone()))),
                        Some(old) => {
                            for (k, _) in old.iter().filter(|(k, _)| !members.contains(k)) {
                                mine.push(LiveEvent::Left { series: *k });
                            }
                            for (index, (k, live)) in values.iter().enumerate() {
                                match old.iter().find(|(o, _)| o == k) {
                                    None => mine.push(LiveEvent::Joined {
                                        series: *k,
                                        info: s.series(SeriesKey::Id(*k))?,
                                        index,
                                        part: Part::Live(live.clone()),
                                    }),
                                    Some((_, was)) if was != live => {
                                        mine.push(LiveEvent::Updated(Updated::Live {
                                            series: *k,
                                            live: live.clone(),
                                        }))
                                    }
                                    Some(_) => {}
                                }
                            }
                        }
                    }
                    *shown = Some(values);
                }
                Read::Tail { sel, at } => match at {
                    None => {
                        let mut r = Registry::default();
                        for e in entries.iter().filter(|e| pos(e.position) <= durable) {
                            if let EntryBody::Registry(ev) = &e.body {
                                r.apply(ev);
                            }
                        }
                        *at = Some((durable, r));
                        mine.push(LiveEvent::Opened(Answer::Tail));
                    }
                    Some((from, r)) => {
                        let mut facts = Vec::new();
                        let above = mpos(*from);
                        for e in entries
                            .iter()
                            .filter(|e| e.position > above && pos(e.position) <= durable)
                        {
                            match &e.body {
                                EntryBody::Registry(ev) => r.apply(ev),
                                EntryBody::Fact(f) => {
                                    let taken = r.get(f.series).is_some_and(|d| {
                                        sel.selects(sid(d.id), &d.names, d.retired)
                                    });
                                    if taken {
                                        facts.push(fact_out(f, s.specs.get(&sid(f.series))));
                                    }
                                }
                                _ => {}
                            }
                        }
                        *from = durable;
                        if !facts.is_empty() {
                            mine.push(LiveEvent::Appended(Appended::Facts(facts)));
                        }
                    }
                },
            }
            mine.sort_by_key(event_rank);
            events.extend(mine.into_iter().map(|e| (*id, e)));
        }
        Ok((!events.is_empty()).then_some(UpdateBatch {
            durable,
            now,
            events,
        }))
    }
}
