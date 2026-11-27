# kvik file format, v1 (design; not yet implemented)

Byte-level rules for the files kvik writes. The semantics are in [`spec.md`](spec.md); where the two differ,
`spec.md` wins and this document is wrong. Part A (common rules, log segments, the durable mark, the store layout,
the platform operations, the manifest framing, cursor and lease files, the registry file, the deep tier's objects, the
derived files) is complete enough to implement. Part B (compacted segments and the remaining files) is an outline; each file is
specified here before the roadmap item that writes it.

## A. Common rules

- All integers are little-endian. Offsets and lengths are in bytes.
- Checksums are CRC32C (Castagnoli polynomial), stored as `u32`.
- Times are `i64` nanoseconds since the Unix epoch, UTC (`changed_at`, A.5, is the one exception).
- A position is `(generation: u32, sequence: u64)`; written as 12 bytes, `generation` then `sequence`.
- **Alignment.** A block header starts at a file offset that is a multiple of 8; a body is zero-padded to a
  multiple of 8. Inside a body only what is listed is aligned: column payloads start at multiples of 8 from the body
  start; an entry starts at a multiple of 8; a field is padded to a multiple of 4. Tables give every other size
  exactly (a column directory entry is 12 bytes).
- **Lengths.** `body_len` excludes the padding after the body. An entry's `length` is the sum of its fields' 4-byte
  headers, values and field padding; it excludes the entry's own padding to 8. `field_len` excludes padding.
- **Reserved and padding.** Writers write zero. Readers do not test reserved fields or padding, so a later minor
  version may use them. Padding between blocks is covered only by a sealed file's `file_crc32c` (A.9).
- **Canonical writing.** For given content a v1 writer produces one byte sequence (field order, run-length form,
  block splitting, heap layout, all stated below); decoding and encoding again any writer output gives identical
  bytes, except that encodings 4 and 5 of an `f64` block are one canonical class (`format-chunk.md` C.10, Q-150).
  Golden files rely on it. A reader applies only the listed checks, so a block that passes them but is not
  canonical (run-length where plain is not larger, an optional column of defaults only, non-zero padding or reserved
  bytes, bytes after the last payload or entry, readings split over more blocks than A.4 allows) is read; a
  canonical check, for writer output, golden files and a scrub, also requires the canonical bytes.
- **Critical and ignorable elements.** Every extension point says whether an unknown element may be ignored: a block
  type by its `skippable` flag (A.2); a column id `< 0x80` is critical, `≥ 0x80` ignorable; an entry type `< 0x80`
  critical, `≥ 0x80` ignorable; a field id `< 0x8000` critical, `≥ 0x8000` ignorable; a fact flag bit not defined in
  A.5 is critical; manifest record types follow block types. An addition that changes the meaning of existing data
  takes a critical id; `changed_at`, versioning, timing and every v1 id are critical.
- **An unknown critical element in a checksum-valid block reached from a valid position, or in a batch that is
  otherwise complete at a resynchronisation candidate (A.2),** is a version refusal (`UnsupportedFormat`, naming the
  file, the offset and the element); a block image at any other candidate is not a block. It is never an invalid
  region, a torn tail or a lost range, and recovery never truncates or overwrites it: a writer that meets one in an unsealed segment, the manifest, a cursor file or the mark
  refuses to open; a reader process returns the error for that file only.
- **Sizes.** `body_len ≤ 16 MiB`; an entry ≤ 8 MiB; a batch ≤ 8 MiB of data blocks plus its commit block (A.4).
- **Scan bound.** A reader's work on a file of L bytes is bounded whatever its bytes. At most 2L bytes are
  checksummed, plus, in a log segment, once more the bytes of the one direct read that found the first damage (at most
  a batch, 8 MiB). Block headers are read at most 16 times per 8 bytes. Working memory, beyond the file and what the
  read reports, is about one batch in a segment, with about 12.3 MiB once damage is found (8 MiB of chain ends, 4 MiB
  of boundary-read verdicts and a bitmap of valid headers), and one block (16 MiB) in the manifest. A body checksum
  is derived from a table of prefix checksums by a CRC shift, so a block image nested in another costs arithmetic, not
  bytes (Q-110).
  A body whose checksum holds is checked by its type (A.5, A.6, A.14). A **failed read** is a candidate read in full,
  with at least one body checked, that is rejected: a resynchronisation candidate or a boundary read that keeps no
  batch (A.2), or a manifest record whose checksum holds and whose fields are rejected (A.14). The file is divided into
  **windows** of w bytes from offset 0: w = 8 MiB (a batch) in a log segment, 16 MiB (a body) in the manifest. Once
  the failed reads starting in one window have checked 16 MiB of body, no other candidate in it is read in full: the
  rest of the window is passed as damaged and reported with the invalid region, corrupt record or torn tail it falls
  in (Q-110). Failed reads caused by damage check disjoint bodies, each inside its own batch, so those starting in one
  segment window check less than the window and one batch beyond it, 16 MiB; a manifest record needs a valid checksum
  to fail this way. Damage to a file the engine wrote, or older complete batches left at their offsets, therefore never
  reach the budget; overlapping bodies, which only a crafted file has, do. Each body a scan keeps is checked once (a
  boundary read's verdicts are not checked again when its batch is read), so in a segment body checks total at most
  L, the direct read that found damage, and per window the budget and the read that reached it (a batch): about
  264 MiB of bodies for a 64 MiB segment and 72 MiB for a 16 MiB one. The checks keep no row or entry and copy no
  text; text values that many rows name are checked at a cost linear in their heap, an entry's fields are split once
  and found by their id, state names are told distinct by a table of their keys under a per-process random hash (with
  a sort once a probe run grows long, so no input costs more than `n log n`), and a manifest record's fields are read
  in one pass that keeps none of a rejected record's (Q-651, Q-870). Measured (release build, one shared 4-core cloud
  machine, the best of three runs, every entry type small and large): at most about 2.9 ns a byte, for definitions of
  65,536 state names of two 4-byte characters, 2.2 to 2.6 ns for other short names, about 1 ns for small definitions,
  at most about 1.3 ns for every other entry and about 0.5 ns for facts, so the body checks of a crafted segment cost
  at most about 0.8 s at 64 MiB and 0.22 s at 16 MiB, against 29 to 170 ms for an undamaged one; those of a crafted
  manifest at most about 0.09 s per 16 MiB for the start's reader and 0.15 s for the whole-file reader (Q-651). The
  header walk is bounded in reads per byte (Q-873). Resynchronisation reads each 8-byte step's header at most once, a
  batch ahead of the candidates, and keeps where the chain of block headers from it comes to: a 4-byte entry per step
  in a ring indexed by offset, found 4 MiB of steps at a time from the highest down, a step taking its successor's
  entry when the successor lies in the same block and each block passing its ends back to the steps whose chains enter
  it. A step is a candidate exactly when its own entry is a commit block that names it, so candidates' chains are
  disjoint. A boundary read that keeps no batch keeps a 4-byte verdict per block it walked in a second ring, so each
  block is judged once and a candidate on such a chain takes only its framing, or only its read in full. A header
  found valid is remembered in a bitmap by offset and read again without its checksum, and a CRC shift costs at most
  eight lookups in nibble tables, so a manifest step reads one header and takes at most one checksum. So once damage is
  found a header checksum is taken at most twice per 8-byte step, once by the chain ends where the type and length could
  be part of a batch and once by a torn tail's commit search where the bytes name a commit block of the commit length (a
  header that fails its checksum is not remembered), plus once more for the headers of the direct read that found the
  damage and for the block where each boundary read stops; a block's header is read at most eight times: once by the
  chain ends, three times by a candidate (its framing and its read in full), twice by a boundary read and twice more by
  the read of the batch it keeps, and twice by the tail's commit search at a commit block. Measured (release build, one
  shared 4-core cloud machine, the best of five runs, the worst at most 40 %
  above it; runs at other hours up to 30 % slower again): a header read costs about 20 to 31 ns with the rest of the
  scan's work, and the dearest crafted 64 MiB segments built cost 0.25 to 0.61 s: a valid header every 8 bytes 0.25
  s (5.7 s before), commit blocks naming data headers 0.24 s, candidates whose framing holds 0.25 to 0.27 s, chains
  of empty skippable blocks read in full 0.35 to 0.43 s, a boundary read that fails at the end of such a chain the
  candidate then reads 0.35 s (2.0 s before), the same chain a batch the candidate keeps 0.61 s, batches of them kept
  after damage 0.33 to 0.57 s, and batches of state names whose every 8-byte step is a plausible header, each body
  checked and each step checksummed, 0.31 s. Undamaged batches of the same 16-byte blocks cost 0.25 to 0.37 s, most of
  it building the report. The reads are bounded and the time measured, not bounded: a crafted manifest's header walk
  measured 0.19 to 0.30 s per 16 MiB with either reader (0.84 to 0.89 s before). What a start is promised for a
  crafted or damaged file is this measured figure per file at the default `log.segment_size` (64 MiB), with no bound
  in seconds (Q-970): the work is linear in a file's length, so a larger segment costs about `segment_size / 64 MiB`
  times as much, and a start pays for each such file it reads. A store with a larger `segment_size` warns when it is
  given a bucket, which others may be able to write (`api.md` Configuration). An undamaged segment is read in one
  pass of direct checksums; the bounded reading starts at the first boundary read that finds no complete batch and
  holds to the end of the scan (Q-231).
  Apart from the window rule, which damage does not reach, the acceptance rules (A.2, A.14) are unchanged.

### A.1 File header (64 bytes, every file except the mark)
| Offset | Size | Field | Notes |
|---:|---:|---|---|
| 0 | 4 | `magic` | ASCII `KVIK` |
| 4 | 1 | `kind` | 1 log segment, 2 compacted segment, 3 manifest, 4 cursor, 5 registry, 6 tree, 7 store descriptor, 8 lease; reserved and laid out by their roadmap items: 9 calendar, 10 revised-key index, 11 version index, 12 local restore record, 13 checkpoint bundle, 14 chunk index; derived files (A.18): 15 log directory, 16 series directory, 17 status file; 18 label part (A.19) |
| 5 | 1 | `format_major` | 1 |
| 6 | 2 | `format_minor` | 0 |
| 8 | 4 | `flags` | reserved |
| 12 | 4 | `generation` | |
| 16 | 8 | `file_id` | unique per store and kind (A.12); for the derived files, per family (A.18) |
| 24 | 8 | `base_sequence` | log: sequence of the first entry this file may hold (for a segment opened by a recovery batch, its F, `spec.md` §3); 0 while a segment created ahead of use is not yet in use (A.4); otherwise 0 |
| 32 | 8 | `created_at` | |
| 40 | 16 | `store_id` | random, fixed for the life of a store; must equal the store descriptor's (A.12); a file with a different id is refused |
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

**Reading blocks.** Sixteen zero bytes at a block boundary are *end of data* only when the reader's limit is the end
of the file (an `.open` segment scanned without a mark) and every byte from there to the end is zero. Below the mark's
offset, or before the seal blocks of a sealed segment, zeros start an invalid region, as any bytes that are not a
complete batch do, also when they run to that limit. Recovery scans an unsealed segment to its end regardless
(`spec.md` §3). A block is **valid** when its header checksum matches,
`body_len` fits in the file, the body checksum matches, its type is known and its body passes the checks of its type
(A.5, A.6). A checksum-valid block of unknown type is skipped if `skippable`, else refused (common rules). Anything
else starts an **invalid region**. Outside a log segment, an invalid region ends the readable part of the file
(the manifest has its own rule, A.14).

**Resynchronisation** (log segments). From an invalid region the reader steps forward 8 bytes at a time (all-zero
words are passed without a checksum) and accepts an offset `o` only if a **complete batch** (A.7) starts there: every
block valid, `block_count` and `batch_bytes` match, the commit block's `generation` is the file header's, its
`first_sequence` is above the last kept sequence, and `batch_crc32c` matches. `batch_crc32c` covers `o` and, through
each data block's `body_crc32c`, every body (A.7), so a batch image at any other offset, or bytes inside a text value
that imitate a batch without its exact blocks, are not accepted. A reader
process applies the same rule below the mark's `offset` and never above it. A structural failure inside a
checksum-valid block (the checks of A.5 and A.6) makes the block invalid; the region is handled as corruption
(`spec.md` §3 Recovery rule 2) and its loss record carries cause 4, invalid content (A.14). Once failed reads
starting in one 8 MiB window of the file have checked 16 MiB of body (common rules: scan bound), the rest of that
window is not read: it lies in the invalid
region, or the torn tail, that holds it.

### A.3 Versioning
- A reader refuses a file whose `format_major` is greater than it supports, with an error that names both
  versions. It never guesses.
- `format_minor` is the writer's minor version when the file was created. It is diagnostic: compatibility is decided
  per element by the critical and ignorable rule (common rules). A writer may append to a file of any minor version
  of its major version, subject to the refusal rule. A change to the layout or meaning of an existing element is a
  new major version.
- Within a major version, additions are new block types, columns, entry types and fields under the critical and
  ignorable rule, and fields in reserved space. Fixed-size record arrays carry their record size, and a reader
  ignores a record's unknown tail (A.8).
- Every released format version has golden files in the repository, read by the current code in CI.

## A.4 Log segment (`kind = 1`)

A log segment is a header followed by **batches**. A batch is one or more data blocks followed by exactly one
commit block. A batch never spans files. Unsealed segments are created ahead of use, zero-filled, and named
`log/<generation>-<file_id>.open` (A.12); sealing turns one into `.kvl`.

| Type | Name | Skippable | Role |
|---:|---|---|---|
| `0x01` | facts | no | readings, in arrival order |
| `0x02` | entries | no | definitions, parameters, aliases, retirements, scale raises, site-zone and tzdata entries, routing changes, recovery entries |
| `0x0F` | commit | no | ends a batch |
| `0x10` | segment index | yes | written at seal: which series the segment holds |
| `0x11` | series batches | yes | written at seal, after the segment index, when series are in few batches: each series' batches (A.8a) |
| `0x7F` | trailer | no | written at seal: marks the file as sealed |

**Sequence assignment.** Entries take consecutive sequences in the order they appear: block by block, and row
by row inside a block. Sequences start at 1; `last_sequence = 0` in any structure means empty. The first entry of a
batch has the sequence after the previous complete batch's last, or after the end of a lost range that lies between
them (`spec.md` §3 Recovery rule 2), except that a batch holding a recovery entry may start higher (A.7); the first
entry of a file has `base_sequence`. An `entries` block that defines a series must come before any `facts` block that
uses it.

