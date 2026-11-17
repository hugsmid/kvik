//! Values by series id: a vector for the dense ids a store assigns from 1, a tree for any id above [`DENSE`], so that
//! a per-fact update is one indexed access and a stray large id costs no more than a tree entry.

use std::collections::BTreeMap;

/// Ids below this are held in the vector.
const DENSE: u32 = 1 << 16;

/// See the module documentation. Iteration is in id order.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ById<V> {
    dense: Vec<Option<V>>,
    sparse: BTreeMap<u32, V>,
}

impl<V> Default for ById<V> {
    fn default() -> Self {
        ById {
            dense: Vec::new(),
            sparse: BTreeMap::new(),
        }
    }
}

impl<V> ById<V> {
    pub(crate) fn get(&self, id: u32) -> Option<&V> {
        match self.dense.get(id as usize) {
            Some(v) => v.as_ref(),
            None if id < DENSE => None,
            None => self.sparse.get(&id),
        }
    }

    /// The value of `id`, inserted with `f` if absent.
    pub(crate) fn get_or_insert_with(&mut self, id: u32, f: impl FnOnce() -> V) -> &mut V {
        if id >= DENSE {
            return self.sparse.entry(id).or_insert_with(f);
        }
        let i = id as usize;
        if i >= self.dense.len() {
            self.dense.resize_with(i + 1, || None);
        }
        self.dense[i].get_or_insert_with(f)
    }

    pub(crate) fn insert(&mut self, id: u32, v: V) {
        if id >= DENSE {
            self.sparse.insert(id, v);
            return;
        }
        let i = id as usize;
        if i >= self.dense.len() {
            self.dense.resize_with(i + 1, || None);
        }
        self.dense[i] = Some(v);
    }

    pub(crate) fn retain(&mut self, mut keep: impl FnMut(u32, &V) -> bool) {
        for (i, slot) in self.dense.iter_mut().enumerate() {
            if slot.as_ref().is_some_and(|v| !keep(i as u32, v)) {
                *slot = None;
            }
        }
        self.sparse.retain(|id, v| keep(*id, v));
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (u32, &V)> {
        self.dense
            .iter()
            .enumerate()
            .filter_map(|(i, v)| v.as_ref().map(|v| (i as u32, v)))
            .chain(self.sparse.iter().map(|(id, v)| (*id, v)))
    }

    pub(crate) fn into_iter(self) -> impl Iterator<Item = (u32, V)> {
        self.dense
            .into_iter()
            .enumerate()
            .filter_map(|(i, v)| v.map(|v| (i as u32, v)))
            .chain(self.sparse)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    proptest::proptest! {
        #[test]
        fn a_map_by_id_holds_what_a_tree_holds(
            ops in proptest::collection::vec(
                (proptest::prop_oneof![0u32..100, (DENSE - 3)..(DENSE + 3), proptest::num::u32::ANY], 0u8..3),
                0..200,
            ),
        ) {
            let mut m = ById::default();
            let mut t = BTreeMap::new();
            for (id, op) in ops {
                match op {
                    0 => {
                        *m.get_or_insert_with(id, || 0u32) += 1;
                        *t.entry(id).or_insert(0u32) += 1;
                    }
                    1 => {
                        m.insert(id, id);
                        t.insert(id, id);
                    }
                    _ => {
                        m.retain(|k, _| k % 3 != id % 3);
                        t.retain(|k, _| k % 3 != id % 3);
                    }
                }
                proptest::prop_assert_eq!(m.get(id), t.get(&id));
            }
            let got: Vec<_> = m.iter().map(|(k, v)| (k, *v)).collect();
            let want: Vec<_> = t.iter().map(|(k, v)| (*k, *v)).collect();
            proptest::prop_assert_eq!(&got, &want);
            proptest::prop_assert_eq!(m.into_iter().collect::<Vec<_>>(), want);
        }
    }
}
