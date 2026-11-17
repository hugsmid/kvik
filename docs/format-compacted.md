# kvik file format, v1: compacted segments and chunk index objects

The compacted segment (`kind = 2`) and its chunk index object (`kind = 14`), outlined in `format.md` Part B. Written
by roadmap 6.2a1 and implemented by the `file` module of the `kvik-chunk` crate. A chunk's data block body and its
column encodings are in [`format-chunk.md`](format-chunk.md). The common rules of `format.md` Part A apply; where
this document and `spec.md` differ, `spec.md` wins.

## B.1 Compacted segment (`kind = 2`, `format_major` 1, `format_minor` 0)

Name: `data/<generation:8 hex>-<file_id:16 hex>.kvc`; while being written, the same name with `.tmp` appended.
`<generation>` is the generation in which the file was created; `base_sequence` is 0. The file is an A.1 header, then
A.2 blocks in this order, then the end marker of A.9. No block is skippable.

| Type | Block | Count |
|---:|---|---|
| `0x36` | description | 1, first |
| `0x30`, `0x31` | chunk data blocks, then that chunk's directory | per chunk |
| `0x37`, `0x02` | generation marker, then entries blocks (A.6 body) | per generation with entries |
| `0x32` | commit times | 1 or more |
| `0x33` | commit-time directory | 1 |
| `0x34` | chunk index | 0 or more |
| `0x35` | index root | 1 |
| `0x7F` | trailer | 1, last |

The chunks lie from the end of the description to the first generation marker, or with no entries to the first
commit-times block: the **chunk section**. The markers and entries blocks are the **entries section**.

### Description (`0x36`)

A field list (A.6 framing), fields in ascending id order: 1 class `u8` (1 current, 2 revision); 2 level `u8` (0 L0,
1 hour, 2 day, 3 week, 4 sparse); 3 period start `i64` and 4 period length `i64` (UTC nanoseconds; the length above
0, their sum inside `i64`); 5 run id `u64` (the lowest `file_id` among the outputs published together; a replacement
keeps the run id and count of the file it replaces) and 6 the number of those outputs `u32`, at least 1; 7 the
inputs, 12 bytes each as in A.14's file description (kind `u8`, 1 or 2, 3 reserved, `file_id` `u64`), strictly
ascending, at least 1 and at most 1,024, never the file itself; 8 and 9 the first and last series name of a split
part, UTF-8 of 1 to 255 bytes, the first at or below the last as bytes (both absent when the period is one file);
11 present, with no value, when the file holds entries (current files only); 12 present, with no value, when it holds
set-aside chunks (revision files only). Fields 1 to 7 are mandatory. An unknown class or level, like an unknown field
below `0x8000`, is a version refusal; a field at or above `0x8000` is skipped. Together with the header and trailer
this is the file's self-description (A.14): kind, class, `file_id`, generation, position range and inputs. The
plaintext SHA-256 is over every byte of the file, header to end marker, and is held by the manifest's file
description and by the file's chunk index object (B.2), not by the file itself.

### Chunk

A chunk holds facts of one series: in a current file with strictly ascending `t` (one fact per key), in a revision
file ascending by `t`, then generation, then sequence. It is 1 to 64 data blocks (`0x30`, body as `format-chunk.md`
C.2), every block but the last holding exactly 4,096 rows, followed by one directory block. A series with more than
262,144 rows in a file has consecutive chunks, each full but the last. Set-aside facts (`spec.md` §8 Restore) known as
such when the file is written are in chunks of their own, split the same way and flagged, in revision files only.

### Chunk directory (`0x31`)

| Offset | Size | Field |
|---:|---:|---|
| 0 | 4 | `series_id` |
| 4 | 2 | `flags`: bit 2 `set_aside`, bit 3 `has_summary`; an undefined bit is critical |
| 6 | 2 | `block_count` (1 ..= 64) |
| 8 | 4 | `row_count` |
| 12 | 4 | `summary_len` (0 without bit 3) |
| 16 | 2 | `record_size` (32 in v1; a reader accepts a longer record and ignores its tail) |
| 18 | 6 | reserved |
| 24 | `record_size` × `block_count` | block records |
| … | `summary_len` | the summary: one node as `format-tree.md` T.2 |

