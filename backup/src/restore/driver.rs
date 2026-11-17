//! [`Restorer`]: the restore driver over a sink and an envelope (`spec.md` §8 Restore, Generations, Restore drill).

use std::collections::BTreeSet;
use std::time::Duration;

use kvik_files::appender::Appender;
use kvik_files::catalogue::Catalogue;
use kvik_files::desc::{FileRef, kind};
use kvik_files::record::{Body, Record};
use kvik_files::restore::{RestoreRecord, RestoredFile};
use kvik_log::manifest::{LossCause, LostRange};
use kvik_log::mark::ResetStamp;
use kvik_log::store::StoreDescriptor;
use kvik_sink_api::{BackupSink, SinkError};
use sha2::{Digest, Sha256};

use super::judge::{FileState, Verdict};
use super::local::{self, Found, invalid};
use super::source::{SourceError, source_records};
use super::{
    MARKER_LISTING_TRIES, Prepared, RestoreConfig, RestoreDriver, RestoreError, RestoreProgress,
    RestoreStep, probe,
};
use crate::envelope::{Envelope, EnvelopeError};
use crate::keys::{ClaimMarker, Keys, ObjectName};
use crate::marker::{self, PutError, Summary};
use crate::plan::{PlanError, PlanFn, PlanInput, PlanMode, RestorePlan};
use crate::uploaded::UploadedRecord;
use crate::{DriverFuture, LocalStore};

fn sha256(b: &[u8]) -> [u8; 32] {
    Sha256::digest(b).into()
}

/// The restore driver (S11). A drill claims nothing, uploads nothing and reads the claims once.
pub struct Restorer<S, E> {
    sink: S,
    envelope: Box<dyn Fn([u8; 16]) -> E + Send>,
    local: LocalStore,
    plan: PlanFn,
    nonce: Box<dyn FnMut() -> u64 + Send>,
    cfg: RestoreConfig,
    run: Option<Run<E>>,
    /// The generation this restorer claimed, kept across failed `prepare` calls (Q-266).
    claimed: Option<Claimed>,
    /// Consecutive `prepare` calls that found the marker ahead of the listing (Q-614).
    marker_ahead: u32,
}

#[derive(Clone, Copy)]
struct Claimed {
    store_id: [u8; 16],
    generation: u32,
    nonce: u64,
}

struct Pending {
    file: RestoredFile,
    done: bool,
    state: FileState,
}

struct Run<E> {
    env: E,
    keys: Keys,
    stamp: ResetStamp,
    appender: Appender,
    wall: i64,
    files: Vec<Pending>,
}

enum Fetched {
    Plain(Vec<u8>),
    Retry,
    Lost,
    Failed(RestoreError),
}

fn verdict(v: Verdict) -> Fetched {
    match v {
        Verdict::Retry(_) => Fetched::Retry,
        Verdict::Lost => Fetched::Lost,
    }
}

pub(super) fn open<E: Envelope>(env: &E, key: &str, bytes: &[u8]) -> Result<Vec<u8>, RestoreError> {
    env.open(key, bytes).map_err(|e| match e {
        EnvelopeError::WrongKey => RestoreError::WrongKey,
        e => RestoreError::Envelope(e),
    })
}

pub(super) async fn list_all<S: BackupSink>(
    sink: &S,
    prefix: &str,
) -> Result<Vec<String>, RestoreError> {
    let mut out: Vec<String> = Vec::new();
    loop {
        let after = out.last().cloned();
        let page = sink.list(prefix, after.as_deref()).await?;
        if page.more && page.keys.is_empty() {
            return Err(SinkError::Failed {
                reason: format!("a listing of {prefix:?} made no progress"),
            }
            .into());
        }
        out.extend(page.keys);
        if !page.more {
            return Ok(out);
        }
    }
}

async fn find_store<S: BackupSink>(sink: &S) -> Result<[u8; 16], RestoreError> {
    let ids: BTreeSet<[u8; 16]> = list_all(sink, "")
        .await?
        .iter()
        .filter_map(|k| Keys::store_id_of(k))
        .collect();
    let mut it = ids.iter();
    match (it.next(), it.next()) {
        (None, _) => Err(RestoreError::NoStore),
        (Some(id), None) => Ok(*id),
        _ => Err(RestoreError::SeveralStores(ids.into_iter().collect())),
    }
}

