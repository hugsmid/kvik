//! The derived files (`format.md` A.18) across crashes: a crash at every write recovers from the previous
//! generation, and a damaged or other generation is never used.

mod dirs_rig;

use dirs_rig::*;

#[test]
fn a_crash_at_any_write_recovers_from_the_previous_generation() {
    let reference = {
        let mut r = waiting();
        r.fs.clear_ops();
        r.run();
        let ops = r.fs.ops().len();
        assert!(
            ops >= 8,
            "a group, the root, the series directory and status: {ops}"
        );
        (ops, content(&r))
    };
    for n in 0..=reference.0 {
        for power in [false, true] {
            let mut r = waiting();
            let before = (r.load().map(|d| d.root), r.series().map(|s| s.1));
            r.fs.crash_after(n);
            r.run();
            if power {
                r.fs.power_loss_partial(n as u64);
            } else {
                r.fs.restart();
            }
            // Whatever a reader finds is true of the segments: after a process crash the old generation or the
            // new one, after a power cut possibly an older one or nothing; never a record that is not the file's.
            let found = (r.load(), r.series().map(|s| s.1));
            let sealed = r.sealed();
            if let Some(d) = &found.0 {
                let listed: Vec<_> = d.segments().cloned().collect();
                assert!(listed.iter().all(|x| sealed.contains(x)), "n {n}");
                if d.groups.iter().all(Option::is_some) {
                    assert!(sealed.starts_with(&listed), "n {n}");
                }
                if !power {
                    assert!(
                        Some(&d.root) == before.0.as_ref() || d.root == reference.1.0,
                        "n {n}"
                    );
                }
            } else {
                assert!(
                    power,
                    "a root of either generation after a process crash, n {n}"
                );
            }
            // A new start rebuilds forward from what it finds and ends where the uninterrupted run did.
            r.open();
            r.settle();
            let after = content(&r);
            assert_eq!(
                after.0.groups, reference.1.0.groups,
                "n {n}, power loss {power}"
            );
            assert_eq!(after.1, reference.1.1, "n {n}, power loss {power}");
            if !power {
                let reused = r.dirs.as_ref().unwrap().stats().described;
                assert!(
                    reused <= 80,
                    "only the segments no valid group lists are read: {reused}"
                );
            }
        }
    }
}

#[test]
fn a_damaged_newest_generation_falls_back_to_the_previous_one() {
    let mut r = Rig::new(cfg());
    ingest(&mut r, 400, 0);
    r.settle();
    ingest(&mut r, 400, 20_000_000);
    r.settle();
    for family in [Family::Root, Family::Series] {
        let gens = directory::generations(&r.fs, &dir(), family);
        assert_eq!(gens.len(), 2, "{family:?}");
        // One bit of the newest's body flipped: invalid, so a reader takes the previous generation.
        let mut newest = r.fs.read(&gens[0].1).unwrap();
        newest[90] ^= 1;
        r.fs.remove_file(&gens[0].1).unwrap();
        let f =
            r.fs.open(&gens[0].1, kvik_log::fs::OpenMode::CreateNew)
                .unwrap();
        f.write_at(&newest, 0).unwrap();
        drop(f);
        let found = match family {
            Family::Root => read_latest(&r.fs, &dir(), family, |b| Root::decode(b, &STORE_ID))
                .unwrap()
                .map(|(f, _)| f.file_id),
            _ => r.series().map(|(f, _)| f.file_id),
        };
        assert_eq!(found, Some(gens[1].0), "{family:?}");
    }
    // The writer rebuilds forward from what it finds and ends where it was.
    r.open();
    r.settle();
    let d = r.load().unwrap();
    assert!(d.groups.iter().all(Option::is_some));
    assert_eq!(d.segments().cloned().collect::<Vec<_>>(), r.sealed());
}

#[test]
fn a_group_of_another_generation_than_the_root_names_is_not_used() {
    let mut r = Rig::new(cfg());
    ingest(&mut r, 300, 0);
    r.settle();
    let d = r.load().unwrap();
    let last = d.root.groups.last().unwrap().number;
    let gens = directory::generations(&r.fs, &dir(), Family::Group(last));
    assert_eq!(gens.len(), 2, "the open group was written at least twice");
    r.fs.remove_file(&gens[0].1).unwrap();
    // The previous generation still matches the catalogue, but it lists fewer segments than the root says.
    let d = r.load().unwrap();
    assert!(d.groups.last().unwrap().is_none());
}

#[test]
fn a_file_under_another_generation_s_name_is_not_taken() {
    let mut r = Rig::new(cfg());
    ingest(&mut r, 400, 0);
    r.settle();
    ingest(&mut r, 400, 20_000_000);
    r.settle();
    for family in [Family::Root, Family::Series] {
        let gens = directory::generations(&r.fs, &dir(), family);
        let newest = gens[0].0;
        // The previous generation's bytes under a newer name: its header's `file_id` is not the name's.
        let old = r.fs.read(&gens[1].1).unwrap();
        let name = dir().join(DERIVED_DIR).join(family.file_name(newest + 5));
        let f = r.fs.open(&name, kvik_log::fs::OpenMode::CreateNew).unwrap();
        f.write_at(&old, 0).unwrap();
        drop(f);
        let found = match family {
            Family::Root => read_latest(&r.fs, &dir(), family, |b| Root::decode(b, &STORE_ID))
                .unwrap()
                .map(|(f, _)| f.file_id),
            _ => r.series().map(|(f, _)| f.file_id),
        };
        assert_eq!(found, Some(newest), "{family:?}");
        r.fs.remove_file(&name).unwrap();
    }
}
