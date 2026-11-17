//! Commit-times blocks (`0x32`, `docs/format-compacted.md` B.1): one row per batch the file holds a position of,
//! in a body shaped as a chunk block body (`docs/format-chunk.md` C.2) after the generation.

use super::error::{Bad, FileElement};
use crate::bytes::{Reader, put_leb128};
use crate::ints::{
    decode_delta, decode_plain, decode_run_length, delta_len, encode_delta, encode_plain,
    encode_run_length,
};
use crate::{MAX_ROWS, encoding};

/// One batch, as its commit block gave it (`format.md` A.7): its whole range, receive time and commit time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Batch {
    /// Its generation.
    pub generation: u32,
    /// Its first sequence.
    pub first_sequence: u64,
    /// Its last sequence, inclusive.
    pub last_sequence: u64,
    /// `received_at` of its data blocks.
    pub received_at: i64,
    /// Its commit time, never below `received_at`.
    pub commit_time: i64,
}

impl Batch {
    /// Whether it holds the sequence `s` of its generation.
    pub fn holds(&self, generation: u32, s: u64) -> bool {
        self.generation == generation && self.first_sequence <= s && s <= self.last_sequence
    }
}

const FIRST: u8 = 1;
const SPAN: u8 = 2;
const RECEIVED: u8 = 3;
const LAG: u8 = 4;

/// The shorter of plain and delta; plain wins a tie (C.9).
fn wide(id: u8, values: &[i64]) -> (u8, u8, Vec<u8>) {
    if delta_len(values) < values.len() * 8 {
        (id, encoding::DELTA, encode_delta(values))
    } else {
        (id, encoding::PLAIN, encode_plain(values))
    }
}

/// The shortest of plain, run-length and delta; a tie goes to the lower encoding id (C.9).
fn shortest(id: u8, values: &[i64]) -> (u8, u8, Vec<u8>) {
    let mut best = (encoding::PLAIN, encode_plain(values));
    let rle = encode_run_length(values);
    if rle.len() < best.1.len() {
        best = (encoding::RUN_LENGTH, rle);
    }
    if delta_len(values) < best.1.len() {
        best = (encoding::DELTA, encode_delta(values));
    }
    (id, best.0, best.1)
}

/// The canonical body of a block of 1 to 4,096 rows of one generation; `extra` adds a plain column (golden files
/// of unknown columns only).
pub(crate) fn encode_times(generation: u32, rows: &[Batch], extra: Option<(u8, &[u8])>) -> Vec<u8> {
    let firsts: Vec<i64> = rows.iter().map(|r| r.first_sequence as i64).collect();
    let spans: Vec<i64> = rows
        .iter()
        .map(|r| (r.last_sequence - r.first_sequence) as i64)
        .collect();
    let received: Vec<i64> = rows.iter().map(|r| r.received_at).collect();
    let lags: Vec<i64> = rows.iter().map(|r| r.commit_time - r.received_at).collect();
    let mut cols = vec![
        wide(FIRST, &firsts),
        wide(SPAN, &spans),
        wide(RECEIVED, &received),
        shortest(LAG, &lags),
    ];
    if let Some((id, payload)) = extra {
        cols.push((id, encoding::PLAIN, payload.to_vec()));
        cols.sort_by_key(|c| c.0);
    }
    let mut b = Vec::with_capacity(8 + cols.iter().map(|c| 5 + c.2.len()).sum::<usize>());
    b.extend_from_slice(&generation.to_le_bytes());
    b.extend_from_slice(&(rows.len() as u16).to_le_bytes());
    b.push(cols.len() as u8);
    for (id, enc, payload) in &cols {
        b.push(*id);
        b.push(*enc);
        put_leb128(&mut b, payload.len() as u64);
    }
    for (_, _, payload) in &cols {
        b.extend_from_slice(payload);
    }
    b
}

