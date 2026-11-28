//! The visible state, the part merge and the cover against direct computation (design 7.1b sections 2, 3.3, 5):
//! overlay and floors against a map oracle, merges that leave `visible` unchanged and write the roll-ups of T2,
//! merge output determinism (T4), and covers that tile a span and merge to the node folded over it.

mod files;

use std::collections::BTreeMap;

use files::{DAY, SECOND, T0, node, part_header, pos};
use kvik_log::header::FileHeader;
use kvik_log::mark::ResetStamp;
use kvik_tree::part::{Class, Entry, Part, Run};
use kvik_tree::root::{self, Covered, Floor, Root};
use kvik_tree::set::{
    Key, Plan, Policy, QUARTER, Seen, SetError, Tile, cover, covered, merge, next_root, part_ref,
    plan, rollups, visible,
};
use kvik_tree::{Kind, LEVELS, MINUTE, Node, Params, Value, fold, merge as merge_nodes};
use proptest::prelude::*;

const KINDS: [Kind; 3] = [Kind::Gauge, Kind::Counter, Kind::State];

fn kind_of(series: u64) -> Kind {
    KINDS[(series % 3) as usize]
}

fn params_of(series: u64) -> Option<Params> {
    Some(Params::new(kind_of(series), Some(5 * MINUTE)))
}

fn width(level: u8) -> i64 {
    LEVELS.iter().find(|l| l.id == level).unwrap().width
}

fn empty_root() -> Root {
    Root {
        header: FileHeader::new(root::KIND, 3, 100, T0 + DAY, files::STORE),
        seq: 100,
        checkpoint: pos(3, 10_000),
        reset: ResetStamp {
            sequence: 1,
            nonce: 7,
        },
        next_file_id: 1_000,
        floor: 0,
        levels: LEVELS.to_vec(),
        parts: Vec::new(),
        covered: Vec::new(),
        floors: Vec::new(),
        retired: Vec::new(),
    }
}

/// Lists `parts` in `root`, from their encodings.
fn listing(root: &mut Root, parts: &[Part]) {
    root.parts = parts
        .iter()
        .map(|p| part_ref(p, &p.encode().unwrap()))
        .collect();
    root.parts.sort_by_key(|r| r.file_id);
}

/// One generated entry: key indices, form (node, empty marker, stale without or with a node) and a seed.
type GenEntry = (u64, u8, i64, u8, u32);

fn entry_of(&(series, level, idx, form, seed): &GenEntry) -> (u64, u8, Entry) {
    let w = width(level);
    let start = T0 + idx.rem_euclid(DAY / w) * w;
    let n = || Some(node(kind_of(series), start, w, 1 + seed % 4, seed));
    let (stale, node) = match form % 8 {
        0..5 => (false, n()),
        5 => (false, None),
        6 => (true, None),
        _ => (true, n()),
    };
    let entry = Entry {
        start,
        stale,
        as_of: pos(3, u64::from(seed % 500)),
        node,
    };
    (series, level, entry)
}

fn part_of(file_id: u64, seq: u64, entries: &[GenEntry]) -> Part {
    let mut by: BTreeMap<(u64, u8), BTreeMap<i64, Entry>> = BTreeMap::new();
    for g in entries {
        let (s, l, e) = entry_of(g);
        by.entry((s, l)).or_default().insert(e.start, e);
    }
    Part {
        header: part_header(file_id),
        seq,
        class: Class::Delta,
        period_start: T0,
        period_len: DAY as u64,
        through: pos(3, seq * 10),
        levels: LEVELS.to_vec(),
        runs: by
            .into_iter()
            .map(|((series, level), e)| Run {
                series,
                level,
                entries: e.into_values().collect(),
            })
            .collect(),
    }
}

/// Parts of one day over three series and every level, some unlisted; floors, an open floor and covered ranges.
#[derive(Clone, Debug)]
struct World {
    root: Root,
    parts: Vec<Part>,
}

