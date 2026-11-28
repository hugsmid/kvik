# kvik file format, v1: summary nodes, tree parts and the tree root

The bytes of one stored summary node (`spec.md` §5.2, §5.6; `format.md` Part B, tree; FC-34), written by roadmap
7.1a, and the files that hold them (T.8 to T.12), written by roadmap 7.1b: tree parts (`kind = 6`) and the tree root
(`kind = 19`). Implemented by the `kvik-tree` crate. The common rules of `format.md` Part A apply;
where this document and `spec.md` differ, `spec.md` wins.

## T.1 Scope
A stored node is the fold of its readings in `t` order (a 1-minute node) or the left-to-right merge of its children
(`spec.md` §5.6), and its bytes are a function of those readings and the series parameters alone, so that a stored
node, a rebuilt one and a chunk summary over the same readings (roadmap 6.2) compare byte for byte (invariants 5
and 8). The node version and `summary_stale` are not functions of the readings, so they belong to the record that
holds a node, not to its bytes. A window evaluation (`spec.md` §6: edge stretches, a held value with no reading
inside, an apportioned amount) has no stored form; an encoder refuses every one the checks of T.6 can tell from a
fold (a stretched window whose stretches keep within its first and last readings cannot be told apart).

Decimal series (`spec.md` §5.6: `i128` sums at the node's `u8` scale) take form ids of their own, laid out with
decimal series in the durable backend; they are additions under the critical rule, not a new major version.

Notation: `i64` and `u16` are little-endian; `f64` is the 8-byte bit pattern, little-endian, finite. `uv` is an
unsigned LEB128: seven bits per byte, the lowest group first, the high bit set on every byte but the last; it is in its
shortest form (a last byte of zero only when it is the only byte) and holds at most 64 bits (`uv64`) or 127 bits
(`uv127`).

## T.2 Node
| Field | Encoding | Notes |
|---|---|---|
| `form` | `u8` | 1 gauge, 2 counter, 3 state, 4 text, 5 interval; every other id is reserved and critical (a version refusal) |
| `start` | `i64` | span start |
| `width` | `uv64` | `end − start`; `end` fits `i64` |
| `count` | `uv64` | readings in the span |
| `coverage` | `uv127` | nanoseconds with a valid value |
| `first_dt` | `uv64` | `count ≥ 1`: first `t` − `start` |
| `first` | value (T.4) | `count ≥ 1` |
| `last_dt` | `uv64` | `count ≥ 2`: last `t` − first `t`, at least 1 |
| `last` | value (T.4) | `count ≥ 2`; with one reading, last is first |
| body | T.5 | per form |

The node ends where its body ends; its length comes from the record that holds it.

## T.3 Exact numbers
Sums, integrals and increases are exact (Q-180). Every one a fold or merge gives is dyadic: a finite `f64` is
m × 2^e with e ≥ −1074, and the fields are sums and differences of `f64` values and their products with whole
nanoseconds. So each is stored as ± mantissa × 2^`exp` with an odd mantissa:

| Field | Encoding | Notes |
|---|---|---|
| `sign` | `u8` | 0 zero (nothing follows), 1 positive, 2 negative |
| `exp` | `i16` | at least −1074 |
| `len` | `uv64` | mantissa bytes, 1 to 272 |
| `mantissa` | `len` bytes, little-endian | odd; last byte not zero |

The magnitude is below 2^1100: a node holds fewer than 2^64 readings and covers less than 2^64 ns, so a sum is below
2^1088, an integral below 2^1088 and an increase below 2^1090. Mantissa and exponent therefore take at most 272
bytes; values of similar magnitude, the usual case, take a few. Measured (`cargo run --release -p kvik-tree --example
fold_cost`): a 1-minute node of a gauge read every 5 s at 0.01 resolution takes 86 bytes in all, its 15-minute node
88 bytes, and the 1-minute node with one subnormal reading among the 12 takes 346 bytes; `gauge.node`, holding both
5e−324 and 1e300, takes 592.

## T.4 Values and state keys
By form: gauge, counter and interval values are `f64`. A state value and a state key are a tag, `u8` 0 false, 1 true,
2 a named state followed by its `u16` index among the series' declared names (`format.md` A.5). A text value is a
`u8` length and that many bytes of UTF-8.

## T.5 Bodies
| Form | Body |
|---|---|
| gauge | `sum` (T.3), `integral` in value × ns (T.3); with `count ≥ 1`, `min` and `max` (`f64`) |
| counter | `increase` (T.3), `resets`, `regressions`, `long_steps` (each `uv64`); the steps are `count − 1` and the elapsed time last `t` − first `t`, so neither is stored |
| state | `transitions` (`uv64`), `entries` (`uv64`), then per state in ascending key order its key (T.4) and its time in ns (`uv127`, at least 1) |
| text | nothing |
| interval | `sum` (T.3); with `count ≥ 1`, `min` and `max` (`f64`) |

Key order: false, true, then named states by index. Min and max are in IEEE total order, so −0.0 is below +0.0
whatever the order of folding (Q-181).

**Canonical writing.** Every field has one encoding, so a node has one byte sequence; decoding any accepted input and
encoding it again gives the same bytes. Golden files rely on it.

## T.6 Checks
A reader refuses, naming the offset: bytes ending inside a field, or bytes after the node; a `uv` not in its shortest
form or out of range; a reserved form (`UnsupportedForm`, a version refusal); an `end` beyond `i64`; coverage with no
readings; coverage beyond last `t` − first `t` on any form but interval (holds lie between consecutive readings); a
first `t` outside the span; a last `t` not after the first or not before `end`; more readings than
nanoseconds from the first `t` to the last, plus one (readings have distinct times); a value or bound that is not finite;
text that is not UTF-8; an unknown state tag; an exact number with an unknown sign, an even mantissa, a mantissa with
a zero last byte, a length outside 1 to 272 or a magnitude outside T.3; a gauge or interval sum with no readings; a
gauge integral with no coverage; first or last outside min and max, or min and max that differ with one reading; a
counter increase with no step, or more resets and regressions, or long steps, than steps; more state transitions than
steps; states not in ascending order, a state time of zero, or state times that do not add up to the coverage. An
encoder refuses every node a reader would refuse.

## T.7 What an implementation must test
Merge associative and the empty node its identity, bitwise; folding any split of readings equals folding them all;
fold, merge and the window credit equal the reference model's; every folded or merged node round-trips; golden files
of every form and an unknown form; a fuzz target whose accepted inputs encode to themselves.

## T.8 Tree part (`kind = 6`)
An immutable file of the nodes of many series over one **period** (a UTC day for a checkpoint's part, longer for
merged ones), under `<dir>/tree/<generation:8 hex>-<file_id:16 hex>.kvt`. Tree files are derived (`format.md` A.18):
never named by a manifest record, never backed up, rebuilt when absent or refused. `file_id` is the tree's own
counter, held in the root (T.12); `generation` is the store generation of the positions the part names.

Layout: an A.1 header (kind 6; flags, `base_sequence` and reserved bytes zero), then A.2 blocks in this order, none
skippable: zero or more **nodes** blocks (`0x60`, T.9), zero or more **directory** blocks (`0x61`, T.10), one
**directory root** (`0x62`, T.10), one **trailer** (`0x63`, T.11); then the 16-byte tail of A.9 (`trailer_offset:
u64`, `KVIKEND\0`). The level table is the part's own (T.11): a level is named by its id, and an entry's width is
the one the part's table gives, so the set of stored levels is not part of the format.

**Canonical writing.** Every part has one byte sequence: the order above; runs (the entries of one series at one
level) in ascending `(series, level)`; a run's entries in ascending start; each run split greedily into nodes blocks,
a block taking the next entry while its body stays within 65,536 bytes, and always at least one; directory blocks of
at most 1,365 records, each full but the last; zeros in every reserved byte, flag and padding. A tree file is
derived, so a reader accepts only canonical bytes (T.11): whatever it accepts encodes to the same bytes.

## T.9 Nodes block (`0x60`)
One run's entries, or a stretch of them. Head, then entries to the end of the body (at least one):

| Field | Encoding | Notes |
|---|---|---|
| `series` | `uv64` | series id |
| `level` | `u8` | an id in the part's level table |
| `base_start` | `i64` | the start of the block's first entry |
| `base_pos` | position (12 bytes) | the least `as_of` of the whole run, the same in each of its blocks |

Entry:

| Field | Encoding | Notes |
|---|---|---|
| `gap` | `uv64` | whole minutes from the previous entry's end; 0 in a block's first entry |
| `flags` | `u8` | bit 0 `stale` (`summary_stale`); every other bit is critical (a version refusal) |
| `as_of_generation` | `uv64` | the node version's generation minus `base_pos`'s (Q-1422, Q-1460) |
| `as_of_sequence` | `uv64` | with `as_of_generation` 0, its sequence minus `base_pos`'s; otherwise the sequence itself |
| `len` | `uv64` | node bytes |
| node | `len` bytes | T.2 verbatim (Q-1426); its `start` is the entry's and its width the level's |

An entry starts on a multiple of its level's width (UTC-aligned) and ends a width later. `len` 0 without `stale` is an
**empty marker**: the node is now empty, hiding an older part's entry. `len` 0 with `stale` is a stale entry with no
node. A node with no readings is never stored in full (it is an empty marker). All nodes of one series, at every
level, have one form.

## T.10 Directory (`0x61`) and directory root (`0x62`)
Each body is `count: u32`, `record_size: u16`, two reserved bytes, then `count` records.

Directory record (48 bytes), one per nodes block in file order: 0 `series: u64`; 8 `level: u8`; 9 reserved (3);
12 `entry_count: u32`; 16 first entry's start `i64`; 24 last entry's end `i64`; 32 the block's file offset `u64`;
40 its length, header and padding included, `u32`; 44 reserved (4).

Directory root record (32 bytes), one per directory block: 0 its first record's `series: u64`; 8 `level: u8`;
9 reserved (3); 12 its record count `u32`; 16 its first record's first start `i64`; 24 the block's file offset `u64`.

## T.11 Part trailer (`0x63`) and the checks of a part
Body (104 bytes, then the level table):

| Offset | Size | Field |
|---:|---:|---|
| 0 | 8 | `seq`: for a delta or rebuilt part that of the first root to list it, for a merged part the highest of its inputs (Q-1540); among a root's parts the higher `seq` overlays |
| 8 | 1 | `class`: 0 delta (a checkpoint), 1 merged, 2 rebuilt; any other value is critical |
| 9 | 1 | reserved |
| 10 | 2 | level count |
| 12 | 2 | level `record_size`, 16 |
| 14 | 2 | reserved |
| 16 | 8 | `period_start: i64` |
| 24 | 8 | `period_len: u64` in ns: positive, the end within `i64` |
| 32 | 8 | least entry start (0 with no entries) |
| 40 | 8 | greatest entry end (0 with no entries) |
| 48 | 8 | least series id (0 with no entries) |
| 56 | 8 | greatest series id (0 with no entries) |
| 64 | 12 | `through`: the checkpoint K the part was written at, at or below the synced mark (invariant 26) |
| 76 | 4 | reserved |
| 80 | 8 | entry count |
| 88 | 8 | file offset of the directory root |
| 96 | 4 | `file_crc32c` over every byte before the trailer's block header |
| 100 | 4 | reserved |

Level record (16 bytes): 0 `id: u8`, not zero, ascending; 1 reserved (7); 8 `width: i64`, a positive whole number of
minutes. Ids 1 to 4 are the levels of the 7.1b design (1 minute, 15 minutes, 2 hours, 1 day; Q-1420, Q-1421); 16 and
17 are reserved for calendar levels (Q-1425), which would take a level record of their own.

**Checks.** A reader refuses a part, naming the offset and the check (`Refusal` in `kvik-tree`): the A.1 header
invalid, of another kind, of another major version or with non-zero flags, `base_sequence` or reserved bytes; no end
marker, or a trailer offset that does not lead to a trailer block ending at it; a block header or body checksum that
fails, a body above 16 MiB or past the trailer, non-zero block flags, reserved bytes or padding; a block of an unknown
type (a version refusal) or out of the order of T.8; a field past the end of its body, a `uv` not in its shortest form
or above 64 bits; a record size other than the one given (16 for levels, 48 and 32 for directory and directory
root records; above it a version refusal), arrays that do not fill the body; an empty
level table, a zero or non-ascending id, a width not a positive whole number of minutes; an unknown class (a version
refusal); an empty period or one ending beyond `i64`; `file_crc32c` wrong; a nodes block with no entries, a level not
in the table, a first `gap` not 0, a start or end beyond `i64` or not on its level's width, an unknown entry flag (a
version refusal), an `as_of` beyond `u32` generations or `u64` sequences, a `base_pos` not the least `as_of` of its
run; node bytes that fail T.6 (an unknown form is a version refusal), whose span is not the entry's, or with no
readings; nodes of one series of different forms, at any levels; runs or entries out of order or overlapping; an entry outside the
period; blocks not split as T.8 says; a directory, directory root or trailer summary that does not match the nodes.
Every check has a named vector in `tree/tests/refusals.rs`.

