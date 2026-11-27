use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use kvik_files::appender::{Appender, Opened};
use kvik_files::compaction::{Judgement, compacted_through};
use kvik_files::desc::{FileClass, FileDescription, FileRef, kind};
use kvik_files::rebuild::{LocalFile, Place, RebuildInput, Verdict, Verifier, plan};
use kvik_files::record::{Body, Record};
use kvik_log::facts::Value;
use kvik_log::fs::{Fs, MemFs, OpenMode};
use kvik_log::manifest::encode_initial;
use kvik_log::name::SegmentName;
use kvik_log::setaside::SetAside;
use kvik_log::store::{LOG_DIR, MANIFEST};
use sha2::{Digest, Sha256};

use super::super::Output;
use super::super::plan::{Candidate, candidates};
use super::super::publish::{
    CleanUp, PublishError, Recompaction, clean_data, final_path, layout, publish,
};
use super::super::run::{DATA_DIR, RunInput, run};
use super::{DAY, STORE, TestBatch, batch, reading, segment_bytes};

const T: i64 = 1_000;

fn dir() -> PathBuf {
    PathBuf::from("store")
}

fn put(fs: &MemFs, path: &Path, bytes: &[u8]) {
    let f = fs.open(path, OpenMode::CreateNew).unwrap();
    f.write_at(bytes, 0).unwrap();
    f.sync_all().unwrap();
}

/// Two sealed segments of generation 0 listed by the manifest; the second holds a fact on another day.
fn store() -> (MemFs, Vec<FileDescription>) {
    let fs = MemFs::new();
    let d = dir();
    fs.create_dir_all(&d.join(LOG_DIR)).unwrap();
    fs.create_dir_all(&d.join(DATA_DIR)).unwrap();
    let segs: [(u64, Vec<TestBatch>); 2] = [
        (
            1,
            vec![batch(
                1,
                3 * DAY,
                vec![
                    reading(1, 2 * DAY, Value::F64(1)),
                    reading(2, 2 * DAY + 5, Value::F64(2)),
                ],
            )],
        ),
        (
            2,
            vec![batch(
                3,
                3 * DAY + 1,
                vec![
                    reading(1, 2 * DAY, Value::F64(3)),
                    reading(1, DAY + 7, Value::F64(4)),
                ],
            )],
        ),
    ];
    let mut descs = Vec::new();
    for (id, batches) in &segs {
        let bytes = segment_bytes(0, *id, batches);
        let name = SegmentName {
            generation: 0,
            file_id: *id,
            sealed: true,
        };
        put(&fs, &d.join(LOG_DIR).join(name.to_string()), &bytes);
        let first = batches[0].first;
        let last = first + batches[0].items.len() as u64 - 1;
        descs.push(FileDescription {
            kind: kind::LOG_SEGMENT,
            class: FileClass::None,
            generation: 0,
            file_id: *id,
            range: Some(((0, first), (0, last))),
            hash: Sha256::digest(&bytes).into(),
            inputs: Vec::new(),
        });
    }
    let mut m = encode_initial(STORE, 0);
    m.extend(
        Record::new(2, 0, T, Body::Sealed(descs.clone()))
            .encode()
            .unwrap(),
    );
    put(&fs, &d.join(MANIFEST), &m);
    fs.sync_dir(&d.join(LOG_DIR)).unwrap();
    fs.sync_dir(&d).unwrap();
    (fs, descs)
}

fn run_input<'a>(
    fs: &'a MemFs,
    d: &'a Path,
    sa: &'a SetAside,
    weeks: &'a BTreeSet<i64>,
) -> RunInput<'a> {
    RunInput {
        fs,
        dir: d,
        store_id: STORE,
        generation: 0,
        first_file_id: 100,
        created_at: 5,
        set_aside: sa,
        weeks_merged: weeks,
        registry: &|_| None,
    }
}