fn world(unique: bool) -> impl Strategy<Value = World> {
    let entry = (
        1u64..4,
        prop_oneof![1 => 1u8..5, 2 => Just(2u8)],
        0i64..12,
        any::<u8>(),
        any::<u32>(),
    );
    let part = (1u64..9, any::<bool>(), prop::collection::vec(entry, 0..20));
    let floor = prop::option::of((0u64..9, prop::bool::weighted(0.2)));
    (
        prop::collection::vec(part, 1..7),
        0u64..4,
        prop::collection::vec(floor, 3),
        prop::collection::vec(
            prop_oneof![
                Just(None),
                Just(Some((0, 1_440))),
                (0i64..8, 1i64..9).prop_map(Some)
            ],
            4,
        ),
    )
        .prop_map(move |(ps, global, floors, cov)| {
            let parts: Vec<Part> = ps
                .iter()
                .enumerate()
                .map(|(i, (seq, _, e))| {
                    let seq = if unique { seq * 8 + i as u64 } else { *seq };
                    part_of(10 + i as u64, seq, e)
                })
                .collect();
            let listed: Vec<Part> = parts
                .iter()
                .zip(&ps)
                .enumerate()
                .filter(|(i, (_, (_, unlisted, _)))| *i == 0 || !unlisted)
                .map(|(_, (p, _))| p.clone())
                .collect();
            let mut root = empty_root();
            listing(&mut root, &listed);
            root.floor = global;
            root.floors = floors
                .iter()
                .enumerate()
                .filter_map(|(i, f)| {
                    f.map(|(seq, rebuilding)| Floor {
                        series: i as u64 + 1,
                        seq,
                        rebuilding,
                    })
                })
                .collect();
            root.covered = cov
                .iter()
                .zip(LEVELS)
                .filter_map(|(c, l)| {
                    c.map(|(a, n)| {
                        let k = DAY / l.width;
                        let a = a.min(k - 1);
                        Covered {
                            level: l.id,
                            from: T0 + a * l.width,
                            to: T0 + (a + n).min(k) * l.width,
                        }
                    })
                })
                .collect();
            World { root, parts }
        })
}

/// The oracle of section 3.3, key by key over the generated domain.
fn oracle(root: &Root, parts: &[Part]) -> Result<BTreeMap<Key, Seen>, SetError> {
    let listed: Vec<&Part> = parts
        .iter()
        .filter(|p| root.parts.iter().any(|r| r.file_id == p.header.file_id))
        .collect();
    let mut out = BTreeMap::new();
    let mut first_tie = None;
    for series in 1u64..4 {
        let floor = root.floors.iter().find(|f| f.series == series);
        for l in LEVELS {
            for start in (0..DAY / l.width).map(|i| T0 + i * l.width) {
                let mut best: Option<(u64, &Entry)> = None;
                let mut tied = false;
                for p in &listed {
                    if p.seq < root.floor || floor.is_some_and(|f| p.seq < f.seq) {
                        continue;
                    }
                    let e = p
                        .runs
                        .iter()
                        .filter(|r| r.series == series && r.level == l.id)
                        .flat_map(|r| &r.entries)
                        .find(|e| e.start == start);
                    let Some(e) = e else { continue };
                    match best {
                        Some((s, _)) if s == p.seq => tied = true,
                        Some((s, _)) if s > p.seq => {}
                        _ => {
                            best = Some((p.seq, e));
                            tied = false;
                        }
                    }
                }
                if let (true, Some((seq, _))) = (tied, best) {
                    first_tie.get_or_insert(SetError::SameSeq {
                        seq,
                        series,
                        level: l.id,
                        start,
                    });
                }
                let Some((_, e)) = best else { continue };
                let shown = floor.is_none_or(|f| !f.rebuilding)
                    && (e.stale || e.node.is_some())
                    && root
                        .covered
                        .iter()
                        .any(|c| c.level == l.id && c.from <= start && start + l.width <= c.to);
                if shown {
                    out.insert(
                        (series, l.id, start),
                        Seen {
                            as_of: e.as_of,
                            stale: e.stale,
                            node: e.node.clone(),
                        },
                    );
                }
            }
        }
    }
    match first_tie {
        Some(e) => Err(e),
        None => Ok(out),
    }
}

fn ceil_to(x: i64, w: i64) -> i64 {
    match x.rem_euclid(w) {
        0 => x,
        r => x + (w - r),
    }
}

fn same_seq(r: &Result<BTreeMap<Key, Seen>, SetError>) -> bool {
    matches!(r, Err(SetError::SameSeq { .. }))
}

/// The left-to-right merge of `nodes` over `[a, b)`, empty nodes in the gaps.
fn merged_over(p: &Params, a: i64, b: i64, nodes: &[&Node]) -> Node {
    let mut acc = Node::empty(p.kind, a, a);
    for n in nodes {
        if acc.end < n.start {
            acc = merge_nodes(p, &acc, &Node::empty(p.kind, acc.end, n.start));
        }
        acc = merge_nodes(p, &acc, n);
    }
    if acc.end < b {
        acc = merge_nodes(p, &acc, &Node::empty(p.kind, acc.end, b));
    }
    acc
}