/// Q-82: the key is judged by `STORE`'s header before any file.
async fn check_key<S: BackupSink, E: Envelope>(
    sink: &S,
    env: &E,
    keys: &Keys,
) -> Result<(), RestoreError> {
    let header = match sink.get(&keys.store(), Some(0..env.header_len())).await {
        Err(SinkError::InvalidRange { .. }) => return Err(EnvelopeError::Truncated.into()),
        r => r?,
    };
    env.check_header(&header).map_err(|e| match e {
        EnvelopeError::WrongKey => RestoreError::WrongKey,
        e => RestoreError::Envelope(e),
    })
}

/// `STORE`'s plaintext, which a restore copies unchanged.
async fn fetch_store<S: BackupSink, E: Envelope>(
    sink: &S,
    env: &E,
    keys: &Keys,
) -> Result<Vec<u8>, RestoreError> {
    let key = keys.store();
    let plain = open(env, &key, &sink.get(&key, None).await?)?;
    let d = StoreDescriptor::decode(&plain).map_err(|e| invalid(format!("{key}: {e}")))?;
    if d.store_id != keys.store_id() {
        return Err(invalid(format!("{key} names another store id")));
    }
    Ok(plain)
}

/// The highest generation claimed at or above `from`, read by `stat` in ascending order (Q-163).
async fn highest_claimed<S: BackupSink>(
    sink: &S,
    keys: &Keys,
    from: u32,
) -> Result<u32, RestoreError> {
    let mut g = from;
    while let Some(next) = g.checked_add(1) {
        if sink.stat(&keys.claim(next)).await?.is_none() {
            break;
        }
        g = next;
    }
    Ok(g)
}

/// Tries of one generation's claim that fail with `HashMismatch` while no marker is stored, before the restore fails.
const CLAIM_TRIES: u32 = 3;

/// Claims the first free generation above `highest` (`spec.md` §8 Generations; Q-161: another marker's content
/// takes the next). A `HashMismatch` takes the next only if `stat` then reports a marker; with none stored the same
/// generation is tried again, so no unclaimed gap is left below the claim (Q-305).
async fn claim<S: BackupSink>(
    sink: &S,
    keys: &Keys,
    highest: u32,
    nonce: u64,
) -> Result<u32, RestoreError> {
    let mut g = highest
        .checked_add(1)
        .ok_or(PlanError::GenerationsExhausted)?;
    let mut tries = 0;
    loop {
        let marker = ClaimMarker {
            generation: g,
            nonce,
        }
        .encode();
        let sha = sha256(&marker);
        let taken = match sink.put(&keys.claim(g), marker.into(), sha).await {
            Ok(_) => return Ok(g),
            Err(SinkError::Exists { .. }) => true,
            Err(e @ SinkError::HashMismatch { .. }) => {
                let stored = sink.stat(&keys.claim(g)).await?.is_some();
                tries += 1;
                if !stored && tries >= CLAIM_TRIES {
                    return Err(e.into());
                }
                stored
            }
            Err(e) => return Err(e.into()),
        };
        if taken {
            g = g.checked_add(1).ok_or(PlanError::GenerationsExhausted)?;
            tries = 0;
        }
    }
}

/// The files of generation `g` in the bucket that no record of `source` names (Q-613).
async fn unnamed<S: BackupSink>(
    sink: &S,
    keys: &Keys,
    g: u32,
    source: &Catalogue,
) -> Result<Vec<FileRef>, RestoreError> {
    let mut out = Vec::new();
    for prefix in [keys.log_prefix(g), keys.files_prefix()] {
        for k in list_all(sink, &prefix).await? {
            if let Some(ObjectName::File { file, generation }) = keys.parse(&k)
                && generation == g
                && source.file(file).is_none()
            {
                out.push(file);
            }
        }
    }
    out.sort();
    Ok(out)
}

fn by_position(files: &[RestoredFile]) -> Vec<RestoredFile> {
    let mut v = files.to_vec();
    v.sort_by_key(|f| (f.description.range.map(|r| r.0), f.description.file_ref()));
    v
}

/// The plan agrees with the input, and fetches exactly the files its record lists (Q-301: a resume derives the
/// files to fetch from the record), each a log segment holding positions (Q-302).
fn check_plan(
    p: &RestorePlan,
    generation: u32,
    nonce: u64,
    source: u32,
    drill: bool,
) -> Result<(), PlanError> {
    let r = &p.record;
    if r.generation != generation
        || r.stamp.nonce != nonce
        || r.source_generation != source
        || r.drill != drill
    {
        return Err(PlanError::Invalid(
            "a plan whose record disagrees with its input",
        ));
    }
    let refs = |fs: &[RestoredFile]| {
        fs.iter()
            .map(|f| f.description.file_ref())
            .collect::<BTreeSet<FileRef>>()
    };
    if refs(&p.fetch) != refs(&r.files) || p.fetch.len() != r.files.len() {
        return Err(PlanError::Invalid(
            "a plan that fetches other files than its record lists",
        ));
    }
    if r.files
        .iter()
        .any(|f| f.description.kind != kind::LOG_SEGMENT || f.description.range.is_none())
    {
        return Err(PlanError::Invalid(
            "a restore installs log segments holding positions only",
        ));
    }
    Ok(())
}

