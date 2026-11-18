# kvik file format, v1: chunk block bodies and column encodings

The body of one block of a compacted chunk (`format-compacted.md` B.1), with the column encodings decided
by roadmap 2.1 ([`measurements/2.1-chunk-encodings.md`](measurements/2.1-chunk-encodings.md)). Written by roadmap
6.1 and implemented by the `kvik-chunk` crate. The common rules of `format.md` Part A apply; where this document and
`spec.md` differ, `spec.md` wins.

## C.1 Scope
A chunk holds the facts of one series for one time range, current facts or revisions (`spec.md` §7), in blocks of at
most 4,096 rows. This document lays out one block's body. The chunk around it (the A.2 framing of its blocks, its
directory of first and last `t`, the summary), the index, the per-batch commit-time table, the trailer and the
self-description fields are laid out in [`format-compacted.md`](format-compacted.md) (roadmap 6.2a1), whose
commit-time table reuses the body shape and encodings below. A block is checked on its own; that each stored scale is
at most the series' scale in force, and other rules that need the series, are checked by roadmap 6.2.

## C.2 Block body
| Offset | Size | Field |
|---:|---:|---|
| 0 | 2 | `row_count` (1 ..= 4,096) |
| 2 | 1 | `column_count` |
| 3 | … | column directory: per column `column_id: u8`, `encoding: u8`, `length` (minimal unsigned LEB128, at most 3 bytes) |
| … | | the payloads, one after another in directory order, with no padding |

The body ends exactly where the last payload ends. Columns appear in ascending id order. An id `< 0x80` that is not
listed below is critical, `≥ 0x80` ignorable (`format.md` common rules). Ids 2–9 mean what they mean in a facts block
(A.5); a chunk is one series, so it has no `series_id`, and it carries the position as two columns.

| Id | Column | Values | Presence | Encodings |
|---:|---|---|---|---|
| 2 | `t` | `i64`, one per row | mandatory | 0, 2 |
| 3 | `quality` | `u8` 0–3, one per row | optional; absent means all good | 0, 1 |
| 4 | `flags` | `u16`, one per row; bits as A.5, 8–15 undefined and critical | optional; absent means all zero | 0, 1 |
| 5 | `value_type` | `u8` 1–5, one per row | mandatory | 0, 1 |
| 6 | `value` | a 64-bit value word per row **without** flag bit 5 (`no_value`), in row order | mandatory (empty when no row has a value) | 0–2; 3–5 only when at least one row has a value word and every such row has `value_type = 1` |
| 7 | text heap | bytes | exactly when a row has `value_type = 5` | 0 |
| 8 | `changed_at` | `u64` per row **with** flag bit 6, in row order | exactly when such a row exists | 0, 2 |
| 9 | `scale` | `u8` 0–18 per row **with** `value_type = 2`, in row order | exactly when such a row exists | 0, 1 |
| 10 | `generation` | `u32`, one per row | mandatory | 0, 1 |
| 11 | `sequence` | `u64`, one per row | mandatory | 0, 2 |

A value word is, by the row's `value_type`: the IEEE-754 bit pattern (1; every pattern, NaN payloads included); the
two's-complement integer (2; never `i64::MIN`); the state index (3; at most `u16::MAX`); 0 or 1 (4); the text's
length in bytes (5; at most 255). Texts are stored in the heap in row order, none shared, so a text starts where the
one before it ends; each is UTF-8 and together they fill the heap exactly. A row flagged `no_value` holds no value
word, no heap bytes, and is read with value 0; a row without flag bit 6 is read with `changed_at` 0; a row whose type
is not 2 is read with scale 0. The value type is per row, so a block may hold facts of several types (orphaned facts
kept through compaction, `spec.md` §2.1).

## C.3 Encoding 0, plain
The values, each in its column's width, little-endian, one after another.

