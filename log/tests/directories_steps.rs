//! The start's steps and what each may do (`format.md` A.18 The start's work).
mod dirs_rig;

use dirs_rig::*;
use kvik_log::directory::catalogue::DESCRIBE_STEP;
use kvik_log::writer::{GROUPS_PER_STEP, REUSED_PER_STEP, TRAILERS_PER_STEP};

fn fresh(r: &Rig) -> Directories {
    Directories::new(
        Arc::new(r.fs.clone()),
        &dir(),
        STORE_ID,
        r.w.as_ref().unwrap().directory_start(),
        DirectoryOptions::default(),
    )
}

/// Runs a fresh start to its end at one instant, calling `each` after every run; the runs it took.
fn start(r: &Rig, mut each: impl FnMut(&Rig)) -> usize {
    let synced = r.w.as_ref().unwrap().stats().mark_synced;
    let mut d = fresh(r);
    let now = Duration::from_secs(1_000);
    for n in 1..100_000 {
        d.run(synced, now, T0);
        each(r);
        if d.started() {
            return n;
        }
    }
    panic!("the start never ends");
}

fn group_files(r: &Rig) -> usize {
    directory::list(&r.fs, &dir())
        .unwrap()
        .into_iter()
        .filter(|(f, _, _)| matches!(f, Family::Group(_)))
        .count()
}

#[test]
fn a_start_that_reuses_every_record_takes_the_stated_steps() {
    let r = two_groups();
    // More group files than one step reads: copies under other numbers, read and set aside like any other.
    let (f, g) = read_latest(&r.fs, &dir(), Family::Group(0), |b| {
        Group::decode(b, &STORE_ID)
    })
    .unwrap()
    .unwrap();
    for n in 0..20 {
        directory::write_file(
            &r.fs,
            &dir(),
            Family::Group(100 + n),
            f.file_id,
            &g.0.encode(f, STORE_ID).unwrap(),
        )
        .unwrap();
    }
    let files = group_files(&r);
    let segments = r.sealed().len();
    assert!(files > GROUPS_PER_STEP && segments > REUSED_PER_STEP);
    let expected = 1
        + files.div_ceil(GROUPS_PER_STEP)
        + 1
        + segments.div_ceil(TRAILERS_PER_STEP)
        + segments / REUSED_PER_STEP
        + 1;
    assert_eq!(start(&r, |_| {}), expected);
}

#[test]
fn a_segment_to_describe_is_hashed_a_bounded_part_at_a_time() {
    let mut r = Rig::new(WriterConfig {
        segment_size: 10 << 20,
        max_batch_bytes: 1 << 20,
        ..cfg()
    });
    let mut k = 0;
    while r.sealed().is_empty() {
        let items = (0..10_000)
            .map(|j| fact(1 + j % 5, (k * 10_000 + j) as i64 * 1_000, Quality::Good))
            .collect();
        r.submit(items);
        r.settle();
        k += 1;
        assert!(k < 1_000, "no segment sealed");
    }
    let sealed = r.sealed();
    assert_eq!(sealed.len(), 1);
    let len = sealed[0].len;
    assert!(len > 2 * DESCRIBE_STEP, "a segment of {len} bytes");
    for p in r.derived() {
        r.fs.remove_file(&p).unwrap();
    }
    let parts = usize::try_from(len.div_ceil(DESCRIBE_STEP)).unwrap();
    assert_eq!(start(&r, |_| {}), 1 + 1 + 1 + 1 + parts + 1);
}

#[test]
fn a_root_written_during_the_start_never_goes_below_the_one_found_valid() {
    let r = two_groups();
    let found = r.load().unwrap().root.through;
    let mut lowest = found;
    start(&r, |r| {
        if let Some(d) = r.load() {
            lowest = lowest.min(d.root.through);
        }
    });
    assert_eq!(lowest, found);
}
