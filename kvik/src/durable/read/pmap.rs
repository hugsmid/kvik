//! A persistent ordered map (query design §3.5, Q-900): a B-tree of reference-counted nodes, so a clone shares every
//! node and a change copies only the nodes on the path it touches.

use std::fmt;
use std::ops::Bound;
use std::sync::Arc;

/// The most entries a node holds.
pub(crate) const NODE: usize = 64;

#[derive(Clone)]
enum Node<K, V> {
    Leaf(Vec<(K, V)>),
    /// Children, each with the lowest key it holds.
    Branch(Vec<(K, Arc<Node<K, V>>)>),
}

/// See the module documentation.
#[derive(Clone)]
pub(crate) struct PMap<K, V> {
    root: Option<Arc<Node<K, V>>>,
    len: usize,
}

impl<K, V> Default for PMap<K, V> {
    fn default() -> Self {
        PMap { root: None, len: 0 }
    }
}

impl<K: fmt::Debug + Ord + Clone, V: fmt::Debug + Clone> fmt::Debug for PMap<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

impl<K: Ord + Clone, V: Clone + PartialEq> PartialEq for PMap<K, V> {
    fn eq(&self, other: &Self) -> bool {
        self.len == other.len && self.iter().eq(other.iter())
    }
}

/// Entries copied because a node was shared, on this thread: the cost of a change to a published map.
#[cfg(test)]
pub(crate) mod copied {
    use std::cell::Cell;

    thread_local!(static COPIED: Cell<u64> = const { Cell::new(0) });

    pub(crate) fn add(n: usize) {
        COPIED.with(|c| c.set(c.get() + n as u64));
    }

    /// Entries copied on this thread since it started.
    pub(crate) fn get() -> u64 {
        COPIED.with(Cell::get)
    }
}

fn make_mut<K: Clone, V: Clone>(n: &mut Arc<Node<K, V>>) -> &mut Node<K, V> {
    #[cfg(test)]
    if Arc::strong_count(n) > 1 {
        copied::add(match &**n {
            Node::Leaf(es) => es.len(),
            Node::Branch(cs) => cs.len(),
        });
    }
    Arc::make_mut(n)
}

/// Splits `v` once it holds more than [`NODE`] entries: off its last entry when that is where the insert went (an
/// append fills nodes), else in halves.
fn split<T>(v: &mut Vec<T>, at_end: bool) -> Option<Vec<T>> {
    if v.len() <= NODE {
        return None;
    }
    let at = if at_end { v.len() - 1 } else { v.len() / 2 };
    Some(v.split_off(at))
}

type Split<K, V> = Option<(K, Arc<Node<K, V>>)>;

fn insert_in<K: Ord + Clone, V: Clone>(
    node: &mut Arc<Node<K, V>>,
    k: K,
    v: V,
) -> (Option<V>, Split<K, V>) {
    match make_mut(node) {
        Node::Leaf(es) => match es.binary_search_by(|(e, _)| e.cmp(&k)) {
            Ok(i) => (Some(std::mem::replace(&mut es[i].1, v)), None),
            Err(i) => {
                es.insert(i, (k, v));
                let right = split(es, i + 1 == es.len());
                (
                    None,
                    right.map(|r| (r[0].0.clone(), Arc::new(Node::Leaf(r)))),
                )
            }
        },
        Node::Branch(cs) => {
            let i = child(cs, &k).unwrap_or(0);
            if k < cs[i].0 {
                cs[i].0 = k.clone();
            }
            let (old, below) = insert_in(&mut cs[i].1, k, v);
            let Some(s) = below else {
                return (old, None);
            };
            cs.insert(i + 1, s);
            let right = split(cs, i + 2 == cs.len());
            (
                old,
                right.map(|r| (r[0].0.clone(), Arc::new(Node::Branch(r)))),
            )
        }
    }
}

fn first_key<K, V>(n: &Node<K, V>) -> &K {
    match n {
        Node::Leaf(es) => &es[0].0,
        Node::Branch(cs) => &cs[0].0,
    }
}

/// The index of the first entry of `s` the lower bound admits.
fn lower<K: Ord, T>(s: &[T], key: impl Fn(&T) -> &K, lo: Bound<&K>) -> usize {
    match lo {
        Bound::Unbounded => 0,
        Bound::Included(b) => s.partition_point(|e| key(e) < b),
        Bound::Excluded(b) => s.partition_point(|e| key(e) <= b),
    }
}

/// The index past the last entry of `s` the upper bound admits.
fn upper<K: Ord, T>(s: &[T], key: impl Fn(&T) -> &K, hi: Bound<&K>) -> usize {
    match hi {
        Bound::Unbounded => s.len(),
        Bound::Included(b) => s.partition_point(|e| key(e) <= b),
        Bound::Excluded(b) => s.partition_point(|e| key(e) < b),
    }
}