/// T2 over a visible state: in every covered roll-up span of a served series, the entry is present exactly when a
/// visible child is, stale when one is, and otherwise the merge of the children at their highest version.
fn check_rollups(
    root: &Root,
    vis: &BTreeMap<Key, Seen>,
    series: &[u64],
    params: impl Fn(u64) -> Params,
) {
    for (parent, child) in rollups(&root.levels) {
        for c in root.covered.iter().filter(|c| c.level == parent.id) {
            for x in (c.from..c.to).step_by(parent.width as usize) {
                for &s in series {
                    let kids: Vec<&Seen> = vis
                        .range((s, child.id, x)..(s, child.id, x + parent.width))
                        .map(|(_, v)| v)
                        .collect();
                    let got = vis.get(&(s, parent.id, x));
                    if kids.is_empty() {
                        assert!(
                            got.is_none(),
                            "{s} {} {x}: roll-up without a child",
                            parent.id
                        );
                        continue;
                    }
                    let got =
                        got.unwrap_or_else(|| panic!("{s} {} {x}: roll-up missing", parent.id));
                    assert_eq!(got.as_of, kids.iter().map(|k| k.as_of).max().unwrap());
                    if kids.iter().any(|k| k.stale) {
                        assert!(got.stale && got.node.is_none());
                        continue;
                    }
                    let p = params(s);
                    let nodes: Vec<&Node> = kids.iter().map(|k| k.node.as_ref().unwrap()).collect();
                    let want = merged_over(&p, x, x + parent.width, &nodes);
                    assert_eq!(got.node.as_ref(), Some(&want));
                }
            }
        }
    }
}

/// Merges `plan` and publishes it: the next root and the parts a reader then has.
fn apply(root: &Root, parts: &[Part], plan: &Plan, now: i64) -> (Root, Vec<Part>) {
    let out = merge(root, parts, plan, &part_header(0), params_of).unwrap();
    let refs: Vec<_> = out
        .iter()
        .map(|p| part_ref(p, &p.encode().unwrap()))
        .collect();
    let next = next_root(root, plan, &refs, now);
    assert_eq!(Root::decode(&next.encode().unwrap()).unwrap(), next);
    let mut after: Vec<Part> = parts
        .iter()
        .filter(|p| !plan.inputs.contains(&p.header.file_id))
        .cloned()
        .collect();
    after.extend(out);
    (next, after)
}

proptest! {
    #[test]
    fn visible_is_the_highest_seq_entry_above_the_floors_inside_covered(w in world(false)) {
        let got = visible(&w.root, &w.parts);
        let want = oracle(&w.root, &w.parts);
        if same_seq(&want) {
            prop_assert!(same_seq(&got));
        } else {
            prop_assert_eq!(got, want);
        }
    }

    #[test]
    fn a_merge_leaves_visible_unchanged_and_writes_the_rollups_of_t2(
        w in world(true),
        pick in (any::<prop::sample::Index>(), any::<prop::sample::Index>()),
        rollup in any::<bool>(),
        minute_from in prop_oneof![Just(i64::MIN), (0i64..2_880).prop_map(|h| T0 + h * MINUTE / 2)],
    ) {
        let mut w = w;
        if rollup {
            w.root.covered.retain(|c| c.level <= 2);
        }
        let before = visible(&w.root, &w.parts);
        let Ok(before) = before else { return Ok(()) };
        let mut seqs: Vec<u64> = w.root.parts.iter().map(|r| r.seq).collect();
        seqs.sort_unstable();
        let (a, b) = (pick.0.get(&seqs), pick.1.get(&seqs));
        let (lo, hi) = if rollup { (seqs[0], *seqs.last().unwrap()) } else { (*a.min(b), *a.max(b)) };
        let inputs: Vec<u64> = w.root.parts.iter().filter(|r| (lo..=hi).contains(&r.seq)).map(|r| r.file_id).collect();
        let plan = Plan { inputs, periods: vec![(T0, DAY as u64)], rollups: rollup, minute_from };
        let (next, after) = apply(&w.root, &w.parts, &plan, T0 + 5 * DAY);

        let mut reversed = w.parts.clone();
        reversed.reverse();
        let one = merge(&w.root, &w.parts, &plan, &part_header(0), params_of).unwrap();
        let two = merge(&w.root, &reversed, &plan, &part_header(0), params_of).unwrap();
        let bytes = |v: &[Part]| v.iter().map(|p| p.encode().unwrap()).collect::<Vec<_>>();
        prop_assert_eq!(bytes(&one), bytes(&two));

        let got = visible(&next, &after).unwrap();
        let cut = |m: &BTreeMap<Key, Seen>| -> BTreeMap<Key, Seen> {
            m.iter()
                .filter(|((_, l, s), _)| {
                    let w = width(*l);
                    (w >= QUARTER || *s >= ceil_to(minute_from, w)) && !(rollup && *l > 2)
                })
                .map(|(k, v)| (*k, v.clone()))
                .collect()
        };
        prop_assert_eq!(cut(&got), cut(&before));
        if rollup {
            check_rollups(&next, &got, &[1, 2, 3], |s| params_of(s).unwrap());
        }
    }
}

