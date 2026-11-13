# kvik file format, v1 (design; not yet implemented)

Byte-level rules for the files kvik writes. The semantics are in [`spec.md`](spec.md); where the two differ,
`spec.md` wins and this document is wrong. Part A (common rules, log segments, the durable mark) is complete
enough to implement. Part B (compacted segments and the remaining files) is an outline that is fixed after the
measurements in the roadmap.

## A. Common rules

- All integers are little-endian. Offsets and lengths are in bytes. Reserved fields are written as zero and
  ignored when read.
- Checksums are CRC32C (Castagnoli polynomial), stored as `u32`.
- Times are `i64` nanoseconds since the Unix epoch, UTC.
- A position is `(generation: u32, sequence: u64)`.
- Every structure is aligned to 8 bytes. Padding is zero bytes and is never covered by a checksum unless stated.

### A.1 File header (64 bytes, every file except the mark)
| Offset | Size | Field | Notes |
|---:|---:|---|---|
| 0 | 4 | `magic` | ASCII `KVIK` |
| 4 | 1 | `kind` | 1 log segment, 2 compacted segment, 3 manifest, 4 cursor, 5 registry, 6 tree |
| 5 | 1 | `format_major` | 1 |
| 6 | 2 | `format_minor` | 0 |
| 8 | 4 | `flags` | reserved |
| 12 | 4 | `generation` | |
| 16 | 8 | `file_id` | segment id for log and compacted segments; unique per kind |
| 24 | 8 | `base_sequence` | log: sequence of the first entry this file may hold; otherwise 0 |
| 32 | 8 | `created_at` | |
| 40 | 16 | `store_id` | random, fixed for the life of a store; a file with a different id is refused |
| 56 | 4 | reserved | |
| 60 | 4 | `header_crc32c` | over bytes 0..60 |

### A.2 Block (16-byte header, then the body)
| Offset | Size | Field | Notes |
|---:|---:|---|---|
| 0 | 1 | `type` | see each file kind; 0 is never a valid type |
| 1 | 1 | `flags` | bit 0 `skippable`: a reader that does not know `type` may skip the block |
| 2 | 2 | reserved | |
| 4 | 4 | `body_len` | at most 16 MiB |
| 8 | 4 | `body_crc32c` | over the `body_len` bytes of the body |
| 12 | 4 | `header_crc32c` | over bytes 0..12 of this header |

The body follows immediately and is padded with zeros to the next multiple of 8. The next block starts there.

**Reading blocks.** At a block boundary: sixteen zero bytes mean *end of data* (unwritten preallocated space).
Otherwise the header checksum must match, the type must be known or skippable, `body_len` must fit in the file,
and the body checksum must match. The first failure ends the readable part of the file; nothing after it is used.

### A.3 Versioning
- A reader refuses a file whose `format_major` is greater than it supports, with an error that names both
  versions. It never guesses.
- Within a major version, additions are new skippable block types, new columns (A.5) and new fields at the end
  of a body. A reader ignores trailing bytes it does not know inside a body whose length says they exist.
- Every released format version has golden files in the repository, read by the current code in CI.

## A.4 Log segment (`kind = 1`)

A log segment is a header followed by **batches**. A batch is one or more data blocks followed by exactly one
commit block. A batch never spans files. Unsealed segments are preallocated (zero-filled) and named
`log/<generation>-<file_id as 16 hex digits>.open`; sealing appends a segment index and a trailer, truncates the
file to its written length, syncs it, renames it to `.kvl`, and syncs the directory.

| Type | Name | Skippable | Role |
|---:|---|---|---|
| `0x01` | facts | no | readings, in arrival order |
| `0x02` | entries | no | definitions, parameters, aliases, retirements, routing changes |
| `0x0F` | commit | no | ends a batch |
| `0x10` | segment index | yes | written at seal: which series the segment holds |
| `0x7F` | trailer | no | written at seal: marks the file as sealed |

