# Format check (roadmap 3.1)

[`format.md`](format.md) checked for implementability against [`spec.md`](spec.md) v0.2. The question asked of
each part was: can the code of roadmap section 4, and its golden files, be written from the documents alone,
without guessing anything that ends up on disk? This document lists problems; it proposes no layouts and does not
change `format.md`. Where a problem is already recorded in [`model/OPEN-QUESTIONS.md`](../model/OPEN-QUESTIONS.md)
(the F-items under "For the format and interface documents", or a Q-item), it is cross-referenced, not restated.

## Outcome at gate 3.3
Gate 3.3 resolved every blocking item in [`format.md`](format.md), [`spec.md`](spec.md) and [`api.md`](api.md); each
item below ends with a line saying where it was resolved, or the roadmap item it was deferred to. Q-97, Q-98 and Q-99
are settled. Of the cross-referenced items: F-1, F-4, F-5, F-7, F-9, F-11, F-13, F-14, F-15 and F-17 are resolved in
`format.md` A.2 and A.4–A.10; F-16 for the site-zone and tzdata entries (A.6), the calendar file deferred to 7.3;
F-12's field ids are fixed (A.6), the interface deferred to 5.1; Q-52 (a) and (c) are settled (A.10, A.14), (b)
deferred to 4.5; Q-76 and Q-48 were settled before the gate; Q-33 stays with 4.5; Q-61, Q-64, Q-70 and Q-71 stay
with their items.

## Severity
- **Blocks:** the document contradicts the specification or lacks a layout or rule that a section 4 item writes to
  disk. To be resolved at gate 3.3, before section 4 starts.
- **Before 4.x:** a choice an implementer would otherwise guess, needed by the named section 4 item. It can be
  decided in that item, recorded in `format.md` in the same pull request, without changing anything accepted at
  gate 3.3.
- **Can wait:** first needed after section 4; to be decided before the named item.

## Summary
| Severity | New (FC) | Existing (cross-referenced) |
|---|---:|---|
| Blocks | 12 | F-1, F-4, F-5, F-9, F-13, F-14, F-16, F-17, Q-52, Q-76 |
| Before 4.x | 18 | F-7, F-11, F-15, Q-33, Q-48 |
| Can wait | 5 | F-12, Q-61, Q-64, Q-70, Q-71 |

Q-52 and Q-76 are listed under "Open at v0.2" as settled by item 3.1. Both need a decision (a change to the manifest
framing and to the specification), which is outside a check that lists problems; they are carried here as blockers
for gate 3.3. Three new questions about the specification itself were found and added to `OPEN-QUESTIONS.md`: Q-97
(FC-21), Q-98 (FC-24) and Q-99 (FC-28). The counts above are of FC items; the cross-referenced items are not counted
in them.

Checked with no finding: byte order (little-endian throughout); the checksum algorithm (CRC32C) and the coverage of
every header, body and batch checksum; the arithmetic of A.1, A.7, A.9 and A.10, each of whose fields sum to its
stated size; a zero block header cannot be valid (type 0 is never valid); expected interval and S have separate field
ids (7 and 8).

## Existing items and when they are needed
| Item | Needed by | Severity | Note |
|---|---|---|---|
| F-1, F-4 | 4.3 | Blocks | The quality column must hold the supplied quality; A.5 cannot be implemented as written |
| F-5 | 4.2, 4.5 | Blocks | Scanning past a corrupt region; A.2 and A.4 rules that end reading at the first failure |
| F-9 | 4.3 | Blocks | `changed_at` column and definition fields; no column or field ids yet |
| F-17 | 4.3 | Blocks | Interval field types are given, field ids are not (FC-14) |
| Q-52 | 4.5 | Blocks | Torn manifest tail versus corrupt record (FC-30); assigned to 3.1 |
| Q-76 | 4.3 | Blocks | Whether a tzdata change is a log entry decides A.6's entry types (FC-15); assigned to 3.1 |
| F-7 | 4.2 | Before 4.2 | First sequence at least 1, last sequence 0 means empty |
| F-11 | 4.4 | Before 4.4 | A.10 says the mark is not synced; §3 syncs it on a timer |
| F-15 | 4.5 | Before 4.5 | A segment copy that omits a lost range |
| Q-33, Q-48 | 4.5 | Before 4.5 | Lost-range and truncated-tail record fields; settled by 4.5 |
| F-13, F-14 | 4.3 | Blocks | Definition fields (timing, interval length and phase) with no ids (FC-14) |
| F-16 | 4.3, 7.3 | Blocks (site-zone entry, FC-15); can wait (calendar file, 7.3) | |
| F-12 | 5.1 | Can wait | Retention class in a parameter change (FC-14) |
| Q-61 | 5.2 | Can wait | Where a condition delivery's sequence is kept (cursor file, FC-31) |
| Q-64, Q-70 | 7.1 | Can wait | What a tree checkpoint holds (FC-32) |
| Q-71 | 6.3 | Can wait | Manifest epochs holding a segment and its chunks |

