//! A persistent ordered map (Q-900): a B-tree of reference-counted nodes, so a clone shares every
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

/// What the maps of this thread have done since it started: the cost of a change to a published map and of a read.
#[cfg(test)]
pub(crate) mod count {
    use std::cell::Cell;

    /// Counts on this thread.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub(crate) struct Counts {
        /// Entries copied because their node was shared.
        pub(crate) copied: u64,
        /// Bytes those copies took.
        pub(crate) bytes: u64,
        /// Allocations made for nodes, copied or new (two a node: its `Arc` and its entries), and by ranges.
        pub(crate) allocations: u64,
        /// Nodes a range descended into or stepped to.
        pub(crate) visited: u64,
    }

    thread_local!(static COUNTS: Cell<Counts> = const {
        Cell::new(Counts { copied: 0, bytes: 0, allocations: 0, visited: 0 })
    });

    pub(crate) fn add(f: impl FnOnce(&mut Counts)) {
        COUNTS.with(|c| {
            let mut v = c.get();
            f(&mut v);
            c.set(v);
        });
    }

    /// The counts on this thread since it started.
    pub(crate) fn get() -> Counts {
        COUNTS.with(Cell::get)
    }

    /// What happened on this thread during `f`.
    pub(crate) fn during<R>(f: impl FnOnce() -> R) -> (R, Counts) {
        let a = get();
        let r = f();
        let b = get();
        let d = Counts {
            copied: b.copied - a.copied,
            bytes: b.bytes - a.bytes,
            allocations: b.allocations - a.allocations,
            visited: b.visited - a.visited,
        };
        (r, d)
    }
}

/// A node's entries, with room for the one an insert adds before it splits: allocated once at the size it keeps.
fn entries<T: Clone>(from: &[T]) -> Vec<T> {
    let mut v = Vec::with_capacity(NODE + 1);
    v.extend_from_slice(from);
    v
}

fn new_node<K, V>(n: Node<K, V>) -> Arc<Node<K, V>> {
    #[cfg(test)]
    count::add(|c| c.allocations += 2);
    Arc::new(n)
}

/// The node, to change; a copy first when it is shared.
fn make_mut<K: Clone, V: Clone>(n: &mut Arc<Node<K, V>>) -> &mut Node<K, V> {
    if Arc::get_mut(n).is_none() {
        let copy = match &**n {
            Node::Leaf(es) => Node::Leaf(entries(es)),
            Node::Branch(cs) => Node::Branch(entries(cs)),
        };
        #[cfg(test)]
        count::add(|c| {
            let (len, size) = match &copy {
                Node::Leaf(es) => (es.len(), std::mem::size_of::<(K, V)>()),
                Node::Branch(cs) => (cs.len(), std::mem::size_of::<(K, Arc<Node<K, V>>)>()),
            };
            c.copied += len as u64;
            c.bytes += ((NODE + 1) * size) as u64;
        });
        *n = new_node(copy);
    }
    Arc::get_mut(n).expect("unique after the copy")
}

