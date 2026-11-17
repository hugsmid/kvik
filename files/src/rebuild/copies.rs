//! Before and after a rebuild: what the start does with a corrupt record given the answers of the mirror path and
//! deep, writing a fetched copy back, and deep's answers after a provisional rebuild.

use std::path::Path;
use std::time::Duration;

use kvik_log::fs::{Fs, OpenMode};

use super::write::{WriteError, replace_manifest};
use crate::MANIFEST;
use crate::appender::{AppendError, Appender, Gap, RebuildState};
use crate::catalogue::Catalogue;
use crate::desc::FileDescription;
use crate::image::{Image, record_at};
use crate::record::{Body, Record};

/// The configuration `manifest.rebuild_wait` (`api.md` Configuration).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RebuildWait {
    /// Rebuild provisionally after this long without an answer from deep (default 15 minutes).
    After(Duration),
    /// Stay read-only until deep answers.
    Never,
}

impl Default for RebuildWait {
    fn default() -> Self {
        RebuildWait::After(Duration::from_secs(15 * 60))
    }
}

/// What a copy source answered for a corrupt record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    /// It holds a copy: the record's block.
    Holds(Vec<u8>),
    /// It holds none, or none is configured.
    HoldsNone,
    /// It could not be reached, so nobody can say whether a copy exists.
    Unreachable,
}

/// What the start does next with a corrupt record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Write this copy back ([`repair_with_copy`]).
    Repair(Vec<u8>),
    /// No copy exists: rebuild, final.
    RebuildFinal,
    /// Stay read-only (`manifest_corrupt`), writing nothing, until this monotonic time (`None`: until deep answers);
    /// ask again meanwhile.
    Wait(Option<Duration>),
    /// The wait is over: rebuild provisionally.
    RebuildProvisional,
}

/// A start that found a corrupt manifest record: the wait counted from its first failed ask (`spec.md` §7 Deep
/// unreachable). A restart makes a new one, so the wait starts again.
#[derive(Clone, Copy, Debug)]
pub struct CorruptStart {
    wait: RebuildWait,
    failed_at: Option<Duration>,
}

impl CorruptStart {
    /// A start configured with `wait`.
    pub fn new(wait: RebuildWait) -> Self {
        Self {
            wait,
            failed_at: None,
        }
    }

    /// Decides from the answers of the mirror path and of deep, at monotonic time `now`.
    pub fn decide(&mut self, mirror: &Answer, deep: &Answer, now: Duration) -> Decision {
        match (mirror, deep) {
            (Answer::Holds(c), _) | (_, Answer::Holds(c)) => return Decision::Repair(c.clone()),
            (Answer::HoldsNone, Answer::HoldsNone) => return Decision::RebuildFinal,
            _ => {}
        }
        let since = *self.failed_at.get_or_insert(now);
        match self.wait {
            RebuildWait::Never => Decision::Wait(None),
            RebuildWait::After(d) => {
                let until = since.saturating_add(d);
                if now >= until {
                    Decision::RebuildProvisional
                } else {
                    Decision::Wait(Some(until))
                }
            }
        }
    }
}