## Dependencies on roadmap section 2
Part A depends on 2.2 (commit latency) for: the sync primitive per platform (FC-26), preallocation and its cost at
sync (FC-27), the default segment size and linger, and the cost of syncing the mark after every update where no boot
identifier exists (FC-23). Part B depends on 2.1 (chunk encodings) for the whole compacted segment, and for whether
kind 2 exists at all (FC-35). None of these block writing the check; each item names its dependency.

## A. Common rules, header and versioning

### FC-1 — An unknown block type is read as corruption
**Where:** A.2, A.3 · **Severity:** blocks (4.2, 4.5) · **Relates to:** §3 Recovery rules 2 and 3, row 32
**Problem.** A.2 ends the readable part of a file at the first block whose type is neither known nor skippable,
exactly as at a checksum failure. A.3 allows new block types within a major version. A reader cannot tell "written by
a newer minor version" from "damaged".
**Scenario.** A store written by format 1.1 adds a non-skippable block type. The machine reboots and an older binary
(1.0) opens the store: recovery sees a batch it cannot complete, treats it as a torn tail (rule 3) and truncates
acknowledged data, or, with a complete batch after it, records a lost range (rule 2). A reader process stops reading
at that block.
**Needed:** a rule that a block whose checksums hold and whose type is unknown and not skippable is a version
refusal, never a torn tail or a lost range; and what `format_minor` means for a writer (whether a writer may append
to a segment or store whose minor version is newer than its own).
**Resolved at gate 3.3:** `format.md` common rules (critical and ignorable elements; an unknown critical element is a version refusal) and A.2.

### FC-2 — Unknown columns, fields and entry types are skipped even when they change meaning
**Where:** A.3, A.5, A.6 · **Severity:** blocks (4.3) · **Relates to:** F-9, F-13, invariants 8 and 15
**Problem.** "A reader skips columns it does not know" and "unknown fields are skipped". Some planned additions
change identity or semantics, not only add detail. The handling of an unknown `entry_type` is not stated at all.
**Scenario.** `changed_at` (F-9) is added as a new column in a minor version. An older reader skips it: ranks fall
back to position, a replay with a lower `changed_at` becomes current, and duplicate identity is lost. Likewise a
skipped `versioned` or `timing` field in a definition, or a skipped retirement entry.
**Needed:** a way for a column, a field and an entry type to say whether it may be ignored (as a block's
`skippable` does), or a rule that additions which change meaning need a new major version.
**Resolved at gate 3.3:** `format.md` common rules: id ranges for critical and ignorable columns, entry types and fields; fact flag bits; block types by `skippable`.

