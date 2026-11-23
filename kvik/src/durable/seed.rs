//! The seed of a seeded start (`spec.md` §3 Order of a start, `format.md` A.16, A.18): the newest series directory
//! that is still a function of the kept log and carries the start counts, with the registry file's entries up to its
//! `through`. A store without one, or written before the start counts, starts from the whole log.

use std::path::Path;

use kvik_log::directory::series::SeriesEntry;
use kvik_log::entries::{Entry, Recovery};
use kvik_log::fs::Fs;
use kvik_log::registry::RegistryFile;
use kvik_log::writer::{LogWriter, Position as LogPos};

#[cfg(test)]
thread_local! {
    /// Set by tests: a start on this thread reads the whole log.
    pub(crate) static WHOLE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// What the last seeded start on this thread met, for tests that measure their reach.
    pub(crate) static PROBE: std::cell::Cell<Probe> = const { std::cell::Cell::new(Probe::NONE) };
}

/// What a seeded start met: why it gave way, if it did, and the recovery entries at or below its seed that name a
/// consumer.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Probe {
    pub(crate) gave_way: Option<crate::durable::start::GiveWay>,
    pub(crate) resume_failed: bool,
    pub(crate) recoveries_naming: usize,
}

#[cfg(test)]
impl Probe {
    const NONE: Probe = Probe {
        gave_way: None,
        resume_failed: false,
        recoveries_naming: 0,
    };
}

#[cfg(test)]
pub(crate) fn probe(f: impl FnOnce(&mut Probe)) {
    let mut p = PROBE.get();
    f(&mut p);
    PROBE.set(p);
}

/// See the module documentation.
#[derive(Debug)]
pub(crate) struct Seed {
    /// Every batch up to here is described by the files.
    pub(crate) through: LogPos,
    /// The series directory's entries, each with its start counts.
    pub(crate) series: Vec<SeriesEntry>,
    /// When the directory's candidates are judged (A.18 `judged_at`, at least its write time).
    pub(crate) judged_at: i64,
    registry: RegistryFile,
}

impl Seed {
    /// The seed the files give a start whose recovery `writer` ran, if any.
    pub(crate) fn find(
        writer: &LogWriter,
        fs: &dyn Fs,
        dir: &Path,
        store_id: &[u8; 16],
    ) -> Option<Seed> {
        #[cfg(test)]
        if WHOLE.get() {
            return None;
        }
        let start = writer.directory_start();
        let (frame, d) = kvik_log::writer::kept_series(fs, dir, store_id, &start)?;
        let durable = writer.stats().durable;
        let usable = d.through.generation == durable.generation
            && d.through.sequence > 0
            && d.through <= durable
            // A series whose latest `t` is the greatest has no floor above it (Q-921).
            && d.entries.iter().all(|e| e.counts.is_some() && e.latest_t != Some(i64::MAX));
        if !usable {
            return None;
        }
        let (_, registry) = kvik_log::registry::read_latest(fs, dir, store_id).ok()??;
        // Q-1001 keeps the file at or above every directory's `through`; a seeded start trusts it up to there.
        if registry.generation != d.through.generation || registry.through < d.through.sequence {
            return None;
        }
        Some(Seed {
            through: d.through,
            judged_at: d.judged_at.max(frame.written_at),
            series: d.entries,
            registry,
        })
    }

    /// The registry entries at or below the seed, with their batches' receive times; A.16 holds every one (Q-1001).
    pub(crate) fn entries(&self) -> impl Iterator<Item = (LogPos, i64, &Entry)> + '_ {
        let generation = self.registry.generation;
        self.registry
            .entries
            .iter()
            .take_while(|(s, _, _)| *s <= self.through.sequence)
            .map(move |(sequence, t, e)| {
                (
                    LogPos {
                        generation,
                        sequence: *sequence,
                    },
                    *t,
                    e,
                )
            })
    }

    /// The recovery entries at or below the seed.
    pub(crate) fn recoveries(&self) -> impl Iterator<Item = (LogPos, Recovery)> + '_ {
        self.entries().filter_map(|(p, _, e)| match e {
            Entry::Recovery(r) => Some((p, r.clone())),
            _ => None,
        })
    }

    /// Where the pass over the log above the seed starts its reader: after the position before the seed, so that
    /// the batch ending at the seed is read first, for its commit time.
    pub(crate) fn reader_after(&self) -> LogPos {
        LogPos {
            generation: self.through.generation,
            sequence: self.through.sequence - 1,
        }
    }
}

#[cfg(test)]
mod tests;