/// One series' readings over two days, at most a few per quarter-hour, with values from seeds.
#[derive(Clone, Debug)]
struct Series {
    p: Params,
    readings: Vec<(i64, Value)>,
}

fn series() -> impl Strategy<Value = Series> {
    (
        0usize..3,
        prop_oneof![Just(None), Just(Some(90 * SECOND)), Just(Some(40 * MINUTE))],
        prop::collection::btree_set(0i64..2 * 1_440 * 4, 0..120),
        any::<u32>(),
    )
        .prop_map(|(k, staleness, ticks, seed)| {
            let kind = KINDS[k];
            let readings = ticks
                .into_iter()
                .map(|t| {
                    let x = (seed ^ t as u32).wrapping_mul(2_654_435_761) % 1_000;
                    let v = match kind {
                        Kind::Counter => Value::F64(f64::from((t / 7) as u32 + x % 3)),
                        Kind::State => Value::State((x % 3) as u16),
                        _ => Value::F64(f64::from(x) / 8.0),
                    };
                    (T0 + t * 15 * SECOND, v)
                })
                .collect();
            Series {
                p: Params::new(kind, staleness),
                readings,
            }
        })
}

fn within(r: &[(i64, Value)], a: i64, b: i64) -> &[(i64, Value)] {
    let i = r.partition_point(|x| x.0 < a);
    let j = r.partition_point(|x| x.0 < b);
    &r[i..j]
}

/// Parts of a two-day series: each quarter-hour's node in one of `spread` deltas per day, behind an older wrong
/// entry or an empty marker where `noise` says so.
fn deltas_of(s: &Series, spread: &[u8], noise: &[u8]) -> Vec<Part> {
    let mut parts: Vec<Part> = Vec::new();
    for day in 0..2i64 {
        let d = T0 + day * DAY;
        let n = 1 + spread[day as usize] % 5;
        let mut bins: Vec<Vec<Entry>> = vec![Vec::new(); n as usize + 1];
        for q in 0..96i64 {
            let a = d + q * QUARTER;
            let r = within(&s.readings, a, a + QUARTER);
            let k = noise[(day * 96 + q) as usize];
            let bin = 1 + (k % n) as usize;
            let entry = |node| Entry {
                start: a,
                stale: false,
                as_of: pos(3, 50 + q as u64),
                node,
            };
            if r.is_empty() {
                if k & 0x80 != 0 {
                    bins[0].push(entry(Some(node(s.p.kind, a, QUARTER, 2, k.into()))));
                    bins[bin].push(entry(None));
                }
                continue;
            }
            if k & 0x40 != 0 {
                bins[0].push(entry(Some(node(s.p.kind, a, QUARTER, 3, k.into()))));
            }
            bins[bin].push(entry(Some(fold(&s.p, a, a + QUARTER, r))));
        }
        for (i, entries) in bins.into_iter().enumerate() {
            let file_id = parts.len() as u64 + 1;
            parts.push(Part {
                header: part_header(file_id),
                seq: 1 + day as u64 * 10 + i as u64,
                class: Class::Delta,
                period_start: d,
                period_len: DAY as u64,
                through: pos(3, file_id),
                levels: LEVELS.to_vec(),
                runs: if entries.is_empty() {
                    Vec::new()
                } else {
                    vec![Run {
                        series: 1,
                        level: 2,
                        entries,
                    }]
                },
            });
        }
    }
    parts
}

