//! The source records of a restore (`spec.md` §8 Generations, Restore step 1): the generation read, its chain
//! (Q-611), the tail checks (Q-612) and its last-uploaded marker (Q-614).

use std::collections::BTreeSet;

use kvik_files::record::{Record, RecordError};
use kvik_sink_api::{BackupSink, SinkError};

use super::driver::{list_all, open};
use super::{EarlierState, MarkerFallback, RestoreError};
use crate::envelope::Envelope;
use crate::keys::{Keys, ObjectName};
use crate::marker::{self, Marker, ReadError};
use crate::plan::{ChainBreak, PlanError, after_last, check_chain, read_tail, records_to_read};
use crate::uploaded::UploadedRecord;

/// Why no source records were read.
#[derive(Debug)]
pub(super) enum SourceError {
    /// The restore fails.
    Failed(RestoreError),
    /// The marker names `epoch`, which `stat` reports and the listing does not show: the listing lags (Q-614).
    MarkerAhead {
        /// The generation.
        generation: u32,
        /// The record.
        epoch: u64,
    },
}

/// The source records found, with what the marker check reports.
#[derive(Debug)]
pub(super) struct Sourced {
    /// The generation read.
    pub generation: u32,
    /// The records read.
    pub records: Vec<Record>,
    /// The latest marker could not be read and an earlier one was used (Q-614).
    pub fallback: Option<MarkerFallback>,
    /// Records a marker names that the bucket does not hold, accepted (Q-962).
    pub earlier: Option<EarlierState>,
}

/// What a restore accepts that the marker check would refuse.
#[derive(Clone, Copy, Debug)]
pub(super) struct Accept {
    /// A generation with records and no marker (Q-614).
    pub unmarked: bool,
    /// Records a marker names that the bucket does not hold (Q-962).
    pub earlier: bool,
}

/// The marker a generation is checked against, and the records it names that are found missing and accepted.
struct Check<'a> {
    m: &'a Marker,
    accept: bool,
    missing: Vec<u64>,
}

impl<T: Into<RestoreError>> From<T> for SourceError {
    fn from(e: T) -> Self {
        Self::Failed(e.into())
    }
}