/// The lost range of a listed file the bucket has lost or corrupted (Q-82, Q-302), under the restore record's reset
/// stamp, the one in force (spec §7 Readers).
fn loss_of(f: &RestoredFile, stamp: ResetStamp) -> Option<LostRange> {
    let (first, last) = f.description.range?;
    Some(LostRange {
        identity: first,
        first,
        last,
        file_id: f.description.file_id,
        offset: 0,
        length: 0,
        cause: LossCause::LostInBucket,
        boot_id: [0; 16],
        id_floor: 0,
        time_before: None,
        time_after: None,
        extends: false,
        stamp: Some(stamp),
    })
}

/// One try of one file (Q-82).
async fn fetch<S: BackupSink, E: Envelope>(
    sink: &S,
    env: &E,
    keys: &Keys,
    cfg: &RestoreConfig,
    f: &RestoredFile,
    state: &mut FileState,
    now: Duration,
) -> Fetched {
    let key = keys.file(&f.description);
    match sink.stat(&key).await {
        Err(e) if e.is_transient() => return verdict(state.transient(cfg, now, e.to_string())),
        Err(e) => return Fetched::Failed(e.into()),
        Ok(None) => {
            return match sink.stat(&keys.store()).await {
                Ok(Some(_)) => verdict(state.absent(cfg, now)),
                Ok(None) => verdict(state.transient(
                    cfg,
                    now,
                    "the bucket reports the store descriptor absent too".into(),
                )),
                Err(e) if e.is_transient() => verdict(state.transient(cfg, now, e.to_string())),
                Err(e) => Fetched::Failed(e.into()),
            };
        }
        Ok(Some(s)) if s.size != f.copy.size || s.sha256 != f.copy.sha256 => return Fetched::Lost,
        Ok(Some(_)) => state.present(),
    }
    let bytes = match sink.get(&key, None).await {
        Ok(b) => b,
        Err(SinkError::HashMismatch { .. }) => {
            return verdict(state.bad_download(
                cfg,
                now,
                "the download does not match the reported SHA-256".into(),
            ));
        }
        Err(e) if e.is_transient() || matches!(e, SinkError::NotFound { .. }) => {
            return verdict(state.transient(cfg, now, e.to_string()));
        }
        Err(e) => return Fetched::Failed(e.into()),
    };
    if u64::try_from(bytes.len()).ok() != Some(f.copy.size) || sha256(&bytes) != f.copy.sha256 {
        return verdict(state.bad_download(
            cfg,
            now,
            "the downloaded bytes are not the copy".into(),
        ));
    }
    match env.open(&key, &bytes) {
        Err(EnvelopeError::WrongKey) => Fetched::Failed(RestoreError::WrongKey),
        Err(e) => verdict(state.bad_download(cfg, now, e.to_string())),
        Ok(p) if sha256(&p) != f.description.hash => {
            verdict(state.bad_download(cfg, now, "the plaintext hash is not the file's".into()))
        }
        Ok(p) => Fetched::Plain(p),
    }
}

impl<S: BackupSink, E: Envelope> Restorer<S, E> {
    /// `envelope` makes the cipher once the store id is known; `nonce` supplies fresh random values: the reset
    /// stamp's nonce and the probe's bytes.
    pub fn new(
        sink: S,
        envelope: Box<dyn Fn([u8; 16]) -> E + Send>,
        local: LocalStore,
        plan: PlanFn,
        nonce: Box<dyn FnMut() -> u64 + Send>,
        cfg: RestoreConfig,
    ) -> Self {
        Self {
            sink,
            envelope,
            local,
            plan,
            nonce,
            cfg,
            run: None,
            claimed: None,
            marker_ahead: 0,
        }
    }

    fn drill(&self) -> bool {
        self.cfg.mode == PlanMode::Drill
    }

