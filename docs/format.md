# kvik file format, v1 (design; not yet implemented)

Byte-level rules for the files kvik writes. The semantics are in [`spec.md`](spec.md); where the two differ,
`spec.md` wins and this document is wrong. Part A (common rules, log segments, the durable mark, the store layout,
the platform operations, the manifest framing, cursor and lease files, the registry file, the deep tier's objects)
is complete enough to implement. Part B (compacted segments and the remaining files) is an outline; each file is
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

### A.1 File header (64 bytes, every file except the mark)
| Offset | Size | Field | Notes |
|---:|---:|---|---|
| 0 | 4 | `magic` | ASCII `KVIK` |
| 4 | 1 | `kind` | 1 log segment, 2 compacted segment, 3 manifest, 4 cursor, 5 registry, 6 tree, 7 store descriptor, 8 lease; reserved and laid out by their roadmap items: 9 calendar, 10 revised-key index, 11 version index, 12 local restore record, 13 checkpoint bundle, 14 chunk index |
| 5 | 1 | `format_major` | 1 |
| 6 | 2 | `format_minor` | 0 |
| 8 | 4 | `flags` | reserved |
| 12 | 4 | `generation` | |
| 16 | 8 | `file_id` | unique per store and kind (A.12) |
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
(`spec.md` §3 Recovery rule 2) and its loss record carries cause 4, invalid content (A.14).

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
segment index block, the trailer block and the 16-byte end marker; (2) set the file length to the end of the end
marker (the file may grow or shrink); (3) `sync_all`; (4) rename `.open` to `.kvl`; (5) sync the directory. The mark
does not move at sealing: it keeps naming S until the first commit in the next segment. An empty segment is never
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
| not in use (no complete batch; header invalid or `base_sequence = 0`) | unused: kept as the segment created ahead if its header is valid, its length is `segment_size` and every byte after its header is zero, else deleted |

A trailer and end marker that fail the first row's conditions are bytes of whatever precedes them (a torn batch or a
value); the file falls under the other rows. In the last segment in use a seal is never expected (Sealing).

A `.kvl` file is sealed and is not scanned by recovery. Whether a segment that is not the last in use lost data is
decided only by the gap between its last sequence and the next segment's `base_sequence` (`spec.md` §3 Recovery
rule 2), never by the bytes after its last batch. A start checks the gap rule for unsealed segments and against the
last sealed one; gaps between sealed segments are checked by the scrub. At most one segment is unused; any number
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
  integer to which the rule that times are `i64` does not apply; other rows hold 0 there. Flag bit 7 `rounded`: the
  value was rounded to the series' scale (`spec.md` §2.1 `excess precision`). Bits 8–15 are undefined and critical.
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
| 20 | 4 | reserved |
| 24 | … | entries |

Each entry: `entry_type: u8`, reserved 3 bytes, `length: u32`, then `length` bytes of fields, padded to 8. Fields
are a list of `field_id: u16`, `field_len: u16`, value, padded to 4. Fields appear in ascending id order; a field
appears once unless marked repeated. A zero-length field means "none". An unknown entry type or field is skipped or
refused by the critical and ignorable rule. **Every default is resolved when the entry is written:** a definition
carries each applicable field explicitly, and an absent field means "not applicable", never "the default in force
when read". Durations are `i64` nanoseconds. Strings are UTF-8 without a terminator.