fn compact(fs: &MemFs, weeks: &BTreeSet<i64>) -> Vec<Output> {
    let d = dir();
    let sa = SetAside::new(vec![]);
    let segs: Vec<Candidate> = candidates(fs, &d, &STORE).unwrap();
    run(&run_input(fs, &d, &sa, weeks), &segs).unwrap()
}

fn open(fs: &MemFs) -> Appender {
    match Appender::open(fs, &dir(), STORE, 0).unwrap() {
        Opened::Ready(a) => *a,
        Opened::Corrupt(_) => panic!("a corrupt record"),
    }
}

fn data_names(fs: &MemFs) -> BTreeSet<String> {
    fs.read_dir(&dir().join(DATA_DIR))
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect()
}

fn log_bytes(fs: &MemFs) -> BTreeMap<PathBuf, Vec<u8>> {
    fs.paths()
        .into_iter()
        .filter(|p| p.starts_with(dir().join(LOG_DIR)))
        .map(|p| {
            let b = fs.read(&p).unwrap();
            (p, b)
        })
        .collect()
}

#[derive(Clone, Copy, Debug)]
enum Stop {
    Process,
    Power,
    Partial(u64),
}

/// After a stop: the outputs are all live under their final names with their bytes, or none is named and none is
/// left; the log is untouched; C(0) is the inputs' end or absent.
fn check_after_start(
    fs: &MemFs,
    outputs: &[(Output, Vec<u8>)],
    log: &BTreeMap<PathBuf, Vec<u8>>,
) -> bool {
    let m = open(fs);
    let clean = clean_data(fs, &dir(), m.catalogue(), true, &BTreeSet::new()).unwrap();
    let named: Vec<bool> = outputs
        .iter()
        .map(|(o, _)| {
            m.catalogue()
                .file(FileRef {
                    kind: kind::COMPACTED,
                    file_id: o.id.file_id,
                })
                .is_some()
        })
        .collect();
    assert!(
        named.iter().all(|n| *n == named[0]),
        "outputs published apart"
    );
    let published = named[0];
    let names = data_names(fs);
    if published {
        let want: BTreeSet<String> = outputs
            .iter()
            .map(|(o, _)| {
                final_path(&dir(), o.id.generation, o.id.file_id)
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(names, want, "a stray file or a missing output");
        for (o, bytes) in outputs {
            assert_eq!(
                &fs.read(&final_path(&dir(), o.id.generation, o.id.file_id))
                    .unwrap(),
                bytes
            );
        }
        assert_eq!(compacted_through(m.catalogue()), BTreeMap::from([(0, 4)]));
        assert!(clean.removed.is_empty());
    } else {
        assert!(names.is_empty(), "left behind: {names:?}");
        assert!(compacted_through(m.catalogue()).is_empty());
    }
    assert_eq!(m.compaction(), &Judgement::Holds);
    assert_eq!(&log_bytes(fs), log, "a log segment changed");
    published
}

fn stop_at_every_operation(stop: Stop) {
    let mut n = 0;
    let mut seen_unpublished = false;
    loop {
        let (fs, _) = store();
        let outputs: Vec<(Output, Vec<u8>)> = compact(&fs, &BTreeSet::new())
            .into_iter()
            .map(|o| {
                let b = fs.read(&o.path).unwrap();
                (o, b)
            })
            .collect();
        assert_eq!(outputs.len(), 3);
        let log = log_bytes(&fs);
        let mut m = open(&fs);
        fs.crash_after(n);
        let only: Vec<Output> = outputs.iter().map(|(o, _)| o.clone()).collect();
        let r = publish(&fs, &dir(), &mut m, &only, T);
        let stopped = fs.crashed();
        drop(m);
        match stop {
            Stop::Process => fs.restart(),
            Stop::Power => {
                fs.restart();
                fs.power_loss();
            }
            Stop::Partial(seed) => {
                fs.restart();
                fs.power_loss_partial(seed.wrapping_add(n as u64));
            }
        }
        let published = check_after_start(&fs, &outputs, &log);
        if r.is_ok() {
            assert!(
                published,
                "a publish that returned is lost after {stop:?} at {n}"
            );
        }
        if !published {
            seen_unpublished = true;
            // The job is planned again and publishes.
            let again = compact(&fs, &BTreeSet::new());
            let mut m = open(&fs);
            publish(&fs, &dir(), &mut m, &again, T).unwrap();
            drop(m);
            assert!(check_after_start(&fs, &outputs, &log));
        }
        if !stopped {
            assert!(r.is_ok(), "{r:?}");
            break;
        }
        n += 1;
    }
    assert!(
        seen_unpublished && n > 3,
        "the loop never stopped inside publish"
    );
}

#[test]
fn a_process_stop_at_every_publish_operation_leaves_all_outputs_live_or_none_and_no_stray_file() {
    stop_at_every_operation(Stop::Process);
}

#[test]
fn a_power_loss_at_every_publish_operation_leaves_all_outputs_live_or_none_and_no_stray_file() {
    stop_at_every_operation(Stop::Power);
}

#[test]
fn a_partial_power_loss_at_every_publish_operation_leaves_all_outputs_live_or_none() {
    for seed in [1, 7, 42] {
        stop_at_every_operation(Stop::Partial(seed));
    }
}

#[test]
fn a_refused_record_removes_the_renamed_outputs() {
    let (fs, _) = store();
    let outs = compact(&fs, &BTreeSet::new());
    let mut m = open(&fs);
    publish(&fs, &dir(), &mut m, &outs, T).unwrap();
    // A second run over the same segments breaks rule 1 and is refused before anything is written.
    let again = run(
        &RunInput {
            first_file_id: 200,
            ..run_input(&fs, &dir(), &SetAside::new(vec![]), &BTreeSet::new())
        },
        &candidates(&fs, &dir(), &STORE).unwrap(),
    )
    .unwrap();
    let e = publish(&fs, &dir(), &mut m, &again, T).unwrap_err();
    assert!(matches!(e, PublishError::Refused(_)), "{e:?}");
    assert_eq!(data_names(&fs).len(), outs.len());
}

#[test]
fn a_failed_rename_removes_every_output_under_either_name() {
    let (fs, _) = store();
    let outs = compact(&fs, &BTreeSet::new());
    let mut m = open(&fs);
    fs.fail_nth(
        kvik_log::fs::FaultOp::Rename,
        ".kvc",
        1,
        std::io::ErrorKind::Other,
    );
    let e = publish(&fs, &dir(), &mut m, &outs, T).unwrap_err();
    assert!(matches!(e, PublishError::Io(_)), "{e:?}");
    assert!(data_names(&fs).is_empty());
    assert_eq!(
        m.catalogue()
            .files()
            .filter(|f| f.description.kind == kind::COMPACTED)
            .count(),
        0
    );
}

#[test]
fn the_start_removes_temporaries_and_unnamed_outputs_only_when_the_manifest_is_known_valid() {
    let (fs, _) = store();
    let outs = compact(&fs, &BTreeSet::new());
    for o in &outs {
        fs.rename(&o.path, &final_path(&dir(), o.id.generation, o.id.file_id))
            .unwrap();
    }
    let data = dir().join(DATA_DIR);
    for name in [
        "00000000-0000000000000099.kvc.tmp",
        "x.spill",
        "00000000-0000000000000001.kvi.tmp",
    ] {
        put(&fs, &data.join(name), b"x");
    }
    let m = open(&fs);
    let held = BTreeSet::from([FileRef {
        kind: kind::COMPACTED,
        file_id: outs[0].id.file_id,
    }]);
    let c = clean_data(&fs, &dir(), m.catalogue(), false, &BTreeSet::new()).unwrap();
    assert_eq!(c.removed.len(), 3);
    assert_eq!(c.kept.len(), outs.len());
    let c = clean_data(&fs, &dir(), m.catalogue(), true, &held).unwrap();
    assert_eq!(
        c,
        CleanUp {
            removed: c.removed.clone(),
            kept: vec![final_path(&dir(), 0, outs[0].id.file_id)]
        }
    );
    assert_eq!(c.removed.len(), outs.len() - 1);
}

/// The local files a rebuild finds: the segments and the outputs under their final names, each with its layout.
fn local_files(fs: &MemFs, segs: &[FileDescription], outs: &[Output]) -> Vec<LocalFile> {
    let mut files: Vec<LocalFile> = segs
        .iter()
        .map(|d| LocalFile {
            description: d.clone(),
            place: Place::Hot,
            verified: true,
            layout: None,
        })
        .collect();
    for o in outs {
        let _ = fs;
        files.push(LocalFile {
            description: super::super::publish::description(o),
            place: Place::Hot,
            verified: true,
            layout: Some(layout(&o.layout)),
        });
    }
    files
}

fn finalise(fs: &MemFs, outs: &[Output]) {
    for o in outs {
        fs.rename(&o.path, &final_path(&dir(), o.id.generation, o.id.file_id))
            .unwrap();
    }
}

fn verdict(fs: &MemFs, files: &[LocalFile], outs: &[&Output], weeks: &BTreeSet<i64>) -> Verdict {
    let sa = SetAside::new(vec![]);
    let v = Recompaction {
        fs,
        dir: &dir(),
        store_id: STORE,
        set_aside: &sa,
        weeks_merged: weeks,
        registry: &|_| None,
    };
    let want: BTreeSet<u64> = outs.iter().map(|o| o.id.file_id).collect();
    let found: Vec<&LocalFile> = files
        .iter()
        .filter(|f| f.description.kind == kind::COMPACTED && want.contains(&f.description.file_id))
        .collect();
    v.verify(&found, &[])
}

#[test]
fn a_set_of_outputs_is_judged_against_its_inputs_by_compacting_them_again() {
    let (fs, segs) = store();
    let outs = compact(&fs, &BTreeSet::new());
    finalise(&fs, &outs);
    let files = local_files(&fs, &segs, &outs);
    let all: Vec<&Output> = outs.iter().collect();
    assert_eq!(verdict(&fs, &files, &all, &BTreeSet::new()), Verdict::Holds);
    assert_eq!(
        verdict(&fs, &files, &all[1..], &BTreeSet::new()),
        Verdict::CannotSay
    );
    // Compaction now puts these rows in a week file: the outputs found are not what their inputs make.
    let week = BTreeSet::from([0]);
    assert_eq!(verdict(&fs, &files, &all, &week), Verdict::Fails);
    // Recompaction leaves nothing behind.
    assert_eq!(data_names(&fs).len(), outs.len());
    fs.remove_file(&segs_path(&segs[1])).unwrap();
    assert_eq!(
        verdict(&fs, &files, &all, &BTreeSet::new()),
        Verdict::CannotSay
    );
}

fn segs_path(d: &FileDescription) -> PathBuf {
    dir().join(LOG_DIR).join(
        SegmentName {
            generation: 0,
            file_id: d.file_id,
            sealed: true,
        }
        .to_string(),
    )
}

/// Publishes the run, appends an empty record and corrupts the publish record.
fn published_then_corrupt(fs: &MemFs) -> (Vec<Output>, kvik_files::image::Image) {
    let outs = compact(fs, &BTreeSet::new());
    let mut m = open(fs);
    publish(fs, &dir(), &mut m, &outs, T).unwrap();
    m.append(Body::Empty, T).unwrap();
    drop(m);
    let path = dir().join(MANIFEST);
    let mut bytes = fs.read(&path).unwrap();
    let image = kvik_files::image::read(&bytes, &STORE).unwrap();
    let rec = image
        .records
        .iter()
        .find(|r| matches!(r.record.body, Body::Publish(_)))
        .unwrap();
    let at = rec.offset as usize + 24;
    bytes[at] ^= 1;
    fs.remove_file(&path).unwrap();
    put(fs, &path, &bytes);
    let Opened::Corrupt(img) = Appender::open(fs, &dir(), STORE, 0).unwrap() else {
        panic!("not corrupt")
    };
    (outs, *img)
}

#[test]
fn a_corrupt_run_record_after_a_crash_keeps_the_outputs_and_the_rebuild_publishes_them_again() {
    let (fs, segs) = store();
    let (outs, img) = published_then_corrupt(&fs);
    // The start does not know the manifest valid: nothing unnamed is removed.
    let empty = kvik_files::catalogue::Catalogue::new();
    let c = clean_data(&fs, &dir(), &empty, false, &BTreeSet::new()).unwrap();
    assert_eq!(c.kept.len(), outs.len());
    let files = local_files(&fs, &segs, &outs);
    let sa = SetAside::new(vec![]);
    let weeks = BTreeSet::new();
    let v = Recompaction {
        fs: &fs,
        dir: &dir(),
        store_id: STORE,
        set_aside: &sa,
        weeks_merged: &weeks,
        registry: &|_| None,
    };
    let input = RebuildInput {
        image: &img,
        files: &files,
        losses: &[],
        lease_epoch: 0,
        mark_file_id: 0,
        written_at: T,
    };
    let p = plan(&input, &v).unwrap();
    assert!(p.discard.is_empty() && p.held.is_empty());
    for o in &outs {
        let f = FileRef {
            kind: kind::COMPACTED,
            file_id: o.id.file_id,
        };
        assert!(p.catalogue.file(f).is_some_and(|e| e.is_live()));
        assert_eq!(p.catalogue.layout(f), Some(&layout(&o.layout)));
    }
    assert_eq!(compacted_through(&p.catalogue), BTreeMap::from([(0, 4)]));
    assert_eq!(
        kvik_files::compaction::judge(&p.catalogue),
        Judgement::Holds
    );
}

#[test]
fn a_corrupt_run_record_whose_outputs_fail_against_local_inputs_discards_them() {
    let (fs, segs) = store();
    let (outs, img) = published_then_corrupt(&fs);
    let files = local_files(&fs, &segs, &outs);
    let sa = SetAside::new(vec![]);
    let week = BTreeSet::from([0]);
    let v = Recompaction {
        fs: &fs,
        dir: &dir(),
        store_id: STORE,
        set_aside: &sa,
        weeks_merged: &week,
        registry: &|_| None,
    };
    let input = RebuildInput {
        image: &img,
        files: &files,
        losses: &[],
        lease_epoch: 0,
        mark_file_id: 0,
        written_at: T,
    };
    let p = plan(&input, &v).unwrap();
    let mut want: Vec<FileRef> = outs
        .iter()
        .map(|o| FileRef {
            kind: kind::COMPACTED,
            file_id: o.id.file_id,
        })
        .collect();
    want.sort();
    assert_eq!(p.discard, want);
    assert!(compacted_through(&p.catalogue).is_empty());
}

#[test]
fn a_process_stop_at_every_run_operation_leaves_no_output_after_the_start() {
    let mut n = 0;
    loop {
        let (fs, _) = store();
        let log = log_bytes(&fs);
        let d = dir();
        let sa = SetAside::new(vec![]);
        let weeks = BTreeSet::new();
        let segs = candidates(&fs, &d, &STORE).unwrap();
        fs.crash_after(n);
        let r = run(&run_input(&fs, &d, &sa, &weeks), &segs);
        let stopped = fs.crashed();
        fs.restart();
        let m = open(&fs);
        clean_data(&fs, &d, m.catalogue(), true, &BTreeSet::new()).unwrap();
        if stopped {
            assert!(
                data_names(&fs).is_empty(),
                "left behind at {n}: {:?}",
                data_names(&fs)
            );
        } else {
            assert_eq!(r.unwrap().len(), 3);
            break;
        }
        assert_eq!(log_bytes(&fs), log);
        assert!(compacted_through(m.catalogue()).is_empty());
        n += 1;
    }
    assert!(n > 3);
}