### FC-3 — No store descriptor, creation procedure or directory layout
**Where:** A.1, A.4 · **Severity:** blocks (4.1, 4.2) · **Relates to:** §5.7, §8 Generations, Q-75 (model), Q-76
**Problem.** `store_id` lives only in file headers. Nothing says where a store records its identity, its site zone
at creation (§5.7; the model refuses to open a store with another zone) or its tzdata version at creation (the model
of Q-76 reads it from "the store header", which `format.md` does not define). Only `log/` and `log/MARK` are named.
**Scenario.** `Store::open` on an empty directory must create a store; on a directory whose segments have all left
hot (90 days, §8 fill pressure) it must still find the store id; on a directory holding only `log/MARK` it must tell
a damaged store from a new one. The locations of the manifest, cursor files, leases, the lock, tree, registry,
calendar files and the local restore record are not given, so a reader process cannot find them (§7: "the documented
layout").
**Needed:** a store descriptor or an equivalent rule, the creation procedure, and the directory layout.
**Resolved at gate 3.3:** `format.md` A.12 (layout, `STORE` descriptor of kind 7, creation and open procedure); `spec.md` §5.7 (the descriptor counts as the first site-zone and tzdata entries).

### FC-4 — File identity and names
**Where:** A.1, A.4 · **Severity:** before 4.2 · **Relates to:** §7 Manifest, §8 Restore
**Problem.** `file_id` is "unique per kind" without a scope or a persisted allocator. The file name gives
`<generation>` without a base, width or case, and the 16 hex digits without a case.
**Scenario.** After a restore, files listed in the restore record that stay in deep are not local; a new compacted
file must not take an id one of them holds. On a case-insensitive file system two spellings of one name are one file;
a reader that matches names exactly misses segments written by another implementation.
**Needed:** the scope of `file_id` (store, generation), how the next id is found after a restart and after a restore,
and the exact name grammar.
**Resolved at gate 3.3:** `format.md` A.12 (`file_id` per store and kind, never reused; next id after a restart and a restore; names).

### FC-5 — Alignment and what lengths include
**Where:** A (common rules), A.5, A.6 · **Severity:** before 4.2 · **Relates to:** A.11 golden files and fuzzing
**Problem.** "Every structure is aligned to 8 bytes", but column directory entries are 12 bytes and entry fields are
"padded to 4". It is not said whether an entry's `length` and a field's `field_len` include their padding, whether
non-zero padding or reserved bytes are an error, or whether an encoding is canonical.
**Scenario.** Two writers emit the same entry, one with `field_len` covering padding and one without; each reader
mis-parses the other's fields. A fuzzer finds a block with non-zero padding: accept or reject?
**Needed:** alignment of each structure as written, the coverage of each length field, and the treatment of non-zero
padding and reserved bytes.
**Resolved at gate 3.3:** `format.md` common rules (alignment, what each length covers, reserved fields and padding).

### FC-6 — Maximum sizes do not fit together
**Where:** A.2, A.4, A.5, A.6 · **Severity:** before 4.3, 4.4 · **Relates to:** §2.1 (text ≤ 255 bytes, state u16),
§3 Segments and Back-pressure
**Problem.** Checked by arithmetic:
- A facts block of 65,536 text rows at 255 bytes needs 16,711,680 bytes of heap plus 1,572,864 bytes of fixed
  columns (4 + 8 + 8 + 1 + 2 + 1 bytes per row), 18,284,544 bytes or about 17.4 MiB, above the 16 MiB `body_len`
  limit. No rule says how a writer splits rows across blocks.
- A state series may declare up to 65,536 names (u16 index); field 13 must hold them all in one field of at most
  65,535 bytes (`field_len` is u16), which is too small even at one byte per name.
- Name and unit have no maximum length.
- A batch never spans files and segment size is configurable. A full ingest queue (65,536 readings, §3) of text
  readings makes a batch of about 17.4 MiB; with a segment size configured below that, no segment can hold it. No
  rule says what happens when the next batch does not fit in the space left in the open segment.
**Scenario.** An adapter for a log-message series submits a full queue of 255-byte texts. The writer cannot put them
in one facts block; whatever split it chooses becomes part of the golden files, and with a segment size of 16 MiB the
batch fits no segment at all.
**Needed:** writer limits per block and per entry, a minimum segment size or maximum batch size, and the rule for a
batch that does not fit.
**Resolved at gate 3.3:** `format.md` common rules (sizes) and A.4 (segment size range, block and batch closing, call limits); `api.md` Writing (`TooLarge`).

## A.4 Log segment

### FC-7 — Sealing and restart are not crash-safe as written
**Where:** A.4, A.9, A.10 · **Severity:** blocks (4.4, 4.5) · **Relates to:** §3 Sealing, Recovery rules 1 and 3
**Problem.** Sealing appends an index and a trailer, truncates, syncs, renames and syncs the directory, but recovery
is defined only in terms of batches and the mark. The segment index and trailer are blocks after the last commit
block, so they look like the start of an incomplete batch.
**Scenario.**
- A crash after the trailer is written and before the rename leaves a `.open` file with a valid trailer. Recovery on
  a new boot scans it as batches, finds blocks that end in no commit block, truncates them as a torn tail and reports
  `possible_loss` although nothing was lost. On the same boot, rule 1 truncates everything after the mark, the
  trailer included.
- A crash after the rename and before the directory sync may leave either name after a power loss.
- It is not said whether the mark moves at sealing, whether the writer appends to an existing `.open` segment after
  a restart or opens a new one, whether an empty `.open` segment (§3: "an empty segment is not sealed") is deleted,
  and whether more than one `.open` segment can exist.
**Needed:** the seal procedure as a crash-safe sequence, recovery's handling of each intermediate state, and the
segment life cycle across restarts.
**Resolved at gate 3.3:** `format.md` A.4 (seal procedure, the states recovery may find and the action for each).

### FC-8 — What "truncate" means for a preallocated segment
**Where:** A.4, A.10 · **Severity:** blocks (4.5) · **Relates to:** §3 Recovery rules 1 and 3
**Problem.** Recovery truncates to the mark (rule 1) or truncates a torn tail (rule 3). For a preallocated file this
can mean setting the file length, which discards the preallocation, or zeroing the bytes in place. Neither is stated.
**Scenario.** Recovery on the same boot truncates after the mark by overwriting only from the mark onwards. The next
batch reuses the truncated batch's positions (rule 1 allows this) and is shorter than it. The stale bytes after the
new batch can still parse as a complete batch whose sequences follow the new one, and a reader or a later recovery
takes back a batch that was never acknowledged.
**Needed:** whether truncation sets the length or zero-fills, whether the zeroing is synced before new batches are
written, and whether the segment is preallocated again afterwards.
**Resolved at gate 3.3:** `format.md` A.4 (truncation is zero-filling, synced before any new batch; accepted limit stated).

### FC-9 — Redundant fields that may disagree
**Where:** A.4, A.5, A.6, A.7 · **Severity:** before 4.3 · **Relates to:** §2.2 (`received_at` per batch)
**Problem.** Each data block and the commit block carry `first_sequence`, and A.4 also derives sequences by counting.
Each data block carries `received_at`, which §2.2 defines once per batch. The commit block repeats the header's
`generation`.
**Scenario.** A block's `first_sequence` disagrees with the count from the previous block, with both checksums
valid. It is not said whether the batch is incomplete (torn tail or lost range), refused as a format error, or read
by one of the two.
**Needed:** which value wins, which disagreements make a batch incomplete, and whether all blocks of a batch must
carry the same `received_at`.
**Resolved at gate 3.3:** `format.md` A.5 and A.6 (one `received_at` per batch) and A.7 (which disagreements make a batch incomplete).

## A.5 Facts block

### FC-10 — Column presence and validation
**Where:** A.5 · **Severity:** before 4.3 · **Relates to:** F-1, F-4, A.11 fuzz target
**Problem.** Not stated: which columns are mandatory; the value of an absent optional column (`quality`, `flags`,
`value_type`); duplicate column ids; overlapping payloads; a column length that does not match `row_count` and its
width; `value_type` that contradicts the series definition; non-zero upper bytes in a state or bool `value`; what the
text offset is relative to; text that is not UTF-8 or longer than 255 bytes; the `value_type` of a `no_value` row.
**Scenario.** A block without a `quality` column passes every checksum. One reader takes all rows as good, another
rejects the block, a third reads it as corrupt and records a lost range.
**Needed:** the mandatory columns and defaults, and for each validation failure whether it is corruption (§3) or a
format error.
**Resolved at gate 3.3:** `format.md` A.5 (presence per column, defaults, checks; a failure is an invalid region with cause invalid content).

### FC-11 — Run-length encoding has no canonical form
**Where:** A.5 · **Severity:** before 4.3 · **Relates to:** A.11 golden files and fuzz target
**Problem.** Zero-length runs, non-minimal LEB128 encodings, the maximum LEB128 length and run lengths that do not
sum to `row_count` are not addressed.
**Scenario.** A writer emits a zero-length run between two runs of 2 for `row_count` 4. One reader accepts it, the
A.11 fuzz target reports it, and the golden file fixes whatever the first implementation happened to do.
**Needed:** which of these are invalid, and whether writers must produce one canonical form.
**Resolved at gate 3.3:** `format.md` A.5 (canonical run-length form; writers use it exactly when smaller than plain).

### FC-12 — A segment cannot be decoded without the registry
**Where:** A.5 · **Severity:** before 4.6 · **Relates to:** §7 Readers, F-9, F-14
**Problem.** A.5 says `value_type` is stored "so that a segment can be decoded without the registry", but the decimal
scale, state names, kind, versioning and interval length live only in definition entries, which may be in an earlier
segment or only in the registry file.
**Scenario.** A reader process opens a sealed segment whose series were defined in a segment that has left hot; it
can read `i64` values but not their scale.
**Needed:** what "decoded" promises, or what a segment must carry to honour it.
**Resolved at gate 3.3:** `format.md` A.5 (what decoding without the registry gives).

## A.6 Entries block

### FC-13 — Field value encodings are not given
**Where:** A.6 · **Severity:** blocks (4.3) · **Relates to:** §2.1, §11 (every default is a configuration key),
principle 2, invariant 8
**Problem.** Field ids are listed without encodings:
- kind values (gauge, counter, state, text, interval) are not enumerated, nor value type values (whether they are
  A.5's `value_type` codes, and how bool differs from a named state);
- `wrap_at` has no type (it would follow the value type: `f64` or scaled `i64`);
- `reset_ceiling` has no representation (a fraction, default 10 %);
- the range of the decimal scale is not given;
- field 13 (state names) has no list encoding (FC-6 for its size);
- it is not said how an absent field differs from "none" (state and text have no expected interval; S is "none if no
  interval"), or whether a definition must carry every field resolved.
**Scenario.** A definition omits field 8 (S), meaning "3 × expected interval". Later the operator changes the
configured default, or a parameter change alters field 7: is S recomputed? If an absent field means "the default in
force when read", a configuration change alters derived values of old series, against principle 2 and invariant 8.
**Needed:** the encoding of every field, and whether defaults are resolved into the entry when it is written.
**Resolved at gate 3.3:** `format.md` A.6 (encoding of every definition field; defaults resolved when written).

### FC-14 — Field ids are missing for parameters the specification defines
**Where:** A.6 · **Severity:** blocks (4.3) for fields of a definition, can wait (5.1) for retention · **Relates to:**
F-9, F-12, F-13, F-14, F-17
**Problem.** These parameters of §2.1 have no field id. The rules for each are in the F-items; what is missing here
is only the id and, where a parameter change may or may not carry it, the list.

| Parameter (§2.1) | Rule in | Allowed in a parameter change |
|---|---|---|
| interval length (tag), d, phase | F-14, F-17 | no |
| interval zone, day start | F-17 | no |
| `versioned`, `changed_at kind` | F-9 | no |
| `timing` | F-13 | no |
| retention class and period | F-12 | yes, at its position, with no effective time (field 20 is then absent) |

The definition's field list also says nothing of field 13 in a parameter change, and of whether fields 9–11 (record
on change, `wrap_at`, `reset_ceiling`) may change at all, which §2.1 does not forbid.
**Scenario.** A `day` interval series with a day start of 05:00 cannot be defined: the length tag, zone and day
start have no ids, so two implementations choose different ones and cannot read each other's segments.
**Needed:** the ids, and the complete list of fields a parameter change may carry.
**Resolved at gate 3.3:** `format.md` A.6 (field ids 14–24; the fields a parameter change may carry).

### FC-15 — Entry types for the site zone and tzdata version are missing
**Where:** A.6 · **Severity:** blocks (4.3) · **Relates to:** F-16, Q-76, §5.7, §2.1 interval starts
**Problem.** §5.7 makes the site zone a log entry with an effective time; there is no entry type for it (F-16). Q-76
(assigned to 3.1) decides whether a tzdata change is a log entry; with the model's reading, A.6 needs a tzdata entry,
and that entry also fixes which version places interval starts (§2.1) and displacement (§5.4).
**Scenario.** The site zone changes. With no entry type the change cannot be logged, so a rebuild from the log
(invariant 8) builds calendar nodes in the old zone. After a host tzdata upgrade, a restart places interval starts
by the new version while the live engine used the old one.
**Needed:** the decision on Q-76, and entry types and fields for both.
**Resolved at gate 3.3:** `format.md` A.6 (types 6 and 7 with encodings), A.12 (the descriptor's site zone and tzdata version); Q-76 was settled before the gate, and Q-93 at it.

### FC-16 — The routing change entry is defined nowhere
**Where:** A.6 entry type 5 · **Severity:** can wait (5.2) · **Relates to:** §4 Routing, Q-61, Q-63, Q-78, `api-check.md`
AC-20
**Problem.** Type 5 is "defined with the interface document", but `api.md` does not define it either. A routing
entry must carry a funnel's identity (Q-61 and Q-63 need one, including its incarnation), its consumer, its selection
(ids or name patterns), operator and condition with their parameters, and, under Q-78's proposal, what the consumer
emits.
**Scenario.** A route added at run time is committed in a layout another build does not know, so catch-up in that
build cannot apply it at its position.
**Needed:** the serialised form of a funnel and of its removal.
**Deferred at gate 3.3** to roadmap 5.2: entry type 5 stays reserved until funnel identity is settled (Q-61, Q-63, Q-78, Q-91).

## A.7 Commit block

### FC-17 — Coverage of `batch_bytes`, and the consecutive rule
**Where:** A.7 · **Severity:** before 4.2 · **Relates to:** F-5, F-7
**Problem.** "From the batch's first block header to this block's header" does not say whether the commit block's
own header is included, nor padding. "Consecutive with the previous batch" needs the lost-range exception of F-5,
and for the first batch of a file the comparison with `base_sequence`. "`commit_time` never earlier than the previous
batch's" is not said to hold across segments, restarts and generations, nor what a reader does with a batch that
breaks it.
**Scenario.** Two implementations compute `batch_bytes` with and without the commit block's 16-byte header; each
judges every batch of the other incomplete and truncates it.
**Needed:** the exact byte range and the validation rules across files.
**Resolved at gate 3.3:** `format.md` A.7 (`batch_bytes` range, `batch_crc32c` over store id, file id, offset and data block headers, completeness rules) and A.2 (resynchronisation).

## A.8, A.9 Segment index and trailer

### FC-18 — Offsets and record arrays
**Where:** A.8, A.9 · **Severity:** before 4.2 · **Relates to:** A.3
**Problem.** `index_offset` and `trailer_offset` do not say whether they point at the block header or the body. A.8's
per-series records (24 bytes) and A.5's column directory entries (12 bytes) have no record size, so A.3's "new fields
at the end of a body" cannot extend a record without breaking older readers.
**Scenario.** A reader seeks to `index_offset`, parses the body as a block header and fails the header checksum. A
writer that adds a field to each per-series record shifts every record after the first for an older reader.
**Needed:** what each offset points at, and how fixed-size records are extended within a major version.
**Resolved at gate 3.3:** `format.md` A.8 (`record_size`; the rule for every fixed-size record array) and A.9 (offsets point at block headers).

### FC-19 — The segment index lists no entries
**Where:** A.8 · **Severity:** before 4.4 · **Relates to:** §4 Routing (catch-up applies the routes in force at each
position), §2.1 registry entries
**Problem.** The index "lets a reader that is catching up, and compaction, skip segments" by series. A segment that
holds none of a consumer's series can still hold a routing change, a definition matched by a pattern, a retirement or
a site-zone entry.
**Scenario.** A consumer catching up skips a segment by its index and misses a route added there; facts in later
segments are delivered by the old routing table.
**Needed:** whether, and where, the index records that a segment holds entries.
**Resolved at gate 3.3:** `format.md` A.8 (`has_entries` flag; when a catching-up reader may skip a segment).

### FC-20 — When the file checksum is verified, and what a mismatch means
**Where:** A.9 · **Severity:** before 4.5 · **Relates to:** §8 Corrupt block, row 11
**Problem.** `file_crc32c` covers the whole sealed file, but it is not said when it is checked (verifying every
sealed segment at open reads all of them), or whether a mismatch with valid block checksums makes the file corrupt
(quarantine, §8) or is ignored.
**Scenario.** A sealed segment has one flipped bit in padding, which no block checksum covers but `file_crc32c`
does; one implementation quarantines the file, another keeps reading it.
**Needed:** when it is verified and how a mismatch is handled.
**Resolved at gate 3.3:** `format.md` A.9 (verified by the sealer, before upload or compaction and by the scrub; a mismatch with complete batches is copied, not reported).

## A.10 Durable mark

### FC-21 — A memory-mapped mark under `forbid(unsafe_code)`
**Where:** A.10 · **Severity:** blocks (4.4) · **Relates to:** §3 Durable mark, `Fs::mmap` in `api.md`, Q-97
**Problem.** The project forbids unsafe code in every crate (`Cargo.toml`, `CONTRIBUTING.md`). Mapping a file into
memory in Rust needs unsafe code, and a sequence lock read by another process through a shared mapping needs atomic
accesses to shared memory, which safe Rust cannot express. The specification itself requires a memory-mapped mark.
**Scenario.** Item 4.4 builds the mark; the crate cannot map the file without unsafe code, and the workspace lint
rejects the build.
**Needed:** a decision between the specification's mechanism and the project rule; recorded as Q-97, since the
mechanism is named in §3.
**Resolved at gate 3.3:** `spec.md` §3 Durable mark and Sync primitive, §7 Readers; `format.md` A.10 (positional I/O, never mapped), A.13; `api.md` seams (no `mmap`). Q-97 settled.

### FC-22 — The mark on disk has no checksum
**Where:** A.10 · **Severity:** blocks (4.4) · **Relates to:** §3 Recovery ("a missing, unreadable or half-updated
mark")
**Problem.** The sequence lock protects concurrent readers, not the copy on disk. The mark is synced on a timer while
the writer keeps updating it, and nothing guarantees that the page written is a snapshot of one moment. With no
checksum, "unreadable" can only mean a wrong magic or version. The mark carries no `store_id`.
**Scenario.** The timed sync writes the page while an update is in progress; the copy on disk holds an even `seq`,
the new `file_id` and the old `offset`. After a power loss it is accepted as valid, and recovery classifies the
region between the two as corruption below a valid mark (rule 2) or truncates at the wrong offset. Separately, a
reader that finds `seq` odd because the writer died in an update has no rule for how long to retry.
**Needed:** how a torn or damaged mark is detected on disk, and the reader's rule for a mark left odd.
**Resolved at gate 3.3:** `format.md` A.10 (two checksummed slots with a counter and the store id; reader retry rule); `spec.md` §3 Recovery.

### FC-23 — The boot identity source per platform
**Where:** A.10 · **Severity:** before 4.5 · **Relates to:** F-5, F-11, §3 Recovery, §7 Leases, roadmap 2.2
**Problem.** No source is named for any platform, nor what counts as "cannot supply" one. The fallback (sync the
mark after every update, always truncate to it) conflicts with §3's "missing mark is a different boot" (F-5) and
roughly doubles syncs per batch, which 2.2 should measure. It is not said whether a container sharing the host's
boot identity, or a virtual machine restored from a snapshot, is supported.
**Scenario.** On a platform without a boot identifier, the mark is missing after a crash: A.10 says truncate to it,
which is impossible, while F-5 says treat it as a different boot and keep complete batches.
**Needed:** the source per platform, the fallback's rules, and the environments in which a boot identity is trusted.
**Resolved at gate 3.3:** `format.md` A.10 (sources, fallback, trusted environments). **Deferred at gate 3.3** to roadmap 4.1 (the macOS dependency) and 4.5 (a Windows source); the fallback without one is stated.

### FC-24 — Same boot identity, page cache lost
**Where:** A.10 · **Severity:** before 4.5 · **Relates to:** §3 Recovery rule 1, design "a crash at any point loses
nothing that was acknowledged", Q-98
**Problem.** Rule 1 assumes the page cache survives whenever the boot identity is unchanged.
**Scenario.** The store is on a removable or separately mounted disk. The process crashes, the file system is
unmounted and mounted again (or the disk reattached) without a reboot, so the mark on disk lags by up to
`mark_sync_interval`. On restart the boot identity matches; rule 1 truncates batches that were synced and
acknowledged after the mark's last sync.
**Needed:** recorded as Q-98 (a specification question).
**Resolved at gate 3.3:** `spec.md` §3 Recovery rule 1 (two or more complete batches beyond the mark on the same boot mean a stale mark; rule 3 applies; the one-batch limit is stated), invariant 10, `design.md` Requirements; `format.md` A.10. Q-98 settled.

### FC-25 — Reader processes and files the writer changes
**Where:** A.4, A.10 · **Severity:** before 4.4 · **Relates to:** §7 Readers, roadmap 4.6
**Problem.** The format does not say whether readers may map log segments or must read them by offset. The writer
truncates segments at sealing and in recovery, and renames them at sealing.
**Scenario.** A reader process maps an `.open` segment; the writer seals and truncates it, and the reader faults when
it touches a page past the new end of file (on Linux and macOS this ends the process). On Windows, truncating or
renaming a file that another process has mapped or opened without delete sharing can fail, and the writer has no rule
for that failure.
**Needed:** how readers access log segments, and what the writer does when truncation or rename fails because a
reader holds the file.
**Resolved at gate 3.3:** `format.md` A.13 (positional reads, shared handles, `SealBlocked` when a rename or length change fails).

## Platform rules

### FC-26 — What "sync" means on each platform
**Where:** A.4, A.10 · **Severity:** before 4.4 · **Relates to:** §3 Commit, Segments, §13 Sync rate, roadmap 2.2
**Problem.** The format says "syncs it" and "syncs the directory" without naming the operation. On Linux, `fdatasync`
or `fsync`; on macOS, `fsync` does not flush the drive's cache (`F_FULLFSYNC` does, at a much higher cost); on Windows
there is no documented way to sync a directory. The guarantee each choice gives decides whether "acknowledged after
sync" holds.
**Scenario.** On macOS with plain `fsync`, a batch is acknowledged and power is lost before the drive's cache
reaches the medium. The batch is damaged below the mark, which recovery then records as corruption (rule 2), not as
a torn tail.
**Needed:** the operation per platform and per object (segment, directory, mark), from the 2.2 measurements.
**Resolved at gate 3.3:** `format.md` A.13 and `spec.md` §3 Sync primitive (Windows a development platform). **Deferred at gate 3.3** to roadmap 4.4 (Windows name durability) and 4.8 (device cache); neither decides whether Windows is accepted.

### FC-27 — What preallocation guarantees
**Where:** A.4 · **Severity:** before 4.4 · **Relates to:** §3 Segments, Disk space, roadmap 2.2
**Problem.** "Preallocated (zero-filled)" does not say whether the zeros must be physical or only logical, nor what is
assumed of the file system. On copy-on-write file systems preallocation reserves no space for overwrites, so a write
into preallocated space can fail with a full disk, which §3's critical mark does not anticipate. Writing into space
that has not yet been written can make a data sync also write metadata, which changes the cost 2.2 measures.
**Scenario.** On a copy-on-write file system that is nearly full, a batch write into the preallocated segment fails
for lack of space. §3 treats a write failure as fatal (read-only), although the critical mark was meant to make
ingest wait first.
**Needed:** the guarantee required of preallocation, and the rule when it cannot be given.
**Resolved at gate 3.3:** `format.md` A.4 Creation (physical zeros; copy-on-write accepted; no space at creation waits, at a batch write it is a write failure); `spec.md` §3 Segments. The rest of Q-86 (b) is deferred to roadmap 4.5.

### FC-28 — The single-writer lock
**Where:** A.4, layout · **Severity:** before 4.1 · **Relates to:** §3 Single writer, Q-99
**Problem.** §3 requires "exclusive `flock` on the store directory". Windows has no `flock`, and the format names no
lock file. Lock behaviour on network file systems, and whether a second open in the same process is refused, are not
stated.
**Scenario.** On Windows the writer cannot take a `flock`; two writer processes open the same store and both append
to the log.
**Needed:** recorded as Q-99; the format then needs the lock file's name if one is used.
**Resolved at gate 3.3:** `spec.md` §3 Single writer; `format.md` A.12 (`LOCK` and `File::try_lock`; network file systems not supported); `api.md` Handles. **Deferred at gate 3.3** to roadmap 4.1: verify the same-process refusal on all three platforms. Q-99 settled.

## B. Part B and section 4

### FC-29 — Section 4 needs Part B files that are only outlined
**Where:** B, A.10 · **Severity:** blocks (4.5, 4.6) · **Relates to:** F-5, F-11, §3 Recovery rules 2 and 3, §7
Leases, Q-48
**Problem.** Part B files are to be specified "before the roadmap section that implements it": the cursor file in
section 5, the manifest in section 6. But recovery (4.5) records lost ranges in the manifest (rule 2), continues
positions above "any local cursor file and any local manifest record" (rule 3) and reports a missing cursor file; and
the reader (4.6) reads an epoch from the manifest and writes a lease file (§7), which has no file kind (F-5).
**Scenario.** Item 4.5 recovers after a reboot and finds a corrupt region below the mark. It must write a lost-range
record to the manifest, whose layout is not due until 6.3.
**Needed:** before 4.5, the manifest's framing and lost-range record and the part of the cursor file recovery reads;
before 4.6, the epoch and the lease file.
**Resolved at gate 3.3:** the parts section 4 needs are now Part A: `format.md` A.14 (manifest framing, lost-range and acknowledgement records) and A.15 (cursor position, lease file).

### FC-30 — Manifest framing
**Where:** B (manifest) · **Severity:** blocks (4.5) · **Relates to:** Q-52, Q-46 (objection), F-7, F-11, F-15, §8
Hashes, Generations
**Problem.** The outline names records and an epoch only. Not given: how a torn tail is told from a corrupt record
(Q-52, assigned to 3.1); whether every record, including one that publishes no file, takes a strictly higher durable
epoch (Q-46's objection); the record types (publish and retire, tier move, lost range with its cause, F-11, and the
commit times of the batches on either side, F-15; truncated tail, Q-48; restore record, F-7; quarantine); the
plaintext and per-upload ciphertext SHA-256 (§8 Hashes); keying by generation.
**Scenario.** After a power loss the last manifest record fails its checksum. If it is a torn tail it can be
dropped; if it is a corrupt record that published files, dropping it changes which files are live (Q-52).
**Needed:** Q-52's decision and the record framing; the record types can follow their items.
**Resolved at gate 3.3:** `format.md` A.14 (records are blocks; every record takes a strictly higher epoch; record types and the lost-range fields). **Deferred at gate 3.3** to roadmap 6.3, 8.1 and 8.3: the publish, sealed-segment, tier, quarantine and restore records; to 4.5: fields for Q-33.

### FC-31 — Cursor file
**Where:** B (cursor) · **Severity:** before 4.5 (position only), can wait (5.2) for the rest · **Relates to:** §4
Consumer state, §6 Latch, Q-47, Q-61
**Problem.** Not given: whether the file is replaced (write and rename, FC-25 and FC-26 for Windows) or appended;
the last confirmed position; one latch per condition funnel (state and position, keyed by funnel identity, FC-16);
where a condition delivery's sequence is kept (Q-61); a "forgot up to" marker (Q-47's objection 1).
**Scenario.** The cursor file is replaced by writing a new file and renaming it, and power is lost between the two.
Recovery must find the old cursor or the new one; on Windows the rename can fail while a reader holds the file
(FC-25).
**Needed:** the layout; for 4.5, at least how the last confirmed position is read and how an unreadable file is
recognised.
**Resolved at gate 3.3** (position part): `format.md` A.15 (appended blocks; the last valid block is the state; unreadable named in the recovery entry). **Deferred at gate 3.3** to roadmap 5.2: latches, the confirmed reset point and timer sequences (Q-61).

### FC-32 — File kinds missing from A.1
**Where:** A.1, B · **Severity:** can wait (lease: blocks 4.6, FC-29) · **Relates to:** F-5, F-7, F-9, F-16, F-17,
Q-64, Q-70, §11 Health
**Problem.** A.1's kinds lack: lease (F-5); calendar file (§5.1, F-16); revised-key index and version index (F-9);
local restore record (F-7, §8 Restore drill); tree checkpoint bundle, which carries the registry, latest recorded `t`
and lateness reference, revised-key and version indexes (F-7, F-17), possibly the routing table (Q-64), chunk indexes
(Q-70) and the health counters "persisted with the checkpoint" (§11, `interval_displaced` §5.4); chunk index; the
deep tier's generation claim marker; the store descriptor (FC-3).
**Scenario.** Item 4.6 writes a lease file. With no kind for it, the first implementation invents one that every
later version must keep reading.
**Needed:** a kind and layout for each, before the item that writes it.
**Resolved at gate 3.3:** `format.md` A.1 kinds 7 (store descriptor) and 8 (lease), laid out in A.12 and A.15. **Deferred at gate 3.3**: kinds 9–14 are reserved and laid out by roadmap 7.1 and 7.3 (calendar, indexes, checkpoint bundle, chunk index) and 8.3 (local restore record); the claim marker by 8.2.

### FC-33 — Objects in the deep tier
**Where:** none (not in `format.md`) · **Severity:** can wait (8.2) · **Relates to:** §8 Encryption, Hashes,
Generations, `api.md` `BackupSink::get` with a range
**Problem.** Object keys, the form of an uploaded manifest record, the generation claim marker and the encryption
envelope (algorithm, nonce, key identification, and a layout that allows range reads, which `BackupSink::get` offers)
are specified nowhere. A restore by another build must be able to read them.
**Scenario.** A store is restored from its bucket by a newer build, which must decrypt objects whose envelope was
never written down.
**Needed:** an object layout section before 8.2.
**Deferred at gate 3.3** to roadmap 8.2 and 8.3 (needs the sink).

### FC-34 — Tree and calendar node layout
**Where:** B (tree) · **Severity:** can wait (7.1) · **Relates to:** §5.2, §5.6 (bitwise reproducible nodes), Q-60
**Problem.** Stored nodes must be bitwise reproducible, so their byte layout is part of the contract: the §5.2 fields
per kind, `i128` sums for a decimal scale against `f64` otherwise, a per-state time map that can have up to 65,536
entries, the node version, the `summary_stale` flag and the checkpoint position.
**Scenario.** Two builds serialise a node's `i128` sum differently and produce different tree files from the same
facts; rebuild-equals-live compares them byte for byte.
**Needed:** the layout before 7.1.
**Deferred at gate 3.3** to roadmap 7.1 and 7.3 (needs the tree).

### FC-35 — The compacted segment depends on 2.1, and Parquet has no A.1 header
**Where:** B (compacted segment) · **Severity:** can wait (6.1) · **Relates to:** roadmap 2.1, 10.2, §7, §8 Hashes
**Problem.** The layout and encodings wait for 2.1, as the outline says. If compacted data is stored as Parquet, A.1's
"every file except the mark" carries a header no longer holds: a Parquet file cannot start with `KVIK`, so the store
id check, the kind and the format version must live elsewhere (for example in its key-value metadata), and chunk
summaries and the chunk index must be described for Parquet files too.
**Scenario.** Item 2.1 finds that the own format does not clearly beat Parquet. Compacted files become Parquet, and
the store id check at open (A.1) has no header to read.
**Needed:** after 2.1, either the kind 2 layout or the rules for Parquet files in place of A.1.
**Deferred at gate 3.3** to roadmap 6.1. Roadmap 2.1 kept the own format, so A.1 applies to every stored file and Parquet is export only; the decided encodings are recorded in `format.md` Part B.