/// Splits `v` once it holds more than [`NODE`] entries: off its last entry when that is where the insert went (an
/// append fills nodes), else in halves.
fn split<T>(v: &mut Vec<T>, at_end: bool) -> Option<Vec<T>> {
    if v.len() <= NODE {
        return None;
    }
    let at = if at_end { v.len() - 1 } else { v.len() / 2 };
    let mut right = Vec::with_capacity(NODE + 1);
    right.extend(v.drain(at..));
    Some(right)
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
                    right.map(|r| (r[0].0.clone(), new_node(Node::Leaf(r)))),
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
                right.map(|r| (r[0].0.clone(), new_node(Node::Branch(r)))),
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

/// A branch's child with the lowest key it holds.
type Child<K, V> = (K, Arc<Node<K, V>>);

/// A place in a map: the branches above a leaf, each with the child taken, the leaf and an entry of it.
struct Cursor<'a, K, V> {
    path: Vec<(&'a [Child<K, V>], usize)>,
    leaf: &'a [(K, V)],
    at: usize,
}

impl<'a, K, V> Cursor<'a, K, V> {
    fn new() -> Self {
        #[cfg(test)]
        count::add(|c| c.allocations += 1);
        Cursor {
            path: Vec::with_capacity(8),
            leaf: &[],
            at: 0,
        }
    }

    fn entry(&self) -> Option<(&'a K, &'a V)> {
        self.leaf.get(self.at).map(|(k, v)| (k, v))
    }

    /// Descends from `n` along its first children (`first`) or its last to a leaf.
    fn descend(&mut self, mut n: &'a Node<K, V>, first: bool) {
        loop {
            #[cfg(test)]
            count::add(|c| c.visited += 1);
            match n {
                Node::Leaf(es) => {
                    self.leaf = es;
                    self.at = if first { 0 } else { es.len().saturating_sub(1) };
                    return;
                }
                Node::Branch(cs) => {
                    let i = if first { 0 } else { cs.len() - 1 };
                    self.path.push((cs, i));
                    n = &cs[i].1;
                }
            }
        }
    }

    /// Moves to the next entry; `None` past the last.
    fn forward(&mut self) -> Option<(&'a K, &'a V)> {
        if self.at + 1 < self.leaf.len() {
            self.at += 1;
            return self.entry();
        }
        while let Some(&(cs, i)) = self.path.last() {
            if i + 1 < cs.len() {
                self.path.last_mut().expect("looked at above").1 = i + 1;
                self.descend(&cs[i + 1].1, true);
                return self.entry();
            }
            self.path.pop();
        }
        None
    }

    /// Moves to the entry before; `None` before the first.
    fn back(&mut self) -> Option<(&'a K, &'a V)> {
        if self.at > 0 {
            self.at -= 1;
            return self.entry();
        }
        while let Some(&(cs, i)) = self.path.last() {
            if i > 0 {
                self.path.last_mut().expect("looked at above").1 = i - 1;
                self.descend(&cs[i - 1].1, false);
                return self.entry();
            }
            self.path.pop();
        }
        None
    }
}

/// The entries of a map between two bounds, read from both ends as they are asked for (Q-900: a read
/// that wants the last fact before a time costs a descent, not the series).
pub(crate) struct Range<'a, K, V> {
    front: Cursor<'a, K, V>,
    back: Cursor<'a, K, V>,
    /// The next entry from each end; `None` once they have met.
    next: Option<(&'a K, &'a V)>,
    last: Option<(&'a K, &'a V)>,
}

impl<'a, K: Ord, V> Range<'a, K, V> {
    fn new(root: Option<&'a Node<K, V>>, lo: Bound<&K>, hi: Bound<&K>) -> Self {
        let mut r = Range {
            front: Cursor::new(),
            back: Cursor::new(),
            next: None,
            last: None,
        };
        let Some(root) = root else { return r };
        r.next = r.front.seek_first(root, lo);
        r.last = r.back.seek_last(root, hi);
        if let (Some((a, _)), Some((z, _))) = (r.next, r.last)
            && a <= z
        {
            return r;
        }
        r.next = None;
        r.last = None;
        r
    }
}

impl<'a, K: Ord, V> Cursor<'a, K, V> {
    /// Places the cursor at the first entry the lower bound admits.
    fn seek_first(&mut self, mut n: &'a Node<K, V>, lo: Bound<&K>) -> Option<(&'a K, &'a V)> {
        loop {
            #[cfg(test)]
            count::add(|c| c.visited += 1);
            match n {
                Node::Leaf(es) => {
                    self.leaf = es;
                    self.at = lower(es, |e| &e.0, lo);
                    return if self.at < es.len() {
                        self.entry()
                    } else {
                        self.at = es.len().saturating_sub(1);
                        self.forward()
                    };
                }
                Node::Branch(cs) => {
                    // A child holds the keys from its own first key to the next child's.
                    let i = match lo {
                        Bound::Unbounded => 0,
                        Bound::Included(b) | Bound::Excluded(b) => child(cs, b).unwrap_or(0),
                    };
                    self.path.push((cs, i));
                    n = &cs[i].1;
                }
            }
        }
    }

    /// Places the cursor at the last entry the upper bound admits.
    fn seek_last(&mut self, mut n: &'a Node<K, V>, hi: Bound<&K>) -> Option<(&'a K, &'a V)> {
        loop {
            #[cfg(test)]
            count::add(|c| c.visited += 1);
            match n {
                Node::Leaf(es) => {
                    self.leaf = es;
                    let end = upper(es, |e| &e.0, hi);
                    return if end > 0 {
                        self.at = end - 1;
                        self.entry()
                    } else {
                        self.at = 0;
                        self.back()
                    };
                }
                Node::Branch(cs) => {
                    let i = upper(cs, |c| &c.0, hi).checked_sub(1)?;
                    self.path.push((cs, i));
                    n = &cs[i].1;
                }
            }
        }
    }
}