    async fn prepare_inner(&mut self, wall: i64) -> Result<Prepared, RestoreError> {
        self.run = None;
        match local::inspect(&self.local)? {
            Found::NotEmpty => Err(RestoreError::NotEmpty),
            Found::Leftovers => {
                local::clear_leftovers(&self.local)?;
                self.fresh(wall).await
            }
            Found::Empty => self.fresh(wall).await,
            Found::Resume {
                store_id,
                written,
                record,
                lost,
            } => {
                if record.drill != self.drill() {
                    return Err(invalid("the directory holds a restore of the other mode"));
                }
                let keys = Keys::new(store_id);
                let env = (self.envelope)(store_id);
                check_key(&self.sink, &env, &keys).await?;
                let store = fetch_store(&self.sink, &env, &keys).await?;
                let appender = local::open_appender(&self.local, store_id)?;
                self.finish(
                    keys,
                    env,
                    appender,
                    *written,
                    *record,
                    &lost,
                    &store,
                    wall,
                    true,
                    Vec::new(),
                )
                .await
            }
        }
    }

    async fn fresh(&mut self, wall: i64) -> Result<Prepared, RestoreError> {
        let store_id = find_store(&self.sink).await?;
        let keys = Keys::new(store_id);
        let env = (self.envelope)(store_id);
        check_key(&self.sink, &env, &keys).await?;
        let store = fetch_store(&self.sink, &env, &keys).await?;
        if !self.drill() {
            probe::probe(&self.sink, &env, &keys, &mut *self.nonce).await?;
        }
        let highest = highest_claimed(&self.sink, &keys, 0).await?;
        let mine = self
            .claimed
            .filter(|c| !self.drill() && c.store_id == store_id);
        let (generation, nonce) = match mine {
            // Q-266: a retry after a failure past the claim runs in the generation claimed, unless superseded.
            Some(c) if highest > c.generation => {
                return Err(RestoreError::Superseded { by: highest });
            }
            Some(c) => (c.generation, c.nonce),
            None => {
                let nonce = (self.nonce)();
                let generation = if self.drill() {
                    highest
                        .checked_add(1)
                        .ok_or(PlanError::GenerationsExhausted)?
                } else {
                    let generation = claim(&self.sink, &keys, highest, nonce).await?;
                    self.claimed = Some(Claimed {
                        store_id,
                        generation,
                        nonce,
                    });
                    generation
                };
                (generation, nonce)
            }
        };
        let found = source_records(
            &self.sink,
            &env,
            &keys,
            generation,
            self.cfg.accept_unmarked,
        )
        .await;
        let (source_generation, records) = match found {
            Ok(v) => {
                self.marker_ahead = 0;
                v
            }
            Err(SourceError::Failed(e)) => {
                self.marker_ahead = 0;
                return Err(e);
            }
            Err(SourceError::MarkerAhead { generation, epoch }) => {
                self.marker_ahead = self.marker_ahead.saturating_add(1);
                if self.marker_ahead >= MARKER_LISTING_TRIES {
                    return Err(RestoreError::MarkerNotListed {
                        generation,
                        epoch,
                        tries: self.marker_ahead,
                    });
                }
                return Err(RestoreError::ListingIncomplete { generation });
            }
        };
        let source = Catalogue::replay(&records)
            .map_err(|_| PlanError::Invalid("the records read contradict each other"))?;
        let unnamed = unnamed(&self.sink, &keys, source_generation, &source).await?;
        let plan = (self.plan)(&PlanInput {
            generation,
            nonce,
            source_generation,
            source: &source,
            mode: self.cfg.mode.clone(),
            recovery_after: &[],
        })?;
        check_plan(&plan, generation, nonce, source_generation, self.drill())?;
        let written = Record::new(1, generation, wall, plan.record.to_body());
        local::write_manifest(&self.local, store_id, &written, wall)?;
        let appender = local::open_appender(&self.local, store_id)?;
        self.finish(
            keys,
            env,
            appender,
            written,
            plan.record,
            &[],
            &store,
            wall,
            false,
            unnamed,
        )
        .await
    }