**Sizes.** `segment_size` is a configuration key, 16 MiB to 1 GiB, a multiple of 4,096, default 64 MiB; it applies
to segments created after it changes. A facts block is closed when the next row would take it past 65,536 rows or a
plain-encoded size of 8 MiB (33 bytes per row plus text bytes); rows fill blocks in order, each block as full as
these limits allow. A batch holds whole emit calls and whole entry calls: it takes whole calls in order while the sum
of their data blocks, each encoded alone, stays within `max_batch_bytes` (default 1 MiB), and at least one; calls
that do not encode together are committed one by one. One call is at most `min(65,536, limits.ingest_queue)` readings
and 8 MiB plain-encoded (`api.md` Writing), so every batch fits an empty
segment. A batch that does not fit the space left in the open segment goes to the next segment, and the open one is
sealed.

**Creation.** A segment is created ahead of use: created new under its final `.open` name, zero-filled to
`segment_size` by ordinary writes (physical zeros, no `fallocate`), its header written with `base_sequence = 0`,
then `sync_all` and a directory sync (A.13). When it is taken into use the 64-byte header is rewritten in place
with the real `base_sequence`; the first batch's sync makes that durable. If creation fails for lack of space, the
partial file is deleted, health is critical, ingest waits, and creation is retried when a free-space check passes
(`spec.md` §3 Segments: at least `disk_critical` + `segment_size` free, checked every second; every 30 seconds where
free space is unknown); nothing is fatal. Any other failure of a creation, a failed sync included, is a write
failure on the log. Lack of space at a batch write or sync stays a write failure (`spec.md` §3). On copy-on-write
file systems zero-filling reserves nothing; that is accepted and documented, not refused.

**In use.** A segment is *in use* when its header is valid with `base_sequence ≥ 1`, or the scan (A.2) finds a
complete batch in it; the *last in use* is the one with the highest `file_id`. A segment is taken into use by the
header rewrite, made durable by the first batch's sync (a roll) or by its own `sync_data` (a timed seal with no
batch waiting). A recovery batch that opens a segment is written before that header rewrite, and both are made
durable by the entry's sync, so no stop leaves a segment in use by its header alone (`spec.md` §3 Order of a start,
step 5). A header taken into use by a sync that failed may be in the page cache only, so a start writes the header
of the segment it resumes in again, in place, before its first sync (`spec.md` §3 Order of a start, step 2).

**Sealing** (off the writer thread) of segment S starts only after the next segment is in use and synced, so a
segment being sealed is never the last in use: (1) at the offset just past S's last complete batch, write the
segment index block, the series batches block where A.8a writes one, the trailer block and the 16-byte end marker; (2) set the file length to the end of the end
marker (the file may grow or shrink); (3) `sync_all`; (4) rename `.open` to `.kvl`; (5) sync the directory. The mark
does not move at sealing: it keeps naming S until the first commit in the next segment, whose batch is acknowledged
only once the mark naming it is synced (`spec.md` §3 Commit, Q-630). An empty segment is never
sealed.

**States recovery may find**, handled in `file_id` order. `spec.md` §3 Recovery rules 1 and 3 (truncation, torn
tail, recovery entry) apply only to the last segment in use; on the same boot the mark covers every batch of an
earlier segment, so nothing else is ever truncated or reported as a torn tail. Every `.open` file matches exactly
one row:

| State of an `.open` file | Action |
|---|---|
| its last non-zero bytes are a valid trailer and end marker (wherever the mark points; the file may still be longer than the end marker, after a crash between seal steps 1 and 2), the seal blocks start exactly at the end of its last complete batch, the trailer's `batch_count` and `last_sequence` equal the scan's, and it is not the last segment in use | seal again from step 1: after a failed `sync_all` on the same boot the trailer and end marker may be in the page cache only, and a repeated sync would not write them (`spec.md` §3 Recovery rule 3); never truncated |
| in use with a complete batch, not the last in use, not in the first row | whatever follows its last complete batch is seal blocks or damage, never a torn batch: seal again from step 1 |
| in use by its header alone, not the last in use | left as it is, never sealed (an empty segment is never sealed) |
| in use, the last in use | `spec.md` §3 Recovery rules 1–3 (a torn first batch included); then appended to while a batch fits; if its first batch is then a recovery batch, the header is rewritten with F after that batch is written |
| header invalid or `base_sequence = 0`, but a complete batch is found | rewrite the header from the file name, the store descriptor and the first batch; then the row for a segment in use with a complete batch, or the last in use |
| not in use (no complete batch; header invalid or `base_sequence = 0`) | unused: kept as the segment created ahead if its header is valid, its length is `segment_size` and every byte after its header is zero, else deleted; beyond the mark's segment, or anywhere with no readable mark, non-zero bytes after its header are first part of a recovery entry's tail (under rule 1 a reset point; with no readable mark rule 3 applies, never a first start), and the file is deleted only once an entry that reports them is on disk: the one written, or one adopted, which must lie in a later file than every such segment (Q-125, Q-320) |

A trailer and end marker that fail the first row's conditions are bytes of whatever precedes them (a torn batch or a
value); the file falls under the other rows. In the last segment in use a seal is never expected (Sealing).

A `.kvl` file is sealed and is not scanned by recovery. Whether a segment that is not the last in use lost data is
decided only by the gap between its last sequence and the next segment's `base_sequence` (`spec.md` §3 Recovery
rule 2), never by the bytes after its last batch. A start checks the gap rule for unsealed segments and against the
last sealed one of the current generation or, with none, against the generation's first position (1 in generation
0, the restore record's in a restored generation), so that a lowest segment damaged, zeroed or missing leaves a
recorded gap (Q-321, Q-264); gaps between sealed segments are checked by the scrub. At most one segment is unused; any number
may be awaiting a seal.

**Truncation is zero-filling.** Recovery never shortens an unsealed segment. It overwrites with zeros from the
truncation offset to the last non-zero byte, working from the end backwards, then syncs. Under rule 1 this is done
before any new batch is written, and rule 1 also zero-fills every later segment in use after its header: their
headers stay, and the writer resumes in the last segment in use. Under rule 3 the recovery batch is written and synced
first and the rest of the torn tail is zeroed after it (`spec.md` §3 Order of a start,
step 5); ingest resumes only after both. Accepted limit: a power loss during the zero-fill can
leave unacknowledged batches after a zeroed hole; the next recovery keeps them and records the hole as `data_lost`
although nothing acknowledged was lost.

**Gaps.** A reader accepts a complete batch whose first sequence is above the previous batch's last by any amount;
only the writer's recovery classifies the gap. A copy of a segment that omits a lost range (`spec.md` §8 Corrupt
block) is an ordinary segment with a new `file_id`, its commit blocks rewritten (A.7) and its trailer counting the
surviving entries.