impl<'a, K, V> Iterator for Range<'a, K, V> {
    type Item = (&'a K, &'a V);

    fn next(&mut self) -> Option<Self::Item> {
        let e = self.next?;
        if self.last.is_some_and(|l| std::ptr::eq(l.0, e.0)) {
            self.next = None;
            self.last = None;
        } else {
            self.next = self.front.forward();
        }
        Some(e)
    }
}

impl<K, V> DoubleEndedIterator for Range<'_, K, V> {
    fn next_back(&mut self) -> Option<Self::Item> {
        let e = self.last?;
        if self.next.is_some_and(|n| std::ptr::eq(n.0, e.0)) {
            self.next = None;
            self.last = None;
        } else {
            self.last = self.back.back();
        }
        Some(e)
    }
}

fn build<K: Clone, V>(mut level: Vec<(K, Arc<Node<K, V>>)>) -> Option<Arc<Node<K, V>>> {
    while level.len() > 1 {
        let mut up = Vec::with_capacity(level.len().div_ceil(NODE));
        let mut it = level.into_iter().peekable();
        while it.peek().is_some() {
            let mut cs = Vec::with_capacity(NODE + 1);
            cs.extend(it.by_ref().take(NODE));
            up.push((cs[0].0.clone(), new_node(Node::Branch(cs))));
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
            let mut es = Vec::with_capacity(NODE + 1);
            es.extend(it.by_ref().take(NODE));
            len += es.len();
            level.push((es[0].0.clone(), new_node(Node::Leaf(es))));
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
            let mut es = Vec::with_capacity(NODE + 1);
            es.push((k, v));
            self.root = Some(new_node(Node::Leaf(es)));
            self.len = 1;
            return None;
        };
        let (old, split) = insert_in(root, k, v);
        if let Some(right) = split {
            let left = root.clone();
            let mut cs = Vec::with_capacity(NODE + 1);
            cs.push((first_key(&left).clone(), left));
            cs.push(right);
            *root = new_node(Node::Branch(cs));
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
    pub(crate) fn range<'a>(&'a self, lo: Bound<&K>, hi: Bound<&K>) -> Range<'a, K, V> {
        Range::new(self.root.as_deref(), lo, hi)
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
        let ((), c) = count::during(|| {
            *m.get_mut(&50_000).unwrap() = 7;
            m.insert(100_000, 1);
        });
        let levels = 3; // 64^2 < 100,000 <= 64^3
        assert!(c.copied <= 2 * levels * NODE as u64, "{c:?}");
        assert!(c.allocations <= 2 * 2 * levels, "{c:?}");
        assert_eq!(snap.get(&50_000), Some(&50_000));
        assert_eq!(snap.get(&100_000), None);
        assert_eq!(m.get(&50_000), Some(&7));
        assert_eq!(m.len(), 100_001);
    }

    #[test]
    fn a_copied_node_takes_the_insert_that_follows_without_growing() {
        let mut m = PMap::from_sorted((0..NODE as u32).map(|k| (2 * k, ())));
        let snap = m.clone();
        m.insert(1, ());
        let Some(Node::Branch(cs)) = m.root.as_deref() else {
            panic!("a full leaf split under a branch")
        };
        for (_, c) in cs {
            let Node::Leaf(es) = &**c else {
                panic!("a leaf")
            };
            assert_eq!(es.capacity(), NODE + 1);
        }
        assert_eq!(snap.len(), NODE);
    }

    #[test]
    fn a_range_read_from_either_end_visits_a_path_not_the_map() {
        let visits = |n: u32| {
            let m = PMap::from_sorted((0..n).map(|k| (k, ())));
            let mid = n / 2;
            let (back, b) =
                count::during(|| m.range(Bound::Unbounded, Bound::Included(&mid)).next_back());
            let (front, f) =
                count::during(|| m.range(Bound::Excluded(&mid), Bound::Unbounded).next());
            assert_eq!((back, front), (Some((&mid, &())), Some((&(mid + 1), &()))));
            (b, f)
        };
        let (small, large) = (visits(1_000), visits(1_000_000));
        for (s, l) in [(small.0, large.0), (small.1, large.1)] {
            // Both ends descend one path; one level more for a thousand times the entries.
            assert!(
                l.visited <= 2 * 4 && l.visited <= s.visited + 4,
                "{s:?} {l:?}"
            );
            assert_eq!(l.allocations, s.allocations, "{s:?} {l:?}");
            assert_eq!(l.allocations, 2);
        }
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
