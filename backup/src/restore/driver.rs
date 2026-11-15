//! [`Restorer`]: the restore driver over a sink and an envelope (`spec.md` §8 Restore, Generations, Restore drill).

use std::collections::BTreeSet;
use std::time::Duration;

use kvik_files::appender::Appender;
use kvik_files::catalogue::Catalogue;
use kvik_files::desc::{FileRef, kind};
use kvik_files::record::{Body, Record};
use kvik_files::restore::{RestoreRecord, RestoredFile};
use kvik_log::manifest::{LossCause, LostRange};
use kvik_log::store::StoreDescriptor;
use kvik_sink_api::{BackupSink, SinkError};
use sha2::{Digest, Sha256};

use super::judge::{FileState, Verdict};
use super::local::{self, Found, invalid};
use super::{
    Prepared, RestoreConfig, RestoreDriver, RestoreError, RestoreProgress, RestoreStep, codec,
};
use crate::envelope::{Envelope, EnvelopeError};
use crate::keys::{ClaimMarker, Keys, ObjectName};
use crate::plan::{PlanError, PlanFn, PlanInput, PlanMode, RestorePlan};
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
}

struct Pending {
    file: RestoredFile,
    done: bool,
    state: FileState,
}

struct Run<E> {
    env: E,
    keys: Keys,
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

fn open<E: Envelope>(env: &E, key: &str, bytes: &[u8]) -> Result<Vec<u8>, RestoreError> {
    env.open(key, bytes).map_err(|e| match e {
        EnvelopeError::WrongKey => RestoreError::WrongKey,
        e => RestoreError::Envelope(e),
    })
}

async fn list_all<S: BackupSink>(sink: &S, prefix: &str) -> Result<Vec<String>, RestoreError> {
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

/// Claims the first free generation above `highest` (`spec.md` §8 Generations; Q-161: another marker's content
/// takes the next).
async fn claim<S: BackupSink>(
    sink: &S,
    keys: &Keys,
    highest: u32,
    nonce: u64,
) -> Result<u32, RestoreError> {
    let mut g = highest
        .checked_add(1)
        .ok_or(PlanError::GenerationsExhausted)?;
    loop {
        let marker = ClaimMarker {
            generation: g,
            nonce,
        }
        .encode();
        let sha = sha256(&marker);
        match sink.put(&keys.claim(g), marker.into(), sha).await {
            Ok(_) => return Ok(g),
            Err(SinkError::Exists { .. } | SinkError::HashMismatch { .. }) => {
                g = g.checked_add(1).ok_or(PlanError::GenerationsExhausted)?;
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// The records of the highest generation below `generation` that has any, from its last rebuild record on (Q-108).
async fn source_records<S: BackupSink, E: Envelope>(
    sink: &S,
    env: &E,
    keys: &Keys,
    generation: u32,
) -> Result<(u32, Vec<Record>), RestoreError> {
    let mut g = generation;
    while let Some(below) = g.checked_sub(1) {
        g = below;
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
        if listed.is_empty() {
            continue;
        }
        listed.sort();
        let mut out: Vec<Record> = Vec::with_capacity(listed.len());
        for (epoch, key) in listed {
            let plain = open(env, &key, &sink.get(&key, None).await?)?;
            let r = codec::decode_block(&plain)?;
            if r.epoch != epoch {
                return Err(RestoreError::Record(
                    kvik_files::record::RecordError::Invalid(
                        "an uploaded record under another epoch's key",
                    ),
                ));
            }
            out.push(r);
        }
        if let Some(i) = out.iter().rposition(|r| matches!(r.body, Body::Rebuild(_))) {
            out.drain(..i);
        }
        return Ok((g, out));
    }
    Err(PlanError::NoRecords.into())
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

/// The lost range of a listed file the bucket has lost or corrupted (Q-82, Q-302).
fn loss_of(f: &RestoredFile) -> Option<LostRange> {
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
        stamp: None,
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
    /// `envelope` makes the cipher once the store id is known; `nonce` supplies the reset stamp's.
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
                    keys, env, appender, *written, *record, &lost, &store, wall, true,
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
        let highest = highest_claimed(&self.sink, &keys, 0).await?;
        let nonce = (self.nonce)();
        let generation = if self.drill() {
            highest
                .checked_add(1)
                .ok_or(PlanError::GenerationsExhausted)?
        } else {
            claim(&self.sink, &keys, highest, nonce).await?
        };
        let (source_generation, records) =
            source_records(&self.sink, &env, &keys, generation).await?;
        let source = Catalogue::replay(&records)
            .map_err(|_| PlanError::Invalid("the records read contradict each other"))?;
        let plan = (self.plan)(&PlanInput {
            generation,
            nonce,
            source_generation,
            source: &source,
            mode: self.cfg.mode.clone(),
            recovery_after: &[],
        })?;
        check_plan(&plan, generation, nonce, source_generation, self.drill())?;
        let written = Record::new(1, generation, wall, codec::to_body(&plan.record));
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
    ) -> Result<Prepared, RestoreError> {
        let g = record.generation;
        // A drill never writes to the bucket, whatever the directory holds.
        if !record.drill && !self.drill() {
            appender.settle(written.epoch, wall)?;
            let key = keys.record(g, written.epoch);
            let sealed = env.seal(
                &key,
                &written
                    .encode()
                    .map_err(|e| RestoreError::Append(e.into()))?,
            )?;
            let sha = sha256(&sealed);
            self.sink.put(&key, sealed.into(), sha).await?;
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
                let recorded = loss_of(&file).is_some_and(|l| lost.contains(&l));
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
            appender,
            wall,
            files,
        });
        Ok(Prepared {
            store_id: keys.store_id(),
            record,
            resumed,
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
                let Some(loss) = loss_of(&p.file) else {
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