| Entry type | Fields |
|---:|---|
| 1 series definition | the table below |
| 2 parameter change | 1 id; 20 effective time; then any of fields 7–12, with 27 whenever 8 is carried. Fields 23 to 26 may also be carried; they take effect at the entry's position, and an entry holding only them has no field 20 (26 only with 25 = 2, at least the scale in force). No other field is allowed (5 changes only by a scale raise; 13–19 and 22 are fixed for life). A change of 7 without 8 is written with field 8 holding the staleness limit in force after the change (`spec.md` §2.1: an S that was set is kept; one that was not becomes 3 × the new interval), and field 27 saying which, so the entry is self-contained |
| 3 alias | 1 id; 2 name |
| 4 retirement | 1 id; 21 `t_r` |
| 5 routing change | 50 consumer name (A.12); 51 change `u8` (1 adds a funnel, 2 removes one, 3 declares `emits` only); 52 funnel id `u32` (changes 1 and 2); for change 1: 53 selection kind `u8` (0 none, 1 ids, 2 name pattern, 3 every series), 54 series id `u32` (repeated: the ids of kind 1; for kind 2, the ids a condition funnel's pattern was resolved to when routed, else none), 55 pattern (UTF-8, 1–255 bytes, kind 2 only), 56 aggregate `u8` (1 avg, 2 sum, 3 integral, 4 min, 5 max, 6 count, 7 increase, 8 rate, 9 last, 10 time in state) with 57 window width `i64` ns > 0, 58 state counted (5 bytes: tag 0 then a boolean byte and 3 zero bytes, or tag 1 then a named state's index `u32`; aggregate 10 only), 63 condition `u8` (1 threshold: 64 activation and 65 clear level, clear ≤ activation; 2 on change: 64 dead-band ≥ 0; 3 held for: 64 level, 66 duration; 4 stale for: 66 duration beyond the series' staleness limit in force at the entry's position), levels finite `f64` and durations `i64` ns ≥ 0; always: 60 the consumer's `emits` kind, 61 its ids (repeated), 62 its pattern, as 53 to 55 but never with resolved ids. Fields 53 to 58 and 63 to 66 appear only in change 1 (`spec.md` §4 Routing) |
| 6 site zone | 30 zone name (UTF-8 IANA name, at most 63 bytes); 20 effective time |
| 7 tzdata | 31 version (ASCII release name, at most 15 bytes) |
| 8 recovery | the only entry of its batch (`spec.md` §3 Recovery rule 3): 40 `after` `u64`; 41 reset stamp (16 bytes: the entry's own position `u64`, then a random nonce `u64`, never zero), written by every writer, an entry without it read as carrying none (`spec.md` §7 Readers); 42 boot id (16 bytes, zero where the platform has none); 43 tail bytes `u64`; 44 readable commit blocks `u32`; 45 mark state `u8` (0 readable, 1 missing, 2 unreadable: no valid slot, A.10; 3 stale, `spec.md` §3 Recovery rule 1); 46 id floor `u32` (0: no id assigned); 47 consumer name (repeated, one per consumer whose cursor file was missing or unreadable) |
| 9 scale raise | 1 id; 5 decimal scale (the new scale, above the series' current scale); takes effect at its position (`spec.md` §2.1 `decimal scale`) |

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

**Checks** (a failure makes the block invalid, A.2; the presence and recovery checks are invalid content): `length`
or a field overrunning its container; a critical field out of order or repeated when not marked repeated; a value
outside its range above; a field missing where the tables say it is present (the "Present" column, field 27 with
field 8, field 20 in a parameter change, the recovery entry's fields, a routing change's fields per change and kind),
or present where it does not apply; a named-state series with no state name; a recovery entry that is not the only
entry of its batch. Kind and value type are not compared beyond the tables. Fields without a stated range take any
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
| 44 | 4 | reserved | |

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
one entries block), then per series, sorted by id, records of `record_size` bytes: `series_id: u32`,
`fact_count: u32`, `min_t: i64`, `max_t: i64`. Readers use `record_size` and ignore a record's unknown tail; this is
the rule for every fixed-size record array. A reader that is catching up, and compaction, skip a segment only if none
of its series is listed **and** `has_entries` is clear; otherwise they read at least its entries blocks.

`min_t` is the least `t`. `max_t` is the greatest *reach* of a reading and does not depend on tzdata: `t` for kinds
other than interval, `t + d` for a fixed interval, `t + 50 h` for `day`, `t + 33 days` for `month`.

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

## A.10 Durable mark (`log/MARK`, version 3)
`log/MARK` is 4,224 bytes: two 128-byte slots at offsets 0 and 4,096 (separate sectors). It has no A.1 header. It is
read and written by positional I/O and never mapped; it is synced on the timer (`mark_sync_interval`, on the writer
thread, also when idle), at once after a batch holding an entry that is not a fact (`spec.md` §3 Commit), at clean
shutdown and by recovery. It is updated in place and never replaced while it
exists; a new file is created only when none exists.

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
- **Same boot, page cache lost.** Rule 1 trusts the boot identity, because it must truncate a batch whose sync failed
  and which sits complete in the page cache. Since a batch is written only after the previous batch's sync and mark
  update, a current mark has at most one complete batch beyond it; two or more on the same boot mean the mark is
  stale and rule 3 applies (`spec.md` §3 Recovery rule 1, with its accepted limit). The recovery entry records mark
  state 3.
- **No usable mark.** With the mark missing, unreadable or stale, recovery cannot rule out the same boot, so it
  writes the last complete batch again before its first sync (`spec.md` §3 Recovery rule 3), with the segment's
  header when that batch is the segment's first. It does the same when the last complete batch is a recovery entry
  that the mark does not cover, whatever the mark, as the start that wrote it may have failed its sync (Q-117).
- Readers of an unsealed segment never read past `offset`; they open the mark by path for each read of it. Sealed
  segments are read whole.
- Recovery follows `spec.md` §3; the order of a start (§3) says when the mark is written and synced.

## A.11 What an implementation must test
- Round trip of every block type, with property tests over row counts, encodings and field combinations.
- Golden files for v1, including: an unsealed segment with a torn final batch; segments with a recovery entry, with
  a gap before a recovery entry, and with an adopted recovery entry followed by the rest of a torn tail; corruption
  inside a segment with a kept batch after it; each seal state of A.4; a mark with one torn slot; a stale mark below
  acknowledged batches after a power loss; a block with an unknown ignorable and an unknown critical element; the
  store descriptor; a manifest with a torn tail and one with a corrupt middle record; a registry file.
- A fuzz target for the block reader and for each body decoder, the mark, manifest record, cursor, lease and registry
  file decoders included: no input may cause a panic or an out-of-bounds read; nothing is allocated from a declared count or
  length before it is checked against the body, and decoded output is bounded by the row and entry limits.
- Truncation at every byte offset of a batch, a recovery batch included: the reader returns exactly the complete
  batches before it.

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
<dir>/tree/, data/, quarantine/              reserved for kinds 2, 6 and 9–14; laid out by roadmap 6.2, 7.1 and 8.1
```
- Hex is lower-case and zero-padded to the stated width; extensions are lower-case. Readers match names exactly and
  ignore any other name. A consumer name is `[a-z0-9][a-z0-9._-]{0,63}`, so it is its own file name.
- **`STORE`** is an A.1 header (kind 7, `file_id` 0, `base_sequence` 0) followed by one block of type `0x01` whose
  body is a field list (A.6 framing): 1 site zone at creation (UTF-8 IANA name, at most 63 bytes); 2 tzdata version
  at creation (ASCII, at most 15 bytes); `0x8001` creating software version (ignorable). The store id is the
  header's. The site-zone and tzdata entries (A.6) override fields 1 and 2 from their positions, so positions start
  at 1 with the application's entries (`spec.md` §5.7). A restore copies `STORE` unchanged.
- **Creation** (only in a directory that does not exist or is empty): create the directory and `LOCK`, take the
  lock; create `log/`, `cursors/`, `leases/`; create `MANIFEST` (header and one empty record) and sync it and the
  directories; **last**, write `STORE.tmp`, sync it, rename it to `STORE` and sync the directory. A directory is a
  store only once `STORE` exists. The first start creates the mark (`spec.md` §3).
- **Open.** `STORE` valid: open. No `STORE`, and every name present is one that creation makes (`LOCK`, `MANIFEST`,
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
  epoch named by the last valid record and by any lease file (`spec.md`
  §7 Manifest).
- Record types: `0x20` empty; `0x23` lost range; `0x24` acknowledgement (repeated field 1: identity, first, last, as
  positions; field 2: the epoch of a rebuild record acknowledged); `0x28` rebuild (1 the corrupt record's epoch as
  presumed, the last valid epoch before it + 1, `u64`; 2 epoch of the last valid record before it, `u64`; 3 files found, `u64`; 4 lost ranges recorded again, `u32`;
  5 `file_id` floor, repeated, one per file kind: kind `u8`, id `u64`, 2^32 above the highest id of that kind in any
  local file name, any header's list of inputs, any valid record and the mark; 6 losses listed under their first
  missing position because no valid record names their identity, `u32`; 7 repeated, each corrupt span the rebuild
  stood in for, as the epochs of the valid records around it in the manifest it rebuilt: after `u64` (0 for none),
  before `u64`, in ascending order and not overlapping), the first record of a rebuilt manifest.
  Every record carried into a rebuilt manifest, whatever its type, holds field 100, its old epoch `u64` (`spec.md`
  §7 Manifest); a record already holding field 100 (carried by an earlier rebuild) keeps it, and a reference to a
  record by epoch, such as a rebuild record's fields 1 and 2, is matched against field 100 where a record has it. `0x21` publish and retire, `0x22` sealed log segment, `0x25` restore and `0x27` copy verified are laid out below. Reserved for its roadmap item:
  `0x26` quarantine (8.1). A rebuild never reconstructs `0x26` or `0x27`: a
  quarantine is shown by the file's place under `quarantine/`, a tier by where the file is found, and "verified in
  another tier" is verified again, so their layouts may hold no fact that only they hold. Until their items lay them
  out, a reader takes a record of a reserved type as a field list (A.6 framing) and a rebuild carries it unchanged
  with field 100 added; the catalogue of files does not apply one. A reserved type stays written and carried as a
  field list once laid out; only its decoding becomes strict.
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
  order of id, at least one. When it is appended relative to the seal is roadmap 6.3b's.
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
  `file_id`, each file once; 10 present, with no value, in a drill's record, which is never uploaded. In every range
  first is at or below last. A record that breaks a rule is invalid; an unknown critical field, here or in a nested
  lost range, is a version refusal. A record must fit one block (A.2 sizes), which bounds a restore at about 130,000
  listed files (Q-292).
- Lost range (`0x23`): 1 identity (position); 2 first (position); 3 last (position); 4 `file_id` `u64`; 5 byte offset
  `u64`; 6 byte length `u64`; 7 cause `u8` (1 corruption, 2 truncated tail, 3 lost in the bucket, 4 invalid
  content, 5 unknown: found again by a manifest rebuild with no cause given, `spec.md` §7 Manifest); 8 boot id
  (16 bytes); 9 id floor `u32`; 10 and 11 commit times before and after (11 absent at the tail); 12 present when the record extends the loss named by field 1; 13 the reset stamp in force when the record was
  written (16 bytes, as A.10 bytes 84..100): the mark's or, with the mark lost, that of the last kept recovery
  entry; absent when none is known, which counts as later than every reset (`spec.md` §7 Readers, Q-124 (c)).
  Fields that settling Q-33 and Q-48 needs are added by roadmap 4.5 as new field ids.
- A torn tail and a corrupt record are told apart as `spec.md` §7 Manifest says: a forward search at 8-byte steps
  for a valid record with a higher epoch; the last record is never acted on irreversibly until a successor is
  synced.
- Every file is self-describing: its header and trailer carry everything a publish record says about it (kind,
  class, `file_id`, generation, position range, input `file_id`s, plaintext SHA-256 of the body). A corrupt record
  with no copy is rebuilt by the writer itself, by the procedure of `spec.md` §7 Manifest (the same as the operator
  call `rebuild_manifest`, roadmap 6.3): the new manifest is written as `MANIFEST.new`, opened by a rebuild record
  (`0x28`), synced, renamed over `MANIFEST`, and the directory synced; a `MANIFEST.new` left by a crash is removed at
  the next start. The rebuild record is acknowledged by an acknowledgement record carrying the rebuild record's own
  epoch in field 2. The rebuilt manifest carries every valid record of the old one, and the rebuild reconstructs
  only what the corrupt record alone held (`spec.md` §7 Manifest); a restore record is never rebuilt. A rebuild record that no
  acknowledgement record names is provisional at a start, also one that was final before it: the store holds uploads, evictions, removals and
  retirements, and refuses to append an acknowledgement of it, until the mirror path and deep have answered in that
  run for every corrupt span its field 7 names (`spec.md` §7 Deep unreachable); a copy is a valid record whose epoch
  lies strictly between the span's two epochs. Field 6 counts restated losses no
  valid record names where the old manifest held two corrupt spans, or one whose next valid epoch is more than 2
  above the last valid one before it.

## A.15 Cursor file and lease file
- **Cursor**, `cursors/<name>.cur`, kind 4: an A.1 header then blocks of type `0x30`, **appended**, one per cursor
  sync. Body: `position` (12 bytes, the last confirmed position), reserved `u32`, then fields (A.6 framing): 1
  consumer name; 2 latch (repeated, in funnel order, 40 bytes: funnel id `u32`, the funnel's incarnation, the
  position of the routing change that added it, 12 bytes, the position the delivery carrying the state was emitted
  at, 12 bytes, state `u8` (0 false, 1 true, 2 on change with no value, 3 on change with a value), 3 reserved bytes,
  the value `f64`, finite, for state 3, else zero); 3 the id of the last reset point the consumer confirmed (12
  bytes, absent before any). An unknown critical field (`< 0x8000`) is a version refusal; fields for timer sequences
  are added by roadmap 5.3. Every position a cursor file names is a source of F (`spec.md` §3 Recovery rule 3), and
  the file is never written naming one above the last synced mark. The header has `file_id` 0 and the generation at
  creation. The state is the last valid block;
  a torn last block leaves the previous, older state, never ahead of a confirmation. No valid header or no valid
  block is "unreadable" and is named in the recovery entry (field 47). Only the writer opens cursor files; when a
  file passes 16 MiB the writer writes `<name>.cur.new` with the current state, syncs it, renames it over the old one
  and syncs the directory.
- **Lease**, `leases/<reader_id>.lease`, kind 8, 128 bytes: an A.1 header (`file_id` 0, the generation at
  creation) then one slot rewritten in place with one positional write: 64 `epoch: u64`; 72 `refresh_counter: u64`; 80
  `boot_id` (16 bytes); 96 `created_at: i64`;
  104 reserved (20 bytes); 124 `crc32c` over bytes 64..124. `reader_id` is 16 random bytes per reader handle. A lease
  is never synced (it is void on another boot). An invalid lease counts as holding epoch 0 until it expires; a zero
  `boot_id` is honoured until expiry. The reader deletes its lease on close; the writer deletes void and expired
  ones.

## A.16 Registry file (`kind = 5`)
The registry entries of the kept log (`spec.md` §2.1: definitions, parameter changes, aliases, retirements, scale
raises, site-zone, tzdata, routing and recovery entries), each with its position, and an id floor, in one file on the
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
  definitions' interval lengths toward the segment index's reach (A.8). Once the log is recovered, the writer
  rebuilds the registry from the kept log and checks the file against it (the Derived check, Q-33): the file must
  hold exactly the kept log's registry entries up to the last synced mark, with an id floor at least the writer's;
  if it does not, it is written again from the kept log, with the writer's id floor, so the id of a lost definition
  stays assigned. The writer writes it again at clean shutdown, after the last mark sync. Until log segments can leave
  the hot tier (roadmap 8.1) or be compacted (6.x), the kept log holds every registry entry, so the file is checked,
  never trusted over the log.

## A.17 Deep tier: object keys, uploaded forms, claim marker, envelope
Every object kvik puts in the bucket (`spec.md` §8 Deep) is an **envelope** (below): backups are always encrypted
and no object holds plaintext. The sink's keys belong to kvik alone; a sink that shares a bucket maps them under
its own prefix, which the engine never sees.

- **Keys.** `<sid>` is the store id (A.1) as 32 lower-case hex digits; every key starts with it. Keys are ASCII,
  at most 128 bytes, hex lower-case and zero-padded as in A.12.
```
<sid>/STORE                                          the store descriptor (A.12)
<sid>/log/<generation:8 hex>-<file_id:16 hex>.kvl    a sealed log segment, under its local name
<sid>/manifest/<generation:8 hex>/<epoch:16 hex>     one manifest record
<sid>/claims/<generation:8 hex>                      a generation claim marker
<sid>/probe                                          the conditional-write probe
<sid>/files/<kind:2 hex>/<generation:8 hex>-<file_id:16 hex>   a file of any other kind (A.1)
<sid>/probe/<nonce:32 hex>                           a checksum probe; stored only by a bucket that is refused (Q-213)
```
  A log segment (kind 1) is only under `log/`; `files/01/` names nothing. A listing of
  `<sid>/manifest/<generation>/` returns a generation's records in epoch order. Generation 0 has no claim marker; it
  counts as claimed (`spec.md` §8 Generations). A reader ignores a key under `<sid>/` that this table does not name,
  and a key that does not start with a store id (32 lower-case hex digits and `/`). A restore finds the store id by
  listing from the first key: none is `NoStore`, more than one `SeveralStores`; `attach_backup` refuses a bucket
  holding a key under another store id (`OtherStore`).
- **Uploaded forms (the plaintext).** `STORE` and a sealed log segment: the local file, byte for byte, so its
  plaintext SHA-256 is the one the manifest records. A manifest record: the record's A.2 block (header, body and
  padding) in its canonical encoding, which for a record this build wrote is the bytes `MANIFEST` holds, and
  nothing else; checks: one valid record block, its `generation` and `epoch` those of its key. A record carried by a rebuild is uploaded under its new epoch with field 100 (A.14).
  The store id is bound by the envelope (key derivation and associated data), not repeated in the plaintext.
- **Claim marker** (12 bytes, exactly): 0 generation `u32`, that of its key; 4 reset nonce `u64`, the claiming
  restore's fresh nonce (`spec.md` §8 Generations), never zero. The store id is in the key and bound by the
  envelope, and the envelope header carries the format version, so the marker repeats neither. The nonce makes
  each attempt's marker unique (Q-161). A marker is written once; a restore that restarts before its restore record claims the next generation and never puts a
  marker again.
- **Probe** (Q-162). The plaintext of a probe is 16 random bytes, never read. `attach_backup`, and a restore before
  it claims, refuse a bucket (`BackupRefused`) when: a put of a fresh probe to `<sid>/probe`, where that key was
  absent, is followed by a second put of another fresh probe that succeeds, or after which `stat` reports the second
  object (`NoConditionalWrites`; where the key was present, the first put must fail and `stat` must report the
  earlier object unchanged); or a put of a fresh probe to `<sid>/probe/<nonce>` with a SHA-256 that is not its own
  (its own with every bit of byte 0 inverted) succeeds, or leaves an object under the key (`NoChecksums`); or a
  `stat` reports no SHA-256 (`NoChecksums`). `attach_backup` also refuses a bucket whose `<sid>/STORE` is present
  with another key check value (`WrongKey`); a restore reports a wrong key as `WrongKey` (`api.md` Backup and
  restore). A bucket that passes keeps one probe object for ever; the engine never
  deletes. Versioning or object lock cannot be probed through the sink and is the operator's to confirm. A drill
  writes nothing and does not probe. How a restore reports a refusal: Q-214.

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

## B. Outline of the remaining files (not yet normative)

Every file below has an A.1 header and is self-describing (A.14). Parquet is the export format only, so A.1 applies
to every stored file.

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

Each is specified in this document before the roadmap section that implements it.