## C.4 Encoding 1, run-length
As A.5 encoding 1: pairs of a run length (minimal unsigned LEB128 of at most 3 bytes, at least 1) and one value in the
column's width; adjacent runs hold different values; the run lengths sum to the value count. The value column's width
is 8 bytes.

## C.5 Encoding 2, delta
For values `x[0..n]`, as `i64` (a `u64` column is read through two's complement), all arithmetic wrapping:
- nothing if `n = 0`;
- `x[0]` (8 bytes, little-endian);
- the differences `d[i] = x[i] − x[i−1]` for `i ≥ 1`, in frames of 128 (the last may be shorter). Per frame: the
  smallest difference `m`, zigzag-encoded (`(m << 1) XOR (m >> 63)` with an arithmetic shift, so 0, −1, 1, −2 …
  become 0, 1, 2, 3 …) as minimal unsigned LEB128 (at most 10 bytes); a bit width `w` (one byte,
  0–64); then each `d[i] − m` as a `w`-bit unsigned integer, packed least significant bit first, the first value in
  the lowest bits of the first byte, in `ceil(count × w / 8)` bytes whose padding bits are zero.

Canonically `m` is the frame's minimum and `w` the fewest bits that hold the largest `d[i] − m`.

## C.6 Encoding 3, scaled decimal with exceptions
For `f64` bit patterns `v[0..n]` and an exponent `e`:

| Size | Field |
|---:|---|
| 1 | `e` (0–18) |
| 2 | exception count `k` (`k ≤ n`) |
| 10 × `k` | per exception: its index (`u16`, strictly ascending, below `n`) and its bit pattern (`u64`) |
| … | the scaled integers of the other `n − k` values, in row order, in encoding 2 |

A value **fits** `e` when `s = round(f × 10^e)` (the `f64` product, rounded half away from zero) is finite with
`|s| < 2^53` and the `f64` quotient `s / 10^e` has exactly the value's bit pattern; its scaled integer is then `s`.
Every operation is a correctly rounded IEEE-754 operation, so the test gives the same answer on every platform.
−0.0, NaN, the infinities and values that need more than 16 significant digits never fit. A reader computes each
value that is not an exception as the `f64` quotient `s / 10^e`.

Canonically the exceptions are exactly the values that do not fit `e`, and `e` is chosen as C.9 says.

## C.7 Encoding 4, XOR
A bit stream, most significant bit first, its last byte padded with zero bits: the first value's 64 bits; then for
each later value the XOR `x` with the one before it:
- `0` if `x = 0`;
- `10` and bits `t .. 64 − l` of `x` (its middle `64 − l − t` bits) when a window `(l, t)` is set and `x` has at
  least `l` leading and `t` trailing zero bits;
- else `11`, `l = min(leading zeros, 31)` in 5 bits, the meaningful length `64 − l − t` minus 1 in 6 bits (`t` the
  trailing zeros), and the meaningful bits; this sets the window `(l, t)`.

The first `10` of a stream follows an `11`, and `l` plus the meaningful length is at most 64.

## C.8 Encoding 5, XOR compressed
The length of an encoding 4 stream (`u32`, little-endian), then that stream as **exactly one** Zstandard frame
(RFC 8878 §3.1.1): it starts with the frame magic number, declares no dictionary (dictionary id flag 0) and no
content checksum (checksum flag 0; the block's own checksum covers it), and ends where the payload ends, so no
skippable frame and no second frame. The length is at most `ceil((64 + 77 × (n − 1)) / 8)` for `n ≥ 1` values (0 for
none), the longest stream encoding 4 can produce, and the frame decompresses to exactly that many bytes. The format
constrains only what a frame decodes to, which RFC 8878 fixes; the compression level and library are a writer's
choice (this writer: level 3), so compressed bytes are never compared (C.10).

## C.9 Choice of encodings
- `t`, `changed_at`, `sequence`: delta when its payload is shorter than plain's, else plain.
- `quality`, `flags`, `value_type`, `scale`, `generation`: run-length when its payload is shorter than plain's, else
  plain (A.5).
- `value`: the shortest payload among the candidates below; a tie goes to the lower encoding id. The candidates are
  plain, run-length and delta, and, when the block has at least one value word and every row with one has
  `value_type = 1`:
  - decimal at its best exponent: among the distinct smallest exponents at which the block's values fit (C.6), the
    one giving the shortest decimal payload, the lowest on a tie; no decimal candidate if no value fits any exponent;
  - the **XOR class** (encodings 4 and 5), when there is no decimal candidate or it has more than one exception per
    eight values. The class wins when the XOR payload (encoding 4) is strictly shorter than every other candidate;
    no compressed length takes part, so every writer makes the same choice whatever its zstd build. Within the
    class a writer writes its compressed XOR when that is strictly shorter than XOR, else XOR.

## C.10 Canonical body
For given rows a v1 writer produces one body (`format.md` common rules, canonical writing): every mandatory column, an
optional column exactly when its presence rule or a non-default value requires it (`quality` when a row is not good,
`flags` when a row has a flag), each in the encoding C.9 chooses, in canonical form, in ascending id order, with no
unknown column. The XOR class is one canonical class: a value column in encoding 4 or 5 is canonical exactly when C.9
chooses the class and the column holds the canonical encoding 4 stream of the values, as encoding 4 or as an
encoding 5 payload shorter than that stream (what some writer's compressed XOR can be); which of the two, and which
valid frame, is not compared. When C.9 does not choose the class, no column in encoding 4 or 5 is canonical. Any
other body read and encoded again gives the same bytes.

## C.11 Checks
A body that fails one of these is invalid (for a block reached in a valid file, invalid content, `format.md` A.2):
- `row_count` outside 1 ..= 4,096; a directory that does not fit or has a malformed length; ids not strictly
  ascending; payloads that do not end exactly at the end of the body;
- a mandatory column missing; an encoding not allowed for its column (an unknown encoding included, and encodings
  3–5 for a value column whose rows are not all `f64` or that holds no value word); a presence rule broken;
- a payload that does not hold exactly its value count in its encoding: a plain payload of the wrong length; run-length
  form broken (C.4); in delta, a width above 64, a malformed LEB128, a frame cut short, non-zero padding bits or bytes
  left over; in decimal, an exponent above 18, more exceptions than values, exception indexes not strictly ascending
  or not below the count, a scaled integer of magnitude 2^53 or more, or a malformed integer payload; in XOR, a `10`
  before any window, a window wider than 64 bits, a stream cut short, 8 or more bits left over or non-zero padding
  bits; in compressed XOR, a declared length above the bound of C.8, a payload that is not exactly one frame with
  no dictionary and no content checksum (a skippable or second frame included), or a frame that does not decompress
  to exactly the declared length;
- a value out of range: quality above 3, a value type outside 1–5, a scale above 18, `i64::MIN` in a decimal row, a
  state index above `u16::MAX`, a bool other than 0 or 1, a text longer than 255 bytes or not UTF-8, texts that do not
  fill the heap exactly.

Undefined flag bits (8–15) and an unknown critical column are a version refusal (`UnsupportedFormat`), not invalid
content; undefined flag bits are refused before any per-row check. An unknown ignorable column is skipped. A reader applies only these checks; a canonical check (writer
output, golden files, a scrub) also requires the canonical body (C.10). The value count of each column comes from
`row_count` and the flags and value types, never from a payload, and nothing is allocated from a length in the body
before it is checked against the body and the row limit.

## C.12 What an implementation must test
Round trip of every column codec and of generated blocks of every value type and flag combination; canonical
re-encoding; truncation at every offset rejected; damaged bodies rejected or decoded to valid content; golden bodies
for each value encoding, each value type, `changed_at`, scale changes, generations, rows without a value, a full
block, and unknown ignorable and critical columns; a fuzz target for the block decoder and each column decoder.