**Sequence assignment.** Entries take consecutive sequences in the order they appear: block by block, and row
by row inside a block. The first entry of a batch has the sequence after the previous batch's last; the first
entry of a file has `base_sequence`. An `entries` block that defines a series must come before any `facts`
block that uses it.

### A.5 Facts block body
| Offset | Size | Field |
|---:|---:|---|
| 0 | 8 | `first_sequence` |
| 8 | 8 | `received_at` |
| 16 | 4 | `row_count` (1 ..= 65 536) |
| 20 | 1 | `column_count` |
| 21 | 3 | reserved |
| 24 | 12 × `column_count` | column directory |
| … | | column payloads, each aligned to 8 |

Column directory entry: `column_id: u8`, `encoding: u8`, reserved `u16`, `offset: u32` (from the start of the
body), `length: u32`. A reader skips columns it does not know.

| Id | Column | Width | Encodings allowed in v1 |
|---:|---|---|---|
| 1 | `series_id` | u32 | plain |
| 2 | `t` | i64 | plain |
| 3 | `quality` | u8: 0 good, 1 estimated, 2 suspect, 3 bad | plain, run-length |
| 4 | `flags` | u16 bit set: 0 future, 1 late, 2 revise, 3 host_time, 4 unsynced_clock, 5 no_value | plain, run-length |
| 5 | `value_type` | u8: 1 f64, 2 i64 (scaled), 3 state index, 4 bool, 5 text | plain, run-length |
| 6 | `value` | 8 bytes per row | plain |
| 7 | `text heap` | bytes | plain; present only if a row has `value_type = 5` |

- `value` holds the IEEE-754 bit pattern (f64), the two's-complement integer (i64), the index or 0/1 in the low
  bytes (state, bool), or for text a `u32` offset and a `u32` length into the text heap.
- A row with `no_value` set has `value = 0` and quality bad.
- **Encoding 0, plain:** `row_count` fixed-width values.
- **Encoding 1, run-length:** repeated pairs of an unsigned LEB128 run length and one fixed-width value; the run
  lengths sum to `row_count`.
- `value_type` repeats what the series definition says. It is stored so that a segment can be decoded without
  the registry.

### A.6 Entries block body
| Offset | Size | Field |
|---:|---:|---|
| 0 | 8 | `first_sequence` |
| 8 | 8 | `received_at` |
| 16 | 4 | `entry_count` |
| 20 | 4 | reserved |
| 24 | … | entries |

Each entry: `entry_type: u8`, reserved 3 bytes, `length: u32`, then `length` bytes of fields, padded to 8. Fields
are a list of `field_id: u16`, `field_len: u16`, bytes (padded to 4). Unknown fields are skipped.

| Entry type | Fields |
|---:|---|
| 1 series definition | 1 id `u32`; 2 name; 3 kind `u8`; 4 value type `u8`; 5 decimal scale `u8`; 6 unit; 7 expected interval; 8 staleness limit; 9 record on change `u8`; 10 `wrap_at`; 11 `reset_ceiling`; 12 lateness bound; 13 state names |
| 2 parameter change | 1 id; 20 effective time; then any of fields 7–12 |
| 3 alias | 1 id; 2 name |
| 4 retirement | 1 id; 21 `t_r` |
| 5 routing change | defined with the interface document |

Durations are `i64` nanoseconds. Strings are UTF-8 without a terminator.

### A.7 Commit block body (48 bytes)
| Offset | Size | Field | Notes |
|---:|---:|---|---|
| 0 | 4 | `generation` | |
| 4 | 4 | `block_count` | data blocks in this batch |
| 8 | 8 | `first_sequence` | |
| 16 | 8 | `last_sequence` | inclusive |
| 24 | 8 | `commit_time` | never earlier than the previous batch's |
| 32 | 8 | `batch_bytes` | from the batch's first block header to this block's header |
| 40 | 4 | `batch_crc32c` | CRC32C over the `body_crc32c` fields of the batch's data blocks, in order |
| 44 | 4 | reserved | |