### A.5 Facts block body
| Offset | Size | Field |
|---:|---:|---|
| 0 | 8 | `first_sequence` |
| 8 | 8 | `received_at` (the batch's; every data block of a batch carries the same value) |
| 16 | 4 | `row_count` (1 ..= 65 536) |
| 20 | 1 | `column_count` |
| 21 | 1 | `dir_entry_size` (12 in v1; a reader uses it and ignores a longer entry's tail) |
| 22 | 2 | reserved |
| 24 | `dir_entry_size` × `column_count` | column directory |
| … | | column payloads, each aligned to 8 |

Column directory entry: `column_id: u8`, `encoding: u8`, reserved `u16`, `offset: u32` (from the start of the
body), `length: u32`. Columns appear in ascending id order; an unknown column is skipped or refused by the critical
and ignorable rule.

| Id | Column | Width | Presence | Encodings allowed in v1 |
|---:|---|---|---|---|
| 1 | `series_id` | u32 | mandatory | plain |
| 2 | `t` | i64 | mandatory | plain |
| 3 | `quality` | u8: 0 good, 1 estimated, 2 suspect, 3 bad | optional; absent means all good | plain, run-length |
| 4 | `flags` | u16 bit set: 0 future, 1 late, 2 revise, 3 host_time, 4 unsynced_clock, 5 no_value, 6 has_changed_at, 7 rounded | optional; absent means all zero | plain, run-length |
| 5 | `value_type` | u8: 1 f64, 2 i64 (scaled), 3 state index, 4 bool, 5 text | mandatory | plain, run-length |
| 6 | `value` | 8 bytes per row | mandatory | plain |
| 7 | text heap | bytes | exactly when a row has `value_type = 5` | plain |
| 8 | `changed_at` | u64 | exactly when a row has flag bit 6 | plain |
| 9 | `scale` | u8, 0–18 | exactly when a row has `value_type = 2` | plain, run-length |

- `value` holds the IEEE-754 bit pattern (f64), the two's-complement integer (i64; never `i64::MIN`, which is out of
  range, `spec.md` §2.1), the index or 0/1 in the low
  bytes with zero upper bytes (state, bool), or for text a `u32` offset and a `u32` length relative to the start of
  the heap payload (at most 255 bytes, UTF-8). Canonically, texts are in row order, each appended to the heap and
  none shared; a zero-length text points at the heap's length at its row, and a `no_value` text row holds 0. A
  reader accepts any offset and length inside the heap, shared bytes included.
- `quality` holds the **adapter-supplied** quality, also on a `no_value` row. The engine's quality is derived, never
  stored: bad for a `no_value` row, else the worse of the supplied quality and suspect when `future` is set.
- A `no_value` row has `value = 0` and the series' declared `value_type`.
- Flag bit 6 `has_changed_at`: the row belongs to a versioned series and column 8 holds its `changed_at`, an unsigned
  integer to which the rule that times are `i64` does not apply; other rows hold 0 there. Ingest refuses a `changed_at`
  above `i64::MAX` on a series of versioning 1 (`spec.md` §2.2, Q-195); a reader accepts any value (Q-760). Flag
  bit 7 `rounded`: the value was rounded to the series' scale (`spec.md` §2.1 `excess precision`). Bits 8–15 are
  undefined and critical.
- Column 9 holds the scale each decimal row was stored at (the series' scale at the row's position); other rows hold
  0 there. A reader rescales exactly to the series' current scale, so a fact never depends on a raise entry being
  kept.
- **Checks** (a failure makes the block invalid, A.2): a mandatory column missing; a column id twice; a payload
  outside the body or overlapping another; a length that does not match `row_count`; an encoding not allowed for the
  column; a value out of the range above, a decimal row (`value_type = 2`) holding `i64::MIN` included (invalid
  content, `spec.md` §2.1); the presence rules of columns 7, 8 and 9 broken. A `value_type` that
  contradicts the series definition is not a block check: the fact is treated as an orphaned fact (`spec.md` §2.1) by
  whoever has the registry.
- **Encoding 0, plain:** `row_count` fixed-width values.
- **Encoding 1, run-length:** repeated pairs of a run length and one fixed-width value. A run length is at least 1,
  written as minimal unsigned LEB128 of at most 3 bytes; adjacent runs differ in value; the run lengths sum to
  `row_count`. A reader rejects anything else. A writer uses run-length for a column exactly when its unpadded payload
  is smaller than plain's; plain wins a tie.
- `value_type` and `scale` let a block be decoded to typed values without the registry (bit pattern, decimal,
  state index, bool, text). State names, kind, versioning and interval length come from definition entries,
  which a reader takes from the registry file and the log.

### A.6 Entries block body
| Offset | Size | Field |
|---:|---:|---|
| 0 | 8 | `first_sequence` |
| 8 | 8 | `received_at` (the batch's) |
| 16 | 4 | `entry_count` |
| 20 | 4 | CRC32C of that manifest record's block bytes: the record is matched by content |
| 24 | … | entries |

Each entry: `entry_type: u8`, reserved 3 bytes, `length: u32`, then `length` bytes of fields, padded to 8. Fields
are a list of `field_id: u16`, `field_len: u16`, value, padded to 4. Fields appear in ascending id order; a field
appears once unless marked repeated. A zero-length field means "none". An unknown entry type or field is skipped or
refused by the critical and ignorable rule. **Every default is resolved when the entry is written:** a definition
carries each applicable field explicitly, and an absent field means "not applicable", never "the default in force
when read". The one named exception is field 20 of a label change (type 10), whose absence means *always*. Durations are `i64` nanoseconds. Strings are UTF-8 without a terminator.

| Entry type | Fields |
|---:|---|
| 1 series definition | the table below |
| 2 parameter change | 1 id; 20 effective time; then any of fields 7–12, with 27 whenever 8 is carried. Fields 23 to 26 and 28 may also be carried; they take effect at the entry's position, and an entry holding only them has no field 20 (26 only with 25 = 2, at least the scale in force). No other field is allowed (5 changes only by a scale raise; 13–19 and 22 are fixed for life). A change of 7 without 8 is written with field 8 holding the staleness limit in force after the change (`spec.md` §2.1: an S that was set is kept; one that was not becomes 3 × the new interval), and field 27 saying which, so the entry is self-contained |
| 3 alias | 1 id; 2 name |
| 4 retirement | 1 id; 21 `t_r` |
| 5 routing change | 50 consumer name (A.12); 51 change `u8` (1 adds a funnel, 2 removes one, 3 declares `emits` only, 4 watches a gate, 5 unwatches one); 52 funnel id `u32` (changes 1 and 2); for change 1: 53 selection kind `u8` (0 none, 1 ids, 2 name pattern, 3 every series), 54 series id `u32` (repeated: the ids of kind 1; for kind 2, the ids a condition funnel's pattern was resolved to when routed, else none), 55 pattern (UTF-8, 1–255 bytes, kind 2 only), 56 aggregate `u8` (1 avg, 2 sum, 3 integral, 4 min, 5 max, 6 count, 7 increase, 8 rate, 9 last, 10 time in state) with 57 window width `i64` ns > 0, 58 state counted (5 bytes: tag 0 then a boolean byte and 3 zero bytes, or tag 1 then a named state's index `u32`; aggregate 10 only), 63 condition `u8` (1 threshold: 64 activation and 65 clear level, clear ≤ activation; 2 on change: 64 dead-band ≥ 0; 3 held for: 64 level, 66 duration; 4 stale for: 66 duration beyond the series' staleness limit in force at the entry's position), levels finite `f64` and durations `i64` ns ≥ 0; always: 60 the consumer's `emits` kind, 61 its ids (repeated), 62 its pattern, as 53 to 55 but never with resolved ids. Fields 53 to 58 and 63 to 66 appear only in change 1 (`spec.md` §4 Routing). A pattern in field 55 or 62 that the grammar of `spec.md` §4 Funnel refuses (written before it) is read as built, each segment a literal and `*` alone a wildcard; its funnel stays in force, and routing it, or declaring its `emits`, again is refused (`model/OPEN-QUESTIONS.md` Q-670 (b)). Gates (`spec.md` §6 Gates), changes 4 and 5, all fields critical: 67 gate id `u32` (both; allocated per consumer, never reused by it); for change 4 only: 68 gate name (1–64 bytes), 69 selector `u8` (1 one series: field 54 once; 2 each: field 55, the pattern), 71 input `u8` (1 value; 2, window, reserved and refused), 72 kind `u8` (1 levels: 73 level, repeated 1 to 16, 32 bytes: `at` `f64`, `clear` `f64`, `hold_up` `i64` ns, `hold_down` `i64` ns, finite, `clear ≤ at`, holds ≥ 0, each `at` above and each `clear` at or above the previous; 2 set: 74 member, repeated 1 to 64, a tag `u8` then the value: 1 `f64` finite, 2 decimal `i64` units then `u8` scale ≤ 18, 3 named state index `u32` < 65,536, 4 boolean byte 0 or 1, 5 text 1–255 bytes; 75 holds, 16 bytes: `hold_on`, `hold_off` `i64` ns ≥ 0; 3 change: 76 dead-band `f64`, finite, ≥ 0), 77 stale after `i64` ns > 0 (absent: never). Fields 52, 53, 56 to 58 and 63 to 66 never appear in changes 4 and 5, field 54 or 55 only as the selector of change 4, and 67 to 77 only in changes 4 and 5. Field 70 is the label item of type 10, not a routing field. Vector (change 4, consumer `hall`, gate 4 `power`, change kind on series 9 with dead-band 0.5, `emits` none), the entry in hex: `05000000 58000000 3200040068616c6c 3300010004000000 3600040009000000 3c00010000000000 4300040004000000 44000500706f776572000000 4500010001000000 4700010001000000 4800010003000000 4c000800000000000000e03f` |
| 6 site zone | 30 zone name (UTF-8 IANA name, at most 63 bytes); 20 effective time |
| 7 tzdata | 31 version (ASCII release name, at most 15 bytes) |
| 8 recovery | the only entry of its batch (`spec.md` §3 Recovery rule 3): 40 `after` `u64`; 41 reset stamp (16 bytes: the entry's own position `u64`, then a random nonce `u64`, never zero), written by every writer, an entry without it read as carrying none (`spec.md` §7 Readers); 42 boot id (16 bytes, zero where the platform has none); 43 tail bytes `u64`; 44 readable commit blocks `u32`; 45 mark state `u8` (0 readable, 1 missing, 2 unreadable: no valid slot, A.10; 3 stale, `spec.md` §3 Recovery rule 1); 46 id floor `u32` (0: no id assigned); 47 consumer name (repeated, one per consumer whose cursor file was missing or unreadable); 48 `written` level in force `u8` (1 when a series is at the `written` level in the registry at `after`, making the entry a `possible_loss`, `spec.md` §3 Recovery rule 3; an entry without it reads as 0) |
| 9 scale raise | 1 id; 5 decimal scale (the new scale, above the series' current scale); takes effect at its position (`spec.md` §2.1 `decimal scale`) |
| 10 label change | 1 id; 20 effective time `i64` ns, absent for *always* (the exception above; a zero-length field 20 is invalid, so *always* has one encoding); 70 label item (repeated, 1 to 64): `op` `u8` (1 set, 2 remove, 3 withdraw; any other value is an unknown critical value, a version refusal), `key_len` `u8`, the key (`key_len` bytes), then for op 1 the value, the rest of the field (`spec.md` §2.1 Labels). Critical: an older build refuses it (Q-882). Not held by the registry file (A.16) but by label parts (A.19) |

Series definition fields, all critical:

| Id | Field | Encoding | Present |
|---:|---|---|---|
| 1 | id | u32 | always |
| 2 | name | UTF-8, 1–255 bytes | always |
| 3 | kind | u8: 1 gauge, 2 counter, 3 state, 4 text, 5 interval | always |
| 4 | value type | u8, the A.5 codes: 1 f64, 2 scaled i64, 3 named state, 4 bool, 5 text | always |
| 5 | decimal scale | u8, 0–18 (value = integer × 10^−scale); 4 when the caller declared none | value type 2 |
| 6 | unit | UTF-8, 0–63 bytes | always |
| 7 | expected interval | i64 ns > 0, or none | always |
| 8 | staleness limit S | i64 ns > 0, or none | always |
| 9 | record on change | u8 0 or 1 | always |
| 10 | `wrap_at` | 8 bytes in the series' value type, or none | counters |
| 11 | `reset_ceiling` | u32 parts per million (10 % = 100,000) | counters |
| 12 | lateness bound | i64 ns | always |
| 13 | state name (repeated, in index order) | UTF-8, 1–63 bytes, distinct (Q-182); at most 65,536 | value type 3 |
| 14 | versioning | u8: 0 not versioned, 1 `changed_at` is a time, 2 a version | always |
| 15 | timing | u8: 0 observed, 1 scheduled, 2 forecast | always |
| 16 | interval length tag | u8: 0 fixed, 1 `day`, 2 `month` | kind 5 |
| 17, 18 | interval d, phase | i64 ns, d > 0, 0 ≤ phase < d | tag 0 |
| 19 | interval zone | UTF-8 IANA name as given, at most 63 bytes | tags 1 and 2 |
| 22 | day start | i64 ns, 0 ≤ x < 24 h | tag 1 |
| 23 | retention class | u8: 0 long-term, 1 diagnostic | always |
| 24 | retention period | i64 ns | class 1 |
| 25 | excess precision | u8: 0 round, 1 refuse, 2 auto-raise | value type 2 |
| 26 | maximum scale | u8, 0–18, at least field 5 | field 25 = 2 |
| 27 | staleness limit set | u8: 1 S was set by the caller, 0 S follows the expected interval | always; also in a parameter change carrying 8 |
| 28 | durability level | u8: 0 `synced`, 1 `written` (`spec.md` §2.1 `durability`) | always; a definition written before roadmap 5.5b without it reads as 0 |

**Checks** (a failure makes the block invalid, A.2; the presence and recovery checks are invalid content): `length`
or a field overrunning its container; a critical field out of order or repeated when not marked repeated; a value
outside its range above; a field missing where the tables say it is present (the "Present" column, field 27 with
field 8, field 20 in a parameter change, the recovery entry's fields, a routing change's fields per change and kind),
or present where it does not apply; a named-state series with no state name; a recovery entry that is not the only
entry of its batch; in a label change, no item or more than 64, a key or value outside the grammar of `spec.md` §2.1
Labels (a reserved key is refused by the calls, not by a reader), `key_len` past the field, a key twice in one
entry, or a value with op 2 or 3. Kind and value type are not compared beyond the tables. Fields without a stated range take any
value, and a parameter change carrying only field 1 is valid. Entry type 5 is laid out since roadmap 5.2 (until then
it was refused as an unknown critical type, Q-106 (f)); field 41 of a recovery entry is 16 bytes with a non-zero
nonce. **Splitting:** consecutive entries share one
entries block until the next would take its body past 16 MiB.

### A.7 Commit block body (48 bytes)
| Offset | Size | Field | Notes |
|---:|---:|---|---|
| 0 | 4 | `generation` | equals the file header's |
| 4 | 4 | `block_count` | blocks in this batch before the commit block, of known type or not |
| 8 | 8 | `first_sequence` | the first data block's |
| 16 | 8 | `last_sequence` | inclusive; the last counted |
| 24 | 8 | `commit_time` | non-decreasing within a generation by construction: `received_at` and the commit time are the wall clock when the batch is formed, raised to the previous batch's commit time; a reader does not test it |
| 32 | 8 | `batch_bytes` | from the first byte of the batch's first block header up to, not including, this block's header, padding included; so this header is at `o + batch_bytes`, where `o` is the batch's offset |
| 40 | 4 | `batch_crc32c` | CRC32C over `store_id` (16 bytes), `file_id` (u64), `o` (u64), then bytes 0..12 of the header of every block before the commit block, in order: each header without its `header_crc32c`, so that the bodies are reached through their `body_crc32c` (a header followed by its own CRC32C would cancel out of the sum, Q-119) |
| 44 | 4 | `flags` | bit 0 `written`: a flagged batch, acknowledged once written (`spec.md` §3 Commit), holding only facts of series at the `written` level; other bits zero, and a set bit not defined here is critical (common rules) |

`flags` was reserved before roadmap 5.5a, and every batch written before then holds zero there: unflagged. A writer
that sets bit 0 cannot be read correctly by a build that predates it, which reads reserved bytes as nothing (common
rules); no release predates it (Q-253). Recovery keeps a complete flagged batch beyond a same-boot mark and counts
only unflagged batches for the stale-mark test (`spec.md` §3 Recovery rule 1).

A batch is **complete** when every data block is valid, the commit block is valid, `block_count` and `batch_bytes`
match what precedes it, `batch_crc32c` matches, each data block's `first_sequence` equals the counted sequence, the
commit's `first_sequence` and `last_sequence` are the first block's and the last counted, `generation` equals the
file header's, every data block has the same `received_at`, and its first sequence follows the previous complete
batch's last, or the end of a lost range between them after an invalid region (`spec.md` §3 Recovery rule 2),
except that a batch holding a recovery entry may start higher; its `after` is the previous batch's last sequence or
the end of a lost range at the tail, and it may follow an invalid region at the tail. A disagreement with valid
checksums is an invalid region. Readers use only complete batches.

`block_count`, `batch_bytes` and `batch_crc32c` cover every block before the commit block, of known type or not; a
skipped block (A.2) takes no sequence and carries no `received_at`, and a batch of skipped blocks alone takes no
sequence and is not complete. Before the seal blocks, every block of a log segment belongs to a batch. The first
batch of a file starts at or above `base_sequence`; that it starts higher only after an invalid region or with a
recovery entry is checked by the writer's recovery (`spec.md` §3), not by a reader.

### A.8 Segment index body (written at seal)
`series_count: u32`, `record_size: u16` (24 in v1), `flags: u16` (bit 0 `has_entries`: the segment holds at least
one entries block; bit 1 `has_labels`: it holds at least one label change, type 10, A.19; set with bit 0 clear, it is invalid), then per series, sorted by id, records of `record_size` bytes: `series_id: u32`,
`fact_count: u32`, `min_t: i64`, `max_t: i64`. Readers use `record_size` and ignore a record's unknown tail; this is
the rule for every fixed-size record array. A reader that is catching up, and compaction, skip a segment only if none
of its series is listed **and** `has_entries` is clear; otherwise they read at least its entries blocks.

`min_t` is the least `t`. `max_t` is the greatest *reach* of a reading and does not depend on tzdata: `t` for kinds
other than interval, `t + d` for a fixed interval, `t + 50 h` for `day`, `t + 33 days` for `month`.

### A.8a Series batches body (written at seal; Q-1150)
Written by the writer that wrote the segment's batches, immediately after the segment index block and before the
trailer, only when the (series, batch) pairs are at most half of the batches times the series the index lists (a
series is on average in at most half the batches), no batch holds a recovery entry, there are
at most 262,144 pairs and the body is at most 16 MiB; a segment sealed by a start (whose batch offsets it does not
keep) carries none. Body: `series_count: u32`, `record_size: u16` (8 in v1), `ref_size: u16` (8 in v1), `ref_count:
u32`, reserved `u32`; then `series_count` records sorted strictly ascending by id, `series_id: u32`, `count: u32`
(at least 1); then `ref_count` references, `offset: u32` (the batch's first block header, a multiple of 8 and at least
64), `length: u32` (the batch's bytes, commit block included, a multiple of 8), the references of each series in
record order and within a series in strictly ascending offset, none overlapping the next or reaching past the index
block. The counts sum to `ref_count` and the arrays fill the body exactly; longer records and references have their
tail ignored. A series' references are exactly the complete batches of the segment that hold at least one of its
facts.

A reader may use the block only when it lists exactly the series of the segment index; it then reads a few series'
facts from their batches alone, each read and verified as a complete batch at its listed place (A.7). A reader that
finds the block invalid or absent, or a listed batch that does not read as listed, reads the segment as before. A
batch holding none of the series read is not read, so damage confined to it leaves that read complete, as a segment
whose index lists none of them is not read (A.8).

### A.9 Trailer body (64 bytes) and end marker
| Offset | Size | Field |
|---:|---:|---|
| 0 | 8 | `entry_count` |
| 8 | 8 | `first_sequence` |
| 16 | 8 | `last_sequence` |
| 24 | 8 | `first_commit_time` |
| 32 | 8 | `last_commit_time` |
| 40 | 8 | `batch_count` |
| 48 | 8 | `index_offset` (file offset of the segment index block header; 0 if there is none) |
| 56 | 4 | `file_crc32c` over every byte from offset 0 to the start of the trailer's block header |
| 60 | 4 | reserved |

The last 16 bytes of a sealed file are `trailer_offset: u64` (file offset of the trailer block header) and the ASCII
bytes `KVIKEND` followed by a zero byte. A file is sealed only if these are present and the trailer is valid.

`file_crc32c` is verified by the sealer when it is written, before a segment is uploaded or used as compaction
input, and by the scrub; not at open and not on reads. A mismatch while every batch is complete means damage in
padding or seal blocks: the segment is copied to a new segment (A.4) and swapped in by one manifest record, with no
loss report.

**Plaintext hash** (`spec.md` §8 Hashes). A log segment's plaintext SHA-256 is over every byte of the sealed file,
header to end marker. The trailer has no room for it (4 reserved bytes), so the file description of a manifest record
(A.14) alone holds it; the trailer is unchanged and no version rule applies. The sealer computes it in the pass that
computes `file_crc32c`, off the commit thread, once per seal (a blocked seal's retry reuses it). After a start the
writer computes it, one segment at a time and only while no seal is ready or running, for every sealed segment that
no record names by generation and `file_id`, after checking the header against the name, the trailer, the end marker
and `file_crc32c`; a segment that fails a check is left unnamed, degrades health (`SegmentDamaged`, `api.md` Health)
and is tried again at the next start, and one that cannot be opened or read whole is tried again in the run, after a
wait that doubles from 10 s to 10 minutes, and degrades health (`SegmentUnreadable`) from the try that sets the
10-minute wait until a read succeeds (Q-220, Q-221, Q-640).

## A.10 Durable mark (`log/MARK`, version 3)
`log/MARK` is 4,224 bytes: two 128-byte slots at offsets 0 and 4,096 (separate sectors). It has no A.1 header. It is
read and written by positional I/O and never mapped; it is synced on the timer (`mark_sync_interval`, on the writer
thread, also when idle), at once after a batch holding an entry that is not a fact or an unflagged first batch in a
segment (`spec.md` §3 Commit, Q-630), at clean shutdown and by recovery. It is updated in place and never replaced
while it exists; a new file is created only when none exists.

| Offset | Size | Field | Notes |
|---:|---:|---|---|
| 0 | 8 | `magic` | ASCII `KVIKMARK` |
| 8 | 4 | `version` | 3; a slot with valid magic and checksum and an unknown version is `UnsupportedFormat` for a writer and a reader alike (common rules): the writer refuses to open and never overwrites it |
| 12 | 4 | `generation` | |
| 16 | 16 | `store_id` | must equal the store descriptor's |
| 32 | 16 | `boot_id` | zero where the platform has none |
| 48 | 8 | `counter` | 1 at creation, +1 per update |
| 56 | 8 | `file_id` | the segment the mark points into |
| 64 | 8 | `offset` | just past the last durable commit block, including padding |
| 72 | 8 | `last_sequence` | |
| 80 | 4 | `max_series_id` | the highest series id assigned; 0 when none is (ids start at 1, `spec.md` §2.1) |
| 84 | 8 | `reset_sequence` | the current reset stamp (`spec.md` §7 Readers): the position of the last recovery entry a start wrote, or the first position of a restored generation; 0 before any reset |
| 92 | 8 | `reset_nonce` | its random nonce, never zero once a reset has happened; 0 before any reset |
| 100 | 24 | reserved | |
| 124 | 4 | `crc32c` | over bytes 0..124 of the slot |

- **Update.** After the batch's sync has returned, the writer writes the whole slot, with one positional write, to
  the slot that does not hold the last synced mark; until the mark file is next synced, every later update goes to
  the same slot, replacing the unsynced one before it, so a torn update falls back no further than the last synced
  mark (Q-122). A start cannot tell which slot was last synced, nor whether a sync of the file failed (bytes whose
  write-back failed may no longer be dirty, so a sync alone need not write them): before its own mark write it writes
  the file again in place, as read, and syncs it; a start that keeps the mark it read (`spec.md` §3 Recovery rule 1)
  writes that slot again in place, as read, and syncs it before the first update. The writer writes the next batch
  only after that update (`spec.md` §3 Recovery rule 1).
- **First mark.** The first start of a new store writes slot 0, counter 1, all position fields 0, the current boot
  id.
- **Reset stamp.** Bytes 84..100 were reserved in version 3 and are read as zero in a slot that predates them, so
  the version is unchanged. A zero `reset_nonce` is no stamp, whatever `reset_sequence` holds. A start that writes or
  adopts a recovery entry writes the entry's stamp into the mark; a restore
  writes its restore record's; every other update carries the stamp unchanged.
- **Read.** Read both slots; the mark is the valid slot (magic, known version, checksum, store id) with the higher
  `counter`; a checksum-valid slot of unknown version is refused as above, never treated as invalid. One slot always holds the last synced mark, so a torn or concurrent write falls back no further than it: a reader
  that meets an update in progress sees a mark at most one `mark_sync_interval` old (`spec.md` §3 Durable mark). If
  neither slot is valid, a reader retries 8 times, back to back, then returns `MarkUnreadable`, as it does for a
  missing mark file; for recovery the mark is unreadable. After a power loss the older slot is still a valid mark of any
  boot for `spec.md` §3 Recovery rule 2.
- **Boot identity.** Linux: `/proc/sys/kernel/random/boot_id` (a file read). macOS: `kern.bootsessionuuid`, through
  a dependency that wraps `sysctlbyname` (to be verified at roadmap 4.1; fallback: none). Windows: none until a
  source is verified (roadmap 4.5). Containers sharing the host's identity, and virtual machines resumed with their
  memory, are supported; reverting a disk under a running machine is not.
- **No boot identity** (`boot_id` zero) or `mark_sync = EveryCommit`: the mark is synced after every update, before
  the batch is acknowledged. Recovery then follows `spec.md` §3 as for any mark: a readable mark gives rule 1; a
  missing or unreadable mark gives rule 3.
- **Same boot, page cache lost.** Rule 1 trusts the boot identity, because it must truncate an unflagged batch whose
  sync failed and which sits complete in the page cache, and keep the flagged batches there (A.7). Since a batch is
  written only after the previous unflagged batch's sync and mark update, a current mark has beyond it flagged
  batches and at most one complete unflagged batch, the last; a complete unflagged batch followed by another
  complete batch on the same boot means the mark is stale and rule 3 applies (`spec.md` §3 Recovery rule 1, with its accepted limit). The recovery entry records mark
  state 3.
- **No usable mark.** With the mark missing, unreadable or stale, recovery cannot rule out the same boot, so it
  writes the last complete batch, and the flagged batches directly before it, again before its first sync
  (`spec.md` §3 Recovery rule 3), with the segment's header when the first of them is the segment's first. It does the same when the last complete batch is a recovery entry
  that the mark does not cover, whatever the mark, as the start that wrote it may have failed its sync (Q-117).
- Readers of an unsealed segment never read past `offset`; they open the mark by path for each read of it. Sealed
  segments are read whole.
- Recovery follows `spec.md` §3; the order of a start (§3) says when the mark is written and synced.

## A.11 What an implementation must test
- Round trip of every block type, with property tests over row counts, encodings and field combinations.
- Golden files for v1, including: an unsealed segment with a torn final batch; segments with a recovery entry, with
  a gap before a recovery entry, and with an adopted recovery entry followed by the rest of a torn tail; corruption
  inside a segment with a kept batch after it; each seal state of A.4; a sealed segment with series batches (A.8a); a mark with one torn slot; a stale mark below
  acknowledged batches after a power loss; flagged batches beyond a same-boot mark, then an unflagged one (A.7); a block with an unknown ignorable and an unknown critical element; the
  store descriptor; a manifest with a torn tail and one with a corrupt middle record; a registry file; a log
  directory root and group, a series directory and a status file (A.18).
- A fuzz target for the block reader and for each body decoder, the mark, manifest record, cursor, lease and registry
  file decoders included: no input may cause a panic or an out-of-bounds read; nothing is allocated from a declared count or
  length before it is checked against the body, and decoded output is bounded by the row and entry limits. The
  segment and manifest targets also check the scan bound (common rules).
- Truncation at every byte offset of a batch, a recovery batch included: the reader returns exactly the complete
  batches before it.
- Derived files (A.18): a rebuilt file equals the live one; a crash at every write leaves the previous generation or
  the new one, and the writer ends where an uninterrupted run would; a replaced segment, a restore, a reset, a lost
  range and a manifest rebuild each invalidate only the part they touch, through a new start of the writer as well
  as in the checks; the listing is contiguous, also after a late seal and an unreadable segment at the start; no file names a position above the last synced mark; a lookup
  located through them equals a full scan; uncovered sealed bytes stay within the allowance at any ingest rate.

## A.12 Directory layout, store descriptor, names and lock
```
<dir>/STORE                                  store descriptor, kind 7; written once, never changed
<dir>/LOCK                                   empty; the writer holds an exclusive OS lock on it
<dir>/MANIFEST                               kind 3 (A.14)
<dir>/MANIFEST.new                           a rebuilt manifest before its rename (A.14); removed at start if left by a crash
<dir>/log/MARK                               A.10
<dir>/log/<generation:8 hex>-<file_id:16 hex>.open | .kvl
<dir>/cursors/<consumer name>.cur            kind 4 (A.15)
<dir>/leases/<reader_id:32 hex>.lease        kind 8 (A.15)
<dir>/registry/<generation:8 hex>-<file_id:16 hex>.reg   kind 5 (A.16)
<dir>/registry/labels/<generation:8 hex>-<after:16 hex>-<through:16 hex>-<file_id:16 hex>.lab   kind 18 (A.19)
<dir>/derived/logdir-<file_id:16 hex>.root  kinds 15–17 (A.18): log directory root and groups, series directory, status file
<dir>/derived/logdir-<group:8 hex>-<file_id:16 hex>.grp | series-<file_id:16 hex>.sdir | status-<file_id:16 hex>.stat
<dir>/tree/, data/, quarantine/              reserved for kinds 2, 6 and 9–14; laid out by roadmap 6.2, 7.1 and 8.1
```
- Hex is lower-case and zero-padded to the stated width; extensions are lower-case. Readers match names exactly and
  ignore any other name. A consumer name is `[a-z0-9][a-z0-9._-]{0,63}`, so it is its own file name.
- **`STORE`** is an A.1 header (kind 7, `file_id` 0, `base_sequence` 0) followed by one block of type `0x01` whose
  body is a field list (A.6 framing): 1 site zone at creation (UTF-8 IANA name, at most 63 bytes); 2 tzdata version
  at creation (ASCII, at most 15 bytes); `0x8001` creating software version (ignorable). The store id is the
  header's. The site-zone and tzdata entries (A.6) override fields 1 and 2 from their positions, so positions start
  at 1 with the application's entries (`spec.md` §5.7). A restore copies `STORE` unchanged.
- **Creation** (only in a directory that does not exist or is empty): create the directory and sync its parent, so the
  store's own entry is durable (directories above the parent that creation made are the caller's), create `LOCK` and
  take the lock; create `log/`, `cursors/`, `leases/`; create `MANIFEST` (header and one empty record) and sync it and
  the directories; **last**, write `STORE.tmp`, sync it, rename it to `STORE` and sync the directory. A directory is a
  store only once `STORE` exists. The first start creates the mark (`spec.md` §3). A restore that creates the store
  directory syncs its parent the same way before it writes the manifest (`spec.md` §8 Restore, Q-871).
- **Open.** `STORE` valid: sync the directory (a creation cut short between the rename and that sync may have left
  `STORE` in the page cache only, as may a rebuild cut short between renaming `MANIFEST.new` and its directory sync,
  A.14), then open. No `STORE`, and every name present is one that creation makes (`LOCK`, `MANIFEST`,
  `STORE.tmp`, and the directories `log/`, `cursors/` and `leases/`, empty): a creation that did not finish; remove
  what it left and create again. No `STORE` and any other name, a file under `log/` included: `NotAStore` (never
  created over; a directory part-way
  through a restore is told apart by its restore record, roadmap 8.3). A `MANIFEST.new` beside a valid `STORE` is a rebuild that did not finish and is removed. `STORE` invalid: `Corrupt`. Accepted limit:
  `STORE` is a single local copy until it is uploaded, repaired by an operator from the bucket's copy (`spec.md` §8
  Deep, roadmap 8.2). Every other file's
  `store_id` must equal `STORE`'s.
- **`file_id`** is unique per store and kind across all generations and never reused. The next id of a kind is 1 +
  the highest id among local file names of that kind, ids named by any manifest record (a restore record lists every
  file it restores from, local or not), any rebuild record's floor of that kind (A.14) and, for log segments, the
  mark's `file_id`. `<generation>` in a name is the generation in which the file was created. A valid header whose
  `file_id` or generation differs from the file name is refused, never scanned or deleted.
- **Lock.** `std::fs::File::try_lock` on `LOCK` (`flock` on Unix, `LockFileEx` on Windows), held for the life of the
  writer and released when the process ends. A second writer, in this or another process, fails with `Locked`.
  `LOCK` stays empty. Readers never take it. Network file systems are not supported. That a second open in the same
  process is refused is verified at roadmap 4.1 on all three platforms; the interface also keeps an in-process table
  of open store paths (`api.md` Handles).

## A.13 Platform operations
All of them go through the `Fs` seam (`api.md`), without unsafe code.

| Operation | Linux | macOS | Windows |
|---|---|---|---|
| Batch sync (zero-filled segment) | `sync_data` (`fdatasync`) | `sync_data` (`F_FULLFSYNC`) | `sync_data` (`FlushFileBuffers`) |
| After creation, length change, seal | `sync_all` | `sync_all` | `sync_all` |
| Mark, manifest, cursor | `sync_data` | `sync_data` | `sync_data` |
| Directory sync | open the directory, `sync_all` | the same (works on APFS, roadmap 2.2) | no directory handle: the created or renamed file itself is flushed (`sync_all`) after the create or rename |
| Positional I/O | `FileExt::read_at` / `write_at` | the same | `FileExt::seek_read` / `seek_write` |
| Lock | `File::try_lock` | the same | the same |
| Free space | a dependency (`statvfs`) | the same | a dependency (`GetDiskFreeSpaceEx`), to be verified at roadmap 4.1 |

- Sync semantics are those of `spec.md` §3 Sync primitive and the roadmap 2.2 measurements. On Windows the flushed
  handle is the rule: the production `Fs` flushes the file after every create and rename, and `sync_dir` does nothing
  more. Windows is a development platform (`spec.md` §3 Sync primitive): whether a name made durable this way
  survives a power loss, and whether `FlushFileBuffers` reaches the device cache, are checked with the owner's
  hardware run before gate 4.9 (moved from 4.4 and then 4.8, Q-115 (j)), but neither decides whether Windows is
  accepted.
- **Readers** use positional reads only and never map a file. All handles are opened sharing read, write and delete
  (std's default on Windows), so the writer's rename and length change succeed while readers hold a file. A
  rename or length change that fails with a sharing error (another program) is retried every 10 seconds with health
  degraded (`SealBlocked`); the segment stays a complete `.open` file. Sharing errors are, on Windows, the system
  errors 5 (access denied), 32 (sharing violation), 33 (lock violation) and 1224 (user-mapped section); on any
  platform, 'resource busy'. Lack of space at the rename is retried the same way. Any other failure of the rename or
  length change is a write failure on the log. If the rename fails but the `.open` name is gone and the `.kvl` name
  exists, the rename happened and its flush failed: a failed sync, never retried. A reader that does not find
  a name lists the directory and tries the other extension once.

## A.14 Manifest framing
- `MANIFEST` is an A.1 header (kind 3) followed by **records, which are A.2 blocks**. A record body is
  `epoch: u64`, `generation: u32`, reserved `u32`, `written_at: i64`, then a field list (A.6 framing). **Every**
  record, empty and lost-range records included, takes an epoch strictly above the previous record's (the first
  record, written when the store is created, takes epoch 1): the previous + 1, or after a start the next above every
  epoch named by the last valid record and by any lease file, in its `epoch` or `highest_read` (A.15; `spec.md`
  §7 Manifest).
- Record types: `0x20` empty; `0x23` lost range; `0x24` acknowledgement (repeated field 1: identity, first, last, as
  positions; field 2: the epoch of a rebuild record acknowledged; field 3: outcome `u8`, 1 restored in full, 2 re-send
  ended, 3 by an operator, absent meaning 3; field 4: positions restored `u64`, present exactly with outcome 1;
  repeated field 5: a corruption seen, below; a record with outcome 1 or 2 is a closing record of self-repair and
  names exactly one loss in field 1, `spec.md` §8 Self-repair; one with outcome 1 closes only the lost-range records of
  that identity with a lower epoch, so a later one opens the loss again; a record names at least one of fields 1, 2 and 5; fields 3 to 5 are
  decoded from roadmap 8.5b, which refuses a record holding them until then, and written from roadmap 8.5c); `0x28` rebuild (1 the corrupt record's epoch as
  presumed, the last valid epoch before it + 1, `u64`; 2 epoch of the last valid record before it, `u64`; 3 files found, `u64`; 4 lost ranges recorded again, `u32`;
  5 `file_id` floor, repeated, one per file kind: kind `u8`, id `u64`, 2^32 above the highest id of that kind in any
  local file name, any header's list of inputs, any valid record and the mark; 6 losses listed under their first
  missing position because no valid record names their identity, `u32`; 7 repeated, each corrupt span the rebuild
  stood in for, as the epochs of the valid records around it in the manifest it rebuilt: after `u64` (0 for none),
  before `u64` (0 for none: a span with no later record, Q-650, only the last), in ascending order and not
  overlapping), the first record of a rebuilt manifest.
  Every record carried into a rebuilt manifest, whatever its type, holds field 100, its old epoch `u64` (`spec.md`
  §7 Manifest); a record already holding field 100 (carried by an earlier rebuild) keeps it, and a reference to a
  record by epoch, such as a rebuild record's fields 1 and 2, is matched against field 100 where a record has it. `0x21` publish and retire, `0x22` sealed log segment, `0x25` restore, `0x26` quarantine and `0x27` copy verified are
  laid out below. A rebuild never reconstructs `0x26` or `0x27`: a
  quarantine is shown by the file's place under `quarantine/`, a tier by where the file is found, and "verified in
  another tier" is verified again, so their layouts may hold no fact that only they hold; after a rebuild a
  file under `quarantine/` that no carried record names is a corruption dated at the rebuild record, and a lost range
  found again with cause 5 is one too, with no new field (`spec.md` §8 Self-repair, Q-244). Until their items lay them
  out, a reader takes a record of a reserved type as a field list (A.6 framing) and a rebuild carries it unchanged
  with field 100 added; the catalogue of files does not apply one. A reserved type stays written and carried as a
  field list once laid out; only its decoding becomes strict.
- Quarantine (`0x26`, `spec.md` §8 Corrupt block, Self-repair): 1 the file, a file description (below); 2 reason
  `u8` (1 damaged on hot, 2 damaged on cold, 3 damaged in deep, 4 lost in the bucket); 3 the copy tried first `u8` (0
  none, 1 hot, 2 cold, 3 deep, 4 the mirror path). Each field exactly once; another field below `0x8000` is
  unsupported. Appended when a file is found damaged or judged lost, before a copy is fetched; it names a
  corruption (`spec.md` §8 Self-repair, Attention). A file lost in the bucket is not moved locally. A reason 4 with no later `0x27` for the file
  takes its copy out of a restore's plan (Q-295) only where recorded lost ranges cover each of its positions that
  no other listed file holds; otherwise (the re-upload from a local copy or the lost-range record not yet written)
  it stays listed and the restore judges it (`spec.md` §8 Restore, Failures and end). A
  re-upload from a verified local copy writes a new `0x27`, as does an object that answers again while its loss is
  open and so restores it in full; one that answers again after its loss was closed gets none, so it stays out of
  every restore's plan and the closed loss stays closed in every generation (Q-600). Decoded strictly
  from roadmap 8.5b, with `0x24` fields 3 to 5 and `0x25` fields 11 to 13; until then it is read as a reserved type.
- Corruption seen (the value of `0x24` field 5 and `0x25` fields 12 and 13, 24 bytes): 0 kind `u8` (1 a file, by its
  quarantine record; 2 a lost range, by its loss's identity; 3 a manifest record); 1 reserved (3 bytes); 4 for a
  file, its kind `u8`, 3 reserved and `file_id` `u64` (12 bytes, as an input entry of a file description), then the
  epoch `u64` of the quarantine record that names this finding of it (matched against field 100 where a record has
  it) or, for a file under `quarantine/` that no valid record names after a rebuild, of that rebuild record (Q-244);
  for a lost range, the identity (a position, 12 bytes) then 8 reserved bytes; for a manifest record, its epoch `u64`
  then 12 reserved bytes. Each finding of a file is a corruption of its own: a file found damaged or judged lost
  again, after its copy was swapped in or its object answered again, has a new quarantine record and is a new
  corruption, so seeing an earlier finding does not see it. Naming a corruption seen twice changes nothing.
- Copy verified (`0x27`, Q-281): 1 the file, a file description (below), so the record repeats the file's kind,
  position range and plaintext hash as every record naming a file does, and a record describing a file otherwise than
  an earlier record is refused; 2 the tier
  holding the copy, `u8` (1 hot, 2 cold, 3 deep); 3 the object's size `u64`; 4 its SHA-256 as the bucket reported it
  (32 bytes; for deep, of the ciphertext, `spec.md` §8 Hashes). Each field exactly once; another field below `0x8000`
  is unsupported. Written only after the bucket reported that size and hash for the uploaded object. One carried
  into a rebuilt manifest (it holds field 100) gives the copy but does not count the file as verified, which takes a
  fresh record (`spec.md` §7 Manifest; Q-280).
- **Canonical writing.** A record's own fields in ascending id order, a repeated field's values in the order stated
  for it; then field 100; then any ignorable fields it was read with, in their order. A record of a reserved or
  unknown skippable type keeps its fields in their order, field 100 placed before the first field with a higher id.
- **File description** (the value of a field that names a file, at most 65,535 bytes): 0 kind `u8` (1 log segment,
  2 compacted segment); 1 class `u8` (0 none, for a log segment; 1 current; 2 revision); 2 flags `u8` (bit 0: the
  file holds positions, given by `first` and `last`; the other bits reserved); 3 reserved; 4 generation `u32`; 8
  `file_id` `u64`; 16 first position; 28 last position; 40 plaintext SHA-256 (32 bytes); 72 input count `u32`; 76
  reserved `u32`; 80 the inputs, 12 bytes each: kind `u8`, 3 reserved, `file_id` `u64`, in strictly ascending order
  of kind and id. Checks: the length is 80 + 12 × the input count; a log segment has class 0 and no inputs; a
  compacted segment has class 1 or 2, positions and at least one input; an input is a log segment or compacted
  segment other than the file itself; first is at or below last. This is everything a self-describing file's header
  and trailer say about it (below), so a record that names a file repeats its kind, position range and hash.
- Publish and retire (`0x21`): repeated field 1, a file published; repeated field 2, a file retired; each a file
  description, each list in strictly ascending order of kind and id, at least one file in all, no file in both.
  One record publishes and retires atomically: a compaction of log segments publishes its outputs and retires
  nothing (log segments are never retired by compaction, `spec.md` §7), a compaction of compacted files publishes its
  outputs and retires its compacted inputs. No epoch lists a compacted file live together with a compacted file it
  names as an input (invariant 14 as the records show it).
- Sealed log segment (`0x22`): repeated field 1, a sealed log segment's description (kind 1), in strictly ascending
  order of id, at least one. It is appended after the seal, never before: from the description the sealer hands over
  once the `.kvl` name and the directory are synced (A.9 Plaintext hash), and a seal does not wait for it; one lost
  in a crash is appended again after the next start (`spec.md` §7 Manifest). A description leaves the writer only
  after the start that preceded the seal has synced every lost-range record it wrote (steps 4 and 6); a start that
  cannot write one is read-only and hands out none. So no sealed-segment record has an epoch below a lost-range record
  of the start that preceded its seal (Q-290).
- Restore (`0x25`, `spec.md` §8 Generations, Restore step 1): 1 generation `u32`, the one the restore claimed, at
  least 1; 2 reset stamp, 16 bytes: sequence `u64`, always 1 (the generation's first position), then nonce `u64`,
  never zero; 3 source generation `u32`, whose records it read, below field 1; 4 restored end (position), in a
  generation below field 1; 5 repeated, a restored end carried from the restore record it read: that restore's
  generation `u32` then its end (position), the read record's own end first and then the ends it carried, in strictly
  descending order of generation, each generation at most field 3 and each end in a generation below its own; 6
  repeated, a lost range carried: the fields of a lost-range record (`0x23`) as a nested field list (A.6 framing), in
  strictly ascending order of identity; 7 repeated, an acknowledged range carried, as `0x24` field 1, in strictly
  ascending order of identity, first and last; 8 repeated, a recovery-entry loss report carried, nested as field 6, in
  the order read; 9 repeated, a file it restores from, set-aside segments included: tier `u8` (1 hot, 2 cold, 3 deep:
  where the restore places it, step 3), 3 reserved, the size `u64` and SHA-256 (32 bytes) of its copy in deep as the
  bucket reported them (ciphertext), then its file description, in strictly ascending order of kind, generation and
  `file_id`, each file once; 10 present, with no value, in a drill's record, which is never uploaded; 11 repeated, the outcome of a carried acknowledged range closed otherwise than by an operator:
  identity (position), outcome `u8` (1 or 2), 3 reserved, positions restored `u64` (0 with outcome 2), in strictly
  ascending order of identity, each naming a field 7 range; 12 repeated, a corruption seen carried, as `0x24` field
  5, in the order read; 13 repeated, a corruption not yet seen carried, as `0x24` field 5, in the order read, each
  named once in fields 12 and 13 together (fields 11 to 13 are decoded from roadmap 8.5b, which refuses a record holding them until then, and written from
  roadmap 8.5c, Q-241). In every range
  first is at or below last. A record that breaks a rule is invalid; an unknown critical field, here or in a nested
  lost range, is a version refusal. A reader keeps a restore record's fields in the order read, as for a reserved
  type, so it is carried unchanged; canonical writing puts them in ascending id order. A record must fit one block (A.2 sizes), which bounds a restore at about 130,000
  listed files (Q-292).
- Lost range (`0x23`): 1 identity (position); 2 first (position); 3 last (position); 4 `file_id` `u64`; 5 byte offset
  `u64`; 6 byte length `u64`; 7 cause `u8` (1 corruption, 2 truncated tail, 3 lost in the bucket, 4 invalid
  content, 5 unknown: found again by a manifest rebuild with no cause given, `spec.md` §7 Manifest); 8 boot id
  (16 bytes); 9 id floor `u32`; 10 and 11 commit times before and after (11 absent at the tail); 12 present when the
  record extends the loss named by field 1 (records of identities whose ranges overlap are one loss under the lowest
  identity, `spec.md` §3 Order of a start step 4; builds before Q-1102 list them apart and are otherwise
  unaffected); 13 the reset stamp in force when the record was written (16 bytes, as A.10 bytes 84..100): the
  mark's or, with the mark lost, that of the last kept recovery entry; absent when none is known, which counts as
  later than every reset (`spec.md` §7 Readers, Q-124 (c)).
  Fields that settling Q-33 and Q-48 needs are added by roadmap 4.5 as new field ids.
- A torn tail and a corrupt record are told apart as `spec.md` §7 Manifest says: a forward search at 8-byte steps
  for a valid record with a higher epoch; the last record is never acted on irreversibly until a successor is
  synced. Once failed reads starting in one 16 MiB window of the file have checked 16 MiB of body (common rules:
  scan bound), the search passes the rest of that window: a valid record beyond it makes the passed bytes part of the
  corrupt span; with none, the passed bytes and everything after them are a corrupt span with no later record, kept
  and never truncated, and repaired by a copy or a rebuild like any corrupt record (Q-650): a copy is written before
  the span's bytes, which stay after it (so with n copies in the bucket the span is settled over n reads, each writing
  the manifest again), and a rebuild takes its epochs above the higher of the last valid epoch before the span and
  every epoch a checksum-valid record image starting in it names, plus the records of 40 bytes, the shortest, that the
  span could hold (Q-872). Damage does not reach the window rule, so a torn tail is never reported
  this way.
- Every file is self-describing: its header and trailer carry everything a publish record says about it (kind,
  class, `file_id`, generation, position range, input `file_id`s, plaintext SHA-256 of the body; a log segment's
  hash is computed from its bytes, A.9). A corrupt record with no copy is rebuilt by the writer itself, by the
  procedure of `spec.md` §7 Manifest (the same as the operator call `rebuild_manifest`, roadmap 6.3): the new manifest is written as `MANIFEST.new`, opened by a rebuild record
  (`0x28`), synced, renamed over `MANIFEST`, and the directory synced; a `MANIFEST.new` left by a crash is removed at
  the next start. The rebuild record is acknowledged by an acknowledgement record carrying the rebuild record's own
  epoch in field 2. The rebuilt manifest carries every valid record of the old one, and the rebuild reconstructs
  only what the corrupt record alone held (`spec.md` §7 Manifest); a restore record is never rebuilt. A rebuild record that no
  acknowledgement record names is provisional at a start, also one that was final before it: the store holds uploads, evictions, removals and
  retirements, and refuses to append an acknowledgement of it, until the mirror path and deep have answered in that
  run for every corrupt span its field 7 names (`spec.md` §7 Deep unreachable); a copy is a valid record whose epoch
  lies strictly between the span's two epochs, or above the first for a span with no later record. Field 6 counts restated losses no
  valid record names where the old manifest held two corrupt spans, or one whose next valid epoch is more than 2
  above the last valid one before it, or one with no later record.

## A.15 Cursor file and lease file
- **Cursor**, `cursors/<name>.cur`, kind 4: an A.1 header then blocks of type `0x30`, **appended**, one per cursor
  sync. Body: `position` (12 bytes, the last confirmed position), reserved `u32`, then fields (A.6 framing): 1
  consumer name; 2 latch (repeated, in funnel order, 40 bytes: funnel id `u32`, the funnel's incarnation, the
  position of the routing change that added it, 12 bytes, the position the delivery carrying the state was emitted
  at, 12 bytes, state `u8` (0 false, 1 true, 2 on change with no value, 3 on change with a value), 3 reserved bytes,
  the value `f64`, finite, for state 3, else zero); 3 the id of the last reset point the consumer confirmed (12
  bytes, absent before any); 4 the **gate cursor** (32 bytes, a *gate point*: tag `u8` (0 nothing confirmed yet, 1
  a transition id, 2 a frontier), `due` presence `u8`, 2 zero bytes, `at` 12 bytes, `due` `i64` (zero when absent),
  gate id `u32`, series id `u32`; tag 0 has every other byte zero, tag 2 has `due` present as the frontier's
  instant and zero gate and series), present in every block written once the consumer's first watch has been
  applied (`spec.md` §6 Gates, Resume; Q-1391). An unknown critical field (`< 0x8000`) is a version refusal; fields
  for timer sequences are added by roadmap 5.3. Vectors: the gate point of transition `(at (0, 12), due 7,000, gate
  4, series 9)` is `01010000 000000000c00000000000000 581b000000000000 04000000 09000000`; of the frontier `(at
  (0, 20), now 5,000)`, `02010000 000000001400000000000000 8813000000000000 0000000000000000`.
- **Gate checkpoint**, block type `0x31` in a cursor file: a consumer's gates engine at a frontier, from which its
  judgement resumes (`spec.md` §6 Gates, Resume). Body, packed, little-endian: `through` (12 bytes, the last position
  applied), `now` `i64` (the commit-clock instant reached), instance count `u32`, series count `u32`, reserved
  `u32` zero; then each instance in `(gate, series)` order, strictly increasing: gate id `u32`, series id `u32`,
  `set_by` (a gate point of tag 1), `crossed` `u16` (a bit per boundary), pending count `u8` (at most 16), state tag
  `u8` (0 unknown, 1 stale, 2 level, 3 match, 4 value), the last evaluation's instant `i64`, the pending holds
  (boundary index `u8`, first instant held `i64`, in index order), the state's byte for tags 2 (boundaries crossed)
  and 3 (0 or 1) or its value for tag 4, then a reference presence `u8` (0 or 1) and, if 1, the value; then each
  series in id order, strictly increasing: series id `u32`, flags `u8` (bit 0 a live-state fact, bit 1 a last
  arrival, bit 2 the live fact is usable, bit 3 it has a revision; bits 2 and 3 only with bit 0), the last arrival
  `i64` if bit 1, then if bit 0 the live fact: `t` `i64`, position 12 bytes, revision `u64` if bit 3, its value. A
  value is a tag `u8` then: 0 none, 1 an `f64`, finite, 2 a boolean byte 0 or 1, 3 a length `u8` and that many
  bytes of UTF-8 (a named state or text). Facts ahead of `now` are not held: they are read again from the log at a
  restore. A body that breaks any of these rules, or has bytes left over, is invalid; nothing is allocated from a
  count before it is checked against the body. A write that carries a checkpoint appends `[0x31][0x30]` in one write
  and one `sync_data`. The file's state is the last valid `0x30` block **and** the last valid `0x31` block; a torn
  `0x31` leaves the previous pair, a torn `0x30` after a valid `0x31` leaves the new checkpoint under the previous
  cursor block (both are exact resume points). A `0x30` block with a gate cursor and no valid `0x31` before it is
  damage: the gates are rebuilt from history. A file written again at 16 MiB holds one `0x31` and one `0x30`. Every
  position a checkpoint names is a source of F. Vector (one levels instance mid-hold: gate 4 on series 9, level 0
  set by `(0, 12)`, boundary 0 held since 4,000, evaluated at 4,500; series 9 live at `t` 4,000, position `(0, 18)`,
  value 45.0, last arrival 4,000; frontier `(0, 20)` at 5,000), the body in hex: `000000001400000000000000
  8813000000000000 01000000 01000000 00000000 04000000 09000000 01000000000000000c000000000000000000000000000000
  0400000009000000 0000 01 02 9411000000000000 00a00f000000000000 00 00 09000000 07 a00f000000000000
  a00f000000000000 000000001200000000000000 010000000000804640`. A store that holds a gate is refused by binaries
  older than this element (unknown critical routing fields and block type, A.3; Q-1397); one without stays readable
  by them. Every position a cursor file names is a source of F (`spec.md` §3 Recovery rule 3), and
  the file is never written naming one above the last synced mark. The header has `file_id` 0 and the generation at
  creation. The state is the last valid block;
  a torn last block leaves the previous, older state, never ahead of a confirmation. No valid header or no valid
  block is "unreadable" and is named in the recovery entry (field 47). Only the writer opens cursor files; when a
  file passes 16 MiB the writer writes `<name>.cur.new` with the current state, syncs it, renames it over the old one
  and syncs the directory.
- **Lease**, `leases/<reader_id>.lease`, kind 8, 128 bytes: an A.1 header (`file_id` 0, the generation at
  creation) then one slot rewritten in place with one positional write: 64 `epoch: u64` (the lowest epoch of the reader's live
  cuts, `spec.md` §7 Readers); 72 `refresh_counter: u64`; 80
  `boot_id` (16 bytes); 96 `created_at: i64`;
  104 `highest_read: u64`, the highest manifest epoch the reader has read, 0 from a reader that does not write it;
  112 reserved (12 bytes); 124 `crc32c` over bytes 64..124. `reader_id` is 16 random bytes per reader handle. A lease
  is never synced (it is void on another boot). An invalid lease counts as holding epoch 0 until it expires; a zero
  `boot_id` is honoured until expiry. The reader deletes its lease on close; the writer deletes void and expired
  ones.

## A.16 Registry file (`kind = 5`)
The registry entries of the kept log (`spec.md` §2.1: definitions, parameter changes, aliases, retirements, scale
raises, site-zone, tzdata, routing and recovery entries; never label changes, type 10, which label parts hold,
A.19), each with its position, and an id floor, in one file on the
hot tier: `registry/<generation>-<file_id>.reg`.

- **Layout.** An A.1 header (kind 5; `generation` the generation its positions belong to; `base_sequence` 0), then
  zero or more entries blocks (type `0x02`, the A.6 body: `first_sequence`, the `received_at` of the batch the entries
  came from, the entries), then one **summary** block (type `0x40`, 24 bytes): 0 `through: u64`, the file holds
  every registry entry of the kept log with a sequence up to it; 8 `max_series_id: u32`, the highest series id its
  writer knew (the mark's, the kept log's, every recovery entry's id floor and the registry file read at the start;
  0 for none); 12 `entry_count: u32`; 16 `block_count: u32`, the entries blocks before it; 20 reserved. Entries come
  in position order, ignored entries (A.6) are not written, and positions rise strictly through the file.
- **Canonical writing.** Consecutive entries share a block when their sequences are consecutive and they came from
  one batch (one `received_at`), until the next would take the body past 16 MiB; a recovery entry is alone in its
  block, as in its batch.
- **Checks.** The header valid, of this store and kind 5; every block valid (A.2) and of type `0x02` or `0x40`, or of
  an unknown type with `skippable`; each entries body passing A.6; the summary the last block, ending the file, its
  counts matching, and no entry above `through`. A file that fails a check is **invalid**: it is passed over, never
  repaired, and removed by the next write. A newer `format_major`, an unknown critical block type, entry type or
  field is a version refusal (common rules): the writer refuses to open and never removes the file.
- **Kept recovery entries.** Every write keeps the recovery entries of the latest valid file of the same generation
  that its content lacks, up to its `through`, so a recovery entry stays in the file after its batch leaves the kept
  log (`spec.md` §11 Health, Q-1033). A recovery entry in the file is a reset point (`spec.md` §4 Resync) unless a
  recorded lost range holds it.
- **Writing.** The whole file is written under the next `file_id` of kind 5 (A.12), synced, and the directory
  synced (on the first write the `registry/` directory is created and the store directory synced); only then are the
  older registry files removed and the directory synced again. A crash at any point leaves the previous file, the new
  one, or both. The file never names a position above the last synced mark (`spec.md` §3 Durable mark): `through`
  is at most the mark's last position as synced. Until roadmap 6.3b publishes registry files, a registry file is
  not named by the manifest; it is found by its name, so a `registry/` directory emptied by hand starts the file ids
  of kind 5 at 1 again, which is harmless until the manifest names registry files.
- **Reading.** The valid file with the highest `file_id`; with none, the registry comes from the log alone.
- **At a start** (`spec.md` §3 Order of a start), the file is a derived file: its `through` is a source of F (Recovery
  rule 3), and `max_series_id` and every id it defines count toward the id floor (`spec.md` §2.1), as do its
  definitions' interval lengths toward the segment index's reach (A.8). Once the log is recovered, the writer rebuilds
  the registry from the kept log and checks the file against it (the Derived check, Q-33): the file must hold exactly
  the kept log's registry entries other than label changes up to the last synced mark, and the recovery entries the
  file read holds that the kept log does not, with an id floor at least the writer's; if it does not, it is written
  again from the kept log and those recovery entries, with the writer's id floor, so the id of a lost definition stays
  assigned. A seeded start (`spec.md` §3 Order of a start) reads the log only above its series directory's
  `through`: its registry is the file's entries up to that `through` (Q-1001 makes them every one) and the log's
  above it, and the file is checked against that, file plus tail, by the same rule (Q-33, Q-1116).
  The writer writes it again at clean shutdown, after the last mark sync, after a routing change, and before
  a series directory (A.18) whose `through` is at or above an entry the file does not hold, up to that `through`
  (Q-1001), so a reader continuing a series directory takes every entry at or below it from the file. Until log
  segments can leave the hot tier (roadmap 8.1) or be compacted (6.x), the kept log holds every registry entry but a
  recovery entry in a segment that cannot be read, so a start from the whole log checks the file and never trusts it
  over the log, apart from those. A seeded start trusts the file up to its seed's `through`, the part it does not
  read: Q-1001 makes the file hold every entry there, and the start gives way to the whole log when the file's
  `through` is below the seed's or its generation differs (Q-1116).

## A.17 Deep tier: object keys, uploaded forms, claim marker, envelope
Every object kvik puts in the bucket (`spec.md` §8 Deep) is an **envelope** (below), with two exceptions: claim
markers and checksum probes are stored unsealed, in the fixed forms given below, since their meaning lies in their
key (Q-340). Backups are always encrypted: no other object holds plaintext, and neither exception holds any of the
store's data. The sink's keys belong to kvik alone; a sink that shares a bucket maps them under
its own prefix, which the engine never sees.

- **Keys.** `<sid>` is the store id (A.1) as 32 lower-case hex digits; every key starts with it. Keys are ASCII,
  at most 128 bytes, hex lower-case and zero-padded as in A.12.
```
<sid>/STORE                                          the store descriptor (A.12)
<sid>/log/<generation:8 hex>-<file_id:16 hex>.kvl    a sealed log segment, under its local name
<sid>/manifest/<generation:8 hex>/<epoch:16 hex>     one manifest record
<sid>/claims/<generation:8 hex>                      a generation claim marker
<sid>/uploaded/<generation:8 hex>/<sequence:16 hex>  a last-uploaded marker (Q-614)
<sid>/probe                                          the conditional-write probe
<sid>/files/<kind:2 hex>/<generation:8 hex>-<file_id:16 hex>   a file of any other kind (A.1)
<sid>/labels/<generation:8 hex>-<after:16 hex>-<through:16 hex>-<file_id:16 hex>   a label part (A.19)
<sid>/probe/<nonce:32 hex>                           a checksum probe; stored only by a bucket that is refused (Q-213)
```
  A log segment (kind 1) is only under `log/` and a label part (kind 18) only under `labels/`; `files/01/` and
  `files/12/` name nothing. A listing of
  `<sid>/manifest/<generation>/` returns a generation's records in epoch order, and one of
  `<sid>/uploaded/<generation>/` its last-uploaded markers in sequence order. Generation 0 has no claim marker; it
  counts as claimed (`spec.md` §8 Generations). A reader ignores a key under `<sid>/` that this table does not name,
  and a key that does not start with a store id (32 lower-case hex digits and `/`). A restore finds the store id by
  listing from the first key: none is `NoStore`, more than one `SeveralStores`; `attach_backup` refuses a bucket
  holding a key under another store id (`OtherStore`).
- **Uploaded forms (the plaintext).** `STORE` and a sealed log segment: the local file, byte for byte, so its
  plaintext SHA-256 is the one the manifest records. A manifest record: `previous` `u64`, the epoch of the record
  before it in the manifest it was read from, 0 for none (a generation's first record, and a rebuild record that
  opens a rebuilt manifest, the one without field 100), then the record's A.2 block (header, body and padding) in
  its canonical encoding, which for a record this build wrote is the bytes `MANIFEST` holds, and nothing else;
  checks: `previous` below the record's epoch, and 0 for a rebuild record without field 100; one valid record block,
  its `generation` and `epoch` those of its key. `previous` comes from the manifest alone, never from the bucket, so
  an upload repeated after a restart sends the same bytes. Records upload in epoch order, so in the bucket each
  names the record of its generation and manifest uploaded before it, and a restore checks that chain (Q-610 to
  Q-612). A record carried by a rebuild is uploaded under its new epoch with field 100 (A.14).
  The store id is bound by the envelope (key derivation and associated data), not repeated in the plaintext.
- **Claim marker** (12 bytes, exactly, unsealed): 0 generation `u32`, that of its key; 4 reset nonce `u64`, the
  claiming restore's fresh nonce (`spec.md` §8 Generations), never zero. The store id and the generation are in the
  key, and a claim is read by whether its key exists (`stat`, Q-163), so the marker holds no store id or version. The nonce makes
  each attempt's marker unique (Q-161). A marker is written once; a restore that restarts before its restore record claims the next generation and never puts a
  marker again.
- **Last-uploaded marker** (Q-614; an envelope; the version 1 plaintext is 60 bytes exactly): 0 magic ASCII `KVKM`; 4
  version `u16` (1); 6 reserved (2); 8 generation `u32`, that of its key; 12 reserved (4); 16 sequence `u64`, that
  of its key, from 1; 24 `epoch` `u64`, the last record of the generation uploaded when it was put; 32 `open` `u64`,
  the first record of the latest manifest uploaded (the latest rebuild record without field 100, or with none the
  first record uploaded); 40 `read_first` and 48 `read_last` `u64`, the first and last record a restore of the
  records uploaded reads (`spec.md` §8 Generations, Q-108), both 0 when it reads none; 56 `marker_crc32c` over bytes
  0..56. Magic and version keep their offsets in every version; a later version may change the rest, its length
  included. Checks, in this order: magic; version (another is a version refusal, whatever the length); length;
  checksum; reserved bytes zero; sequence at least 1; `1 ≤ open ≤ epoch`; the read range both 0, or
  `1 ≤ read_first ≤ read_last ≤ epoch` with `read_first = open` or `read_last < open`; generation and sequence those
  of its key. Objects are never
  overwritten, so a generation's markers are a series with no gap in their sequences, and the latest is the one
  that counts: a reader lists `<sid>/uploaded/<generation>/` and asks `stat` for the sequences after the highest
  listed, doubling the step and then halving back, so a listing that lags hides no marker. A marker is put after
  each record of its generation has been put and before any later object, never before: by the upload driver at
  its next step, and by a full restore after its restore record (sequence 1). A put that meets another marker's
  content under its key takes the next sequence. At its first step the upload driver reads the latest marker, the
  records listed above it and those `stat` reports within 8 epochs after the highest it knows (as a restore asks,
  Q-612), checks that their uploaded forms chain from the marker's epoch (a break whose predecessor `stat` reports
  is a lagging listing: the step is retried), and puts a marker at once if they are ahead of it (a restart between a
  record and its marker, a latest marker that cannot be read where the one before names less, a bucket written
  before markers). A restore checks the
  latest marker that opens and decodes, passing over and reporting those after it that do not, but never one of a
  later version (`spec.md` §8 Generations, `api.md` Restore); the upload driver starts from the same marker, or from
  none when none can be read. A marker of a version it does not read stops its uploads
  (`UploadError::MarkerVersion`): an earlier version is never put after a later one that opens. The version is inside
  the envelope, so a later-version marker damaged so that its envelope does not open cannot be told from a damaged
  version 1 marker and is passed over. The scan down has no bound: N unreadable markers in a row cost N `get` (Q-1030).
- **Probe** (Q-162). The conditional-write probe at `<sid>/probe` is an envelope whose plaintext is never read. A
  checksum probe at `<sid>/probe/<nonce>` is unsealed: the 19 ASCII bytes `kvik checksum probe` (Q-340).
  `attach_backup`, and a restore before it claims, refuse a bucket (`BackupRefused`) when: a put of a fresh probe to
  `<sid>/probe`, where that key was absent, is followed by a second put of another fresh probe that succeeds, or
  after which `stat` reports the second object (`NoConditionalWrites`; where the key was present, the first put must
  fail and `stat` must report the earlier object unchanged); or a put of the checksum probe to a fresh
  `<sid>/probe/<nonce>` with a SHA-256 that is not its own (its own with every bit of byte 0 inverted) succeeds, or
  leaves an object under the key (`NoChecksums`); or a `stat` reports no SHA-256 (`NoChecksums`). `attach_backup`
  also refuses a bucket whose `<sid>/STORE` is present with another key check value (`WrongKey`); a restore reports a
  wrong key as `WrongKey` (`api.md` Backup and restore). A bucket that passes keeps one probe object for ever; the
  engine never deletes. Versioning or object lock cannot be probed through the sink and is the operator's to
  confirm. A drill writes nothing and does not probe. A restore reports a refusal as `RestoreError::Refused`
  (Q-214).

**Envelope.** XChaCha20-Poly1305 from the audited `chacha20poly1305` crate; HMAC-SHA-256 as the key-derivation and
nonce function. `K` is the application's 256-bit key (`api.md` `Encryption::Key`), `sid` the 16 store id bytes,
and labels are ASCII without a terminator.

- **Keys derived from `K`.** Encryption key `E = HMAC(K, "kvik envelope 1 encryption" ‖ sid)`; nonce key
  `N = HMAC(K, "kvik envelope 1 nonce" ‖ sid)`; key check value = bytes 0..8 of
  `HMAC(K, "kvik envelope 1 key check" ‖ sid)`. The key check value names the key without revealing `E` or `N`.
- **Header** (64 bytes, plaintext, bound as associated data): 0 magic ASCII `KVKE`; 4 version `u8` (1); 5
  algorithm `u8` (1: XChaCha20-Poly1305 with the derivation above); 6 reserved (2); 8 plaintext length `u64`; 16
  key check value (8 bytes); 24 nonce base (19 bytes); 43 reserved (17); 60 `header_crc32c` over bytes 0..60.
- **Nonce base**: bytes 0..19 of `HMAC(N, object key ‖ SHA-256(plaintext))`, the object key being the full key
  above as bytes (injective: the hash has a fixed length). The writer hashes the plaintext it encrypts, never a
  hash handed to it. The envelope is therefore **deterministic** in (`K`, store id, object key, plaintext): a `put`
  repeated after a crash or a restart sends identical bytes, which the sink accepts (`api.md` No overwrite), with
  no local record of the attempt. It reveals only that two objects under the same object key and key are identical,
  and keys are never reused for other content, so nothing is learnt. Two different plaintexts share a nonce base
  only by a collision of a 152-bit output.
- **Body**: the plaintext in chunks of 65,536 bytes, all full except the last, which holds 1 to 65,536 bytes; an
  empty plaintext is one empty chunk, and a length that is a multiple of 65,536 has no empty trailing chunk. So the
  chunk count is `n = max(1, ⌈length / 65,536⌉)`, at most 2^32 (a longer plaintext is refused, `TooLarge`). Chunk `i`
  is its ciphertext (as long as its plaintext) then its 16-byte tag, sealed under `E` with nonce
  `nonce base ‖ i as u32 ‖ final`, `final` a byte 1 for chunk `n − 1` and 0 otherwise, and associated data
  `header ‖ object key`. Truncation, extension, reordering, a chunk moved from another object, and an object
  copied under another key all fail authentication.
- **Size and ranges.** The object is `64 + length + 16 × n` bytes; a reader computes it with checked arithmetic
  from the header and refuses an object of any other size. Chunk `i` starts at object offset `64 + i × 65,552`.
  Plaintext bytes `[a, b)` (`a < b ≤ length`) lie in chunks `⌊a / 65,536⌋` to `⌊(b − 1) / 65,536⌋`; a ranged read
  fetches the header (bytes 0..64) and those chunks and opens each alone.
- **Reading, in this order.** Magic or `header_crc32c` wrong: a damaged object. A `version` or `algorithm` the
  reader does not know: a version refusal (`UnsupportedFormat`), never damage. Key check value not the key's:
  `WrongKey`, before any chunk is opened, never a loss (Q-82). A chunk that fails authentication under the right
  key: a damaged object. The ciphertext SHA-256 that `put` sends and the bucket reports (`spec.md` §8 Hashes) is
  over the whole object.

## A.18 Derived files: log directory (`kind = 15`), series directory (`kind = 16`), status file (`kind = 17`)
Files the writer derives from the log so that another process finds sealed segments, per-series state and the
writer's counters without scanning the log (`spec.md` §7 Derived files). Each is an A.1 header and exactly **one
block** (A.2) that ends the file; nothing else follows. Header: `generation` the store generation its positions
belong to; `file_id` its generation within its family (below); `base_sequence` 0; `created_at` its write time.

| Block type | File | Body |
|---:|---|---|
| `0x50` | log directory root | below |
| `0x51` | log directory group | below |
| `0x52` | series directory | below |
| `0x53` | status file | below |

- **Names** (A.12), under `derived/`: `logdir-<file_id:16 hex>.root`; `logdir-<group:8 hex>-<file_id:16 hex>.grp`;
  `series-<file_id:16 hex>.sdir`; `status-<file_id:16 hex>.stat`. Each name stem is a **family** (each group number
  its own); `file_id` is unique per family, not per kind (A.1), rises by one per successful write, and a header whose
  `file_id` differs from its name is invalid.
- **Fixed-size records.** Every record array carries a count and a `record_size`; a reader uses `record_size`,
  ignores a record's tail beyond the fields it knows (A.8's rule) and refuses a `record_size` below the v1 size, or
  zero, with a non-zero count. Positions are 12 bytes (common rules); a reset stamp is 16 bytes as A.10 bytes
  84..100, a zero nonce meaning none.
- **Checks.** The header valid, of this store and kind; one valid block of the family's type; nothing after it; the
  record arrays inside the body and filling it exactly; the checks below. A file that fails one is **invalid**. A
  block of another known derived type, or an unknown skippable one, is invalid; an unknown type without `skippable`
  or a newer `format_major` is a version refusal (common rules).
- **Writing.** Written whole to `<name>.tmp`, renamed to its name, **never synced**; then every generation of the
  family older than the previous one is removed (the status file keeps none; a full group keeps only the generation
  the root names). A write that fails removes its temporary file, is counted (status counter 14) and is tried again
  under the same `file_id` after a wait that doubles from 100 ms to a minute; none is tried while free space is below
  `limits.disk_critical`. The log directory, the series directory, the status file and the start's listings each
  wait on their own failures, so one that keeps failing holds back no other. It never makes the store read-only, and neither does a seal hook that fails. A writer
  removes `*.tmp` files of these families at its start, and writes nothing more after the store turns read-only. No
  derived file names a position above the last synced mark (`spec.md` §3 Durable mark): a sealed segment is listed
  only once the mark on stable storage is past it, also at the start, and the writer forces that mark sync. On
  Windows a rename that a reader's handle refuses is retried at the next write.
- **The start's work** (the writer's): the files there are, then two small reads of each sealed segment's trailer,
  then a read of each segment no valid group record describes, in steps between creating and sealing segments. A
  step is one of: the listing of `derived/` with the manifest, the root and the series directory; at most 8 group
  files; the listing of `log/`; at most 8 trailers; at most 256 reused records, or at most 4 MiB hashed of one
  segment. The two listings and the manifest are read whole (after a year at the defaults, about 400 group files and
  some 100,000 names in `log/`). Its worst case (a first start, a restore, a lost `derived/`)
  reads every kept segment once in the background; ingest does not wait for it, a shutdown does not finish it, and
  until it ends the files there are stand (a root written meanwhile never goes below one found valid).
- **Reading.** The newest generation of a family that is valid (and, for the root, matches the catalogue, below);
  failing that the previous one; failing both, the family is absent. A reader treats a missing, invalid or behind
  file as absent; a version refusal is an error for that file. The derived files are never backed up.
- **Sources of F.** The highest position of the current generation that a valid root (`through`) or series directory
  (`through`) names is a source of F (`spec.md` §3 Recovery rule 3); the status file names none. A file of a newer
  version is passed over there, and the writer writes none of that family and does not try to.

**Root body** (104 bytes, then the arrays in this order):

| Offset | Size | Field |
|---:|---:|---|
| 0 | 8 | `epoch`: the manifest record the root is complete through |
| 8 | 8 | `manifest_end`: the offset just past that record; a reader parses the manifest from here |
| 16 | 4 | `through`'s generation |
| 20 | 4 | CRC32C of that manifest record's block bytes: the record is matched by content |
| 24 | 8 | `through`'s sequence: every sealed segment whose last position is at or below `through` is listed; at or below the last synced mark |
| 32 | 8 | `segment_count`: segments listed in all groups |
| 40 | 16 | the mark's reset stamp when written |
| 56 | 16 | C(E): generation `u32`, reserved `u32`, sequence `u64`; zero for none (reserved for roadmap 6.3e) |
| 72 | 4 × 5 | counts: groups, lost ranges, quarantined files, compacted files, restored ends |
| 92 | 2 × 5 | `record_size` of each, in the same order: 72, 40, 16, in v1 0, and 16 |
| 102 | 2 | reserved |

Group entry (72): 0 `number` `u32`; 4 `segment_count` `u32`; 8 first and 16 last segment's `file_id`; 24 first
position; 36 last position; 48 first and 56 last commit time; 64 residence `u8` (1 hot; 8.1 adds the others), 3
reserved; 68 the group file body's CRC32C, which names the generation of the group this root was written with.
Lost range (40): 0 first, 12 last position; 24 `file_id`; 32 cause `u8` (A.14 field 7), 7 reserved: the manifest's
lost ranges as of `epoch`. Quarantined file (16): 0 kind `u8`, 3 reserved, 4 generation `u32`, 8 `file_id`; none
until roadmap 8.1 lays out the quarantine record. Compacted files: reserved (roadmap 6.3e may use them or leave the
catalogue to the manifest's publish record); count 0 in v1. Restored end (16): 0 the restore's generation `u32`; 4
the generation and 8 the sequence `u64` of the end it restored: the restored ends in force (`spec.md` §8
Generations, the restore record's own and those it carries); none in generation 0 (roadmap 8.3 fills them). The
epoch, `manifest_end`, lost ranges and restored ends are those of the writer's start; the manifest changes only at a
start today (Q-704), and a reader parses it from `manifest_end` (Q-775). Checks: groups numbered from 0, each with 1
to 256 segments and all but the last with 256, the first group starting its generation and each joining the one
before it (Contiguity, below), the counts summing to `segment_count`, and the last group's last position equal to
`through` (zero with no group); restored ends in strictly ascending order of the restore's generation, each end in a
generation below it.

**Group body** (16 bytes, then the records): 0 `number` `u32`; 4 count `u32`, at most 256; 8 `record_size` `u16`
(104 in v1); 10 reserved (6). Group *n* holds the sealed segments `256n` to `256n + 255`, counted in position order;
a full group's membership never changes: a segment removed locally (Q-745) keeps its record, with flag bit 1 "not
local" (Q-779, laid out by roadmap 6.3e). Segment record (104): 0 `file_id`; 8 generation `u32`; 12 flags `u8` (bit 0
`has_entries`, A.8; bit 1 reserved as above; bit 2 `has_labels`, copied from A.8), 3 reserved; 16
first and 24 last sequence; 32 first and 40 last commit time (A.9); 48 fact count (the sum of the segment index's
counts); 56 the file's length; 64 the trailer's `file_crc32c`, 4 reserved; 72 SHA-256 of the whole file (32). Checks:
records in strictly ascending position, each segment's last above the previous one's, first at or below last in
sequence and commit time.

**Contiguity.** Sealed segments follow one another, so a log directory has no hole. The first record starts its
generation: its first sequence is 1, or the lost ranges the root lists cover every sequence below it. Within a
generation each record's first sequence is the previous record's last + 1, or the lost ranges cover every position
between (adjacent ranges joining). A record of a later generation joins one of an earlier generation only when it
starts its generation as the first record does, a restored end in the root for the earlier generation, of a restore
in a generation above it up to the later one, is at or below the earlier record's last position (the lowest such end
where there are several; positions past it are set aside), and no restored end of that span names a position in a
generation between them. A group whose records do not join, and a root whose groups do not, are invalid. The writer never lets `through` pass a sealed segment it has not listed: a seal handed over late holds the
later ones back, and a sealed segment the start cannot read ends the listing below it (status counter 15).

**Validation against the catalogue.** A group is used only while every record names a live sealed segment of the
catalogue with the same `file_id`, generation and last sequence and the same plaintext hash (the manifest's file
description, A.14); until the manifest lists sealed log segments (roadmap 6.3b) the writer compares the length and
`file_crc32c` that the segment's trailer gives instead (Q-775). A root is used only while its header's generation is
the store's and the manifest holds a valid record ending at `manifest_end` with epoch `epoch` and that CRC32C. A group
is that root's only if its body CRC32C, number, count and first and last position are those of the root's entry. So a segment replaced
under a group (A.9, §8 Corrupt block) invalidates that group alone, a restore every file of the older generation,
and a manifest rebuild, which renumbers records, the root alone; a group file whose body CRC32C is not the one the
root names is not that root's. A part not used is read from the segments by a reader and rebuilt by the writer.

**Series directory body** (48 bytes, then the entries): 0 `through` (12); 12 `lost` `u32`, the positions of `through`'s
generation at or below it that the root's lost ranges (the manifest's, merged by identity) covered when it was
written, saturating at `u32::MAX`; 16 the mark's reset stamp when written; 32 `judged_at` `i64`, the
time the candidates and pending facts are judged at (the later of the write time and every `received_at` the entries
cover); 40 count `u32`; 44 `record_size` `u16` (96 in v1, 120 with the start counts); 46 reserved. Entries are in strictly ascending series id
and hold the series' state as of `through` (`spec.md` §7 Derived files). A file is a function of the kept log only
while no position at or below `through` has been lost since (more covered now than `lost`, a loss extended under its
identity included) and `lost` is not saturated: a reader and the writer drop it otherwise. A candidate whose `t` is above the reader's `now` (after a clock step back) is found again by
`at(series, now)`. The series directory advances only while the writer's process takes every durable batch (its
deliverer's index); a refresh it skips (Q-704) delays it, never falsifies it.

| Offset | Size | Entry field (96 bytes) |
|---:|---:|---|
| 0 | 4 | `series_id` |
| 4 | 2 | flags: bit 0 `latest_t` present; 1 `lateness_reference` present; 2 candidate present; 3 candidate unsure; 4 pending fact present; 5 more pending facts may exist; 6 start counts present; 7 the last fact carries a `changed_at` |
| 6 | 1 | the candidate's value kind: 0 read at the position (value and quality: a candidate continued from a pending fact), 1 `f64`, 2 decimal, 3 state, 4 boolean, 5 text (read at the candidate's position) |
| 7 | 1 | the candidate's adapter-supplied quality (A.5 codes, never bad; 0 for kind 0) |
| 8 | 8 | latest recorded `t` (`spec.md` §2.2) |
| 16 | 8 | lateness reference (§2.2) |
| 24 | 8 | fact count |
| 32 | 8 | orphaned facts (§2.1): facts of an id with no definition as of `through` |
| 40 | 8 | the **live candidate**'s `t`: the latest current fact (§2.3) that is not bad and not ahead of the write time |
| 48 | 12 | its position |
| 60 | 1 | its decimal scale; 3 reserved |
| 64 | 8 | its value: `f64` bits, decimal units (`i64`), state index, boolean 0 or 1; 0 for text |
| 72 | 8 | the **next pending fact**'s `t`: the earliest good current fact ahead of the write time |
| 80 | 12 | its position; 4 reserved |
| 96 | 8 | **start counts** (bit 6): facts flagged `late` |
| 104 | 8 | facts flagged `future` |
| 112 | 8 | the receive time of the series' last fact by position |