    /// After the record is synced: settle and upload it, read the claims again (Full), copy `STORE`, list what is
    /// left to fetch.
    #[allow(clippy::too_many_arguments)]
    async fn finish(
        &mut self,
        keys: Keys,
        env: E,
        mut appender: Appender,
        written: Record,
        record: RestoreRecord,
        lost: &[LostRange],
        store: &[u8],
        wall: i64,
        resumed: bool,
        unnamed: Vec<FileRef>,
    ) -> Result<Prepared, RestoreError> {
        let g = record.generation;
        // `record.drill` is the configured mode here: `prepare_inner` refuses a directory of the other mode and
        // `check_plan` a plan of it, so a drill never writes to the bucket.
        if !record.drill {
            appender.settle(written.epoch, wall)?;
            let key = keys.record(g, written.epoch);
            // A generation's first record names no predecessor (format A.17, Q-610).
            let plain = UploadedRecord {
                previous: 0,
                record: written.clone(),
            }
            .encode()
            .map_err(|e| RestoreError::Append(e.into()))?;
            let sealed = env.seal(&key, &plain)?;
            let sha = sha256(&sealed);
            self.sink.put(&key, sealed.into(), sha).await?;
            // Q-614: the generation's first marker, after its first record.
            marker::put(&self.sink, &env, &keys, g, 1, Summary::of([&written]))
                .await
                .map_err(|e| match e {
                    PutError::Sink(e) => RestoreError::Sink(e),
                    PutError::Envelope(e) => RestoreError::Envelope(e),
                })?;
            let top = highest_claimed(&self.sink, &keys, g).await?;
            if top > g {
                return Err(RestoreError::Superseded { by: top });
            }
        }
        local::write_store(&self.local, store)?;
        let names = local::installed(&self.local)?;
        let files = by_position(&record.files)
            .into_iter()
            .map(|file| {
                let recorded = loss_of(&file, record.stamp).is_some_and(|l| lost.contains(&l));
                Pending {
                    done: recorded || local::is_installed(&names, &file.description),
                    file,
                    state: FileState::default(),
                }
            })
            .collect();
        self.run = Some(Run {
            env,
            keys: keys.clone(),
            stamp: record.stamp,
            appender,
            wall,
            files,
        });
        Ok(Prepared {
            store_id: keys.store_id(),
            record,
            resumed,
            unnamed,
        })
    }

    async fn step_inner(&mut self, now: Duration) -> RestoreStep {
        let Some(run) = self.run.as_mut() else {
            return RestoreStep::Failed(invalid("step before a successful prepare"));
        };
        let Some(i) = run
            .files
            .iter()
            .position(|p| !p.done && p.state.not_before <= now)
        else {
            return match run
                .files
                .iter()
                .filter(|p| !p.done)
                .min_by_key(|p| p.state.not_before)
            {
                None => RestoreStep::Done,
                Some(p) => RestoreStep::Retry {
                    after: p.state.not_before.saturating_sub(now),
                    file: p.file.description.file_ref(),
                    reason: p.state.reason.clone(),
                },
            };
        };
        let p = &mut run.files[i];
        let file = p.file.description.file_ref();
        let got = fetch(
            &self.sink,
            &run.env,
            &run.keys,
            &self.cfg,
            &p.file,
            &mut p.state,
            now,
        )
        .await;
        match got {
            Fetched::Plain(plain) => match local::install(&self.local, &p.file.description, &plain)
            {
                Ok(()) => {
                    p.done = true;
                    RestoreStep::Installed { file }
                }
                Err(e) => RestoreStep::Failed(e.into()),
            },
            Fetched::Lost => {
                let Some(loss) = loss_of(&p.file, run.stamp) else {
                    return RestoreStep::Failed(invalid(
                        "a listed log segment holding no position",
                    ));
                };
                match run.appender.append(Body::LostRange(loss.clone()), run.wall) {
                    Ok(_) => {
                        p.done = true;
                        RestoreStep::Lost { file, loss }
                    }
                    Err(e) => RestoreStep::Failed(e.into()),
                }
            }
            Fetched::Failed(e) => RestoreStep::Failed(e),
            Fetched::Retry => {
                let reason = p.state.reason.clone();
                let next = run
                    .files
                    .iter()
                    .filter(|p| !p.done)
                    .map(|p| p.state.not_before)
                    .min()
                    .unwrap_or(now);
                RestoreStep::Retry {
                    after: next.saturating_sub(now),
                    file,
                    reason,
                }
            }
        }
    }
}

impl<S: BackupSink, E: Envelope> RestoreDriver for Restorer<S, E> {
    fn prepare<'a>(&'a mut self, wall: i64) -> DriverFuture<'a, Result<Prepared, RestoreError>> {
        Box::pin(self.prepare_inner(wall))
    }

    fn step<'a>(&'a mut self, now: Duration) -> DriverFuture<'a, RestoreStep> {
        Box::pin(self.step_inner(now))
    }

    fn progress(&self) -> RestoreProgress {
        let Some(run) = &self.run else {
            return RestoreProgress::default();
        };
        let mut p = RestoreProgress::default();
        for f in &run.files {
            p.files_total = p.files_total.saturating_add(1);
            p.bytes_total = p.bytes_total.saturating_add(f.file.copy.size);
            if !f.done {
                p.files_left = p.files_left.saturating_add(1);
                p.bytes_left = p.bytes_left.saturating_add(f.file.copy.size);
            }
        }
        p
    }
}