/// Why a copy of a corrupt record was not taken.
#[derive(Debug, thiserror::Error)]
pub enum CopyError {
    /// The copy is not a valid record, or its epoch lies outside the corrupt span's.
    #[error("the copy is not the corrupt record")]
    NotTheRecord,
    /// The manifest has no corrupt record.
    #[error("the manifest has no corrupt record")]
    NothingCorrupt,
    /// Writing it failed.
    #[error(transparent)]
    Write(#[from] WriteError),
    /// Appending it failed.
    #[error(transparent)]
    Append(#[from] AppendError),
}

/// The record in `copy` if it is one valid record whose epoch lies inside `gap`.
fn check_copy(copy: &[u8], gap: Gap) -> Result<Record, CopyError> {
    match record_at(copy, 0, gap.after) {
        Ok(Some(p))
            if p.end as usize == copy.len() && gap.before.is_none_or(|b| p.record.epoch < b) =>
        {
            Ok(p.record)
        }
        _ => Err(CopyError::NotTheRecord),
    }
}

/// Writes a verified copy of the first corrupt record back (`spec.md` §7 Manifest). Where it fits in the corrupt
/// bytes it is written in place over their start and synced; bytes of the span it does not fill stay as they are,
/// so another corrupt record there is found by the next open, which fetches it again, rebuilds or waits as for
/// any. A copy longer than the span is written by writing the manifest again with the copy in the span's place, as
/// `MANIFEST.new` renamed over `MANIFEST`. No handle may hold `MANIFEST`.
pub fn repair_with_copy(
    fs: &dyn Fs,
    dir: &Path,
    image: &Image,
    bytes: &[u8],
    copy: &[u8],
) -> Result<(), CopyError> {
    let span = image.corrupt.first().ok_or(CopyError::NothingCorrupt)?;
    check_copy(
        copy,
        Gap {
            after: span.after_epoch,
            before: span.before_epoch,
        },
    )?;
    if copy.len() as u64 <= span.end - span.offset {
        let f = fs
            .open(&dir.join(MANIFEST), OpenMode::ReadWrite)
            .map_err(WriteError::from)?;
        f.write_at(copy, span.offset)
            .and_then(|()| f.sync_data())
            .map_err(WriteError::from)?;
        return Ok(());
    }
    let mut out = bytes[..span.offset as usize].to_vec();
    out.extend_from_slice(copy);
    out.extend_from_slice(&bytes[span.end as usize..]);
    replace_manifest(fs, dir, &out).map_err(WriteError::from)?;
    Ok(())
}

/// What of `body` the catalogue does not state already: a copy of the corrupt record may publish a file that a
/// later valid record retired, or restate what the rebuild reconstructed. An acknowledgement of a rebuild in it is
/// dropped: a lost acknowledgement is given again (`spec.md` §7 Manifest). A record left naming nothing is carried
/// as an empty record with its old epoch.
fn news(c: &Catalogue, body: Body) -> Body {
    let known = |d: &FileDescription| c.file(d.file_ref()).is_some();
    let gone = |d: &FileDescription| c.file(d.file_ref()).is_some_and(|e| !e.is_live());
    match body {
        Body::Publish(mut p) => {
            p.published.retain(|d| !known(d));
            p.retired.retain(|d| !gone(d));
            if p.published.is_empty() && p.retired.is_empty() {
                Body::Empty
            } else {
                Body::Publish(p)
            }
        }
        Body::Sealed(mut s) => {
            s.retain(|d| !known(d));
            if s.is_empty() {
                Body::Empty
            } else {
                Body::Sealed(s)
            }
        }
        Body::Acknowledgement(mut a) => {
            a.rebuild = None;
            if a.losses.is_empty() {
                Body::Empty
            } else {
                Body::Acknowledgement(a)
            }
        }
        other => other,
    }
}

/// The copy sources answer for one corrupt record of a provisional rebuild (`spec.md` §7 Deep unreachable). With a
/// copy from either, the copy is appended, carried with its old epoch, so it replaces what was reconstructed; with
/// both answering that they hold none (or not configured), nothing is written; while either cannot be reached,
/// nothing changes. The rebuild is final once every corrupt record it stands for ([`Appender::pending`]) has its
/// answer. Returns the state after the answer.
pub fn resolve(
    app: &mut Appender,
    gap: Gap,
    mirror: &Answer,
    deep: &Answer,
) -> Result<RebuildState, CopyError> {
    if !matches!(app.rebuild_state(), RebuildState::Provisional { .. })
        || !app.pending().contains(&gap)
    {
        return Ok(app.rebuild_state());
    }
    match (mirror, deep) {
        (Answer::Holds(copy), _) | (_, Answer::Holds(copy)) => {
            let mut r = check_copy(copy, gap)?;
            r.old_epoch = Some(r.old_epoch.unwrap_or(r.epoch));
            r.body = news(app.catalogue(), r.body);
            app.append_record(r)?;
        }
        (Answer::HoldsNone, Answer::HoldsNone) => {}
        _ => return Ok(app.rebuild_state()),
    }
    app.answered(gap);
    Ok(app.rebuild_state())
}