**Start counts.** A writer's seeded start (`spec.md` §3 Order of a start) continues the counts of §11 and each
series' last arrival (§6 Restart and resume) from fields 96 to 120; bit 7 says the last fact by position carries a
`changed_at`, so it may have been outranked and the last arrival is read from the log instead. A reader takes start
counts only from a record of 120 bytes or more with bit 6 set; a shorter record (v1, or a writer before them) gives
none, whatever its flags say. A writer starts seeded only from a directory every entry of which carries them, and
continues no fold from one that lacks them, so the first directory it writes after a start from the whole log
carries them (Q-1116).

Checks: a known value kind and a quality up to 2 when the candidate is present (0 for kind 0), a decimal scale at most 18, a state
index that fits `u16`, a boolean 0 or 1. Bit 3 means the candidate may not be the live fact (it was corrected to bad,
or a fact ahead that the entry does not name may have fallen due): a reader finds the fact in force instead (§2.3
`at`), counted. Bit 5 means facts ahead may exist that the entry does not name: once the named one falls due, a
reader finds the next. Fields whose flag is clear are zero.

**Status file body** (48 bytes, then the arrays): 0 the periodic interval in nanoseconds (0: written on a change of
health or pressure only); 8 the last synced mark (12), diagnostic: a reader reads the mark from `MARK`; 20 health
`u8` (0 ok, 1 degraded, 2 critical; a reader shows any other value as "other"); 21 pressure flag `u8` (0 or 1; set by
roadmap 9.0g, 0 before it); 22 flags `u8` (bit 0 **partial**: the level covers only the reasons the file carries, set
until `kvik health`, roadmap 9.0e, adds the rest); 23 reserved; 24 when the flag was last set (0 for never); 32 the
store's query slot count `u32` (roadmap 9.0g; 0 is unknown); 36 reason count `u32`; 40 counter count `u32`; 44 reason
`record_size` `u16` (4); 46 counter `record_size` `u16` (16). Reason (4): code `u16`, 2 reserved; codes 1 data lost,
2 ingest waiting, 3 disk low, 4 disk critical, 5 seal blocked, 6 manifest rebuilt, `0xffff` other (codes 1 and 6 are
not written before roadmap 9.0e). Counter (16): id `u16`, 6 reserved, value `u64`, in strictly ascending id; ids 1
queue depth, 2 time waited (ns), 3 batches, 4 syncs, 5 to 7 commit latency p50, p99 and largest (ns), 9 sealed
segments, 10 free bytes, 14 derived-file writes that failed, 15 sealed segments the log directory cannot list; 8 and
11 to 13 are unassigned. Every reason code and counter id is ignorable. A file older, by `created_at`, than twice
its interval (or with no interval) says nothing about the writer: it may have stopped, crashed or turned read-only,
after which it writes no file; a reader ignores its pressure flag and its level.