/// The child of a branch whose keys `k` falls among; `None` below the first.
fn child<K: Ord, V>(cs: &[(K, Arc<Node<K, V>>)], k: &K) -> Option<usize> {
    cs.partition_point(|(f, _)| f <= k).checked_sub(1)
}

fn leaves<'a, K: Ord, V>(
    n: &'a Node<K, V>,
    lo: Bound<&K>,
    hi: Bound<&K>,
    out: &mut Vec<&'a [(K, V)]>,
) {
    match n {
        Node::Leaf(es) => {
            let a = lower(es, |e| &e.0, lo);
            let b = upper(es, |e| &e.0, hi);
            if a < b {
                out.push(&es[a..b]);
            }
        }
        Node::Branch(cs) => {
            // A child holds the keys from its own first key to the next child's.
            let a = match lo {
                Bound::Unbounded => 0,
                Bound::Included(b) | Bound::Excluded(b) => child(cs, b).unwrap_or(0),
            };
            let b = upper(cs, |c| &c.0, hi);
            for (_, c) in cs.get(a..b).unwrap_or_default() {
                leaves(c, lo, hi, out);
            }
        }
    }
}

fn build<K: Clone, V>(mut level: Vec<(K, Arc<Node<K, V>>)>) -> Option<Arc<Node<K, V>>> {
    while level.len() > 1 {
        let mut up = Vec::with_capacity(level.len().div_ceil(NODE));
        let mut it = level.into_iter().peekable();
        while it.peek().is_some() {
            let cs: Vec<_> = it.by_ref().take(NODE).collect();
            up.push((cs[0].0.clone(), Arc::new(Node::Branch(cs))));
        }
        level = up;
    }
    level.pop().map(|(_, n)| n)
}

impl<K: Ord + Clone, V: Clone> PMap<K, V> {
    /// A map of `entries`, which are in key order with no key twice.
    pub(crate) fn from_sorted(entries: impl IntoIterator<Item = (K, V)>) -> Self {
        let mut len = 0;
        let mut level = Vec::new();
        let mut it = entries.into_iter().peekable();
        while it.peek().is_some() {
            let es: Vec<_> = it.by_ref().take(NODE).collect();
            len += es.len();
            level.push((es[0].0.clone(), Arc::new(Node::Leaf(es))));
        }
        PMap {
            root: build(level),
            len,
        }
    }

    /// The number of entries.
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// Whether the map holds no entry.
    pub(crate) fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Inserts `v` under `k`; returns the value it replaced.
    pub(crate) fn insert(&mut self, k: K, v: V) -> Option<V> {
        let Some(root) = self.root.as_mut() else {
            self.root = Some(Arc::new(Node::Leaf(vec![(k, v)])));
            self.len = 1;
            return None;
        };
        let (old, split) = insert_in(root, k, v);
        if let Some(right) = split {
            let left = root.clone();
            *root = Arc::new(Node::Branch(vec![(first_key(&left).clone(), left), right]));
        }
        if old.is_none() {
            self.len += 1;
        }
        old
    }

    /// The value under `k`.
    pub(crate) fn get(&self, k: &K) -> Option<&V> {
        let mut n = self.root.as_deref()?;
        loop {
            match n {
                Node::Leaf(es) => {
                    return es
                        .binary_search_by(|(e, _)| e.cmp(k))
                        .ok()
                        .map(|i| &es[i].1);
                }
                Node::Branch(cs) => {
                    let i = child(cs, k)?;
                    n = &cs[i].1;
                }
            }
        }
    }

    /// The value under `k`, to change; copies the nodes above it that are shared.
    pub(crate) fn get_mut(&mut self, k: &K) -> Option<&mut V> {
        self.get(k)?;
        let mut n = self.root.as_mut()?;
        loop {
            match make_mut(n) {
                Node::Leaf(es) => {
                    let i = es.binary_search_by(|(e, _)| e.cmp(k)).ok()?;
                    return Some(&mut es[i].1);
                }
                Node::Branch(cs) => {
                    let i = child(cs, k)?;
                    n = &mut cs[i].1;
                }
            }
        }
    }

    /// The value under `k`, inserting `make()` first if there is none.
    pub(crate) fn get_or_insert_with(&mut self, k: K, make: impl FnOnce() -> V) -> &mut V {
        if self.get(&k).is_none() {
            self.insert(k.clone(), make());
        }
        self.get_mut(&k).expect("inserted above")
    }