The body is exactly `24 + record_size × block_count + summary_len` bytes. Block record: `first_t: i64`, `last_t: i64`,
`offset: u32` (of the block's header, from the chunk's first byte), `body_len: u32`, `row_count: u16` (1 ..= 4,096),
reserved `u16`, `body_crc32c: u32` (the data block's). The records' row counts sum to `row_count`. The summary field is
defined and a v1 writer leaves it empty (`summary_len` 0, bit 3 clear, Q-803); the item that lays out the tree files
says when it is written and when a reader may use it; until then a reader does not read it.

### Entries

Per generation, in ascending order: a marker (`0x37`, 8 bytes: `generation: u32`, `block_count: u32`, at least 1;
a reader ignores a longer body's tail) and that many entries blocks holding the non-fact entries of that generation
that the file holds, in position order, grouped as A.16's canonical writing groups them (one batch, consecutive
sequences, a recovery entry alone, at most 16 MiB a body). An entries block's `received_at` is its batch's. Entries are
in current files only. A file with entries and no chunk is valid. The section is read whole: it is bound to the
trailer by a CRC32C over its bytes, from the first marker's header to the first commit-times block's (padding
included), and holds exactly `entry_count − fact_count` positions.

### Commit times (`0x32`)

`generation: u32`, then a body shaped as `format-chunk.md` C.2 (`row_count` 1 ..= 4,096, `column_count`, the
directory, the payloads) with one row per batch and four columns: 1 `first_sequence` (`u64`; encodings 0, 2), 2
`last_sequence − first_sequence` (`u64`; 0, 2), 3 `received_at` (`i64`; 0, 2), 4 `commit_time − received_at` (`i64`,
never negative; 0, 1, 2). Ids ascend; an unknown id below `0x80` is critical, at or above it ignorable. Encodings are
chosen as C.9 chooses them: columns 1 to 3 delta when its payload is shorter than plain's, else plain; column 4 the
shortest of plain, run-length and delta, a tie going to the lower encoding id. A batch has a row exactly when the
file holds at least one of its positions; the row gives the batch's whole range as its commit block did (A.7). Rows
ascend by generation, then `first_sequence`, and do not overlap; within a generation commit times do not decrease
(A.7 by construction; here a reader's check). A block holds one generation and every block but the last of a
generation is full. A row of the file (fact or entry) takes its `received_at` and commit time from the batch row that
contains its position; a position no row contains makes the file invalid.

### Commit-time directory (`0x33`)

`record_size: u16` (48; a longer record's tail is ignored), reserved `u16`, `block_count: u32` (at least 1), then per
commit-times block, in file order: `generation: u32`, `body_crc32c: u32` (that block's), `first_sequence: u64` (its
first row's), `last_sequence: u64` (its last row's last), `first_commit_time: i64` and `last_commit_time: i64` (its
first and last row's), `offset: u64` (file offset of the block header).

### Chunk index (`0x34`)

`record_size: u16` (96; a longer record's tail is ignored), `entry_count: u16` (1 ..= 1,024; every block but the last
is full), reserved `u32`, then the entries, strictly ascending by `series_id`, then `min_t`, then first position:

| Offset | Size | Field |
|---:|---:|---|
| 0 | 4 | `series_id` |
| 4 | 2 | `flags`: bit 0 `has_bounds`, bit 1 `has_nan`, bit 2 `set_aside`, bit 3 `has_summary`; an undefined bit is critical |
| 6 | 1 | `value_type` of the bounds (1–4 as A.5; 0 exactly when there are no bounds) |
| 7 | 1 | `scale` of decimal bounds (0–18), else 0 |
| 8 | 8 | `min_t` |
| 16 | 8 | `max_t` |
| 24 | 8 | `reach` (the greatest reach of a row, as A.8 defines it) |
| 32 | 4 | generation of the least position |
| 36 | 4 | generation of the greatest position |
| 40 | 8 | sequence of the least position |
| 48 | 8 | sequence of the greatest position |
| 56 | 8 | `chunk_offset` (file offset of the chunk's first block header) |
| 64 | 4 | `chunk_len` (through the end of its directory block, padding included) |
| 68 | 4 | `dir_len` (the directory block, padding included, is the last `dir_len` bytes of the chunk) |
| 72 | 4 | `row_count` |
| 76 | 4 | `dir_crc32c` (the directory block's `body_crc32c`) |
| 80 | 8 | `min_value` |
| 88 | 8 | `max_value` |

Bits 2 and 3 repeat the directory's flags. `min_value` and `max_value` are 0 without bounds.

**Bounds** (Q-802). Over every row that has a value word, whatever its quality; a row flagged `no_value` takes no
part. They are written only when every such row has one value type and that type is not text.
- `f64`: the bit patterns of the least and greatest values in IEEE 754 total order with NaN left out (so −0.0 is
  below +0.0; infinities take part). If any value word is a NaN, `has_nan` is set. If every value word is a NaN there
  are no bounds, and `has_nan` is set. `has_nan` is set only on a chunk whose valued rows are all `f64`. A reader
  compares bounds numerically, so a predicate on zero matches either sign, and treats `has_nan` as "a row that
  satisfies no ordered comparison exists".
- Decimal: `i64` at `scale`, the highest stored scale among the rows that take part, each value multiplied exactly by
  the power of ten that brings it to that scale. If any product leaves `i64`, there are no bounds: bounds never
  saturate. A reader rescales its predicate to `scale` exactly, or does not prune.
- State: the least and greatest index, as `u64`. Bool: 0 or 1.

`row_count` counts every row. The entry is a bound for predicates on `t`, position and value only; quality, flags and
rank are tested on decoded rows.

### Index root (`0x35`)

`record_size: u16` (32; a longer record's tail is ignored), reserved `u16`, `block_count: u32` (0 with no chunk),
then per index block: `first_series_id: u32`, `last_series_id: u32`, `first_min_t: i64`, `offset: u64` (from the
first index block's header), `entry_count: u32`, `body_crc32c: u32` (that index block's). A reader finds a series by
one read of the root and one read of each index block whose series range holds it; the index is never loaded whole.

### Trailer (`0x7F`)

144 bytes in v1; a reader accepts a longer body and ignores its tail.

| Offset | Size | Field |
|---:|---:|---|
| 0 | 8 | `entry_count`: positions held, facts and entries (at least 1) |
| 8 | 8 | `fact_count` |
| 16 | 12 | least position held |
| 28 | 12 | greatest position held |
| 40 | 8 | `first_commit_time`: the least commit time of a batch row |
| 48 | 8 | `last_commit_time`: the greatest commit time of a batch row |
| 56 | 8 | `min_t` (0 with no fact) |
| 64 | 8 | `max_reach` (0 with no fact) |
| 72 | 4 | `chunk_count` |
| 76 | 4 | commit-times block count |
| 80 | 8 | `entries_offset`: the first marker; 0 with no entries |
| 88 | 8 | `times_offset`: the first commit-times block |
| 96 | 8 | `times_dir_offset` |
| 104 | 8 | `index_offset`: the first index block; 0 with no chunk |
| 112 | 8 | `index_root_offset` |
| 120 | 4 | `file_crc32c` over every byte from offset 0 to the trailer's block header |
| 124 | 4 | the index root's `body_crc32c` |
| 128 | 4 | the commit-time directory's `body_crc32c` |
| 132 | 4 | the description's `body_crc32c` |
| 136 | 4 | `entries_crc32c`: CRC32C of the entries section; 0 with no entries |
| 140 | 4 | reserved |

The end marker follows as in A.9.

### Order of chunks

By the series' primary name in force at the file's greatest position, as UTF-8 bytes; then series with no definition
(orphaned facts), by id; within a series by `min_t`, then first position (a set-aside chunk sorts among the series'
other chunks).

### Canonical writing

For given rows, registry state at the greatest position (names and reach), class, level, period and inputs, a v1
writer produces one byte sequence, apart from `file_id`, `created_at`, the run id and the compressed-XOR class
(`format-chunk.md` C.10). Golden files and the manifest rebuild's comparison rely on it.

### A reader's checks, on the path it reads

A failure is an invalid file (`spec.md` §8 Corrupt block); an unknown critical element a version refusal. Nothing is
allocated from a length before it is checked against the file. A block a pointer leads to must be valid (A.2) and of
the stated type, and must lie below the structure that bounds it; where a parent records the child's `body_crc32c` it
must be that one, so a valid block image from another offset or an older file is not accepted.

1. **At open.** The header: magic and checksum, `kind` 2 (else refused), `format_major` (a newer one is a version
   refusal), `store_id`, and the `file_id` and generation of the name (else refused, A.12). The end marker, its
   `trailer_offset` at or above 64; the trailer block ending exactly at the end marker. The trailer: `fact_count` at
   most `entry_count`, positions and commit times ordered, `chunk_count` at most `fact_count`, no fact exactly when no
   chunk and exactly when `index_offset` is 0, `min_t` and `max_reach` 0 with no fact, at least one commit-times
   block, `entries_crc32c` 0 with no entries, and the non-zero section offsets strictly rising, multiples of 8 and
   below the trailer. The description at offset 64, bound by the trailer; field 11 present exactly when
   `entries_offset` is not 0. The commit-time directory, bound by the trailer: as many records as the trailer
   counts, the first at `times_offset`, every one below the directory, ascending and not overlapping, commit times
   not decreasing within a generation, their least and greatest commit time the trailer's, their range covering the
   trailer's positions. The root, bound by the trailer: each record naming 1 to 1,024 entries, records in index order
   with rising offsets, the first at offset 0, every one below the root, the entry counts summing to `chunk_count`.
2. **The manifest's layout** (provisional, Q-810). Where the opener has the layout the manifest records for
   the file, the trailer's counts, ranges and commit times and the description's fields must equal it; a difference
   is refused, naming the field. This ties the trailer to the file it is found in.
3. **An index block**, bound by its root record: as many entries as the record says, its first and last `series_id`
   and first `min_t` the record's, entries in index order. Each entry: an undefined flag bit refused; `min_t` at most
   `max_t`; the least position at most the greatest; `row_count` at least 1 and at most `fact_count`; `has_bounds`
   exactly when `value_type` is not 0; `value_type` at most 4; `scale` 0 but for decimal bounds and at most 18;
   `dir_len` below `chunk_len`; the chunk inside the chunk section, `chunk_offset` and `dir_len` multiples of 8; its
   positions inside the trailer's range, `min_t` at or above the trailer's, `reach` at or below `max_reach`;
   `set_aside` only in a revision file.
4. **A chunk directory**, bound by its index entry's `dir_crc32c`, its block exactly `dir_len` bytes: an undefined
   flag bit refused; 1 to 64 records, the body length as stated, no summary without bit 3, record row counts of 1 to
   4,096 summing to `row_count`, each `first_t` at most `last_t`; its `series_id` and `row_count` the entry's; its
   flags the entry's bits 2 and 3; its block records laid end to end, the first at offset 0 and the last ending where
   the directory starts, so `block_count` is tied to `chunk_len − dir_len`.
5. **A data block**, bound by its block record: its `body_len` the record's; the checks of `format-chunk.md` C.11;
   its row count and first and last `t` the record's.
6. **A commit-times block**, bound by its directory record: the body's checks (columns 1 to 4 present, each in an
   allowed encoding with the checks of C.3 to C.5; last sequence and commit time inside their types; no negative
   lag; rows ascending, not overlapping, commit times not decreasing); its generation, first row's `first_sequence`
   and commit time and last row's last sequence and commit time the record's. A position is found through the one
   record whose generation is the position's and whose range holds it, then the one row that holds it.
7. **The entries section**, bound by `entries_crc32c`: markers of strictly ascending generation with a block count
   of at least 1, each followed by that many valid entries blocks passing A.6, the last ending at `times_offset`;
   positions rising strictly through the section and inside the trailer's range, each inside a batch row whose
   `received_at` its block carries; as many positions as `entry_count − fact_count`.

Damage stays inside what it hits: a damaged chunk fails the reads of that chunk, a damaged commit-times block the
reads of positions in it, a damaged entries section the read of entries; only a damaged description, commit-time
directory, root or trailer fails the open.

### Canonical checks

The writer's read-back, golden files and the scrub read the whole file through the reader's checks, with the
registry state of the file's greatest position, and require: every position held once and no ignored entry; and the
file written again from what it holds (description, header identity and `created_at`, facts by series and set-aside
chunk, entries, batch rows) equal byte for byte, a data block in the XOR class keeping the file's own compressed
payload where that is a canonical body of the same rows (C.10). This covers block order, every block reached by a
pointer, the trailer's counts, the index's order and statistics, data blocks canonical and passing C.11, commit-time
rows ascending and not overlapping, rows only for held batches, and `file_crc32c`.

## B.2 Chunk index object (`kind = 14`)

Name: `data/<generation>-<file_id>.kvi`, with the generation and `file_id` of its compacted segment; `.tmp` appended
while it is written. It is a function of that segment and its hash: its header has the segment's generation,
`file_id`, `created_at` and `store_id`. It is written for upload as its own object (`<sid>/files/0e/…`, A.17) and when
the segment's data leaves the hot tier; a segment on hot is read through its own tail. Blocks, in this order:

- the description (`0x36`, the segment's body unchanged);
- `0x38`: the segment's trailer body, unchanged, then the segment's length `u64` and its plaintext SHA-256 (32 bytes),
  so that a manifest rebuild can restate the record of a segment that is not local;
- the commit-time directory (`0x33`, unchanged: its offsets are the segment's);
- the index blocks and root (unchanged: root offsets are relative, index entries' offsets are the segment's);
- a trailer (`0x7F`, 32 bytes in v1; a longer body's tail is ignored): `index_offset` (0 with no chunk),
  `index_root_offset` and `times_dir_offset`, each in this file, `file_crc32c` over every byte from offset 0 to the
  trailer's block header, and the root's `body_crc32c`;
- the end marker.

**Checks.** The header as B.1's, of kind 14 and the segment's identity; the end marker and trailer; `file_crc32c`,
at every open (the file is small). The description, its `body_crc32c` the segment trailer's; the `0x38` block right
after it, its trailer body passing B.1's trailer checks, field 11 consistent with its `entries_offset`, its
`index_root_offset` below the segment's length; the commit-time directory right after `0x38`, at `times_dir_offset`,
bound by the segment trailer and passing B.1's directory checks; the first index block right after it; the root,
bound by both trailers, and index blocks as in B.1 against the segment's trailer.

**Recording.** The object's size and SHA-256 are fields `0x8001` and `0x8002` of the segment's copy-verified record
(`0x27`, A.14), which is appended only once both the segment and the object are verified in the bucket; so the
object is uploaded and verified before that record, and a restore that finds no such fields falls back to the ranged
read. Where the object is missing, the same blocks are read from the segment's tail by a ranged read at the offsets
its trailer gives.

## B.3 What an implementation must test

Round trip and canonical re-encoding of generated files of every class and level; index and directory statistics equal
to direct computation; truncation at every offset and a flipped bit in every block rejected, failing only the reads
that reach the block; a valid block moved to another place, or taken from an older file, rejected by its parent's
checksum; the layout comparison; as-of time from the commit-time tables equal to as-of time from the log's commit
blocks; golden files: an L0 current file with every value type and encoding, one file of each level, a revision file,
entries in two generations, a file of entries only, a recovery entry, a set-aside chunk, a decimal chunk across a
scale raise, decimal bounds that overflow, a chunk with a NaN word and one of NaN words only, a text chunk and an
orphaned chunk of mixed types (no bounds), state and bool bounds, an interval series' reach, two chunks of one series
(more than 64 blocks), more than 1,024 chunks, more than 4,096 batches, a split part, a week-period L0 file, a longer
trailer, the kind 14 extraction, unknown critical and ignorable fields and columns, an undefined flag bit; fuzz
targets for the body decoders (trailer, description, index, directory, commit times) and the whole-file readers of
both kinds.