**Cadence** (the writer's; a reader relies only on the allowance). The log directory: the open group and the root at
each seal, once the mark is synced past it. The series directory: once the sealed bytes it does not cover, plus the
largest of the last 16 seals, would pass 32 MiB, and otherwise at least every 15 minutes, and at a clean shutdown,
after the last mark sync, through what the deliverer took, when more than 65,536 positions follow the last one's
`through` (so the next start's seed is near the head, `spec.md` §3 Order of a start); so between writes they stay
at most 32 MiB, and while a due write waits for the mark sync (or for the deliverer to take the batches) at most 32
MiB plus the seals since. Segments that seal by size (the default 64 MiB) are covered at every seal; small timed
seals at the 15-minute rule. A reader whose uncovered sealed bytes pass its 64 MiB allowance waits one mark-sync
interval and tries again before `DirectoryUnavailable` (Q-773; built with roadmap 9.0e: until then a reader
reads whatever lies above the directory it continues, Q-1003). The cadence holds while one seal (its read, CRC32C
and SHA-256) takes less time than a segment takes to fill. The status file: every 60 s and at once on a change of
health or pressure (looked at once a second).

**Crash and rebuild.** The files are unsynced, so after a power cut the newest may be invalid or absent. The writer
keeps the previous generation and rebuilds forward from it: a group's records are reused wherever they still match
the catalogue and only segments no valid group lists are read again; the series directory is continued from the
newest one that is still a function of the kept log (`spec.md` §7 Derived files), with the facts above its `through`.
A full rebuild from the log is the last resort.

## A.19 Label part (`kind = 18`)
The label changes (A.6 type 10) of the kept log of one generation with a sequence in `(after, through]`, each with its
position, in an immutable file on the hot tier: `registry/labels/<generation>-<after>-<through>-<file_id>.lab`
(`spec.md` §2.1 Labels, Q-892). Never on cold.

- **Layout.** An A.1 header (kind 18; `generation` that of its positions; `base_sequence` 0), then zero or more
  entries blocks (type `0x02`, the A.6 body, type 10 entries only), then one **summary** block (type `0x40`, 24
  bytes): 0 `after: u64`; 8 `through: u64`; 16 `entry_count: u32`; 20 `block_count: u32`, the entries blocks before
  it. Entries come in position order, positions rising strictly, each above `after` and at most `through`. Canonical
  writing as A.16.
- **Name and id.** `file_id` is the next of kind 18 by A.12's rule: 1 + the highest of kind 18 among local names,
  ids named by any manifest record (each upload is recorded there) and any rebuild record's floor. It is in the
  header and the name and breaks a tie between two parts of one range. **A part is removed locally**, for whatever
  reason (invalid, merged, set aside), only once a part with a higher `file_id` is durable (synced, its directory
  synced), so the highest local id never falls: no name, and with a backup no bucket key, is ever used twice
  (Q-1042). When nothing replaces a removed part, a part with no entries is written first to carry the higher id. A header whose
  generation or `file_id` differs from the name, or a summary whose `after` or `through` does, is invalid.
- **Writing.** Written to a temporary name, synced, renamed, and the directory synced (on the first write
  `registry/labels/` is created and `registry/` synced); never changed. A part never names a position above the last
  synced mark. The writer writes one after a seal whose segment carries `has_labels` (A.8), covering the label
  changes above the last part, and at a clean stop for label changes not yet sealed. A crash between a seal and its
  part leaves a flagged segment with no part.
- **Chains.** The first part of a generation has `after` 0; each later one starts where one in force ends. The label
  state is the fold over the generations in order, each up to the position its successor was restored from.
- **The parts in force** are the valid covering set of a generation with the fewest parts, the higher `file_id`
  winning a tie; a part wholly covered by a valid merged part is ignored. Parts are merged when more than a threshold
  exist (provisionally 64, 16 on the minimal class; the writer's choice, which no reader depends on): one new part
  replaces them, and the inputs are removed after the grace hour (`spec.md` §7 Retired files). A merge exists only once renamed after its sync; leftover temporary files are removed at a start.
- **Reading.** Every sealed segment above the last part in force whose index carries `has_labels` is read for its
  entries blocks, then the tail above the sealed segments. A segment without the flag holds no label change. The
  writer's start lists `registry/labels/` once and opens no part; parts are loaded and checked when labels are first
  needed. A reader lists the directory when it first needs labels, and again only when its cut passes a flagged
  sealed segment above the parts it holds or a part it holds can no longer be opened.
- **At a start.** Whether a part's `through` is a source of F, as the registry file's is (A.16), is open (Q-1043);
  a `through` read from a name alone is not checked and is never evidence by itself.
- **Checks.** As A.16, with kind 18, only type 10 entries, positions inside `(after, through]` and the name matching.
  A part that fails a check, is missing, overlaps a recorded lost range, holds positions set aside by a restore, or
  differs from the local log of its range is invalid and rebuilt from the kept log (compacted files' entries and
  flagged segments); a newer `format_major` or an unknown critical element is a version refusal.
- **Restore to P.** Parts wholly at or below P stay; the entries at or below P of a part spanning P are written as a
  new part ending at P; parts above P go with the positions set aside.
- **Backup.** Each part is uploaded once under `labels/` (A.17) and recorded like any uploaded file; a restore lists
  the part objects and takes the covering set up to its position, and one that finds none rebuilds labels from the
  uploaded files, or fails. The engine never deletes from the bucket (`spec.md` §8 Bucket): an input part's object is
  retired only in the backup record, which stops a restore from taking it, and only after the merged part covering
  it is uploaded and verified and the grace hour has passed; the object stays.
- **Release.** A log segment that carries `has_labels` is removed locally only once a part covering its label
  changes has been written and read back, and, on a store with a backup, uploaded and verified (`spec.md` §8
  Eviction, Q-745).

## B. Outline of the remaining files (not yet normative)

Every file below has an A.1 header and is self-describing (A.14). Parquet is the export format only, so A.1 applies
to every stored file.

- **Compacted segment (`kind = 2`) and chunk index (`kind = 14`)** are laid out in [`format-compacted.md`](format-compacted.md) (roadmap 6.2a1), which wins where the two outlines below differ.
- **Compacted segment (`kind = 2`).** Per-series chunks (one series, one time range) holding either current
  facts or revisions, each with its position range and a summary, followed by an index and a trailer in the
  style of A.9. Roadmap 2.1 decided the encodings: delta with frame-of-reference bit-packing per 128 values for `t`,
  positions and `i64`; scaled decimal with per-value exceptions and a zstd-compressed XOR fallback; run-length for
  quality, flags and states; 4,096-row blocks behind a directory of first and last `t`. Roadmap 6.1 laid out a
  block's body and its column encodings in [`format-chunk.md`](format-chunk.md); the chunk layout, the per-batch
  `received_at` table and the self-description fields are written by roadmap 6.2. A chunk of a decimal series
  keeps the per-fact scale column of A.5, and its summary, like a tree node of a decimal series, carries a `u8`
  scale: the highest stored scale among the current facts it covers that are not bad (`spec.md` §5.6). The index can
  be fetched without the chunk data: by a ranged read at an offset and length the trailer gives, or as its own object.
- **Chunk index (`kind = 14`).** Carries the compacted file's self-description (A.14: kind, class, `file_id`,
  generation, position range, input `file_id`s, plaintext hash), so that a manifest rebuild can read the inputs of a
  file that is not local (`spec.md` §7 Manifest).
- **Local restore record (`kind = 12`).** A copy of the restore record, written by every restore beside the
  manifest's, from which a corrupt restore record is read again (`spec.md` §7 Manifest, §8 Generations).
- **Manifest (`kind = 3`).** Framing, the empty, publish-and-retire, sealed-segment, lost-range, acknowledgement,
  restore, copy-verified (Q-281) and rebuild records are in A.14. Still to be laid out: the quarantine record. The restore record also carries
  the lost ranges, acknowledgements and recovery-entry loss reports of the generations it restores from. Every record that
  names a file repeats its kind, position range and plaintext hash, and every record that names a loss repeats its
  identity, so that no such fact is held by one record alone once a second names it (`spec.md` §7 Manifest).
- **Cursor (`kind = 4`).** A.15; latches and sequences are added by roadmap 5.2.
- **Registry (`kind = 5`).** Laid out in A.16 by roadmap 5.1; routing changes join it with roadmap 5.2, and its
  copy in tree checkpoints is laid out with roadmap 7.1.
- **Tree (`kind = 6`).** Per-series stored summary levels with the position up to which they are complete; each node
  of a decimal series carries its `u8` scale (`spec.md` §5.6). Roadmap 7.1a laid out a stored node's bytes in
  [`format-tree.md`](format-tree.md); the file, levels and checkpoint are written by roadmap 7.1b.

Each is specified in this document before the roadmap section that implements it. The derived files of roadmap 9.0a
(log directory, series directory, status file) are normative in A.18.