/// The records a restore reads (`plan::records_to_read`, Q-108) of the highest generation below `generation` that has
/// any. A generation that lists none counts as having none only if `stat` reports no record at epoch 1 (Q-304) and it
/// has no marker. The records from the first read to the last listed must chain (Q-611), `stat` must report no record
/// after the last listed or, where the records read end before the listing, after the last read (Q-612), and the
/// listing must reach what the generation's latest readable marker names (Q-614); a generation with records and no
/// marker fails unless `accept.unmarked`, and a record a marker names that the bucket does not hold unless
/// `accept.earlier` (Q-962), which reports it.
pub(super) async fn source_records<S: BackupSink, E: Envelope>(
    sink: &S,
    env: &E,
    keys: &Keys,
    generation: u32,
    accept: Accept,
) -> Result<Sourced, SourceError> {
    let mut g = generation;
    let mut earlier = None;
    let mut fallback = None;
    while let Some(below) = g.checked_sub(1) {
        g = below;
        // Q-614: before the listing, so that the records it names were uploaded before the listing began.
        let found = latest_marker(sink, env, keys, g).await?;
        let marker = found.as_ref().map(|(m, _)| *m);
        let mut check = marker.as_ref().map(|m| Check {
            m,
            accept: accept.earlier,
            missing: Vec::new(),
        });
        let mut listed: Vec<(u64, String)> = list_all(sink, &keys.records_prefix(g))
            .await?
            .into_iter()
            .filter_map(|k| match keys.parse(&k) {
                Some(ObjectName::Record { generation, epoch }) if generation == g => {
                    Some((epoch, k))
                }
                _ => None,
            })
            .collect();
        let epochs: BTreeSet<u64> = listed.iter().map(|(e, _)| *e).collect();
        let incomplete = RestoreError::ListingIncomplete { generation: g };
        let Some(last) = epochs.last().copied() else {
            // Q-304: absence from a listing proves nothing; a generation's first record is at epoch 1.
            if sink.stat(&keys.record(g, 1)).await?.is_some() {
                return Err(incomplete.into());
            }
            if let Some(c) = &mut check {
                let e = c.m.summary.epoch;
                reach(sink, keys, c, &epochs, e).await?;
                if earlier.is_none() {
                    earlier = c.report(None);
                    fallback = found.and_then(|(_, f)| f);
                }
            }
            continue;
        };
        match &mut check {
            Some(c) => {
                let e = c.m.summary.epoch;
                reach(sink, keys, c, &epochs, e).await?;
            }
            None if !accept.unmarked => {
                return Err(RestoreError::MarkerMissing { generation: g }.into());
            }
            None => {}
        }
        // Q-612: right after the listing, so that a record uploaded since, or hidden by it, is seen.
        if any_reported(sink, keys, g, after_last(last)).await? {
            return Err(incomplete.into());
        }
        listed.sort();
        let mut uploaded: Vec<UploadedRecord> = Vec::with_capacity(listed.len());
        for (epoch, key) in listed {
            let plain = open(env, &key, &sink.get(&key, None).await?)?;
            let u = UploadedRecord::decode(&plain)?;
            if u.record.epoch != epoch || u.record.generation != g {
                return Err(
                    RecordError::Invalid("an uploaded record under another record's key").into(),
                );
            }
            uploaded.push(u);
        }
        let records: Vec<Record> = uploaded.iter().map(|u| u.record.clone()).collect();
        let read = records_to_read(&records);
        // With nothing read the listing starts with an unfinished rebuilt manifest, checked from its first record.
        let first = read.first().map_or(0, |f| {
            records.iter().position(|r| r.epoch == f.epoch).unwrap_or(0)
        });
        match check_chain(&uploaded[first..]) {
            Ok(()) => {}
            Err(ChainBreak::Missing { previous, .. })
                if reported(sink, keys, g, previous).await? =>
            {
                return Err(incomplete.into());
            }
            Err(
                ChainBreak::Missing { epoch, previous } | ChainBreak::Forked { epoch, previous },
            ) => {
                return Err(RestoreError::BrokenChain {
                    generation: g,
                    epoch,
                    previous,
                }
                .into());
            }
            Err(ChainBreak::Repeated { epoch }) => {
                return Err(SinkError::Failed {
                    reason: format!("a listing of generation {g}'s records repeats epoch {epoch}"),
                }
                .into());
            }
        }
        if any_reported(sink, keys, g, read_tail(&records, read)).await? {
            return Err(incomplete.into());
        }
        if let Some(c) = &mut check {
            check_read(sink, keys, c, &epochs, read).await?;
            if earlier.is_none() {
                earlier = c.report(read.last().map(|r| r.epoch));
            }
        }
        if fallback.is_none() {
            fallback = found.and_then(|(_, f)| f);
        }
        return Ok(Sourced {
            generation: g,
            records: read.to_vec(),
            fallback,
            earlier,
        });
    }
    Err(PlanError::NoRecords.into())
}

/// The latest readable marker of `g`, `None` where it has none (Q-614). One that does not open or decode is passed
/// over for the one before it, reported as a fallback; with none readable the restore fails on the latest. One of a
/// version this build does not read fails the restore: a later build wrote it.
async fn latest_marker<S: BackupSink, E: Envelope>(
    sink: &S,
    env: &E,
    keys: &Keys,
    g: u32,
) -> Result<Option<(Marker, Option<MarkerFallback>)>, SourceError> {
    let latest = marker::latest(sink, keys, g).await?;
    let mut unreadable = Vec::new();
    let mut first_reason = None;
    for sequence in (1..=latest).rev() {
        let reason = match marker::read(sink, env, keys, g, sequence).await {
            Ok(m) => {
                let fallback = (!unreadable.is_empty()).then(|| MarkerFallback {
                    generation: g,
                    unreadable: unreadable.clone(),
                    used: sequence,
                });
                return Ok(Some((m, fallback)));
            }
            Err(ReadError::Version(v)) => {
                return Err(RestoreError::MarkerUnreadable {
                    generation: g,
                    sequence,
                    reason: format!("a marker of version {v}, written by a later build"),
                }
                .into());
            }
            Err(ReadError::Sink(SinkError::NotFound { .. })) => {
                "the bucket no longer holds it".to_owned()
            }
            Err(ReadError::Sink(e)) => return Err(e.into()),
            Err(ReadError::Unreadable(reason)) => reason,
        };
        first_reason.get_or_insert(reason);
        unreadable.push(sequence);
    }
    match first_reason {
        None => Ok(None),
        Some(reason) => Err(RestoreError::MarkerUnreadable {
            generation: g,
            sequence: latest,
            reason,
        }
        .into()),
    }
}