/// Decodes a body: its generation and rows. Columns 1–4 are mandatory, an unknown id below `0x80` is a version
/// refusal and one at or above it is skipped; rows ascend and do not overlap, a commit time is never below its
/// receive time, and commit times do not decrease. The flag says an ignorable column was skipped.
pub(crate) fn decode_times(body: &[u8]) -> Result<(u32, Vec<Batch>, bool), Bad> {
    let bad = || Bad::body("commit-times block");
    let mut r = Reader::new(body);
    let generation = r.u32().map_err(|_| bad())?;
    let n = usize::from(r.u16().map_err(|_| bad())?);
    if n == 0 || n > MAX_ROWS {
        return Err(bad());
    }
    let count = r.u8().map_err(|_| bad())?;
    let mut dir = Vec::with_capacity(usize::from(count));
    let mut total = 0usize;
    for _ in 0..count {
        let id = r.u8().map_err(|_| bad())?;
        let enc = r.u8().map_err(|_| bad())?;
        let len = r.leb128(3).map_err(|_| bad())? as usize;
        total += len;
        dir.push((id, enc, len));
    }
    if let Some(&(id, _, _)) = dir
        .iter()
        .find(|&&(id, _, _)| id < 0x80 && !(FIRST..=LAG).contains(&id))
    {
        return Err(Bad::Unsupported(FileElement::TimesColumn(id)));
    }
    if !dir.windows(2).all(|w| w[0].0 < w[1].0) {
        return Err(bad());
    }
    if body.len() - r.position() != total {
        return Err(bad());
    }
    let skipped = dir.iter().any(|c| c.0 >= 0x80);
    let mut cols: [Option<Vec<i64>>; 5] = Default::default();
    for (id, enc, len) in dir {
        let payload = r.bytes(len).map_err(|_| bad())?;
        let allowed = match id {
            FIRST | SPAN | RECEIVED => matches!(enc, encoding::PLAIN | encoding::DELTA),
            LAG => matches!(
                enc,
                encoding::PLAIN | encoding::RUN_LENGTH | encoding::DELTA
            ),
            _ => continue,
        };
        if !allowed {
            return Err(bad());
        }
        let values = match enc {
            encoding::PLAIN => decode_plain::<i64>(payload, n),
            encoding::RUN_LENGTH => decode_run_length::<i64>(payload, n),
            _ => decode_delta(payload, n),
        }
        .map_err(|_| bad())?;
        cols[usize::from(id)] = Some(values);
    }
    let [_, Some(firsts), Some(spans), Some(received), Some(lags)] = cols else {
        return Err(bad());
    };
    let mut rows = Vec::with_capacity(n);
    for i in 0..n {
        let first = firsts[i] as u64;
        let last = first.checked_add(spans[i] as u64).ok_or_else(bad)?;
        let commit = received[i].checked_add(lags[i]).ok_or_else(bad)?;
        if lags[i] < 0 {
            return Err(bad());
        }
        let row = Batch {
            generation,
            first_sequence: first,
            last_sequence: last,
            received_at: received[i],
            commit_time: commit,
        };
        if rows.last().is_some_and(|p: &Batch| {
            p.last_sequence >= row.first_sequence || p.commit_time > row.commit_time
        }) {
            return Err(bad());
        }
        rows.push(row);
    }
    Ok((generation, rows, skipped))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows() -> Vec<Batch> {
        (0..300u64)
            .map(|i| Batch {
                generation: 3,
                first_sequence: 1 + 5 * i,
                last_sequence: 5 + 5 * i,
                received_at: 1_000 + 7 * i as i64,
                commit_time: 1_002 + 7 * i as i64,
            })
            .collect()
    }

    #[test]
    fn commit_times_round_trip_with_the_shortest_encodings() {
        let b = encode_times(3, &rows(), None);
        assert_eq!(&b[..4], &3u32.to_le_bytes());
        // Column 4 holds one run: run-length is the shortest.
        assert_eq!(decode_times(&b), Ok((3, rows(), false)));
        let mut at = 7;
        let mut encs = vec![];
        for _ in 0..4 {
            encs.push((b[at], b[at + 1]));
            at += 2;
            while b[at] & 0x80 != 0 {
                at += 1;
            }
            at += 1;
        }
        assert_eq!(
            encs,
            [
                (1, encoding::DELTA),
                (2, encoding::DELTA),
                (3, encoding::DELTA),
                (4, encoding::RUN_LENGTH)
            ]
        );
    }

    #[test]
    fn unknown_columns_follow_the_critical_rule() {
        let ignorable = encode_times(3, &rows()[..2], Some((0x90, &[1, 2, 3])));
        assert_eq!(
            decode_times(&ignorable),
            Ok((3, rows()[..2].to_vec(), true))
        );
        let critical = encode_times(3, &rows()[..2], Some((0x05, &[1])));
        assert_eq!(
            decode_times(&critical),
            Err(Bad::Unsupported(FileElement::TimesColumn(5)))
        );
    }

    #[test]
    fn overlapping_rows_and_a_negative_lag_are_invalid() {
        let mut r = rows()[..2].to_vec();
        r[1].first_sequence = r[0].last_sequence;
        assert!(decode_times(&encode_times(3, &r, None)).is_err());
        let mut r = rows()[..2].to_vec();
        r[1].commit_time = r[1].received_at - 1;
        assert!(decode_times(&encode_times(3, &r, None)).is_err());
    }
}
