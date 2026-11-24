//! A lookup's wanted ranges by series, looked up for every row a lookup decodes (Q-1132).

use std::collections::BTreeMap;

/// The wanted ranges by series: in a table by id when the ids are small, as every row of a read is looked up.
pub(in crate::reader) enum BySeries {
    Table(Vec<u16>, Vec<Vec<(i64, i64)>>),
    Map(BTreeMap<u32, Vec<(i64, i64)>>),
}

/// The greatest series id [`BySeries`] keeps a table for.
const TABLE_IDS: u32 = 4_096;

impl BySeries {
    pub(in crate::reader) fn new(map: BTreeMap<u32, Vec<(i64, i64)>>) -> Self {
        match map.keys().next_back() {
            Some(&max) if max <= TABLE_IDS => {
                let mut table = vec![u16::MAX; max as usize + 1];
                let mut lists = Vec::with_capacity(map.len());
                for (k, (id, ranges)) in map.into_iter().enumerate() {
                    table[id as usize] = k as u16;
                    lists.push(ranges);
                }
                BySeries::Table(table, lists)
            }
            _ => BySeries::Map(map),
        }
    }

    /// Whether `series` has a wanted range.
    pub(super) fn has(&self, series: u32) -> bool {
        match self {
            BySeries::Table(table, _) => table.get(series as usize).is_some_and(|&k| k != u16::MAX),
            BySeries::Map(map) => map.contains_key(&series),
        }
    }

    pub(super) fn wants(&self, series: u32, t: i64) -> bool {
        let ranges = match self {
            BySeries::Table(table, lists) => match table.get(series as usize) {
                Some(&k) if k != u16::MAX => &lists[usize::from(k)],
                _ => return false,
            },
            BySeries::Map(map) => match map.get(&series) {
                Some(r) => r,
                None => return false,
            },
        };
        ranges.iter().any(|(a, z)| (*a..=*z).contains(&t))
    }
}