impl Check<'_> {
    /// What was accepted as missing, if anything; `read_end` is the last record read.
    fn report(&self, read_end: Option<u64>) -> Option<EarlierState> {
        (!self.missing.is_empty()).then(|| EarlierState {
            generation: self.m.generation,
            sequence: self.m.sequence,
            missing: self.missing.clone(),
            read_end,
            marked_end: self.m.summary.epoch,
        })
    }
}

/// Record `epoch`, which the marker names, is listed; not listed, `stat` tells a lagging listing from a record the
/// bucket does not hold, which fails the restore unless accepted (Q-962).
async fn reach<S: BackupSink>(
    sink: &S,
    keys: &Keys,
    c: &mut Check<'_>,
    epochs: &BTreeSet<u64>,
    epoch: u64,
) -> Result<(), SourceError> {
    let m = c.m;
    if epochs.contains(&epoch) {
        return Ok(());
    }
    if reported(sink, keys, m.generation, epoch).await? {
        return Err(SourceError::MarkerAhead {
            generation: m.generation,
            epoch,
        });
    }
    if c.accept {
        if !c.missing.contains(&epoch) {
            c.missing.push(epoch);
        }
        return Ok(());
    }
    Err(RestoreError::MarkerUnreached {
        generation: m.generation,
        sequence: m.sequence,
        epoch,
    }
    .into())
}

/// The records read start and end no earlier than the marker's read range (Q-614): records uploaded after the marker
/// can only move what a restore reads later. `want.0` cannot be unlisted once the chain check passed; it is asked as
/// a defence.
async fn check_read<S: BackupSink>(
    sink: &S,
    keys: &Keys,
    c: &mut Check<'_>,
    epochs: &BTreeSet<u64>,
    read: &[Record],
) -> Result<(), SourceError> {
    let Some(want) = c.m.summary.read else {
        return Ok(());
    };
    let have = read
        .first()
        .zip(read.last())
        .map(|(f, l)| (f.epoch, l.epoch));
    if have.is_some_and(|h| h >= want) {
        return Ok(());
    }
    for e in [want.0, want.1] {
        reach(sink, keys, c, epochs, e).await?;
    }
    if c.missing.contains(&want.0) || c.missing.contains(&want.1) {
        return Ok(());
    }
    Err(RestoreError::MarkerMismatch {
        generation: c.m.generation,
        sequence: c.m.sequence,
        marked: want,
        read: have,
    }
    .into())
}

/// Whether `stat` reports record `epoch` of generation `g`; an unanswered `stat` fails the restore (Q-612).
async fn reported<S: BackupSink>(
    sink: &S,
    keys: &Keys,
    g: u32,
    epoch: u64,
) -> Result<bool, RestoreError> {
    Ok(sink.stat(&keys.record(g, epoch)).await?.is_some())
}

/// Whether `stat` reports any of `epochs`, asked in order until one is.
async fn any_reported<S: BackupSink>(
    sink: &S,
    keys: &Keys,
    g: u32,
    epochs: impl IntoIterator<Item = u64>,
) -> Result<bool, RestoreError> {
    for e in epochs {
        if reported(sink, keys, g, e).await? {
            return Ok(true);
        }
    }
    Ok(false)
}