    /// The entries with keys between `lo` and `hi`, in key order; nothing for an empty or inverted range.
    pub(crate) fn range<'a>(
        &'a self,
        lo: Bound<&K>,
        hi: Bound<&K>,
    ) -> impl DoubleEndedIterator<Item = (&'a K, &'a V)> + use<'a, K, V> {
        let mut out = Vec::new();
        if let Some(r) = self.root.as_deref() {
            leaves(r, lo, hi, &mut out);
        }
        out.into_iter().flat_map(|s| s.iter().map(|(k, v)| (k, v)))
    }

    /// Every entry in key order.
    pub(crate) fn iter(&self) -> impl DoubleEndedIterator<Item = (&K, &V)> {
        self.range(Bound::Unbounded, Bound::Unbounded)
    }

    /// Every value in key order.
    pub(crate) fn values(&self) -> impl DoubleEndedIterator<Item = &V> {
        self.iter().map(|(_, v)| v)
    }

    /// The entry with the highest key.
    pub(crate) fn last(&self) -> Option<(&K, &V)> {
        let mut n = self.root.as_deref()?;
        loop {
            match n {
                Node::Leaf(es) => return es.last().map(|(k, v)| (k, v)),
                Node::Branch(cs) => n = &cs.last()?.1,
            }
        }
    }

    /// Keeps the entries `keep` admits; a map it changes is built again (a void, `spec.md` §7 Readers).
    pub(crate) fn retain(&mut self, mut keep: impl FnMut(&K, &V) -> bool) {
        if self.iter().all(|(k, v)| keep(k, v)) {
            return;
        }
        let kept: Vec<(K, V)> = self
            .iter()
            .filter(|(k, v)| keep(k, v))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        *self = PMap::from_sorted(kept);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use proptest::prelude::*;

    use super::*;

    fn bound(b: Option<(bool, u16)>) -> Bound<u16> {
        match b {
            None => Bound::Unbounded,
            Some((true, k)) => Bound::Included(k),
            Some((false, k)) => Bound::Excluded(k),
        }
    }

    #[derive(Clone, Debug)]
    enum Op {
        Insert(u16, u32),
        Snapshot,
        Retain(u16),
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            8 => (0u16..600, any::<u32>()).prop_map(|(k, v)| Op::Insert(k, v)),
            1 => Just(Op::Snapshot),
            1 => (0u16..600).prop_map(Op::Retain),
        ]
    }

    proptest! {
        #[test]
        fn a_map_and_its_snapshots_read_as_ordered_maps_do(
            ops in prop::collection::vec(op(), 0..400),
            ranges in prop::collection::vec(
                (prop::option::of((any::<bool>(), 0u16..600)), prop::option::of((any::<bool>(), 0u16..600))),
                1..8,
            ),
        ) {
            let mut m = PMap::default();
            let mut b = BTreeMap::new();
            let mut snaps = Vec::new();
            for op in ops {
                match op {
                    Op::Insert(k, v) => prop_assert_eq!(m.insert(k, v), b.insert(k, v)),
                    Op::Snapshot => snaps.push((m.clone(), b.clone())),
                    Op::Retain(above) => {
                        m.retain(|k, _| *k <= above);
                        b.retain(|k, _| *k <= above);
                    }
                }
            }
            snaps.push((m, b));
            for (m, b) in &snaps {
                prop_assert_eq!(m.len(), b.len());
                prop_assert_eq!(m.last(), b.last_key_value());
                for k in [0u16, 1, 299, 599] {
                    prop_assert_eq!(m.get(&k), b.get(&k));
                }
                for (lo, hi) in &ranges {
                    let (lo, hi) = (bound(*lo), bound(*hi));
                    let empty = match (lo, hi) {
                        (Bound::Included(a) | Bound::Excluded(a), Bound::Included(z) | Bound::Excluded(z)) => {
                            a > z || (a == z && !matches!((lo, hi), (Bound::Included(_), Bound::Included(_))))
                        }
                        _ => false,
                    };
                    let want: Vec<_> = if empty { Vec::new() } else { b.range((lo, hi)).collect() };
                    let got: Vec<_> = m.range(lo.as_ref(), hi.as_ref()).collect();
                    prop_assert_eq!(&got, &want);
                    let back: Vec<_> = m.range(lo.as_ref(), hi.as_ref()).rev().collect();
                    prop_assert_eq!(back, want.into_iter().rev().collect::<Vec<_>>());
                }
            }
        }
    }

    #[test]
    fn a_change_to_a_shared_map_copies_one_node_a_level_and_leaves_the_snapshot_as_it_was() {
        let mut m = PMap::from_sorted((0..100_000u32).map(|k| (k, k)));
        let snap = m.clone();
        let before = copied::get();
        *m.get_mut(&50_000).unwrap() = 7;
        m.insert(100_000, 1);
        let levels = 3; // 64^2 < 100,000 <= 64^3
        assert!(copied::get() - before <= 2 * levels * NODE as u64);
        assert_eq!(snap.get(&50_000), Some(&50_000));
        assert_eq!(snap.get(&100_000), None);
        assert_eq!(m.get(&50_000), Some(&7));
        assert_eq!(m.len(), 100_001);
    }

    #[test]
    fn appended_keys_fill_their_nodes() {
        let mut m = PMap::default();
        for k in 0..(NODE * NODE) as u32 {
            m.insert(k, ());
        }
        let Some(Node::Branch(cs)) = m.root.as_deref() else {
            panic!("a branch")
        };
        assert_eq!(cs.len(), NODE);
    }
}