The checks come in two tiers. A **whole-file** decode (the scrub, a merge's inputs, tests) makes every check above. A
**block read** (a reader that takes the trailer, the directory and single nodes blocks of a large part) makes the
checks of the header, the tail and trailer, the directory records it reads and each block it reads, framing and
entries; it cannot check the split, the directory against the nodes, a run's `base_pos` against blocks it has not
read, or `file_crc32c`, and relies on the writer writing only canonical files (T.8).

## T.12 Tree root (`kind = 19`)
`<dir>/tree/root-<seq:16 hex>.kvr`: an A.1 header (kind 19, `file_id` equal to `seq`, flags, `base_sequence` and
reserved bytes zero) and exactly one block (`0x64`, not skippable) that ends the file, as A.18's derived files. Body
(88 bytes, then five record arrays in this order):

| Offset | Size | Field |
|---:|---:|---|
| 0 | 8 | `seq`: rises by one per root |
| 8 | 12 | K, the checkpoint: the visible nodes are the canonical nodes of the current view at K |
| 20 | 4 | reserved |
| 24 | 16 | the mark's reset stamp when written (A.10 bytes 84..100) |
| 40 | 8 | `next_file_id`: above every part `file_id` the root names, live or retired |
| 48 | 8 | global floor: entries of a part whose `seq` is below it are invisible |
| 56 | 4 × 5 | counts: levels, parts, covered ranges, floors, retired |
| 76 | 2 × 5 | `record_size` of each, in the same order: 16, 80, 24, 24, 16 |
| 86 | 2 | reserved |

- **Level** (16): as T.11.
- **Part** (80), ascending `file_id`; no two parts of one `seq` overlap in both period and series range
  (Q-1540): 0 `file_id: u64`; 8 `seq: u64`; 16 `class: u8`;
  17 reserved (3); 20 `file_crc32c: u32`; 24 period start `i64`; 32 period length `u64`; 40 least start; 48 greatest
  end; 56 least series; 64 greatest series; 72 the file's length `u64`. The fields repeat the part's trailer.
- **Covered** (24), ascending level then start: 0 `level: u8`; 1 reserved (7); 8 `from: i64`; 16 `to: i64`. A range
  is non-empty, on its level's width, and neither overlaps nor touches the next of its level. Inside a range the
  visible parts are authoritative for the level (an absent entry is an empty node); outside it the reader takes facts.
- **Floor** (24), ascending series: 0 `series: u64`; 8 `seq: u64`, below which the series' entries are invisible;
  16 `flags: u8`, bit 0 `rebuilding` (the series is not served from the tree), every other bit critical; 17 reserved.
- **Retired** (16), ascending `file_id`, none also live: 0 `file_id: u64`; 8 `retired_at: i64` on the commit clock.

**Checks.** As T.11 for the header, the block, the integers, record sizes, arrays, the level table, classes and
periods; and a root refuses a header `file_id` other than `seq`, anything after its block, parts out of order, two
parts of one `seq` whose periods (half-open) and series ranges (inclusive) both overlap, a part or retired `file_id` at or above `next_file_id`, a covered range that breaks the rule above or
names a level not in the table, floors out of order or with an unknown flag (a version refusal), retired files out of
order or also live.

## T.13 What an implementation of T.8 to T.12 must test
Every encoded part and root decodes to what was encoded, and every accepted input encodes to the same bytes (generated
files and a fuzz target over raw and re-checksummed bytes); every refusal of T.11 and T.12 has a named vector; golden
files of a part, an empty part, a newer major version and a root.
