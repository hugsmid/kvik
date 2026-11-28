//! What a reader in any process takes from the tree's files (design 7.1b section 6 The cut): the newest root it may
//! use, each part it lists checked against it, and the root less the parts that failed, whose periods it then reads
//! from facts. No lock and nothing from the writer's memory (promise 4).

use std::io;
use std::path::Path;

use kvik_log::fs::Fs;

use crate::frame::FileError;
use crate::part::Part;
use crate::root::{Covered, Root};
use crate::set::part_ref;
use crate::write::{DIR, parse_root, part_name, read_file};

/// Why a listed part cannot be used.
#[derive(Debug, thiserror::Error)]
pub enum PartError {
    /// The file would not read.
    #[error("read: {0}")]
    Io(#[from] io::Error),
    /// The bytes would not decode.
    #[error("decode: {0}")]
    Decode(#[from] FileError),
    /// Length, checksum or trailer differ from what the root says.
    #[error("the part differs from the root's record")]
    Mismatch,
}

/// The newest root under `store_dir` that decodes and that `usable` accepts; `None` when there is none (no tree
/// directory included).
pub fn newest_root(fs: &dyn Fs, store_dir: &Path, usable: impl Fn(&Root) -> bool) -> Option<Root> {
    let dir = store_dir.join(DIR);
    let mut roots: Vec<(u64, String)> = fs
        .read_dir(&dir)
        .ok()?
        .into_iter()
        .filter_map(|e| parse_root(&e.name).map(|s| (s, e.name)))
        .collect();
    roots.sort_unstable_by(|a, b| b.cmp(a));
    roots.into_iter().find_map(|(_, name)| {
        let r = Root::decode(&read_file(fs, &dir.join(name)).ok()?).ok()?;
        usable(&r).then_some(r)
    })
}

/// The part `file_id` of `root`, read and checked against the root's record of it.
pub fn read_part(
    fs: &dyn Fs,
    store_dir: &Path,
    root: &Root,
    file_id: u64,
) -> Result<Part, PartError> {
    let r = root
        .parts
        .iter()
        .find(|p| p.file_id == file_id)
        .ok_or(PartError::Mismatch)?;
    let path = store_dir
        .join(DIR)
        .join(part_name(root.header.generation, file_id));
    let bytes = read_file(fs, &path)?;
    if bytes.len() as u64 != r.byte_len {
        return Err(PartError::Mismatch);
    }
    let part = Part::decode(&bytes)?;
    if part_ref(&part, &bytes) != *r {
        return Err(PartError::Mismatch);
    }
    Ok(part)
}

/// `root` with the parts `damaged` unlisted and their periods taken out of every covered range, so that a reader
/// takes facts there (section 6, `tree_damaged`).
pub fn without(root: &Root, damaged: &[u64]) -> Root {
    let mut out = root.clone();
    let periods: Vec<(i64, i64)> = root
        .parts
        .iter()
        .filter(|p| damaged.contains(&p.file_id))
        .map(|p| {
            (
                p.period_start,
                p.period_start.saturating_add(p.period_len as i64),
            )
        })
        .collect();
    out.parts.retain(|p| !damaged.contains(&p.file_id));
    for (a, b) in periods {
        out.covered = out
            .covered
            .iter()
            .flat_map(|c| {
                let left = Covered {
                    to: c.to.min(a),
                    ..*c
                };
                let right = Covered {
                    from: c.from.max(b),
                    ..*c
                };
                [left, right].into_iter().filter(|x| x.from < x.to)
            })
            .collect();
    }
    out
}

/// `root` covering only what lies in `[from, to)`, for a reader that has the parts of that span alone.
pub fn within(root: &Root, from: i64, to: i64) -> Root {
    let mut out = root.clone();
    out.covered = root
        .covered
        .iter()
        .filter_map(|c| {
            let w = root.levels.iter().find(|l| l.id == c.level)?.width;
            let a = c.from.max(ceil(from, w));
            let b = c.to.min(to - to.rem_euclid(w));
            (a < b).then_some(Covered {
                from: a,
                to: b,
                ..*c
            })
        })
        .collect();
    out
}

fn ceil(x: i64, w: i64) -> i64 {
    match x.rem_euclid(w) {
        0 => x,
        r => x.saturating_add(w - r),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::root::{Covered, PartRef};
    use crate::set::DAY;

    fn root_of(parts: Vec<PartRef>) -> Root {
        Root {
            header: kvik_log::header::FileHeader::new(crate::root::KIND, 1, 3, 0, [7; 16]),
            seq: 3,
            checkpoint: Default::default(),
            reset: Default::default(),
            next_file_id: 9,
            floor: 0,
            levels: crate::LEVELS.to_vec(),
            parts,
            covered: Vec::new(),
            floors: Vec::new(),
            retired: Vec::new(),
        }
    }

    fn part_at(file_id: u64, day: i64) -> PartRef {
        PartRef {
            file_id,
            seq: file_id,
            class: crate::part::Class::Delta,
            file_crc32c: 0,
            period_start: day * DAY,
            period_len: DAY as u64,
            least_start: 0,
            greatest_end: 0,
            least_series: 1,
            greatest_series: 1,
            byte_len: 100,
        }
    }

    #[test]
    fn without_splits_a_covered_range_around_a_damaged_period_and_unlists_the_part() {
        let mut root = root_of(vec![part_at(4, 10), part_at(5, 11)]);
        let p = root.parts[0];
        root.covered = vec![Covered {
            level: 2,
            from: p.period_start - DAY,
            to: p.period_start + p.period_len as i64 + DAY,
        }];
        let out = without(&root, &[p.file_id]);
        assert_eq!(out.parts, vec![root.parts[1]]);
        assert_eq!(
            out.covered,
            vec![
                Covered {
                    level: 2,
                    from: p.period_start - DAY,
                    to: p.period_start,
                },
                Covered {
                    level: 2,
                    from: p.period_start + p.period_len as i64,
                    to: p.period_start + p.period_len as i64 + DAY,
                },
            ]
        );
        assert_eq!(without(&root, &[]), root);
    }

    #[test]
    fn within_trims_each_covered_range_to_the_span_on_its_level_width() {
        let mut root = root_of(Vec::new());
        root.covered = vec![
            Covered {
                level: 2,
                from: 0,
                to: 10 * DAY,
            },
            Covered {
                level: 4,
                from: 0,
                to: 10 * DAY,
            },
            Covered {
                level: 4,
                from: 20 * DAY,
                to: 30 * DAY,
            },
        ];
        let q = 15 * crate::MINUTE;
        let out = within(&root, 2 * DAY + q + 1, 5 * DAY + q + 1);
        assert_eq!(
            out.covered,
            vec![
                Covered {
                    level: 2,
                    from: 2 * DAY + 2 * q,
                    to: 5 * DAY + q,
                },
                Covered {
                    level: 4,
                    from: 3 * DAY,
                    to: 5 * DAY,
                },
            ]
        );
    }
}