A batch is **complete** when every data block is valid, the commit block is valid, `block_count` and
`batch_bytes` match what precedes it, and the sequences are consecutive with the previous batch. Readers use
only complete batches.

### A.8 Segment index body (written at seal)
`series_count: u32`, reserved `u32`, then per series, sorted by id: `series_id: u32`, `fact_count: u32`,
`min_t: i64`, `max_t: i64`. Lets a reader that is catching up, and compaction, skip segments.

### A.9 Trailer body (64 bytes) and end marker
| Offset | Size | Field |
|---:|---:|---|
| 0 | 8 | `entry_count` |
| 8 | 8 | `first_sequence` |
| 16 | 8 | `last_sequence` |
| 24 | 8 | `first_commit_time` |
| 32 | 8 | `last_commit_time` |
| 40 | 8 | `batch_count` |
| 48 | 8 | `index_offset` (0 if there is no segment index) |
| 56 | 4 | `file_crc32c` over every byte from offset 0 to the start of the trailer's block header |
| 60 | 4 | reserved |

The last 16 bytes of a sealed file are `trailer_offset: u64` and the ASCII bytes `KVIKEND` followed by a zero
byte. A file is sealed only if these are present and the trailer is valid.

## A.10 Durable mark (`log/MARK`, 64 bytes, memory-mapped, not synced)
| Offset | Size | Field | Notes |
|---:|---:|---|---|
| 0 | 8 | `magic` | ASCII `KVIKMARK` |
| 8 | 4 | `version` | 1 |
| 12 | 4 | `seq` | odd while the writer is updating |
| 16 | 16 | `boot_id` | changes on every boot of the machine |
| 32 | 4 | `generation` | |
| 36 | 4 | reserved | |
| 40 | 8 | `file_id` | the segment the mark points into |
| 48 | 8 | `offset` | just past the last durable commit block, including padding |
| 56 | 8 | `last_sequence` | |

- The writer updates the mark only after the sync of the batch has returned successfully: increment `seq` to
  odd, write the fields, increment `seq` to even.
- A reader copies the fields and accepts them only if `seq` was even and unchanged before and after the copy.
- Readers of an unsealed segment never read past `offset`. Sealed segments are read whole.
- Recovery follows `spec.md` §3: same `boot_id` as the running machine, truncate the open segment to the mark;
  different `boot_id`, keep complete batches, sync, then advance the mark.
- On a platform that cannot supply a boot identifier, the mark file is synced after every update instead, and
  recovery always truncates to it.

## A.11 What an implementation must test
- Round trip of every block type, with property tests over row counts, encodings and field combinations.
- Golden files for v1, including an unsealed segment with a torn final batch.
- A fuzz target for the block reader and for each body decoder: no input may cause a panic, an out-of-bounds
  read or an allocation larger than the declared `body_len`.
- Truncation at every byte offset of a batch: the reader returns exactly the complete batches before it.

## B. Outline of the remaining files (not yet normative)

- **Compacted segment (`kind = 2`).** Per-series chunks (one series, one time range) holding either current
  facts or revisions, each with its position range and a summary, followed by an index and a trailer in the
  style of A.9. Candidate encodings: delta-of-delta timestamps, ALP for decimal values with an XOR fallback,
  run-length for quality, flags and rarely changing values. The layout and encodings are fixed after the
  encoding measurements; if they do not clearly beat Parquet on size, compacted data is stored as Parquet and
  this file kind is not used.
- **Manifest (`kind = 3`).** An append-only sequence of checksummed records; one record publishes a set of
  files and retires another set atomically, with an epoch.
- **Cursor (`kind = 4`).** Per consumer: last confirmed position and one latch per condition funnel.
- **Registry (`kind = 5`).** The compacted series registry.
- **Tree (`kind = 6`).** Per-series stored summary levels with the position up to which they are complete.

Each is specified in this document before the roadmap section that implements it.
