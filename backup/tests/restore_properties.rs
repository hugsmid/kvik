//! Property tests of the restore executor (Q-82; `spec.md` §8 Restore, the paragraph after step 4).

mod support;

use std::sync::Arc;

use kvik_backup::plan::PlanMode;
use kvik_files::desc::FileRef;
use kvik_log::fs::MemFs;
use proptest::prelude::*;
use support::*;

fn fault() -> impl Strategy<Value = Option<Fault>> {
    prop_oneof![
        Just(None),
        Just(Some(Fault::Lose)),
        Just(Some(Fault::Damage)),
        Just(Some(Fault::Mismatch)),
        (1u32..8).prop_map(|n| Some(Fault::Delay(n))),
    ]
}

fn mode(drill: bool) -> PlanMode {
    if drill {
        PlanMode::Drill
    } else {
        PlanMode::Full
    }
}

proptest! {
    #[test]
    fn any_fault_schedule_ends_with_exactly_the_lost_and_damaged_files_recorded(
        faults in prop::collection::vec(fault(), 1..5),
        drill in any::<bool>(),
    ) {
        let mut src = source(ID, faults.len() as u64, false);
        let sink = Arc::new(FaultSink::new(mem_with(&bucket(&mut src))));
        let keys = kvik_backup::keys::Keys::new(ID);
        let mut gone: Vec<FileRef> = Vec::new();
        for ((d, _), f) in src.segments.iter().zip(&faults) {
            sink.set(&keys.file(d), *f);
            if matches!(f, Some(Fault::Lose | Fault::Damage | Fault::Mismatch)) {
                gone.push(d.file_ref());
            }
        }
        let fs = MemFs::new();
        let (p, o) = restore(&mut restorer(sink.clone(), &fs, mode(drill), KEY)).unwrap();
        let mut lost: Vec<FileRef> = o.lost.iter().map(|l| l.0).collect();
        lost.sort();
        prop_assert_eq!(&lost, &gone);
        for (f, loss, _) in &o.lost {
            let d = &src.segments.iter().find(|(d, _)| d.file_ref() == *f).unwrap().0;
            prop_assert_eq!(Some((loss.first, loss.last)), d.range);
        }
        assert_installed(&fs, &src, &gone);
        prop_assert_eq!(manifest(&fs).1.len(), gone.len());
        prop_assert_eq!(sink.inner.puts(), if drill { 0 } else { 3 });
        prop_assert_eq!(p.record.drill, drill);
    }

    #[test]
    fn a_crash_at_any_file_operation_then_a_partial_power_loss_restores_the_same_files(
        n in 0usize..40,
        seed in any::<u64>(),
        segments in 1u64..4,
        drill in any::<bool>(),
    ) {
        let mut src = source(ID, segments, false);
        let objects = bucket(&mut src);
        let sink = Arc::new(mem_with(&objects));
        let fs = MemFs::new();
        fs.crash_after(n);
        let crashed = {
            let mut r = restorer(sink.clone(), &fs, mode(drill), KEY);
            restore(&mut r).is_err() || fs.crashed()
        };
        if crashed {
            fs.power_loss_partial(seed);
        } else {
            fs.restart();
        }
        let (p, o) = restore(&mut restorer(sink.clone(), &fs, mode(drill), KEY)).unwrap();
        prop_assert!(o.lost.is_empty());
        assert_installed(&fs, &src, &[]);
        let (restores, lost) = manifest(&fs);
        prop_assert_eq!((restores.len(), lost.len()), (1, 0));
        prop_assert_eq!(restores[0].generation, p.record.generation);
        prop_assert_eq!(p.record.files.len() as u64, segments);
        if drill {
            prop_assert_eq!(sink.objects(), objects);
        }
    }
}
