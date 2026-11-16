//! Property tests of self-repair (spec §8 Self-repair) over histories of
//! damage, lost objects, lookups, swaps, closing and restarts, and of the
//! lost-object rule over lookup times (Q-82, Q-85).

use std::collections::{BTreeMap, BTreeSet};

use kvik_model::engine::{Alert, Corruption, FileId, Model, Reason, RepairConfig};
use kvik_model::recovery::{Loss, LossKind, LossState, Outcome};
use kvik_model::types::{DAY, MINUTE, Nanos, Position, SECOND, SeriesId};
use proptest::prelude::*;

use super::{finding, sealed_facts, store};

fn cases(n: u32) -> ProptestConfig {
    ProptestConfig {
        cases: std::env::var("PROPTEST_CASES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(n),
        ..ProptestConfig::default()
    }
}

#[derive(Clone, Debug)]
enum Op {
    /// A new sealed segment of three facts.
    Segment,
    Upload,
    /// The hot copy of a segment verified in the bucket leaves the fast disk.
    Evict(usize),
    /// The hot copy of a segment is damaged and a scrub runs; with `true`
    /// while the engine is stopped.
    Damage(usize, bool),
    Lose(usize),
    Find(usize),
    /// One scheduled check of the bucket.
    Check,
    /// An evicted object is lost, then three checks 5 minutes apart.
    LoseEvicted(usize),
    /// Damage while stopped, a start, and the copy swapped in.
    DamageStoppedRestore(usize),
    Minutes(u16),
    Days(u16),
    Restart,
    Restore,
    Repair,
    RepairDone,
    AckLosses,
    AckCorruptions,
    Reachable(bool),
}

fn arb_op() -> impl Strategy<Value = Op> {
    prop_oneof![
        1 => Just(Op::Segment),
        2 => Just(Op::Upload),
        1 => (0usize..6).prop_map(Op::Evict),
        4 => (0usize..6, proptest::bool::weighted(0.3)).prop_map(|(k, s)| Op::Damage(k, s)),
        2 => (0usize..6).prop_map(Op::Lose),
        2 => (0usize..6).prop_map(Op::Find),
        3 => Just(Op::Check),
        3 => (0usize..6).prop_map(Op::LoseEvicted),
        3 => (0usize..6).prop_map(Op::DamageStoppedRestore),
        2 => prop_oneof![Just(3u16), Just(12)].prop_map(Op::Minutes),
        2 => prop_oneof![Just(1u16), Just(7), Just(40)].prop_map(Op::Days),
        1 => Just(Op::Restart),
        3 => Just(Op::Restore),
        2 => Just(Op::Repair),
        1 => Just(Op::RepairDone),
        1 => Just(Op::AckLosses),
        1 => Just(Op::AckCorruptions),
        1 => proptest::bool::weighted(0.7).prop_map(Op::Reachable),
    ]
}

/// What the histories reached, for their guard.
#[derive(Default, Debug)]
struct Cover {
    restored: usize,
    relost: usize,
    judged: usize,
    second_findings: usize,
    late_copies: usize,
    copying_acknowledged: usize,
    suspect: usize,
}

struct World {
    m: Model,
    s: SeriesId,
    segs: Vec<FileId>,
    /// Positions of sealed segments readable at some step.
    readable: BTreeSet<Position>,
    /// Every recorded corruption, with whether it was seen.
    known: BTreeMap<Corruption, bool>,
    /// Losses closed otherwise than restored in full, with the outcome.
    closed: BTreeMap<Position, Outcome>,
    /// Each loss's state at the last check.
    states: BTreeMap<Position, Option<LossState>>,
    findings: BTreeSet<FileId>,
}

fn restored(state: Option<LossState>) -> bool {
    matches!(state, Some(LossState::Closed(Outcome::Restored(_))))
}

fn covers(l: &Loss, p: Position) -> bool {
    l.kind == LossKind::DataLost && l.first.zip(l.last).is_some_and(|(a, b)| a <= p && p <= b)
}

impl World {
    fn new() -> Self {
        let (mut m, s) = store(RepairConfig::default());
        let segs = (0..3).map(|_| sealed_facts(&mut m, s).0).collect();
        m.upload();
        World {
            m,
            s,
            segs,
            readable: BTreeSet::new(),
            known: BTreeMap::new(),
            closed: BTreeMap::new(),
            states: BTreeMap::new(),
            findings: BTreeSet::new(),
        }
    }

    fn seg(&self, k: usize) -> FileId {
        self.segs[k % self.segs.len()]
    }

    fn up(&mut self) {
        if !self.m.is_up() {
            self.m.restart();
        }
    }

    fn intact_hot(&self, f: FileId) -> bool {
        let d = &self.m.disk;
        d.hot.contains(&f) && !d.corrupt.contains(&f) && !d.quarantined.contains(&f)
    }

    /// Checks that a step finding `files` damaged or lost adds exactly that
    /// many corruptions, each alerting (Q-240, Q-247).
    fn findings_added(&mut self, before: usize, files: &[FileId]) -> Result<(), TestCaseError> {
        prop_assert_eq!(self.m.corruptions().len(), before + files.len());
        for f in files {
            let id = finding(&self.m, *f);
            prop_assert!(self.m.attention().contains(&Alert::Corruption(id)));
        }
        Ok(())
    }

    fn apply(&mut self, op: &Op, cover: &mut Cover) -> Result<(), TestCaseError> {
        match op {
            Op::Segment if self.segs.len() < 6 => {
                self.up();
                let (f, _) = sealed_facts(&mut self.m, self.s);
                self.segs.push(f);
            }
            Op::Upload => {
                self.m.upload();
            }
            Op::Evict(k) => {
                let f = self.seg(*k);
                if self.intact_hot(f) && self.m.disk.in_bucket(f) {
                    self.m.disk.hot.remove(&f);
                }
            }
            Op::Damage(k, stopped) => {
                let f = self.seg(*k);
                if self.intact_hot(f) {
                    if *stopped {
                        if self.m.is_up() {
                            self.m.crash();
                        }
                    } else {
                        self.up();
                    }
                    let before = self.m.corruptions().len();
                    self.m.corrupt_hot(f);
                    self.m.scrub();
                    if !*stopped {
                        self.findings_added(before, &[f])?;
                        cover.second_findings += usize::from(!self.findings.insert(f));
                    }
                }
            }
            Op::Lose(k) => {
                let f = self.seg(*k);
                if self.m.disk.bucket.objects.contains_key(&f) {
                    self.m.lose_object(f);
                }
            }
            Op::Find(k) => self.m.find_object(self.seg(*k)),
            Op::Check => {
                self.up();
                let before = self.m.corruptions().len();
                let judged = self.m.check_bucket().unwrap();
                cover.judged += judged.len();
                self.findings_added(before, &judged)?;
            }
            Op::LoseEvicted(k) => {
                let f = self.seg(*k);
                if self.m.disk.bucket.objects.contains_key(&f)
                    && !self.m.disk.quarantined.contains(&f)
                {
                    self.m.disk.hot.remove(&f);
                    self.m.lose_object(f);
                    self.up();
                    for i in 0..3 {
                        if i > 0 {
                            self.m.advance(5 * MINUTE);
                        }
                        let before = self.m.corruptions().len();
                        let judged = self.m.check_bucket().unwrap();
                        cover.judged += judged.len();
                        self.findings_added(before, &judged)?;
                    }
                }
            }
            Op::DamageStoppedRestore(k) => {
                let f = self.seg(*k);
                if self.intact_hot(f) && self.m.disk.in_bucket(f) {
                    if self.m.is_up() {
                        self.m.crash();
                    }
                    self.m.corrupt_hot(f);
                    self.m.scrub();
                    self.m.restart();
                    self.check(cover)?;
                    self.m.restore_file(f);
                }
            }
            Op::Minutes(n) => self.m.advance(i64::from(*n) * MINUTE),
            Op::Days(n) => self.m.advance(i64::from(*n) * DAY),
            Op::Restart => {
                if self.m.is_up() {
                    self.m.crash();
                }
                self.m.restart();
            }
            Op::Restore => {
                self.up();
                let q: Vec<FileId> = self.m.disk.quarantined.iter().copied().collect();
                for f in q {
                    self.m.restore_file(f);
                }
            }
            Op::Repair => {
                self.up();
                self.m.repair().unwrap();
            }
            Op::RepairDone => {
                self.up();
                if let Some(r) = self.m.resend_requests().first() {
                    self.m.repair_done(r.identity).unwrap();
                }
            }
            Op::AckLosses => {
                self.up();
                let copying: Vec<Position> = self
                    .m
                    .losses()
                    .iter()
                    .filter(|l| l.state == Some(LossState::Copying))
                    .map(|l| l.identity)
                    .collect();
                let ids: Vec<Position> = self.m.losses().iter().map(|l| l.identity).collect();
                self.m.acknowledge_losses(&ids).unwrap();
                // Q-248: a loss whose copy is coming is not closed.
                for l in self.m.losses() {
                    if copying.contains(&l.identity) {
                        prop_assert_eq!(l.state, Some(LossState::Copying), "Q-248");
                    }
                }
                cover.copying_acknowledged += copying.len();
            }
            Op::AckCorruptions => {
                self.up();
                let ids: Vec<Corruption> = self.m.corruptions().iter().map(|c| c.id).collect();
                self.m.acknowledge_corruptions(&ids).unwrap();
            }
            Op::Reachable(r) => self.m.disk.bucket.reachable = *r,
            Op::Segment => {}
        }
        self.check(cover)
    }

    /// Whether `p`'s only copy is an object the bucket reports absent that
    /// is not yet judged lost: the lost-object rule has not spoken.
    fn pending(&self, p: Position) -> bool {
        let d = &self.m.disk;
        self.segs.iter().any(|f| {
            d.bucket.missing.contains(f)
                && !d.judged_lost.contains(f)
                && d.files[f].entries().any(|e| e.position == p)
        })
    }

    fn check(&mut self, cover: &mut Cover) -> Result<(), TestCaseError> {
        if !self.m.is_up() {
            return Ok(());
        }
        let all = self.m.disk.all_entries();
        let sealed: BTreeSet<Position> = self
            .segs
            .iter()
            .flat_map(|f| self.m.disk.files[f].entries().map(|e| e.position))
            .collect();
        self.readable
            .extend(sealed.iter().filter(|p| all.contains_key(p)));
        let losses = self.m.losses();
        // A position once readable and no longer readable lies in a loss that
        // is open or closed otherwise than restored in full (§8 Self-repair stage 1).
        for p in &self.readable {
            let open = losses.iter().any(|l| covers(l, *p) && !restored(l.state));
            prop_assert!(
                all.contains_key(p) || open || self.pending(*p),
                "{:?} unreadable outside an open loss: {:?}",
                p,
                losses
            );
        }
        for l in &losses {
            // A loss restored in full holds its facts again.
            if restored(l.state) {
                for p in sealed.iter().filter(|p| covers(l, **p)) {
                    prop_assert!(all.contains_key(p) || self.pending(*p), "{:?} hidden", p);
                }
            }
            // Q-246: a loss closed otherwise stays closed so.
            if let Some(LossState::Closed(o)) = l.state
                && !matches!(o, Outcome::Restored(_))
            {
                self.closed.insert(l.identity, o);
            }
            let was = self.states.insert(l.identity, l.state).flatten();
            cover.restored += usize::from(restored(l.state) && !restored(was));
            cover.relost += usize::from(restored(was) && !restored(l.state));
        }
        for (id, o) in &self.closed {
            let now = losses
                .iter()
                .find(|l| l.identity == *id)
                .and_then(|l| l.state);
            prop_assert_eq!(now, Some(LossState::Closed(*o)), "Q-246");
        }
        // Q-240: an alert for each corruption until seen; none is lost or
        // unseen again.
        let corruptions = self.m.corruptions();
        let alerts = self.m.attention();
        for (id, was_seen) in &self.known {
            let now = corruptions.iter().find(|c| c.id == *id);
            prop_assert!(
                now.is_some_and(|c| c.seen || !was_seen),
                "{:?} lost or unseen",
                id
            );
        }
        for c in &corruptions {
            prop_assert_eq!(alerts.contains(&Alert::Corruption(c.id)), !c.seen);
            if !matches!(c.id, Corruption::File(_, None)) {
                self.known.insert(c.id, c.seen);
            }
        }
        // Q-247: 2 corruptions in 30 days, seen or not, make the storage
        // suspect, as an alert and in health.
        let since = self.m.wall - 30 * DAY;
        let n = corruptions
            .iter()
            .filter(|c| c.at.is_none_or(|at| at >= since))
            .count();
        let n = u32::try_from(n).unwrap();
        let h = self.m.health();
        let suspect = n >= 2;
        prop_assert_eq!(alerts.contains(&Alert::StorageSuspect(n)), suspect);
        prop_assert_eq!(h.reasons.contains(&Reason::StorageSuspect(n)), suspect);
        cover.suspect += usize::from(suspect);
        // A quarantined file is fatal only while a swap can still happen:
        // a copy is there to swap in, or the lost-object rule has not spoken.
        if !self.m.is_read_only() {
            for f in self.m.disk.quarantined.clone() {
                let holds_closed = losses.iter().any(|l| {
                    matches!(l.state, Some(LossState::Closed(o)) if !matches!(o, Outcome::Restored(_)))
                        && self.m.disk.files[&f].entries().any(|e| covers(l, e.position))
                });
                let mut c = self.m.clone();
                let swap = c.restore_file(f);
                let d = &self.m.disk;
                let waits = d.bucket.objects.contains_key(&f)
                    && d.bucket.missing.contains(&f)
                    && !d.judged_lost.contains(&f)
                    && !holds_closed;
                prop_assert_eq!(h.reasons.contains(&Reason::Quarantined(f)), swap || waits);
                cover.late_copies += usize::from(holds_closed && d.verified_elsewhere(f));
            }
        }
        Ok(())
    }
}

fn run(ops: &[Op], cover: &mut Cover) -> Result<(), TestCaseError> {
    let mut w = World::new();
    w.check(cover)?;
    for op in ops {
        w.apply(op, cover)?;
    }
    w.up();
    w.check(cover)
}

#[derive(Clone, Debug)]
enum Look {
    /// One scheduled check, with the bucket reachable or not.
    Check(bool),
    /// The clocks move on this many seconds.
    Wait(u16),
    Restart,
}

fn arb_look() -> impl Strategy<Value = Look> {
    prop_oneof![
        4 => proptest::bool::weighted(0.8).prop_map(Look::Check),
        3 => (0u16..400).prop_map(Look::Wait),
        1 => Just(Look::Restart),
    ]
}

proptest! {
    #![proptest_config(cases(256))]

    /// The self-repair rulings over generated histories: every finding alerts
    /// and counts (Q-240, Q-247, a second finding of the same file
    /// included), a position stops being readable only inside an open loss
    /// or one closed otherwise than restored in full, a loss restored in full
    /// holds its facts, a closed loss stays closed (Q-246), an operator does
    /// not close a loss whose copy is coming (Q-248), and a quarantined file
    /// is fatal only while a swap can still happen.
    #[test]
    fn s8_self_repair_histories_keep_the_rulings(
        ops in proptest::collection::vec(arb_op(), 1..60)
    ) {
        run(&ops, &mut Cover::default())?;
    }

    /// The lost-object rule (Q-82): an object is judged lost at the first
    /// lookup that makes 3 failed lookups, since the last start, spanning at
    /// least 10 minutes while the bucket answers; a lookup while it cannot
    /// be reached counts nothing.
    #[test]
    fn s8_self_repair_lost_object_rule_over_lookup_times(
        looks in proptest::collection::vec(arb_look(), 1..40)
    ) {
        let (mut m, s) = store(RepairConfig::default());
        let (f, _) = sealed_facts(&mut m, s);
        m.upload();
        m.disk.hot.remove(&f);
        m.lose_object(f);
        let mut tries: Vec<Nanos> = Vec::new();
        let mut judged = false;
        for look in &looks {
            match look {
                Look::Check(reachable) => {
                    m.disk.bucket.reachable = *reachable;
                    if *reachable && !judged {
                        tries.push(m.wall);
                    }
                    let want = !judged
                        && tries.len() >= 3
                        && tries[tries.len() - 1] - tries[0] >= 10 * MINUTE;
                    let got = m.check_bucket().unwrap();
                    prop_assert_eq!(&got, &if want { vec![f] } else { vec![] }, "{:?}", tries);
                    judged |= want;
                }
                Look::Wait(secs) => m.advance(i64::from(*secs) * SECOND),
                Look::Restart => {
                    m.crash();
                    m.restart();
                    tries.clear();
                }
            }
            prop_assert_eq!(m.losses().len(), usize::from(judged));
        }
    }
}

/// Guards the history property: the generator reaches losses restored in
/// full, re-lost after that, objects judged lost, second findings of a file,
/// late copies of closed losses, acknowledgements during stage 1 and a
/// suspect store.
#[test]
fn s8_self_repair_history_coverage() {
    use proptest::strategy::ValueTree;
    use proptest::test_runner::TestRunner;
    let mut runner = TestRunner::deterministic();
    let strat = proptest::collection::vec(arb_op(), 40..60);
    let mut cover = Cover::default();
    for _ in 0..200 {
        let ops = strat.new_tree(&mut runner).unwrap().current();
        run(&ops, &mut cover).unwrap();
    }
    eprintln!("{cover:?}");
    assert!(cover.restored > 100 && cover.relost > 75 && cover.judged > 150);
    assert!(cover.second_findings > 30 && cover.late_copies > 20);
    assert!(cover.copying_acknowledged > 10 && cover.suspect > 500);
}