/// Runs the planner and its merges until it plans nothing.
fn settle(
    mut root: Root,
    mut parts: Vec<Part>,
    now: i64,
    policy: &Policy,
    p: Params,
) -> (Root, Vec<Part>) {
    for _ in 0..8 {
        let plans = plan(&root, now, policy);
        assert_eq!(
            plans,
            plan(&Root::decode(&root.encode().unwrap()).unwrap(), now, policy)
        );
        if plans.is_empty() {
            return (root, parts);
        }
        let mut seen = std::collections::BTreeSet::new();
        for pl in &plans {
            assert!(
                pl.inputs.iter().all(|i| seen.insert(*i)),
                "an input in two plans"
            );
        }
        for pl in plans {
            let out = merge(&root, &parts, &pl, &part_header(0), |_| Some(p)).unwrap();
            let refs: Vec<_> = out
                .iter()
                .map(|x| part_ref(x, &x.encode().unwrap()))
                .collect();
            let next = next_root(&root, &pl, &refs, now);
            parts.retain(|x| !pl.inputs.contains(&x.header.file_id));
            parts.extend(out);
            root = next;
        }
    }
    panic!("the planner did not settle");
}

proptest! {
    #[test]
    fn a_cover_tiles_the_span_and_merges_to_the_node_folded_over_it(
        s in series(),
        spread in prop::collection::vec(any::<u8>(), 2),
        noise in prop::collection::vec(any::<u8>(), 192),
        base in prop_oneof![Just((0i64, 192i64)), (0i64..192, 0i64..193)],
        span in prop_oneof![Just((0i64, 192i64)), (0i64..193, 0i64..193)],
        late in any::<bool>(),
    ) {
        let parts = deltas_of(&s, &spread, &noise);
        let mut root = empty_root();
        listing(&mut root, &parts);
        let (c0, c1) = (base.0.min(base.1), base.0.max(base.1));
        if c0 < c1 {
            root.covered = vec![Covered { level: 2, from: T0 + c0 * QUARTER, to: T0 + c1 * QUARTER }];
        }
        let policy = Policy { late_window: DAY, minute_keep: 7 * DAY };
        let now = if late { T0 + 2 * DAY + 3 * 3_600 * SECOND } else { T0 + 40 * DAY };
        let (root, parts) = settle(root, parts, now, &policy, s.p);
        let vis = visible(&root, &parts).unwrap();
        check_rollups(&root, &vis, &[1], |_| s.p);

        let (a, b) = (span.0.min(span.1), span.0.max(span.1));
        let (a, b) = (T0 + a * QUARTER, T0 + b * QUARTER);
        let tiles = cover(&root, &vis, 1, a, b);
        let mut acc = Node::empty(s.p.kind, a, a);
        let mut x = a;
        let mut quarters = 0;
        let mut hours = 0;
        for t in &tiles {
            let n = match *t {
                Tile::Node { level, start, end, seen } => {
                    prop_assert_eq!(start, x);
                    prop_assert!(covered(&root, level, start, end));
                    quarters += usize::from(level == 2);
                    hours += usize::from(level == 3);
                    prop_assert!(seen.is_none_or(|v| !v.stale));
                    x = end;
                    seen.and_then(|v| v.node.clone()).unwrap_or_else(|| Node::empty(s.p.kind, start, end))
                }
                Tile::Facts { start, end } => {
                    prop_assert_eq!(start, x);
                    for q in (start..end).step_by(QUARTER as usize) {
                        prop_assert!(!covered(&root, 2, q, q + QUARTER));
                    }
                    x = end;
                    fold(&s.p, start, end, within(&s.readings, start, end))
                }
            };
            acc = merge_nodes(&s.p, &acc, &n);
        }
        prop_assert_eq!(x, b);
        prop_assert_eq!(acc, fold(&s.p, a, b, within(&s.readings, a, b)));
        if covered(&root, 4, T0, T0 + 2 * DAY) {
            prop_assert!(quarters <= 14 && hours <= 22, "{} {}", quarters, hours);
        }
    }
}

#[test]
fn a_root_listing_a_part_not_supplied_is_refused() {
    let p = part_of(10, 1, &[(1, 2, 0, 0, 3)]);
    let mut root = empty_root();
    listing(&mut root, std::slice::from_ref(&p));
    assert_eq!(visible(&root, &[]), Err(SetError::Missing(10)));
}

#[test]
fn an_open_floor_sends_the_whole_span_to_facts() {
    let mut root = empty_root();
    root.covered = vec![Covered {
        level: 2,
        from: T0,
        to: T0 + DAY,
    }];
    root.floors = vec![Floor {
        series: 1,
        seq: 0,
        rebuilding: true,
    }];
    let vis = BTreeMap::new();
    assert_eq!(
        cover(&root, &vis, 1, T0, T0 + DAY),
        vec![Tile::Facts {
            start: T0,
            end: T0 + DAY
        }]
    );
    assert_eq!(cover(&root, &vis, 2, T0, T0 + QUARTER).len(), 1);
}
