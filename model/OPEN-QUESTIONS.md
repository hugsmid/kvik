# Open questions

Questions in [`docs/spec.md`](../docs/spec.md) found by building the reference model, and the places where the
model does not yet follow the specification. Section numbers refer to `docs/spec.md`. Each question gives a
concrete scenario, what the model does for now, and proposed wording. Where a test pins the model's behaviour, it
is named; tests are in `tests/open_questions.rs` unless stated. Identifiers are never reused.

| ID | Section | Question |
|---|---|---|
| Q-16 | §5.4, §2.3, §4 | Which commits refresh live staleness, when a fact falls due, and across a restart |
| Q-33 | §2.1, §3 Recovery, invariants 5 and 8 | The Derived check: derived files after a lost range |
| Q-44 (b) and (c) | §2.1 timing, §2.2, §2.3 Views, §2.4, §8 Restore step 2, §11 | Withdrawn values on scheduled and forecast series, far-future values on any series |
| Q-52 (b) | §7 Manifest, §3 Recovery rule 3, Order of a start steps 3 and 4 | Whether writing the last manifest record, or the last log batch, again in place makes the next sync write it |
| Q-83 | §5.6, §5.2 holds, §2.1 parameters, invariants 5 and 8 | Node contents after a past-effective change of the staleness limit |
| Q-84 | §2.3 Live state, §6 Conditions, §8 Restore step 3, invariant 16 | A condition or live state that stays unknown |
| Q-86 (b) | §3 Write or sync failure, Disk space | No space at a log write before any sync, checked on a file system that tracks pages |
| Q-90 | §8 Restore step 1, §8 Acknowledging a loss, §11 Health | Whether a restore record's set-aside report holds health degraded |
| Q-92 | §2.4 revised-key index, §8 Restore step 2, invariant 8, principle 2 | The "incomplete" mark on a revised-key index entry |
| Q-93 (b) and (c) | §5.7 tzdata version in force, §7 Readers | A release name standing for its rules; a writer that cannot write, and values other than calendar results in a reader of another release |
| Q-94 | §7 compaction order, §8 Fill pressure, Corrupt block, §3 Disk space | Compaction in position order when the next segment is not local or is quarantined |
| Q-96 | §6 Restart and resume, §4 Resync, Behind consumers | A resume replay with a segment missing in the middle, and `resync` per funnel or per consumer |
| Q-111 | `format.md` common rules (Sizes), A.4 (Sizes) | A full facts block against the batch limit |
| Q-115 (e), (f), (j), (k), (n) | §3 Commit, Segments, Disk space, Back-pressure; `format.md` A.8, A.13 | The commit path: the points still open |
| Q-116 (a), (c), (k), (n) | §3 Recovery, Order of a start; `format.md` A.14; `design.md` Budgets | Recovery at a start: the points still open |
| Q-118 (c), (f), (k) | §7 Readers, Leases, Manifest; §8 Generations | The reader process and leases: the points still open |
| Q-123 (a), (c)–(g) | §2.2, §3 Commit, §11; `api.md` Writing, Series, Health | The interface on the durable log: points left open |
| Q-140 | §5.4, §2.3, §7 Readers; `api.md` Reader processes | Live staleness of an observed series in a reader process |
| Q-141 | §2.2 `received_at`, `format.md` A.7 `commit_time` | A fact's `received_at` after the wall clock steps back |
| Q-142 | §5.5, §2.1 counter rules; `format.md` A.6 field 11 | A reset ceiling off the parts-per-million grid |
| Q-161 | §8 Generations; `api.md` Backup and restore | Telling a restore's own claim marker from another restore's |
| Q-163 | §8 Generations, Restore step 1 | Reading the claims again on a bucket whose listings lag |
| Q-164 | §8 Retention class, Restore; `api.md` Reading, Backup and restore | Interface points deferred to 8.2 that need tiers or restore |
| Q-173 | §7 Manifest, `api.md` | The operator call `rebuild_manifest`: finality, a readable manifest, several corrupt records |
| Q-175 | §7 Manifest (rebuild step 2) | An intermediate output no record and no local file describes |
| Q-198 | §8 Deep, Encryption, Generations, Restore; `api.md` Backup and restore | The backup and restore interfaces ahead of their implementations |
| Q-202 | §3 Back-pressure, Commit; `api.md` Writing | A definition waiting for its acknowledgement holds up every other decision |
| Q-203 | §3 Recovery rule 2, Order of a start step 4; §8 Acknowledging a loss | The identity of a loss found while the store is open that grows downward before a start records it |
| Q-210 | §8 Bucket; `api.md` Backup and restore; `format.md` A.17 Probe | Telling a bucket that reports no checksum from another sink failure |
| Q-211 | §7 Manifest, §8 Generations; `format.md` A.14, A.17 | An uploaded epoch written again after the last two synced records are damaged |
| Q-212 | §8 Continuous backup, Hashes; `format.md` A.17 | A key already taken by an object that is not the file |
| Q-220 | §8 Hashes, §7 Manifest; `format.md` A.9, A.14 | Where a log segment's plaintext hash lives, what it covers, and when it is computed |
| Q-221 | §3 Write or sync failure, §7 Manifest, §8 Corrupt block; `format.md` A.9 | A read that fails while hashing, and sealed segments no record names |
| Q-243 | §8 Self-repair; `api.md` `repair_done` | `repair_done` under manual acknowledgement, across a restart |
| Q-249 | §8 Self-repair stage 3, §7 Manifest | A torn closing record of a loss restored in full |
| Q-250 | §1 principle 5, §3 Recovery rule 1, invariant 10 | Unsynced faster-level batches that a remount within the boot removes |
| Q-251 | §3 Recovery rule 3, Adoption, §11 Health; `format.md` A.6 field 48 | Which recovery entries report a `possible_loss` for the faster level |
| Q-252 | §2.1 `durability`; `api.md` Writing, Configuration | A per-call wait for the sync on a faster-level series, and the `max_unsynced` default |
| Q-253 | `format.md` A.7 `flags`, A.3 | The commit-block flag in formerly reserved bytes, which earlier builds read as nothing |
| Q-254 | §1 principles 3 and 5, §2.1 `durability`, §3 Write or sync failure, Recovery rule 1, rows 7 and 12, invariant 10; `api.md` Series | Acknowledged faster-level batches evicted after a failed write-back, then a restart on the same boot |
| Q-255 | §2.1 `durability`, §3 Batches, Commit, Group commit; `api.md` Writing | How calls of two levels are cut into batches and acknowledged |
| Q-256 | §2.1 `durability`; `api.md` Series | The level changed at run time by `set_durability` |
| Q-257 | §2.1 `durability`, §3 Recovery rule 2, invariant 10 | Faster-level readings lost out of order reported as `data_lost` |
| Q-258 | §3 Recovery rule 1, invariant 10 | Default-level batches after flagged ones truncated by a remount where they were kept before |
| Q-259 | §1 principle 3, invariants 2 and 11 | What sees a faster-level reading before it is durable |
| Q-261 | §8 Generations, §3 Recovery; `format.md` A.10, A.12 | Files of another generation at a start |
| Q-263 | §8 Restore step 1, §7 Readers | What is set aside: whole batches, by their first position |
| Q-265 | §8 Generations; `api.md` Backup and restore | When a writer reads the claims, and what it does on a higher one |
| Q-267 | `api.md` (log seams); roadmap 6.3b, 6.3c | The start options 8.3c does not act on |
| Q-268 | §7 Readers, §8 Restore step 1; `format.md` A.10 | A reader on a restored directory |
| Q-270 | §8 Encryption; `format.md` A.17 Reading | Envelope damage that `EnvelopeError` has no variant for |
| Q-280 | §7 Manifest (rebuild), §8 Continuous backup, Hashes | How a rebuilt manifest queues files for upload again |
| Q-281 | §8 Hashes; `format.md` A.14 | The copy-verified record (`0x27`): layout and fold |
| Q-282 | §8 Continuous backup, Generations; §7 Manifest | The upload queue, the record cursor and which records wait |
| Q-283 | §8 Hashes, Bucket, Generations | The attach probe |
| Q-284 | §8 Hashes, Upload stalled | Upload failures: retries, other content, an object not reported after its `put` |
| Q-285 | §8 Continuous backup; `format.md` A.12 | Uploading files with no local layout yet, and the hash a file is checked by |
| Q-291 | §8 Acknowledging a loss, Generations, Restore step 1 | Recovery-entry loss reports of the generation a restore reads |
| Q-292 | §8 Generations; `format.md` A.2 sizes, A.14 `0x25` | A restore record larger than one block |
| Q-294 | §8 Restore step 1, Generations; model `Model::restore_from` | Where the model's restore differs from the plan |
| Q-301 | §8 Restore (the paragraph after step 4) | The files a resumed restore fetches |
| Q-302 | §8 Restore (Failures and end), §8 Corrupt block | The lost range of a listed file the bucket has lost, and files other than log segments |
| Q-303 | §8 Generations, §7 Readers; `format.md` A.1 kind 12 | The local restore record file |
| Q-310 | `design.md` Budgets; §3 Group commit | The delivery budget's p50 at exactly 100 calls a second with the default bound (settled by an engineering fix; its schedule superseded by Q-980) |
| Q-322 | §3 Durable mark; `format.md` A.10 Update | A torn mark update that makes a damaged older slot valid again |
| Q-341 | §8 Bucket; `format.md` A.17 Probe | The attach probe at `<sid>/probe`: its wrong-hash put there, and an attach after a full restore |
| Q-342 | §8 Continuous backup, Generations; `api.md` `BackupStats` | `backed_up_through` in a restored generation |
| Q-601 | §8 Self-repair, Generations; `format.md` A.14 corruption seen, `0x25` fields 12 and 13 | A corruption named by an epoch across restore generations |
| Q-610 | §8 Generations; `format.md` A.17 Uploaded forms | The chain of epochs in uploaded manifest records: the field and where it starts |
| Q-611 | §8 Generations, Restore step 1; `api.md` Backup and restore | What a broken chain of uploaded records is, and what a restore does with one |
| Q-613 | §8 Generations, Restore step 1 | Files of the generation read that no record names |
| Q-615 | §8 Generations, Restore step 1 | The source records are read after the claim |
| Q-670 (c) | §4 Funnel; `api.md` Consuming, Errors | The pattern grammar and `RouteError` as a public promise |
| Q-680 | `api.md` Writing, Health | `Outcome::Recorded` carrying the level and `LogStats.acknowledged` |
| Q-681 | §2.1 `durability`, §3 Commit, invariant 10; `api.md` Configuration | What `max_unsynced` bounds |
| Q-704 | §7 Readers, Leases, Retired files | The writer's own delivery reader skips a refresh at the writer's durable position; that needs a manifest that changes only at a start |
| Q-773 | §7 Derived files; `format.md` A.18 Cadence | The series directory's cadence against a reader's allowance (decided; confirmed by 9.0c) |
| Q-774 | `format.md` A.18, A.12 | The derived files' layout, names and generations (decided by the lead, awaiting review) |
| Q-775 | §7 Derived files; `format.md` A.18 Validation | Checking a log directory before the manifest lists sealed segments |
| Q-776 | §7 Derived files, Reset stamp and saved points | When the writer keeps a series directory across a reset |
| Q-778 | §7 Derived files; `format.md` A.18 | What the status file holds before the query envelope exists |
| Q-780 | §8 Generations, Restore; `format.md` A.18 Contiguity | The log directory across a generation change |
| Q-850 | §7 Readers; `api.md` Reader processes | The error of a second reset stamp change during one materialised call (settled by the lead with roadmap 9.0f, Q-1061) |
| Q-851 | §7 Readers | The second read of the mark after a pass that took something (decided, awaiting review) |
| Q-852 | §7 Readers, Manifest; `format.md` A.13 | What a reader call reads once caught up (decided, awaiting review) |
| Q-853 | §7 Readers; `api.md` `ReaderConfig` | `cut_max_age`'s default and bound, and the clock it runs on (decided, awaiting review) |
| Q-854 | §7 Manifest, Leases; `format.md` A.15 | The highest epoch a reader has read, recorded in its lease (decided by the lead) |
| Q-900 | §7 Readers; query design §3.5 | How a reader's published views share structure (decided, awaiting review) |
| Q-901 | query design §3.5 Verified by | The model checker for the single-flight refresh (decided, awaiting review) |
| Q-902 | §7 Readers; query design §3.5 | The single-flight refresh: rounds, followers and a leader's error (decided, awaiting review) |
| Q-903 | §7 Readers, Leases; query design §3.3, §3.5 | A call's cut when calls share views (decided, awaiting review) |
| Q-904 | query design §3.5; roadmap 9.0b2 | The recent window's shard locks, left for the first query that reads the recent window |
| Q-751 | §5.1, §11, §13 | The recent windows' memory: per series and store-wide |
| Q-752 | §2.4 revised-key index, §5.1, §8 tree checkpoints, §13 Restart | A start's time and the revised-key index before tree checkpoints exist |
| Q-753 | §2.3 Live state, §4 Outranked facts, §5.1, §6 Restart and resume | What the deliverer keeps in memory and reads from the log |
| Q-754 | §7 Readers | The lookup's batch maps in a reader of another process across a reset stamp or generation |
| Q-755 | §2.4, §3 Recovery rule 2, §8 Corrupt block, §11 Health | Damage found during a run recorded durably as a loss |
| Q-710 | §3 Group commit, §13 Sync rate and SSD wear; `api.md` Configuration | The default floor of the batch-sync interval |
| Q-711 | `kvik_log::WriterConfig`, `kvik_log::LogStats` | New fields in the log crate's `WriterConfig` and `LogStats` |
| Q-712 | §3 Group commit | The constants of the adaptive interval |
| Q-760 | §2.2 `changed_at`; `format.md` A.5 | A stored `changed_at` of kind `time` above `i64::MAX` |
| Q-761 | `docs/export.md` Partitioned layout | Long and renamed series names in the partitioned layout |
| Q-832 | §5.1, §8 tree checkpoints, §11 `series_stats`, §13 Restart | A start independent of the log's length without tree checkpoints |
| Q-834 | `format.md` A.9 `file_crc32c`, A.4 Sealing | The seal's file checksum from the block checksums |
| Q-835 | §3 Order of a start, §11 Health | How much of a start may be deferred past `open` |
| Q-836 | §3 Recovery rule 2, §7 Readers, §11 Health | Facts a damaged recovery entry voided read again as live |
| Q-871 | `format.md` A.12 Creation and Open | Store directory entries a stop or a power loss can undo |
| Q-937 | §3 Recovery rule 2; `api.md` Health | A damaged last batch of a sealed segment reported as lost to the head of the log |
| Q-811 | `format-compacted.md` B.1, B.2 | Format details settled while writing the compacted segment's layout |
| Q-872 | §7 Manifest step 4; `format.md` A.14 | The epochs a rebuild takes above a corrupt span with no later record |
| Q-990 | `format.md` A common rules (Scan bound) | A batch kept after damage, with its content kept (decided by the lead) |
| Q-991 | `format.md` A.18 Status file | What the count of failed derived-file writes counts (decided by the lead) |
| Q-1000 | §7 Readers; query design §4 | The fact index of a reader that serves latest values (decided by the lead) |
| Q-1001 | `format.md` A.16, A.18 | The registry file before a series directory that covers an entry (decided by the lead) |
| Q-1002 | §7 Derived files, §11 Health; `format.md` A.18 | No series directory over a loss no lost range records (OWNER) |
| Q-1003 | §7 Readers, Derived files; `format.md` A.16, A.18 | Which series directory a reader continues (decided by the lead) |
| Q-1004 | §2.3, §7 Readers; `format.md` A.18 | What a reader finds in the log because the state cannot tell (decided by the lead) |
| Q-1005 | `api.md` Reading | `Snapshot`: what it answers at one cut (decided by the lead) |
| Q-1006 | §7 Readers; `api.md` Health | A reader whose latest values no longer follow the log (decided by the lead) |
| Q-1007 | query design §1 S1 | Where S1 stands: the writer's own latest values (decided by the lead) |
| Q-1010 | `api.md` Live reads; query design §2.1, §3.6 | What a live session sends and when (decided by the lead) |
| Q-1011 | §3 Recovery, §7 Readers; `api.md` Live reads | Resets and losses in a live session (decided by the lead) |
| Q-1012 | `api.md` Live reads; labels design §2 | A live read's selection, membership and canonical order (decided by the lead) |
| Q-1013 | query design §3.6 | The shared tail of a reader and slow consumers (decided by the lead) |
| Q-1014 | query design §3.6 | A row limit met inside one refresh's read (decided by the lead) |
| Q-1015 | query design §1 S13, §3.6 | Waking, pacing and due times of a live session (decided by the lead) |
| Q-1016 | query design §2.1; roadmap 8.1g | A live read and an unavailable segment or tier (decided by the lead) |
| Q-1017 | query design §1 S13 | The empty poll of a reader process against S13 (decided by the lead) |
| Q-1050 | query design §6 | The tool's command line: the store, the live commands' end, and a tail from a time (decided by the lead) |
| Q-1051 | query design §6; `format.md` A.15, A.18 | What `health` and `info` read besides the reader (decided by the lead) |
| Q-1052 | query design §1 | How S1 to S3 and S13 are measured on a year of seals (decided by the lead) |
| Q-1053 | query design §1 S3, 9.0b1 | S3 on a year of seals: a reader's start no longer lists the log directory (decided by the lead) |
| Q-1054 | query design §1 S13, §3.1; Q-1017 | The empty poll of a reader process against S13: the held mark, and the machine's wake-up cost (decided by the lead) |
| Q-1055 | query design §1 S1; Q-1007 | S1 after a write misses its desktop target until the hub feeds the writer's reader (decided by the lead) |
| Q-1056 | query design §6; `format.md` A.18 | Per-series counts in `kvik series` now; `tail --since` and `--once` with 9.0f (decided by the lead) |
| Q-1060 | §7 Readers; query design §2.2, §4 | The log tail as the one source of `facts` and `at` (decided by the lead) |
| Q-1061 | §7 Readers; `api.md` Reader processes | An iteration whose cut ends or whose reset stamp changes (decided by the lead) |
| Q-1062 | query design §2.2 | The order of several series read in one pass (decided by the lead) |
| Q-1063 | query design §1 Memory, §2.2, §3.5; `api.md` `ReaderConfig` | The memory accountant and the cap with multi-pass (decided by the lead) |
| Q-1064 | §2.3 As-of time; query design §3.2, §4 | As of a time from the log's commit times (decided by the lead; across a restore, open) |
| Q-1065 | `api.md` Reading | `at` searched back from its time (decided by the lead) |
| Q-1066 | Q-951; §7 Readers | A segment gone from under the sources' reads (decided by the lead) |
| Q-1067 | query design §1 Memory; `api.md` `ReaderConfig` | Rows waiting for delivery are held under their grant (decided by the lead) |
| Q-1068 | query design §2.2; `api.md` `ReaderConfig` | One event time holding more than a pass (decided by the lead) |
| Q-1069 | query design §1 Memory; `api.md` `ReaderConfig` | What the memory cap counts, measured (decided by the lead) |
| Q-1095 | query design §3.5; Q-1067 | Admissions that empty shelves at once (decided by the lead) |
| Q-1096 | §2.3; Q-1067 | Where a stream resumes after its shelf is emptied (decided by the lead) |
| Q-1097 | query design §3.5; Q-1095 | An admission with nothing to empty, and shelved rows no shelf holds (decided by the lead) |
| Q-1100 | query design §3.5; Q-1097 | A shelf made counts as a change (decided by the lead) |
| Q-1101 | §11 Health; Q-1011, Q-1060 | A far reader that built its fact index for a broken state, and damage below what it read (decided by the lead) |
| Q-1102 | §3 Order of a start step 4, §11 Health; Q-940 | A start's lost range across two recorded losses (ruled by the owner) |
| Q-1103 | query design §1 Memory; Q-1069 | The first call's memory bound and the refresh within it (decided by the lead) |
| Q-1104 | §11 Health; Q-1002, Q-1071 | A series directory the store wrote over damage no start has met (decided by the lead) |
| Q-1107 | query design §1 Memory; Q-1069, Q-1103 | The first refresh's bound and the series directory written at the seal (decided by the lead) |
| Q-1108 | §8 Acknowledging a loss, Self-repair; `api.md` Health; Q-1102 | Acknowledgements, listing and corruption counts across a fold (OWNER) |
| Q-1109 | §4 Provisional streams, §3 Commit; `api.md` Provisional streams | Delivery before durability: an automation may act on a reading later withdrawn (ruled by the owner) |
| Q-1110 | §4 Provisional streams; `api.md` Provisional streams; Q-1109 | A stream's end after a writer panic, a call larger than `max_behind`, the watermark's rule and the ring's hold (ruled by the owner) |
| Q-1114 | §3 Order of a start; Q-1115 | The damage scan below a seed starts again at every open (decided by the lead, provisional) |
| Q-1115 | §3 Order of a start, Recovery rule 2; Q-947 | A seeded start scans for damage below its seed after the open returns (ruled by the owner) |
| Q-1116 | §3 Order of a start; `format.md` A.16, A.18; Q-33, Q-1001 | What a seeded start continues from, and when it reads the whole log (decided by the lead) |
| Q-1117 | `format.md` A.18 Writing; Q-1116 | A power loss can take the seed with the unsynced series directory (decided by the lead) |
| Q-1118 | query design §3.6; `api.md` Live reads; Q-1013, Q-1015 | Selective fan-out of live sessions: what a session passed over no longer counts (decided by the lead, provisional) |
| Q-1119 | §4 Delivery; `api.md` Consuming; Q-706 | Consumer deliveries taken off the emit's path, and wakes per consumer (decided by the lead, provisional) |
| Q-1130 | §6 Chart buckets; `api.md` Reader; Q-1000, Q-1060 | Windows, chart buckets and calendar results from the sources (decided by the lead) |
| Q-1131 | §6 Chart buckets; `api.md` Reading; Q-1062, Q-1130 | Several series' windows, buckets and facts from one read (decided by the lead) |
| Q-1132 | query design §2.2; Q-1063, Q-1068 | Passes sized from counts before they are read (decided by the lead) |
| Q-1133 | `api.md` Reader processes; Q-1131, Q-1132 | A reader call on more than one thread (OWNER) |
| Q-1150 | `format.md` A.8a; Q-1063, Q-1130, Q-1131 | One series' facts from its own batches: series batches at seal (decided by the lead) |
| Q-1151 | `api.md` Reader; Q-1133, Q-1150 | Several callers' many-series reads at once (for review) |
| Q-1140 | §6 Gates; `api.md` Gates (proposal) | Gates beside today's condition funnels, and what the model leaves to later steps (decided by the lead, provisional) |
| Q-1141 | §6 Gates | When a gate's input has no value (decided by the lead, provisional) |
| Q-1142 | §6 Gates; `api.md` Gates (proposal) | A transition's identity names the instance's series, not an instance number (decided by the lead, provisional) |
| Q-1143 | §6 Gates | An `Each` past its 256 instances (decided by the lead, provisional) |
| Q-1144 | §6 Gates | A timer due at an entry's commit time (decided by the lead, provisional) |
| Q-1145 | §6 Gates | A gate's state when it starts (settled by the owner: seeded from history) |
| Q-1146 | §6 Gates | Holds across `Unknown` and `Stale` (decided by the lead, provisional) |
| Q-1147 | §6 Gates, Windows | Window inputs in the model: sampling and the reading at `now` (decided by the lead, provisional) |
| Q-1148 | §6 Gates; §4 Future facts, Outranked facts; Q-16 | What counts as an arrival for a gate's staleness (decided by the lead, provisional) |
| Q-1149 | §6 Gates, Durations | The longest duration of a gate (decided by the lead, provisional) |
| Q-1200 | §6 Gates; §4 Future facts, Outranked facts; Q-1148 | A future fact outranked before its `t` (settled by the owner) |
| Q-1201 | §4 Consumers; `api.md` Memory | Records held for a consumer not taken in this run (settled by the owner) |
| Q-1202 | §4 Resync; `api.md` Consumers | `CursorBehindLog` on the durable backend (settled by the owner) |
| Q-1203 | `api.md` Live reads; Q-1011, Q-1118 | A fan-out round that fails after taking events (settled by the owner) |
| Q-1390 | §6 Gates, Transitions; Q-1145 | The seed at a gate's start: how far back, and the value before the first reading (decided by the lead, provisional; amended by Q-1560) |
| Q-1391 | §6 Gates, Resume; `api.md` Gates | Durable resume state: one gate cursor and a checkpoint, the bound from the checkpoint (decided by the lead, provisional) |
| Q-1393 | §6 Gates, Clock; `api.md` Gates | The clock live gate timers run on (OWNER) |
| Q-1560 | §6 Gates, Transitions; Q-1390 | Facts inside the seed's look-back that were never the live value: what the seed reads (decided by the lead, provisional) |
| Q-1880 | §6 Gates; Q-1560 | What the gates core's history reads must return, and what they cost (decided by the lead, provisional) |
| Q-1881 | §6 Gates; Q-1880 | How a failed or short history read reaches the gates core (decided by the lead, provisional) |
| Q-1882 | §6 Gates; Q-1560 | Whether the seed's start state counts a fact still ahead as an arrival at the look-back's start (decided by the lead, provisional) |
| Q-1561 | §6 Gates, Transitions; Q-1390 | Arrivals inside the seed: a series stale throughout the look-back can complete a hold (decided by the lead, provisional) |
| Q-1580 | `format.md` A.6 | The routing field ids of a watch and an unwatch, and the member value form (decided by the lead, provisional; settle with conditions step 4) |
| Q-1581 | `api.md` Gates | The gate interface as built: `Transition` without flags, `TransitionCause`, `gates()`, two more `GateError`s (decided by the lead, provisional; settle with conditions step 4) |
| Q-1582 | §6 Gates; Q-1140 | Entries other than facts, definitions and the consumer's own gates move the judgement's frontier (decided by the lead, provisional; settle with conditions step 4) |
| Q-1583 | `api.md` Gates; crate README | What the model backend's gates do not do yet (decided by the lead, provisional; settle with conditions step 5) |
| Q-1810 | `format.md` A.6; Q-1580 | The bounds of a set member: decimal scale and state index (decided at review, provisional) |
| Q-1811 | `api.md` Gates, Naming | A set gate with the same members in another order is not an equal spec (decided at review, provisional) |
| Q-1812 | `api.md` Gates; Q-1583 | A named-state member on the durable backend (decided at review, provisional) |
| Q-1800 | §6 Gates; Q-1390 | A fact at position `0:0` in a history given to `judge` (decided by the lead) |
| Q-2000 | §6 Gates, Resume; `api.md` Gates | The durable gate cursor in memory until the cursor file holds it (decided by the lead, provisional; settle with cond2 step 5) |
| Q-2001 | §6 Gates; Q-1880 | What the durable backend's gate history reads cost (decided at review, provisional; settle with cond2 step 5) |
| Q-2002 | §4 Overflow; §6 Gates, Resume; Q-1396 | Gate state after an overflow catch-up: a resync, cause `Resume` (decided at review, provisional) |
| Q-2003 | §6 Gates, Resume; Q-1391 | When a resync's gate states are taken and the gate cursor its confirmation sets (decided by the lead, provisional) |
| Q-2250 | §4.3 held snapshot; G5 | When a run takes the snapshot that becomes `held`: only with nothing outstanding and nothing deferred (decided by the lead, provisional; settle with cond2 step 5) |
| Q-2251 | §6 Gates, Resume; Q-2003 | Deferred transitions older than a dropped one, delivered after the resync that ended the quiet (decided by the lead, provisional; settle with cond2 step 5) |
| Q-961 | `api.md` Backup and restore, Restore | The last-uploaded marker at `Store::restore`: the switches, the errors and the reports |
| Q-980 | §3 Group commit, Commit | Batch syncs paced by a timer on an idle disk: group commit on a sync budget |
| Q-981 | §3 Group commit, §13 Sync rate and SSD wear; `api.md` Configuration | The interval's bounds as store keys: `sync_interval_floor` and `min_sync_interval` |
| Q-982 | §3 Group commit, Sealing | What spends the sync budget: batch syncs only, counted at the start of their turn |
| Q-983 | §3 Group commit, §13 Sync rate and SSD wear; `design.md` Budgets | The device-time term of the budget against the latency of loads above it (OWNER) |
| Q-984 | §3 Group commit; `kvik_log::LogStats` | The smoothed sync time and `sync_interval` under the budget |
| Q-985 | §3 Group commit | The burst of the sync budget: four batch syncs |
| Q-880 | §2.1 Labels | The label rules and limits as a promise (OWNER) |
| Q-881 | §2.1 Labels | Reserved label keys (OWNER) |
| Q-884 | §2.1 Labels; `api.md` Labels | Labels in a definition are initial only (OWNER) |
| Q-1020 | §2.1 Labels | Label entries in the model kept apart from registry events (decided by the lead) |
| Q-1021 | §2.1 Labels; `api.md` Labels | A label call that appends nothing, and the outcome's position (decided by the lead) |
| Q-1022 | §2.1 Labels; `format.md` A.6 | What `limits.label_bytes` and `limits.label_versions` count (decided by the lead) |
| Q-1023 | `api.md` Labels | The order of a label call's checks (decided by the lead) |
| Q-1024 | §2.1 Labels | A limit refuses only a call that adds versions (decided by the lead) |
| Q-1025 | §2.1 Labels, Duplicates | Duplicates within one batch of label changes (decided by the lead) |
| Q-1026 | §2.1 Labels, §2.3 Label views | A withdrawal with nothing to withdraw (decided by the lead) |
| Q-1027 | §2.1 Labels; `api.md` Labels | What `declare_labels` checks and reports (decided by the lead) |
| Q-1028 | §2.3 Label views | Label views of orphaned and retired series, and as of a time (decided by the lead) |
| Q-1029 | §2.1 Labels; `api.md` Health | The 80 % notices and the fixed key limit (decided by the lead) |
| Q-1040 | §2.1 Labels, Duplicates; `api.md` Labels | A call that writes nothing resolves once what it duplicates is synced (decided by the lead) |
| Q-1041 | §2.1 Labels; `api.md` Labels, Errors | A definition whose labels pass a limit (decided by the lead) |
| Q-1042 | `format.md` A.19 | Label part file ids are never reused (decided by the lead) |
| Q-1043 | `format.md` A.16, A.19; §3 Recovery rule 3 | A label part's `through` at a start |
| Q-1031 | §4 Resync, §11 Health | A recovery entry in a segment that cannot be read is a reset point (decided by the lead) |
| Q-1032 | §3 Recovery, Order of a start | The next position after a start that cannot read a published segment (decided by the lead) |
| Q-1033 | §4 Resync, §11 Health; `format.md` A.16 | The registry file keeps every recovery entry; the durable deliverer takes reset points from it (decided by the lead) |
| Q-1034 | §11 Health, `api.md` Health | A sealed segment whose body is zeroed, passed over with health `Ok` |

## Q-16 — Which commits refresh live staleness
**Scenario.** S = 3 minutes. (a) A series has been silent for 10 minutes and is stale; a late fact for last week is
committed. Read literally, "since its last commit of a fact that is not bad quality" makes the series fresh again,
although live state still shows the 10-minute-old reading. (b) A revision for the same `t` as the live-state fact,
committed after S of silence, likewise refreshes it. (c) A future fact is excluded until due (§4), but it is not
said whether staleness counts from its commit or from when it fell due, nor whether "due" follows `now` (which a
forward clock jump moves) or the monotonic clock (§4 Timers). (d) The monotonic clock restarts at boot, so after a
restart there is no last-commit instant to measure from. (e) "Bad quality" may mean the supplied or the engine
quality.
**Model.** Stale-for conditions measure monotonic time since the last arrival of any reading. A resume, which has no
monotonic instant from before it, rebuilds that time on the host clock: the wall clock now less the `received_at` of
the last reading committed, not below zero. A stale-for condition true now is reported true in `resync`
(`q16_stale_for_true_now_reported_true_on_resync`), and a backward correction of the clock neither fires nor
suppresses it (`q16_backward_clock_correction_neither_fires_nor_suppresses_stale_for`); a forward correction while
the engine is down still makes the last reading look older.
**Tried and rejected.** Counting from a staleness reference that restarts only when the live state's `t` rises past
a high-water mark, for facts not flagged `late`, with "due" on the monotonic clock. Review found that this breaks
principle 2 (when a fact fell due depends on a clock the log does not hold, so replay cannot rebuild it), that one
reading far in the future flags every later reading `late` and keeps the series stale, and that a device always
more than the lateness bound behind would never be fresh.
**Needed.** A rule that is a function of the log, ignores late facts and same-`t` revisions, survives a restart and
a clock jump, and says whether a skewed device counts as stale.
**Proposal.** Live staleness follows live state (§2.3). The **refresh instant** of an observed series is that of its
live-state fact's key: the commit of the earliest-recorded fact for that `(series, t)` whose recorded quality (after
any lowering by the engine) is not bad and which was not outranked at commit, or, if it was committed ahead of
`now`, the instant `now` reached it (§4 Timers). The series is stale when more than S has passed since then, or when
it has no live-state fact. A commit that leaves the live-state `t` unchanged (a late fact for an earlier time, a
revision of the same `t`, a bad-quality, outranked or not-yet-due fact) refreshes nothing; the first good reading that
raises the live-state `t` refreshes the series even when flagged `late`, so a device whose clock runs behind is fresh
while it reports and health reports the skew (row 5). For a refresh instant before the engine's present start, the
time passed is taken once, at the first `now` after the clock is reported synchronised, as `now − max(received_at,
t)` of that fact, not below zero; until then a stale-for condition keeps its latched state. Proposed additions: a row
for a restart with a series silent since before it, a row for a live-state fact revised to bad quality, and an
invariant that a commit leaving the live-state `t` unchanged does not change live staleness.
**Objections to the proposal.** (1) A host with no real-time clock restarts unsynchronised: until the time passed is
taken, staleness is undefined for a consumer with no latch (a new consumer, or any consumer after a restore), for the
staleness `resync` carries, and for `reader.live()`. (2) The seed `now − max(received_at, t)` fails when
`received_at` ran ahead of real time (the clock stepped back after synchronisation, or facts were committed before
it), so a series stale before a restart can be fresh after it. (3) A first good reading that raises the key "however
old its `t`" makes a week-old value fresh, against "a reading is valid for at most S" (§5.4), row 3, and `api.md`
`live()` returning the "latest valid value". (4) A parameter entry that changes S with an effective time at or
before the live-state `t` changes staleness without changing `t`, against the proposed invariant, and whether the S
in force at `t` or at `now` applies is not stated. (5) Already so in the current text: a reader process (§7) has no
refresh instant, since the writer's monotonic clock is not in the log.
**Roadmap 5.4.** The durable backend gives live state and staleness as the model backend does (`kvik/src/durable/`
`windows.rs` and `clock.rs`): an observed series is stale when more than S has passed on the monotonic clock since its
last commit, in the writer's process, of a fact that is recorded, not outranked, not bad quality and not `future`
(the instant is taken when the log acknowledges the call, after the covering sync, whether or not the caller waits); with no such commit since the store was
opened, in event time, `now − t` of the live-state fact; a scheduled series in event time, a forecast series never.
So (a) and (b) still refresh it, as in the model, and (d) falls back to event time. A reader process has no refresh
instant at all (objection 5, Q-140). Not settled: the owner's ruling on the proposal and its objections is pending
(`kvik/tests/windows.rs`, `a_reader_process_answers_after_shutdown_and_power_loss`), and 5.4 stays unticked until it
is given.

## Q-33 — The Derived check: derived files after a lost range
**Scenario.** The kept-log principle, orphaned facts and id floors are settled (§2.1; see Settled). A lost range
(§3 Recovery) held a definition, parameter change, alias, routing change, site-zone or tzdata entry, or facts that tree files and
the revised-key and version indexes were built over. The registry file may still hold the lost entry and the derived
files the lost facts, so rebuilding from the kept log no longer equals the live structures (invariants 5 and 8). The
same holds for a lost range that a restore records when the bucket has lost or corrupted a listed file (§8 Restore,
Failures and end). A lost log segment whose facts were already compacted loses only its non-fact entries: a scattered
set of positions, not a range between batches, so the report's bytes and commit times on either side (§11 Health)
are undefined for it, the batch `commit_time` that as-of-time queries over the surviving facts need (§2.3) is lost
with it, and which definitions count as lost is not said (a chunk or the registry file may still hold one). Since
Q-100, each decimal series' largest magnitude and highest stored scale are derived values too, carried by tree
checkpoints: a checkpoint taken before a lost range may carry a magnitude or scale held only by lost facts, so the
scale in force (§2.1, which counts kept facts only) and the range check must be brought back to the kept log with
the other derived values (a carried magnitude only makes the range check stricter; a carried scale would raise the
scale in force without a kept fact or raise entry behind it).
**Model.** Not modelled: a lost range comes only from a damaged batch of facts in the open segment (row 11a), and
`Model::corrupt_open_batch` refuses a batch holding a registry, routing or zone entry
(`tests/edge_cases.rs` `row_11a_fault_injection_limits`).
**Tried and rejected.** Syncing every non-fact entry to the registry file and re-appending lost ones to the log with
their original positions; keeping facts of undefined series apart until a definition names their id (review found
that re-appended entries put lower sequences after higher ones, §2.2, `format.md` A.4, and that a definition taking
effect from its earliest fact contradicts §2.1). A stamp recorded with each lost range and compared with the stamp
each derived file was built under (`loss_stamp`) fails on requirements 1 to 3 below.
**Needed.** How the registry file, trees and indexes are brought back to a function of the kept log after a lost
range (the Derived check). Whatever is designed must hold these:
1. It covers the writer's memory, not only files: a lost range found at run time below the in-memory applied position
   must not be checkpointed under a fresh stamp. Candidate: the writer restarts recovery when it records one.
2. It names a rebuild base that exists when the range lies below the newest uploaded checkpoint, as for a range
   recorded by a restore (§8 Restore, Failures and end).
3. It covers a lost segment whose facts were already compacted: the non-fact positions lost are scattered, the
   report's bytes and commit times are undefined, and series still held by chunks must not be orphaned by dropping
   their definition from the registry file. Candidate: a definition is lost only if no local file holds it.
4. No fact is accepted for a series between a run-time lost-range record and the registry correction.
5. It is recomputed at every start, rule 1 included, with no stored label: the cut-back of derived files must not
   depend on a step a crash can skip (the mark written and the process crashing on the same boot while the registry
   file still holds a definition from a lost position).
Until then invariants 5 and 8 hold "lost ranges excepted" (§10), as the model has it (damage only to batches of
facts).
**Roadmap 5.1, for the registry file** (`format.md` A.16, `kvik/src/durable/`). Every start rebuilds the registry from
the kept log and checks the file against it: the file must hold exactly the kept log's registry entries up to the last
synced mark; otherwise it is written again from the kept log, carrying the writer's id floor, so the id of a lost
definition stays assigned (`kvik/tests/durable.rs`, `the_registry_file_follows_the_kept_log`,
`the_registry_file_carries_the_id_floor_when_the_mark_is_lost`). This meets requirement 5 (recomputed at every start,
no stored label). Requirements 1 and 4 hold for now because no lost range is recorded at run time: only a start finds
one, and the registry is rebuilt after it. Requirements 2 and 3 cannot arise yet: until log segments leave the hot tier
(roadmap 8.1) or are compacted (6.x) and checkpoints exist (7.1), the kept log holds every registry entry, and the file
is checked, never trusted over the log. The rest of this question (requirements 1 to 4, the indexes and trees,
invariants 5 and 8 across a lost range) moved to 6.2 at the 5.1 review (no derived file other than the registry
exists before 6.2). Recovery entries, reset points, rows 7b and 7c and invariants 25–27 are modelled since roadmap 5.1a.

## Q-44 (b) and (c) — Withdrawn and far-future values
**Scenario.** (b) A newer issue that no longer covers a valid time cannot withdraw the older issue's value there, so
the current view can mix issues. (c) A wrong value far in the future on a scheduled or forecast series is never
flagged, since those series are exempt from `future`. Parts (a), a silent feed, and (d), forecast accuracy, are
settled (see Settled).
**Model.** Not modelled.
**Proposal for (b) and (c).** The engine sets no horizon and infers no withdrawal. A source withdraws a value by a
reading for that `t` with a higher `changed_at` (on a series that is not versioned, a later reading) and quality bad,
carrying the value it withdraws, which summaries, windows and live state ignore (§2.5, §2.3). A wrong value far ahead
is corrected by a revision or withdrawn the same way, and per-series statistics show the latest recorded `t`.
**Objections to the proposal.** (b) Under §12 D6 the withdrawing bad reading is ignored and the previous value's hold
continues up to S, so withdrawing the 14:00 value of a half-hourly price credits the 13:30 price across 14:00 with
full coverage; "coverage ends where the remaining values' validity ends" holds only at the tail of an issue. On a
series that is not versioned, a later unchecked replay of the withdrawn value (§12 D5) can revert the withdrawal
where the key's revised-key index entry is incomplete (§2.4, since Q-72), which is the hazard §2.4 gives for
declaring a series versioned. (c) Neither a revision nor a withdrawal lowers the
latest recorded `t`, so a wrong value for 2099 disables the duplicate fast path (§2.4) for good, makes every fact
during §8 Restore step 2 `duplicates_unchecked`, and keeps the latest-`t` statistic wrong. The same holds on an
observed series: since Q-74 the lateness reference leaves out a `future` fact, but the latest recorded `t` still
includes it, so a device glitch to 2099 disables the fast path for good and makes Restore step 2 count every fact of
the series `duplicates_unchecked`.
**Roadmap 5.1.** The durable backend's current view implements the proposal for (b): the fact of highest rank per
`(series, t)`, with no horizon and no inferred withdrawal, so a withdrawal is a bad-quality reading with a higher
`changed_at`. Not settled: the owner's ruling on the proposal and its objections is pending, and 5.1 stays unticked
until it is given.

## Q-52 (b) — Whether writing the last manifest record, or the last log batch, again in place makes the next sync write it
**Scenario.** Parts (a) and (c) are settled at gate 3.3 (see Settled). §7 Manifest writes the last valid record's
identical bytes again in place at every start, so that a record kept from the page cache after a failed sync is on
disk before a successor is appended. Since Q-87, §3 Recovery rule 3 does the same for the last complete log batch
(and its segment's header) whenever the mark is missing, unreadable or stale, and `format.md` A.4 seals again from
step 1 an `.open` file whose trailer is found, writing the segment index, trailer and end marker again. Whether the next sync then writes those
bytes, on each platform, was not measured by the roadmap 2.2 spike.
**Model.** Not modelled; manifest records are not corrupted and no batch is written again.
**Needed.** A check, with the page-tracking fault file system and on each platform (on Linux, with a device that
fails writes on demand), that each rewrite reaches the disk. If it does not, the proposed fallbacks are: for the
manifest, rewrite it to a new file and rename it; for the log, copy the kept batches to a new segment and rename it;
for a seal, the same copy before the rename to `.kvl`.
To be settled on each platform. Roadmap 4.8 did not settle it: the check needs a device that fails writes on demand
(on Linux, for example a device-mapper target that fails or drops writes) or power cuts, which its timing runs do not
give (`docs/measurements/4.8-commit-latency.md`). A hosted Linux runner with root could build such a device-mapper
target; gate 4.9 will try that, with the owner's run on the target hardware.
**Gate 4.9** keeps (b) open until a trial passes. The `fault-trials` workflow (`.github/workflows/fault-trials.yml`,
run by hand and weekly) with `scripts/trial-rewrite.sh` and the example `log/examples/fault_trial.rs` checks it on
ext4 and XFS on a loop device: a device-mapper `error` target is swapped in over exactly the file's extents, so a
sync fails while the file system stays writable; a control arm syncs again without writing, a treatment arm writes
the same bytes again and syncs, and the device is read directly in both. The engine is then checked the same way
for the last batch, the mark slot and the manifest record: the writer stops at the fault, the start writes the bytes
again, and every acknowledged position reads back after a remount (mark synced at every commit, a new boot id per
remount). If the treatment arm passes on both file systems, (b) is settled for Linux as written; if it fails on
either, the fallbacks above are adopted.
**Settled on Linux at gate 4.9** by the third run of that workflow (`docs/measurements/4.9-fault-trials.md`): on ext4
and on XFS every arm passed. The control arm showed the behaviour the rewrite rules exist for (Q-87, Q-117): after a
failed write-back a sync alone left the device without the new bytes, as the page was clean; the treatment arm,
writing the same bytes again, made the next sync write them. The engine's start wrote the last batch, the mark slot
and the manifest record again, the device then held each, and every acknowledged position (and the recorded lost
range) read back after a remount. The rewrite rules stand as written; the fallbacks are not needed on Linux. Still
open: owner decision pending on the deferral wording for macOS and Windows, where no device fails writes on demand
without kernel code.

## Q-83 — Node contents after a past-effective change of the staleness limit
**Scenario.** A gauge with an expected interval of 30 s (S = 90 s) stores the 15-minute level and calendar nodes; it
reports every 30 s but falls silent for 5 minutes every hour. A parameter entry committed today sets S to 10 minutes,
effective a week ago (§2.1: a parameter applies to readings with `t` at or after its effective time). The hold of
each reading uses the S in force at its `t` (§5.2): the hold of the reading before each outage grows from 90 s to the
whole gap, so the coverage and integral of every stored node of that week holding an outage change. Since Q-60 no
node is created or deleted by a parameter entry, but §5.6 recomputes only the minutes a batch's facts touch, and a
parameter entry touches no fact: it is not said how the nodes from the effective time are folded again, whether all
at once at commit (a week of nodes, or years of them, some over detail that is not local) or later, and what nodes
and results show meanwhile. Until they are, invariant 5 fails for nodes still folded under the old S unless those
nodes are flagged, and invariant 8 (a rebuild gives the live nodes) fails as well. The same holds for any other
parameter a node depends on: an expected interval that sets S by default, `wrap_at` and `reset_ceiling` on a counter.
**Model.** Every parameter is fixed for the life of a series (no parameter entries), so the case does not arise (see
"Effective-dated parameters" in the table below).
**Needed.** How stored nodes from a past effective time are brought to the parameters now in force, and what holds
until they are.
**Proposal (smallest rule).** Treat the entry as late data (§5.6) for every minute from its effective time on: those
nodes are recomputed from their facts under the parameters now in force, ancestors from their children, and a node
whose facts are not local is flagged `summary_stale` and recomputed when they are fetched (row 29).
**Objection.** As for late data, only results are flagged while the recompute is pending, not nodes, so invariant 5
fails for nodes not yet recomputed; and counting the recompute complete when nodes over detail that is not local are
only flagged conflicts with invariant 8. A rule must say which nodes carry the flag, and when invariant 8 is judged.

## Q-84 — A condition or live state that stays unknown
**Scenario.** Since Q-49 (b), during a restore a condition whose evaluation has no local detail, and live state of a
series whose latest recorded `t` no local fact or stored node holds, are unknown (§2.3, §6 Conditions). A state
series' last fact is in a revision chunk or log segment that step 3 does not bring back (it stays in deep), or in a
file recorded lost (§8 Restore, Failures and end). Its state stays unknown until `restoring` ends; then §2.3's scope
ends and the live state silently becomes none, and the condition false, with no delivery, so invariant 16's
exception is never discharged.
**Model.** Not modelled: nothing is unknown in the model (see the table below).
**Needed.** What ends an unknown state whose fact never becomes local: a fetch on demand for it, a delivery when
`restoring` ends, or a state that stays unknown (and what `resync` then carries).

## Q-86 (b) — No space at a log write before any sync
Found by spike 2.2 (`docs/measurements/2.2-commit-latency.md`). To be settled by roadmap item 4.8 (moved from 4.5); moved again, to gate 4.9, by 4.8 (see the end of the question).
**Scenario.** (a) Free space is above the critical mark at the last check; another process then fills the file
system, and creating the next preallocated segment (§3 Segments) fails with `ENOSPC`, leaving a partial `.open`
file. Every acknowledged batch is durable and nothing has been written to the new file. (b) On a copy-on-write file
system, a write of a batch into preallocated space returns `ENOSPC` before any sync; no acknowledged byte and no
dirty page is lost. Both are write failures on the log (§3 Write or sync failure: read-only, fatal, recovery by
restart), and both are the disk being full (§3 Disk space: ingest waits).
**Spike.** Follows Write or sync failure (`spikes/commit/tests/faults.rs`
`no_space_when_preallocating_the_next_segment_is_fatal_although_nothing_is_lost`,
`no_space_on_a_log_write_is_fatal_and_nothing_is_acknowledged`).
**Needed.** Which rule applies to each case, and what recovery does with a partial `.open` segment that holds no
batch (`format.md` A.4 does not say).
**Proposal.** (a) is Disk space: the partial file is deleted, health is critical, ingest waits, and the creation is
retried when a free-space check passes. (b) stays fatal, as now: on a copy-on-write file system the same shortage can
as well surface at the sync, where the batch's fate is unknown, and the critical mark makes either rare. Recovery
deletes an `.open` segment that holds no complete batch and is shorter than the preallocation or has an invalid
header.
**Settled at gate 3.3.** (a) as proposed: `spec.md` §3 Segments, `format.md` A.4 Creation and the states recovery
may find. (b) stays a write failure (`spec.md` §3 Segments); what remains is to confirm with the page-tracking fault
file system of roadmap 4.5 that no acknowledged byte or dirty page is lost. **Roadmap 4.5** does not confirm it: its
in-memory file system fails a write for lack of space only when a file grows, so a batch written into a preallocated
segment never meets the case; it is confirmed by trials on a copy-on-write file system at roadmap 4.8. **Roadmap 4.8**
ran none: filling a copy-on-write file system needs a small volume of its own, which its timing harness does not
create, and filling the development machine's own disk is not an option. A hosted Linux runner with root could fill
Btrfs on a loop device; gate 4.9 will try that, with the owner's hardware run
(`docs/measurements/4.8-commit-latency.md`).
**Gate 4.9** keeps (b) open until a trial passes. The Btrfs job of the `fault-trials` workflow
(`scripts/trial-no-space.sh`) fills a Btrfs loop device while the writer commits; it records which call failed
(write or sync) and the error kind, which must be storage-full, checks that nothing after the fault is
acknowledged, and that every acknowledged position reads back after a remount with the disk still full and again
after space is freed, when a new batch must commit. A second arm repeats it on a log directory with copy-on-write off
(`chattr +C`). If it passes, (b) stays a write failure as settled at gate 3.3.
**Settled on Linux at gate 4.9** by the first run of that job (`docs/measurements/4.9-fault-trials.md`): with
copy-on-write on, lack of space surfaced at the batch write (storage-full, before any sync), the writer stopped and
acknowledged nothing after it, and every acknowledged position read back after a remount with the disk still full
(the start stays read-only) and after space was freed, when a new batch committed. (b) stays a write failure, as
settled at gate 3.3; the text does not change. With copy-on-write off for the log directory no write failed: the
zero-filled segment filled, and the next creation then waited for space (Q-86 (a)). Still open: owner decision
pending on whether `format.md` A.4 advises copy-on-write off for the log directory (the second arm shows that it
removes the failure), and on the wording for macOS and Windows, where the check is not made.

## Q-90 — Whether a restore record's set-aside report holds health degraded
**Scenario.** Q-38 left clearing the set-aside report to Q-34 (c). Q-34 (c) settled only the loss reports:
`data_lost` holds health degraded until acknowledged and `possible_loss` does not (§11 Health, §8 Acknowledging a
loss). Each restore record's set-aside range (§8 Restore step 1) is derived from the record at every start, so a
restart does not clear it, and `api.md` lists it among the health reasons. After the operator has exported the
set-aside facts and re-ingested what is wanted, nothing ends that report, so a store restored once would report
degraded health for ever; and it is not said whether the report degrades health at all.
**Model.** Not reported (see the table below).
**Needed.** Whether the set-aside report holds health degraded, and if so what ends it (for example
`acknowledge_losses` naming the range's first position).

## Q-92 — The "incomplete" mark on a revised-key index entry
**Scenario.** Since Q-72 an entry of the revised-key index is incomplete when it was started by an unchecked fact or
rebuilt during a restore while an earlier fact of its key was not local (§2.4). Whether an entry is incomplete
depends on which tier held a fact when another arrived, which the log does not record, so a rebuild from the log
(invariant 8, principle 2) cannot give the same mark; it is the same kind of exception as "completed when its chunk
is back" (§8 Restore step 2), but now outside a restore too. Nothing is said to complete an entry outside a restore,
or for a chunk that stays in deep after one: §2.4 says "until the detail is fetched and completes it", but a fetch on
demand for a read is not said to update the index, and a key whose earlier fact is never read stays incomplete for
good, so a replay of its earlier value can revert a correction indefinitely (row 2a).
**Model.** A full in-memory history of every key stands in for the index, so no entry is incomplete outside a
restore (see "Revised-key index outside a restore" in the table below).
**Needed.** Whether the mark is derived state excepted from invariant 8 (and stated as such), and what completes an
entry outside a restore and for detail left in deep.

## Q-93 (b) and (c) — A release name standing for its rules; what else a reader of another release computes
**Scenario.** Part (a) is settled at gate 3.3 by the owner (see Settled): readers never refuse a tzdata release, a
reader computes calendar results with its own release from the UTC data, using a stored calendar node only where its
recorded release is the reader's, and the writer adopts a new host release without a restart. What remains:
(b) The version is a release name: a patched or partial build under the same name, or a vendor's build with other
rules, is trusted as the release the log names, so the derived values of two hosts with the same name can differ.
(c) The ruling covers readers and calendar results. Not said: which rules a writer that cannot write (read-only after
a failed sync, §3) uses for its reads, and how a reader of another release gives the other values that depend on
tzdata (interval starts and displaced readings of calendar-length series, §2.1, §5.4). Nor whether a reader whose
release does not know the site zone or an interval zone answers `QueryError::UnknownZone` (`api.md` Errors) or
computes as the version in force would, given that reads are not to fail because a release changed.
**Model.** The model holds several releases and refuses nothing (see "Adopting a tzdata release" in the table below).
**Needed.** (b) Whether the version must identify the rules (a fingerprint of the zones used) rather than the
release name. (c) Whether a read-only writer reads as a reader does, and whether interval starts in a reader follow
the version in force (named by the log) or the reader's own release.
**Roadmap 5.4 (c).** The durable backend takes local days for calendar results from the zone rules the reference
model embeds (`kvik_model::calendar::Tzdb`), in the writer's process and in a reader process alike, not from the
release `Env::tz` reports: the `TzRelease` seam gives offsets and transitions, and the day spans of §5.7 (a midnight
that does not exist or occurs twice, zone changes) would need a second implementation over it. So `Env::tz` names
the release in the log, but the rules used are the embedded ones whatever it reports; with the default tzdata seam
the two are the same. Needed with (c): whether a reader's "own release" is the seam's (then calendar spans move onto
it) or the build's embedded rules (`kvik/src/durable/windows.rs`).

## Q-94 — Compaction in position order when the next segment is not local or is quarantined
**Scenario.** Since Q-71 compaction takes sealed log segments in position order, and a segment at or below the last
position held by a live compacted file counts as compacted (§7). While compaction is paused at the low mark (§3 Disk
space), fill pressure (§8) moves verified sealed segments to cold or deep, uncompacted ones included; and a segment
can be quarantined (§8 Corrupt block). When compaction resumes, the next segment in position order is not local, or
is quarantined. It is not said whether compaction stalls until the segment is local, fetches it from deep, or skips
it; skipping and compacting a later segment would put the skipped positions below the mark, so they would count as
compacted although no chunk holds them.
**Model.** Compaction takes every sealed segment not yet compacted, wherever it is held: the model reads entries from
any tier.
**Needed.** What compaction does when the next segment in position order is not local or is quarantined.

## Q-96 — A resume replay with a segment missing in the middle, and `resync` per funnel or per consumer
**Scenario.** (a) A consumer resumes after its cursor (§6 Restart and resume) and the log segments after it are local
except one in the middle, moved to deep by fill pressure (§8). §4 Resync and §8 Behind consumers cover a cursor older
than the oldest local log segment; a gap in the middle is not covered: the replay cannot deliver the missing
positions without a download, which §8 forbids for catch-up. (b) §6 Restart and resume has "the funnel then sends
`resync`", one per funnel, while §4 Resync describes one `resync` delivery carrying the state of every funnel of the
consumer; a consumer with several funnels would see one `resync` or several, and Q-61's "forgets the condition
deliveries it has seen" on `resync` depends on which.
**Model.** A consumer whose cursor is too far behind the local log segments receives `resync` alone; a gap in the
middle is not examined. One `resync` delivery carries the state of every funnel in force (`engine::Resync`).
**Needed.** What a resume replay does at a non-local segment in the middle, and whether `resync` is one delivery per
consumer or per funnel.

## Q-111 — A full facts block against the batch limit
Found at the review of roadmap 4.3 (crate `kvik-log`). To be settled at gate 4.9.
**Scenario.** `format.md` A.4 closes a facts block at a plain-encoded size of 8 MiB and lets one call be up to 8 MiB
plain-encoded, "so every batch fits an empty segment"; the common rules limit a batch to 8 MiB of data blocks plus its
commit block. A call that fills one facts block to 8 MiB of plain-encoded rows becomes a block whose body adds the
24-byte prefix and the column directory, and whose header adds 16 bytes: its data blocks exceed 8 MiB, so the batch
is over the limit although the call is within its own.
**Implemented.** A call is measured by its data blocks encoded alone, not by its plain size: an incompressible 8 MiB
call is refused, a compressible larger one accepted.
**Needed.** One of: measure the batch limit in plain-encoded size, as the call and block limits are; or allow a batch
8 MiB plus the per-block overhead (prefix, directory, header and padding of each data block). Related: A.6 lets
consecutive entries share one entries block up to a body of 16 MiB (Q-106 (e)), a split that the 8 MiB batch limit
means is never reached; whichever option is chosen, the entries rule should name the limit that actually applies.
**Gate 4.9** recommends keeping the one hard limit and stating the call limit in the same unit, as implemented: one
call is at most `min(65,536, limits.ingest_queue)` readings and 8 MiB of data blocks when encoded alone, so every
batch fits an empty segment (`EmitError::TooLarge` otherwise, its `bytes` the encoded size); A.6 then says that in a
log segment the 8 MiB batch limit applies before the 16 MiB body limit of A.2. Owner decision pending: the wording in
`api.md` Writing, whose unit changes while the figure does not.

## Q-115 (e), (f), (j), (k), (n) — The commit path: points `spec.md` §3 and `format.md` A.8 and A.13 leave open
Found while building the log writer (roadmap 4.4, crate `kvik-log`: `store`, `writer`, `fs`); (n) recorded at gate
4.9. Parts (a)–(d), (g)–(i), (l) and (m) are settled at gate 4.9 (see Settled). Tests in `log/tests/writer.rs` pin
what is implemented.
**Scenario.** (e) *The low mark.* "5 % of the filesystem" needs the file system's size, which the `Fs` seam (`api.md`,
accepted at gate 3.3) does not give; and on a platform with no free-space source (Windows, A.13) neither mark can be
judged. (f) *The segment index's reach.* A.8's `max_t` for an interval series needs the series' interval length,
which the writer knows only for definitions it committed itself; the registry (roadmap 5.1) holds the rest. (j)
*Windows name durability.* A.13 assigned to roadmap 4.4 the check that a name made durable by flushing the file
survives a power loss on Windows; it needs a Windows machine and power-loss trials. (k) *The interface.* `api.md`
names the waits `QueueFull`, `DiskFull` and `UnsyncedClock` and lists `IngestStats.waited` and commit latency; the
log writer reports `QueueFull`, `DiskCritical` and `NoSegment` (a roll waiting for the segment created ahead) and
measures neither. (n) *Bounds on ingest and on blocked seals.* The ingest queue bounds entries, not bytes: 65,536
one-entry calls of up to 8 MiB each are accepted (facts are small, so this needs entry calls of large definitions).
Blocked seals have no bound: each holds a file handle, is read again every 10 seconds, and adds a segment to every
later start's scan.
**Implemented.** (e) The low mark uses the byte floor alone (`disk_low_bytes`); an unknown free space applies neither
mark, so ingest never waits for the disk there. (f) Since roadmap 5.1 a start takes the interval of every series the
registry file defines (`format.md` A.16), and then of every definition in the unsealed segments; definitions this
writer commits add theirs. The registry file is written at every start and at clean shutdown, so a series lacks its
reach only when the file is missing or invalid, until the next start writes it (`log/src/writer/recovery.rs`). (j) Not done: a cloud runner can
time `FlushFileBuffers` but cannot show whether a device cache is flushed or a name survives a power loss
(`docs/measurements/4.8-commit-latency.md`). Nothing in the writer depends on it. (k) Done by roadmap 5.1: the
interface reports the log's `QueueFull` and `NoSegment` as `Wait::QueueFull` (a roll waiting for its segment holds the
queue as a full queue does) and `DiskCritical` as `Wait::DiskFull`, measures `IngestStats.waited` as the time a call
spends waiting for room in the queue and commit latency from queueing to acknowledgement (percentiles over the last
1,024 calls), and takes batches and syncs from the log (`kvik/src/durable/mod.rs`). (n) Neither is bounded.
**Gate 4.9.** (e) Recommended: the seam returns size and free space (`fn space(&self, path: &Path) ->
io::Result<Space>` with `Space { free, total }` replacing `free_space`; low mark = `max(total × 5 %,
disk_low_bytes)`; Windows stays unsupported, and neither mark applies where the platform gives no free space), in
its own pull request before 5.1 wires the interface; the low mark only pauses compaction, which does not exist
before roadmap 6. Owner decision pending: the seam change. (f) Deferred to 5.1: the registry gives the writer every
series' interval at open; until then `max_t` may be low for series defined before the start, so the index must not
be used for pruning before 5.1. (j) Proposed: A.13 says Windows name durability and the device cache are not
verified, the flushed handle is the rule the engine follows, no durability guarantee is given there, and a trial on
Windows hardware with power cuts is left to roadmap 10.1 and gates no item. Owner decision pending: that wording.
(k) Deferred to 5.1, as the question says. (n) Recommended: a byte bound on the queue (64 MiB of data blocks, a limit
in `api.md`) and an ingest wait at 8 blocked seals (`Wait::SealBlocked`), or at least a health figure for blocked
seals. Owner decision pending: both, as each adds a limit or a wait.
**Needed.** The owner's decisions on (e), (j) and (n), at gate 4.9; (f) and (k) with roadmap 5.1.

## Q-116 (a), (c), (k), (n) — Recovery at a start: points `spec.md` §3 and `format.md` A.14 leave open
Found while building recovery (roadmap 4.5, crate `kvik-log`: `writer::recovery`, `manifest`). Parts (d)–(j), (l),
(o) and (p) are settled at gate 4.9, and (b) and (m) at the review of roadmap 5.2 (see Settled). Tests in
`log/tests/recovery.rs` pin what is implemented.
**Scenario.** (a) *A corrupt manifest record.* §7 fetches it from deep or the mirror path, or rebuilds the manifest
(roadmap 6.3); neither exists yet. (c) *Loss history.* `api.md` reads loss reports "from the manifest and the recovery entries at every start"; reading every
earlier recovery entry means reading every segment that holds one. (k) *No space for the recovery entry's segment.*
A creation that fails for lack of space waits (§3 Segments); at a start that needs a new segment for its entry,
nothing can be written until it exists. (n) *Restart time.* A start reads every unsealed segment whole,
the last sealed one for its trailer, and the sealed ones above the registry file's `through` for their routing
changes (Q-130), against the restart budget of `design.md`.
**Implemented.** (a) The start refuses to open (`StoreError::ManifestCorrupt`). (c) A start reports what it found and
what the manifest holds. Since roadmap 5.1 the interface reads every recovery entry of the kept log at every start and
every reader refresh, as it reads the whole kept log to rebuild the registry anyway: `losses()` lists the manifest's
lost-range records and, for each recovery entry that reports a torn tail or names a consumer whose cursor file was
missing or unreadable, a `PossibleLoss` (`kvik/src/durable/index.rs`). The cost is that of reading the log, which
roadmap 6.x bounds. (k) The start stays read-only; the next start retries. (n) Measured by roadmap 4.8 on macOS:
130–241 ms to open a store of 7–10 segments (446–639 MiB) again, after a clean shutdown with a warm page cache, against the 1 s
budget, and 129–138 ms for two segments holding almost nothing, so the time is mostly reading the unsealed 64 MiB
segments whole. The cloud runners of `bench.yml` (warm cache, not durability-representative) took 0.24–0.68 s for 3–5
segments. On the Linux NVMe target (three runs, warm cache, clean shutdown) a store of 7–10 segments (446–639 MiB)
opened again in 392–742 ms on the same boot and 393–739 ms on another. A cold cache and a crash are still to measure
(`docs/measurements/4.8-commit-latency.md`).
**Gate 4.9.** (a) Deferred to 6.3: the refusal stays until deep fetch or a rebuild exists. (c) Deferred to 5.1: the
interface reads it. (k) Recommended as implemented: `spec.md` §3 step 5 to say that if the
segment cannot be created for lack of space the start stays read-only and reports it, and the next start retries, so
a full disk after a power loss needs space freed and a restart. Owner decision pending: that availability wording.
(n) Open: the owner's run on the Linux NVMe target measured a warm cache only (0.39–0.74 s, inside the budget with
0.26 s to spare at worst); a cold start is still to measure. If a cold start there is over 1 s, a start scans segments
awaiting a seal only up to their last complete batch; independently, a start should fold each batch's content into
the segment's summary while scanning rather than hold it all at once.
**Roadmap 6.3a.** (a) The crate `kvik-files` reads a manifest past its corrupt records (`image::read`), writes a
verified copy back in place or by a rewritten manifest (`rebuild::repair_with_copy`), decides between copy, rebuild
and wait from the answers of the mirror path and deep (`rebuild::CorruptStart`), and rebuilds the manifest from the
local files, final or provisional (`rebuild::rebuild_manifest`); the log reads cause 5 and passes the publish and
sealed-segment records over. The start still refuses (`StoreError::ManifestCorrupt`) until roadmap 6.3b calls these
from step 4 and supplies the local files, the scan's losses and the sink. Points for 6.3b found at the review of
6.3a: the caller of `rebuild::repair_with_copy` must pass a copy of the first record of the corrupt span (a shorter
copy is written over the span's start, the rest left corrupt); `appender::Irreversible` is `Copy` and bound to no
appender or run, so it must be bound to both before anything consumes it; and while a rebuild stays
unacknowledged, each start that is given deep's copy appends it again (the catalogue takes it as a no-op, but the
manifest grows by one record per such start). Open points found: Q-171 to Q-175; Q-171 and Q-174 settled at the review of 6.3a, Q-172 withdrawn (spec §7 step 2 settles it).
**Needed.** (a) with roadmap 6.3 (6.3b), (c) with 5.1; (k) the owner's decision and (n) a cold start on the Linux
target, at gate 4.9.

## Q-118 (c), (f), (k) — The reader process and leases: points `spec.md` §7 and §8 leave open
Found while building the reader process (roadmap 4.6, crate `kvik-log`: `reader`, `lease`). Parts (a), (b), (d),
(e), (g)–(j) and (l) are settled (see Settled). Tests in `log/tests/reader.rs` pin every part.
**Scenario.** (c) *A new generation.* A restore (roadmap 8.3) sets positions aside "also in reader processes"; a
reader that sees the mark's generation change has no restore record to read yet. (f) *Which epoch a refresh takes.*
"Readers take an epoch ... and refresh both": whether a refresh moves to the manifest's current epoch, and whether a
read that only renews keeps the old one. (k) *What removal must honour.* Retired files (§7 Manifest) and tier moves
(§8) do not exist yet.
**Implemented.** (c) Everything taken is void (`VoidCause::Generation`) and the log is read again from its start.
(f) A refresh takes the current epoch; `keep_alive` keeps the epoch unless the lease may have expired. With no files
to retire this is safe; once tree and compacted files exist, whether a refresh may move the epoch under an open
iteration is for roadmap 6.3. (k) `LeaseTable::oldest_live_epoch` is the input for roadmap 6.3 and 8.1; today the
writer deletes only segments that hold no batch.
Not a question: the reset nonce comes from std's OS-seeded hash keys for now; it must come from `Env::entropy`
(`api.md` Environment seams) when the interface wires the log in.
**Needed.** (c) with roadmap 8.3; (f) and (k) with 6.3 (deferred there at gate 4.9).

## Q-123 (a), (c)–(g) — The interface on the durable log: points `spec.md` §2.2, §3 and `api.md` leave open
Found while putting the `kvik` interface on the log (roadmap 5.1, `kvik/src/durable/`). Each part names the reading
implemented for now; `kvik/tests/durable.rs` and `kvik/tests/api.rs` pin it.
**Scenario.** (a) *Stamping before the commit time.* `future` and the second clause of `late` compare `t` with
`received_at`, "host clock at commit" (§2.2), but a call's outcomes, flags included, are decided before it is queued,
and the log takes the batch's receive time when it writes the batch (Q-115 (i)), a few milliseconds later or, under
back-pressure, much later: a reading at the 60 s edge can be flagged against one clock reading and stored with
another. (b) *A call with nothing to record* (settled, see below). A call whose readings are all refused or
duplicates writes no batch; its duplicates may be duplicates of facts queued by an earlier call that is not yet
acknowledged, and if that call's sync then fails, the store turns read-only having answered `Duplicate` for a
reading that is not stored. (c) *The
log's timers and the clock seam.* `api.md` makes every clock a seam; with a manual clock that stands still, the
log's `min_sync_interval` never passes and a second batch is never synced. (d) *Acknowledging a loss.*
`acknowledge_losses` appends an acknowledgement record to the manifest (A.14), which the log's writer does not write
at run time yet. (e) *Counts that are not in the log.* Per-series duplicates, refusals and `outranked_on_arrival`
are not recorded anywhere, and a value recorded without one does not say whether it was a supplied `Absent` or a NaN
(`absent_supplied` against D1). (f) *Definitions and ordering.* A definition resolves after its mark sync (§3 Commit);
a second definition of the same name, or a reading of the new series, that is decided before the first is
acknowledged would rest on an entry that may still be lost. (g) *The id floor with both carriers unreadable.* A registry file that
cannot be read is passed over (`format.md` A.16: the log alone suffices). A definition lost from the log (synced bytes
damaged) is then remembered by the mark's `max_series_id` and by the registry file; if the mark is also unreadable,
neither carries its id. What still protects it: the id floor of every recovery entry in the kept log (field 46, the
floor at that start), every series id in a kept fact (the series' orphaned facts), and a definition still readable in
the torn tail. A gap remains for a definition written after the last recovery entry, with no kept fact of its series:
its id can be assigned again. This is the residual §2.1 already accepts for a disk that loses synced data ("an id
defined only in the lost bytes can be assigned again"), here reached by damage to three files.
**Implemented.** (a) The wall clock read when the call is decided stamps the flags; the stored `received_at` is the
log's. (b) Settled at the review of roadmap 5.1, as the existing promise that an acknowledgement means durable
requires: a duplicate of a reading queued by an earlier call that is not yet acknowledged resolves only when that
call does, with its result (the call's other outcomes are final at once); an outcome that rests only on facts already
acknowledged resolves at once. A call with readings to record resolves with its own batch, which is acknowledged after
every earlier batch, so its duplicates need nothing more (`api.md` Writing; `kvik/tests/durable.rs`,
`a_duplicate_of_a_queued_reading_waits_for_the_original`). (c) The log's timers run on the process's monotonic
clock; `received_at` comes from `Env::clock`. (d) Acknowledgements are held in memory until the store closes. (e)
Recorded, late and future facts are counted from the log at every open; the others count since the store was opened.
(f) Calls that append an entry other than facts hold the decision lock until their mark sync returns, blocking the
calling thread; readings are decided against acknowledged definitions only. An `emit` that waits for the clock to be
synchronised looks again every millisecond with `std::thread::sleep` on the polling thread, as there is no event to
wait for. (g) Accepted for now as the §2.1 residual; nothing further is done.
Roadmap 6.3a builds the run-time manifest appender (`kvik-files`, `appender::Appender`) that (d) needs; the
interface's use of it is roadmap 6.3b.
**Needed.** A rule for each other part: (a) whether a batch's receive time may be taken before it is queued, or flags
decided at write time on the writer thread; (c)
whether the seam gains a monotonic clock for the engine's timers or tests take `min_sync_interval` 0; (d) the run-time
manifest append (with roadmap 6.3 at the latest); (e) whether these counts must survive a restart, which would need
them recorded; (f) whether a definition, and an emit waiting for the clock, may wait without blocking the caller's thread; (g)
whether the residual is accepted. To be settled at gate 5.6.

## Q-140 — Live staleness of an observed series in a reader process
**Scenario.** S = 3 s. The writer commits a reading for `t` = −2 s at 0 s; at 2 s a reader in another process asks
`live()`. The writer's own reader answers fresh (2 s since the commit, §5.4 monotonic rule); the reader process
cannot, since the commit instant on the writer's monotonic clock is in no file (Q-16 objection 5). The same holds for
the writer's own process after a restart, before its next refreshing commit (Q-16 (d)).
**Implemented (roadmap 5.4).** A reader process, and a writer before its first refreshing commit since it was opened,
measure live staleness in event time, `now − t` of the live-state fact against S: stale at 2 s in the scenario
(4 s since `t`). This is the model backend's fallback after a restart. A reader process keeps its own window clock
`now` from its open (max(wall clock, previous value), advanced by the monotonic clock), so it can also differ from
the writer's after the wall clock stepped back before the reader opened. Every other result (windows, buckets,
calendar results) is a function of the facts and `now`, and agrees (`kvik/tests/windows.rs`).
**Needed.** Whether `live().stale` in a reader process may differ from the writer's, or the refresh instant must be
made a function of the log (with Q-16), or the reader must say it cannot tell (an `Unknown` staleness, an `api.md`
change).

## Q-141 — A fact's `received_at` after the wall clock steps back
**Scenario.** A batch commits at wall-clock 12:00:00; the clock is corrected back by 1 s; the next batch is formed at
11:59:59.5. §2.2 defines `received_at` as the host clock at commit (11:59:59.5) and `commit_time` as
`max(received_at, previous commit_time)` (12:00:00). The `commit_time` row of `format.md` A.7 (commit block body) says
"`received_at` and the commit time are the wall clock when the batch is formed, raised to the previous batch's commit
time", so the log keeps one value, 12:00:00, as both. The model backend shows 11:59:59.5 in the fact's `received_at`;
the durable backend shows 12:00:00. Flags are stamped from the wall clock when the call is decided (Q-123 (a)), so
`future` and `late` agree; only `received_at` differs, and with it anything a client derives from it.
**Implemented.** As the log has it: `received_at` is the batch's commit time. The differential tests clear
`received_at` when the wall clock stepped back (`kvik/tests/windows.rs`).
**Needed.** Which of the two texts holds: a separate `received_at` in the batch (a `format.md` change), or §2.2
reworded to the raised value.

## Q-142 — A reset ceiling off the parts-per-million grid
**Scenario.** A counter is defined with `reset_ceiling` 0.3333335; readings 10,000,000 then 3,333,337. §5.5 with
that ceiling: 3,333,337 is not below 0.3333335 × 10,000,000 = 3,333,335, so the decrease is a regression and
contributes 0. The log keeps the ceiling in whole parts per million (`format.md` A.6 field 11), 333,334 after
rounding, so a store read back from the log would take 3,333,337 < 3,333,340 as a reset contributing 3,333,337: the
increase differs with the arithmetic used.
**Implemented (roadmap 5.4).** Both backends refuse at definition a ceiling between 0 and 1 that does not survive
the round trip through parts per million (`DefineError::Invalid` naming `counter`); every ceiling of at most six
decimal places is kept exactly (`kvik/tests/api.rs`, `a_reset_ceiling_off_the_parts_per_million_grid_is_refused`).
**Needed.** Whether the refusal stands in `spec.md` §5.5 and `api.md` Series, or the field holds the ratio exactly
(the `f64` bits or a rational, a `format.md` change).

## Q-161 — Telling a restore's own claim marker from another restore's
Recorded at roadmap 8.2. To be settled before roadmap 8.3 (8.3a). Roadmap 8.1a laid out the marker (`format.md` A.17)
with the claiming restore's reset nonce, as suggested below; the procedure is still open.
**Scenario.** Restores R1 and R2 of the same bucket both read generation 2 as the highest claimed and try to claim
3 at once. R1's create-if-absent upload of the marker is stored but its reply is lost; R2's is refused. R2 moves on
to 4. Had R2's upload landed first instead, R1's retry would meet a marker for 3 already there. The sink accepts a
repeated `put` when the object has the same size and SHA-256 (so that a retry after a lost reply succeeds); if every
marker has the same content (empty, say), R1 takes R2's marker for its own and both restores own generation 3.
**Model.** Not modelled; a claim is a create-if-absent step that cannot be confused.
**Needed.** Marker content unique to the restore attempt. Suggested: the marker holds the restore's reset stamp
nonce (fresh random, §8 Generations), so a `put` that succeeds proves the marker is this restore's. Other
content is `Exists` only when no attempt can have reached the bucket, and `HashMismatch` after an attempt that may have landed (`api.md` Backup and restore);
either way the restore treats the claim as possibly lost and takes the next generation, except that a `HashMismatch`
with no marker reported by a following `stat` retries the same generation (Q-305).

## Q-163 — Reading the claims again on a bucket whose listings lag
Recorded at roadmap 8.2. To be settled with roadmap 8.3.
**Scenario.** §8 Generations: after uploading its restore record, a restore "reads the claims again" and stops if a
higher generation is claimed. Restore R1 claims 3; R2 claims 4 a moment later. R1 lists the claims, but the
bucket's listing does not yet show R2's marker (some S3-compatible stores list with a delay; the fake injects it,
`a_lagging_listing_does_not_decide_whether_a_key_is_taken`). R1 accepts ingest in generation 3 while R2 does in 4.
**Model.** Not modelled.
**Needed.** A way of reading claims that does not depend on listings. Suggested: read the claims by `stat` of
the markers above one's own generation, in ascending order, until one is absent; a `stat` of a key is consistent
after its write on buckets with read-after-write consistency, which the bucket requirements would then name.

## Q-164 — Interface points deferred to 8.2 that need tiers or restore
Recorded at roadmap 8.2, which settled the sink only. To be settled by roadmap 8.1 and 8.3.
**Scenario.** Three interface points were deferred at gate 3.3 to roadmap 8.2: (a) whether `Reader::facts` fetches
detail that is not local itself (`api-check.md` AC-12), which would put network calls in a `Reader`,
whose calls are blocking file I/O and which owns no thread; (b) what happens to files sealed while no sink is
attached (AC-25); (c) the restore life cycle (AC-24). None can be decided without tiering (8.1) or restore (8.3).
**Model.** Not modelled.
**Needed.** (a) and (b) settled with 8.1, (c) with 8.3. Suggested for (a): `facts` does not fetch; detail not local
is reported as such and fetched by an asynchronous `Store` call on the maintenance thread.

## Q-170 — Durability levels chosen per source

Decided by the owner: a source may choose between the default level, where a reading is acknowledged only after the
sync that covers it, and a faster level, where a reading is acknowledged once it is written (in the operating
system's cache, before the sync). The faster level survives a crash or restart of the process but not a power cut or
a kernel crash; the background sync bounds what such an event can lose. The level is chosen per source (per series
definition), with the default level unless the definition says otherwise.

Scenario: a price feed defined at the faster level emits a reading, gets its acknowledgement within microseconds, and
a consumer acts on it; a power cut 200 ms later removes it. A meter series in the same store, at the default level,
is acknowledged only after its sync and never loses an acknowledged reading.

To settle (design at roadmap 5.5a): the definition parameter and its default; the bound on unsynced data (for example
`max_unsynced`, default 1 s, never beyond the timed sync); what an acknowledgement and `Outcome` say about the level;
a "durable up to" watermark in health and statistics; how a reading at the faster level that a power cut removes is
reported (a reset point and `possible_loss`, as rule 1's reported truncation does), and what consumers that acted on
it receive; batches mixing levels (a batch is acknowledged per reading at its own level; a definition or route
change is always synced); principle 1 and invariant wording ("acknowledged at its level"); whether a per-call
override (wait for the sync on a faster-level series) is offered; and, since rule 1 truncates everything beyond the
mark on the same boot (Q-125), rule 1, principle 3 and invariant 2 rewritten for the faster level so that a process
crash keeps checksum-valid complete batches beyond the mark.

**Settled at roadmap 5.5a** (the owner's decision above; the design below; open parts in Q-250 to Q-253).
- *Parameter.* `durability`, `synced` (default) or `written`, a definition field (`format.md` A.6 field 28),
  changed by a parameter change that takes effect at its position, like the retention class: it changes no fact
  and recovery reads the level from each batch, never from the registry (`spec.md` §2.1). The model fixes it for the
  life of a series, as it fixes every option (Q-60).
- *Batches.* A batch holds one level; a flagged batch (commit block `flags` bit 0, A.7) holds only facts of
  `written` series. A call mixing levels is one unflagged batch, acknowledged at the default level: an
  acknowledgement is per call, and the stricter level never weakens a promise. Definitions, routing and every other
  entry that is not a fact are unflagged and force the mark sync as before (`spec.md` §3 Batches, Commit).
- *Writer order.* A batch is written only after the previous unflagged batch's sync has returned; flagged batches
  need none between them. So beyond a current same-boot mark lie flagged batches and at most one unflagged batch,
  the last; a faster-level reading arriving during a default-level sync waits for it, which keeps delivery in
  position order (invariant 3). Reason: rules 1 to 3 all derive what can lie beyond the mark from this order.
- *Bound.* `max_unsynced`, default 1 s, at most the mark-sync interval (the owner's example); also a sync before a
  seal or roll, at clean shutdown and before a write naming a position above the mark.
- *Acknowledgement and delivery.* A flagged batch is acknowledged, put in live state and delivered once written
  (principle 3 rewritten; invariants 2 and 11 allow it); readers still see only the durable mark. `Outcome::Recorded`
  carries the level; `LogStats` gains `acknowledged` beside `durable` ("durable up to").
- *Rule 1 (Q-125 kept).* On the same boot every complete flagged batch beyond the mark is kept: written again in
  place (a sync vouches only for bytes the start writes, Q-117), synced, then the mark moves past it. Whatever lies
  after the last kept batch is truncated, and a complete batch or any non-zero byte there is still a reset point and
  `possible_loss` (Q-125, Q-320), with K the last kept position. The stale-mark test counts unflagged batches only: a
  complete unflagged batch beyond the mark followed by any complete batch. A process crash therefore loses no
  acknowledged faster-level reading the page cache still holds and costs no reset point (a remount, Q-250, or pages
  evicted after a failed write-back, Q-254, can remove them unreported)
  (`tests/durability.rs` `s3_process_crash_keeps_flagged_batches_beyond_the_mark_with_no_reset_point`,
  `s3_process_crash_keeps_flagged_batches_and_resets_after_an_unflagged_one`,
  `s3_flagged_batches_do_not_make_a_same_boot_mark_stale`).
- *Rule 2.* An invalid region followed by a complete batch stays corruption: flagged batches written with no sync
  between them can reach the disk in any order, and either way the region may have been acknowledged, so a lost
  range (`data_lost`, positions not reused) is right. Not modelled (the model injects no damage beyond the mark);
  the engine's kill loop at 5.5b.
- *Rule 3.* Another boot, or the mark lost or stale: complete batches kept as before; a flagged batch lost by a power
  loss is in the torn tail or left no byte, and the recovery entry is a reset point (`forget_above` K) for consumers
  that acted on it; its `possible_loss` is Q-251. The "last batch written again" rule covers the flagged batches
  directly before the last complete batch, back to the nearest unflagged one
  (`s3_power_loss_removing_flagged_batches_is_a_reset_point_and_possible_loss`; the recovery histories of invariants
  25–27 commit flagged batches, sync the log, keep flagged batches on a same-boot start and lose them in power
  losses, guarded by `recovery_generator_coverage`).
- *Not offered yet:* a per-call wait for the sync (Q-252).


## Q-173 — The operator call `rebuild_manifest`: finality, a readable manifest, several corrupt records
Found while building the manifest rebuild (roadmap 6.3a, `kvik-files` `rebuild::rebuild_manifest`).
**Scenario.** (a) §7 names `rebuild_manifest` as the way out for a process that restarts more often than the wait, but
not whether the rebuild it makes is final (the operator vouching that no copy exists) or provisional (deep has still
not answered). (b) Whether the call may rebuild a manifest with no corrupt record, and what the rebuild record's
fields 1 and 2 then hold. (c) With two corrupt records, fields 1 and 2 name one corrupt record.
**Implemented.** (a) The caller passes the finality; the crate decides nothing. (b) A readable manifest is rebuilt like
any other; fields 1 and 2 are 0. (c) Fields 1 and 2 name the first corrupt record.
**Needed.** (a) and (b) with the call's signature in `api.md` (roadmap 6.3b); proposed: provisional unless deep has
answered, and allowed on a readable manifest. (c) a sentence in A.14 if the first is not what is meant.

## Q-175 — An intermediate output no record and no local file describes
Found at the review of roadmap 6.3a (`kvik-files` `rebuild::plan`).
**Scenario.** Merge A (compacted 1 and 2 into 3) and merge B (3 and 4 into 5) are both corrupt records; a later valid
record retires 5, naming 3 among its inputs. 3 was retired by B and removed locally after its grace period, and no
valid record describes it, so nothing says what 3's own inputs were: 1 and 2, live by the valid records that
published them, cannot be retired. If they are still on disk they are listed beside 5's replacement, which holds
their facts (invariant 14 as the records show it is not broken, as no live file names them; positions are held
twice). Not reachable with one corrupt record.
**Implemented.** Each input no valid record names is followed through its description (a valid record or a local
file) to a fixpoint; one described nowhere is reported (`RebuildPlan::undescribed`) and its inputs stay as the valid
records leave them. An output that names such an input is never discarded for it: the input is not at hand, so the
output is live from its named inputs, even where 1 and 2 are gone too and the output is the only compacted file
holding their facts. `files/tests/rebuild.rs`, `two_corrupt_merges_retire_every_input_down_the_chain`;
`files/tests/rebuild_scenarios.rs`, `q175_an_output_whose_unnamed_input_is_gone_stays_live_and_is_never_discarded`.
**Needed.** A rule: proposed, a compacted file whose position range lies within a live output that names, through
any chain of described files, an undescribed input is retired; or the chunk index of 3 (kind 14, roadmap 6.2), which
carries its inputs, is kept until every file naming 3 is retired. With roadmap 6.3b.

## Q-180 — Stored nodes keep sums, integrals and increases exact, not in `f64`

**Scenario.** A gauge read every 5 s stores 15-minute nodes. A window over a day takes 96 stored nodes in the middle
and computes its edges from facts (§6 Evaluation). With `f64` node fields, the window's sum depends on how the parts
are bracketed: the same day computed from facts alone, or from 1-minute nodes, differs in the last bits, and the
reference model, which is exact, differs again. §5.6 asked for bitwise reproducible stored nodes and compared other
orders within a relative 1e-9; `format-check.md` FC-34 assumed `f64` fields for a series without a decimal scale.
**Settled at the review of roadmap 7.1a:** exact. Every field a fold or merge gives is a sum of `f64` values or their
products with whole nanoseconds, so a dyadic number: ± an odd mantissa × 2^e with e ≥ −1074 and a magnitude below
2^1100 stores it exactly (`format-tree.md` T.3). Every aggregate is then identical to the model's whatever the
bracketing, and invariants 6, 7, 21 and 22 hold exactly (`spec.md` §5.6, §6, §10). `kvik-tree` folds with a dyadic
accumulator (an integer mantissa and a binary exponent, so no gcd runs); a rational appears only after the division of
a window's pro rata interval credit, and window nodes are never stored. Measured in release
(`cargo run --release -p kvik-tree --example fold_cost`, one machine): a gauge read every 5 s folds in 2.0 µs per
reading with rationals and 0.05 µs with the dyadic accumulator; a counter in 0.40 µs and 0.03 µs. Node sizes are the
same either way: 86 B for a 1-minute gauge node of 12 readings, 88 B for its 15-minute node of 180 readings, and 346 B
for a 1-minute node holding one subnormal reading.

## Q-181 — Which zero is the minimum of +0.0 and −0.0

**Scenario.** A gauge reads 0.0 and then −0.0 in one minute. IEEE `minNum` does not specify which zero it returns, nor
does Rust's `f64::min`, so two builds, or two orders of merging, can store different bytes for the same readings,
and rebuild-equals-live (invariant 8) compares bytes. Results do not differ: both zeros give the result 0.
**Settled at the review of roadmap 7.1a:** min and max are taken in IEEE total order, −0.0 below +0.0 (`spec.md`
§5.2, §5.6; `format-tree.md` T.5), so stored bytes depend neither on the order of folding nor on the platform. The
model's `f64::min` may pick the other zero, which compares equal; the tree's tests compare min and max by bit pattern
(`tree/tests/properties.rs`).

## Q-182 — A state name declared twice

**Scenario.** A state series declares the names `["on", "on"]` and reads index 0, then index 1. The durable backend
folds states by index, so it counts a transition and splits the time in state between two keys; the model folds by
name, counting no transition and one key. The two backends' `transitions` and time-in-state then differ for the same
facts.
**Settled at the review of roadmap 7.1a:** the names of a state series are distinct. A definition declaring a name twice
is refused by both backends (`kvik/tests/api.rs` `a_state_name_declared_twice_is_refused`), and a definition entry
carrying one is invalid content in the log (`format.md` A.6 field 13; `log/tests/entries_checks.rs`), so index and name
count the same transitions (`spec.md` §2.1 `value type`).

## Q-190 — The Parquet export schema as a user contract

**Scenario.** A user loads monthly exports into an analytics tool by column name. If a later release renames
`supplied_quality` or turns `generation` signed, their queries break, although no fact changed.
**Draft (roadmap 6.4a).** `docs/export.md` drafts the columns, their Parquet logical types and the file metadata; every
file carries `kvik.export_version` = `0`, and the crate `kvik-export` writes and reads it back (round-trip tests in
`export/tests/`).
**Settled by the owner, 2026-11-16,** as recommended: the schema as drafted is a user contract, and every file
carries `kvik.export_version` = `1`. Adding a column or a metadata key is not a breaking change; renaming, retyping or
removing one raises the version (`docs/export.md` Compatibility; `export/tests/rows.rs`
`file_metadata_names_the_series_view_and_range`).

## Q-191 — The default view of an export, and Parquet for orphaned and set-aside facts

**Scenario.** A versioned price series has three revisions of the 14:00 value. An export in the current view holds
one row for 14:00; one in the history view holds all three, which re-ingest into an empty store (§8 Re-ingest)
needs to reproduce the revisions and the duplicate rule's state.
**Draft (roadmap 6.4a).** `ExportOptions::view` defaults to `Current`, and `History`, `AsOf(P)` and `AsOfTime(T)` are
options. Orphaned and set-aside facts are returned as facts by their own calls (`api.md`), not written to Parquet.
**Settled by the owner, 2026-11-16,** as recommended. (a) The default view is `Current`; `docs/export.md` Options
names `History` as the view to export for re-ingest. (b) Orphaned and set-aside facts get the same schema, one file
per call with rows of several series, added with roadmap 8.3 (settle-by row).

## Q-192 — Decimal values in the export, and the scale in force on `SeriesInfo`

**Scenario.** A series is defined at scale 2 and raised to scale 4 (§2.1 `decimal scale`); facts are stored at
both. One Parquet column has one `DECIMAL(p, s)`.
**Draft (roadmap 6.4a).** `value` is `DECIMAL(19, s)`, `s` the series' scale, each value rescaled exactly (19 digits
hold any `i64` units), and `stored_scale` keeps each fact's stored scale, so the units and stored scale come back
unchanged; a fact stored above `s` is an error, never rounded. `SeriesInfo` exposes only the definition's scale, so the
exporter uses it; decimal series are not stored by either backend yet, and the tests use synthetic rows
(`export/tests/rows.rs` `decimals_round_trip_with_their_stored_scale`,
`a_decimal_stored_above_the_export_scale_is_an_error_not_a_rounding`).
**Settled by the owner, 2026-11-16,** as recommended. (a) `DECIMAL(19, scale in force)` with `stored_scale`. (b) The
scale in force reaches `SeriesInfo` or the export call with decimal series on the durable backend (roadmap 6.4b,
settle-by row); until then the export refuses a fact stored above the definition's scale (`ScaleAboveExport`).

## Q-193 — Fact flags as boolean columns or a bitfield

**Scenario.** A user wants every late reading of last week: `WHERE late` against `WHERE flags & 2 <> 0`.
**Draft (roadmap 6.4a).** Seven `BOOLEAN` columns, named as in §2.2 and §2.5.
**Settled by the owner, 2026-11-16,** as recommended: seven `BOOLEAN` columns (`export/tests/rows.rs`
`each_flag_lands_in_the_column_of_its_name`).

## Q-194 — Quality and named states in the export

**Scenario.** A dashboard filters `quality = 'good'`; a re-ingest needs a named state's index, which a reordered
list of names would change.
**Draft (roadmap 6.4a).** `quality` and `supplied_quality` are strings (`good`, `estimated`, `suspect`, `bad`),
dictionary-encoded; a named state is its index (`INT(16, false)`) in `value` with its declared name in `state_name`, and
the declared names in the metadata.
**Settled by the owner, 2026-11-16,** as recommended: quality as dictionary-encoded strings (the outcome pinned by
`export/tests/rows.rs` `quality_columns_are_dictionary_encoded`; the writer's explicit request matches its default
today); a named state as its index in `value` with `state_name` beside it.

## Q-195 — A `changed_at` of kind `time` beyond the timestamp range; forecast labels

**Scenario.** An adapter sends `changed_at` = 2^63 on a series of `changed_at kind` `time`. §2.2 makes `changed_at` a
`u64` and the store records it; a Parquet timestamp is a signed 64-bit integer and cannot hold it.
**Draft (roadmap 6.4a).** The export fails with `ChangedAtOutOfRange` (`export/tests/rows.rs`
`changed_at_must_match_the_series_versioning_and_fit_a_timestamp`). A forecast series is labelled only by `kvik.timing`
= `forecast`: `t` is the valid time and `changed_at` the issue time (F-13).
**Settled by the owner, 2026-11-16,** as recommended. (a) A `changed_at` above `i64::MAX` on a series of `changed_at
kind` `time` is refused at ingest and counted, as every other time in `format.md` is an `i64`:
`Refusal::ChangedAtOutOfRange` on both backends, checked after `ChangedAtMissing`, `ChangedAtUnexpected` and
`ReviseOnVersioned` (`spec.md` §2.2, `api.md` Errors; `model/tests/rules.rs`
`s2_2_a_time_changed_at_above_i64_max_is_refused`, `kvik/tests/api.rs`
`a_time_changed_at_above_i64_max_is_refused_and_a_version_is_not`). `i64::MAX` itself is accepted; a `changed_at` of
kind `version` stays any `u64`. The export keeps `ChangedAtOutOfRange` for rows that reach it otherwise (`export_rows`,
or a log that holds one, Q-760). (b) Forecast exports keep the column names `t` and `changed_at`; the label
`kvik.timing` = `forecast` says which is the valid time and which the issue time.

## Q-196 — Export file layout, compression and store-level metadata

**Scenario.** A user exports a hundred series: a hundred files, each with a typed `value` column, or one file whose
value column cannot have one type. Later they want to know which store and which durable point an export came from.
**Draft (roadmap 6.4a).** One file per series per call; row groups of at most 131,072 rows; zstd level 1. The metadata
names the series, view and range, not the store: its id, tzdata version, durable position and reset stamp are not
reachable through the public `Reader` without a side effect (`refresh`).
**Settled by the owner, 2026-11-16,** as recommended, and beyond the draft. (a) One file per series per period. (b) Row
groups of at most 131,072 rows; zstd level 1. Beyond the draft: store and site identity in every file's metadata now,
and a partitioned path layout `site=<site>/series=<series>/month=<YYYY-MM>/` for consolidating many sites from a
bucket. Applied as follows (`docs/export.md` Options, Partitioned layout, Metadata; `export/tests/layout.rs`), the
choices below being the implementation's, not the ruling's:
- `kvik.store_id` (32 lower-case hex digits, as `format.md` A.17 writes a store id) is in every file `export_series`
  and `export_partitioned` write: they take it from `Reader::store_id`, an additive read-only item of the interface
  that returns the id read at open (`api.md` Reader processes; `kvik/tests/api.rs`
  `the_store_id_is_the_same_through_every_reader_and_after_a_reopen`), and refuse a different one in the options.
  `kvik.site` is in every partitioned file, which needs a site; `export_series` writes it when given. `export_rows`,
  which has no reader, writes either only when given.
- The rest of the store-level metadata (tzdata version, durable position, reset stamp) is not reachable through the
  public `Reader` without a side effect (`refresh`), so it is not yet in the files; it comes with the export call in
  the store's interface, and the saved point of a continued export (row 7b) after 5.3 (c, settle-by row 8.3).
- A site label is 1 to 64 bytes of `a`–`z`, `0`–`9`, `-` and `_`, starting with a letter: one folder on every file
  system and bucket, case-folding or not, never `.` or `..`, and never read as a number by a Hive-style reader
  (pyarrow and polars read `site=0042` as 42).
- `export_partitioned` writes one file per series per UTC month with a fact, at
  `site=<site>/area=<s1>/device=<s2>/metric=<s3>/month=<YYYY-MM>/<view>.parquet` (the amendment below). Each
  segment is escaped on its own, byte by byte: `a`–`z`, `0`–`9`, `-` and `_` kept, every other byte `%XX` in
  upper-case hex; reversible, one escaped form per segment, and injective under case folding (property tests
  `any_name_round_trips_through_its_segment`, `each_name_segment_round_trips_through_its_own_path_segment`,
  `a_segment_built_from_valid_tokens_is_the_escape_of_its_name`, `a_segment_with_one_invalid_token_does_not_unescape`).
  An all-digit segment stays all digits, so `docs/export.md` tells readers to declare the partition columns as
  strings. The month is the UTC month of `t` for every series, interval series included (an interval goes under
  the month of its start): it needs no tzdata, so it is the same in every release and site zone and months line up
  across sites (`months_cut_at_utc_midnight_on_the_first`, `a_month_holds_its_timestamps_and_starts_where_the_last_ends`
  against a days-from-civil oracle). The file name is the view (`current`, `history`, `as_of-<g>-<s>`,
  `as_of_time-<ns>`), so exports in different views do not replace one another; a month the range cuts is named
  `<view>.partial-<from_ns>_<to_ns>.parquet`, so it never replaces a whole month's file. The files of one call are
  not one cut of the store, and a caller publishes them after the call returns (`docs/export.md` Partitioned
  layout). Long and renamed names: Q-761.

**Amended by the owner, 2026-11-17.** With three-segment names (Q-670), the path splits the series name into its
segments, `site=<site>/area=<segment 1>/device=<segment 2>/metric=<segment 3>/month=<YYYY-MM>/`, so cloud engines can
prune by area, device or metric; file contents and export version 1 unchanged. Applied as follows, the choices being
the implementation's:
- The name is split at `/` (`name_segments`), and each segment escaped as above; a segment holds no `/`, so no
  escaped segment holds `%2F`. Until Q-670 is enforced at definition and alias, `export_partitioned` refuses a series
  whose name has fewer or more than three segments, or an empty one, with `UnsupportedSeries` before it reads a fact
  or calls `open`, rather than filing it elsewhere
  (`a_name_without_exactly_three_non_empty_segments_has_no_partition_folder`,
  `a_name_of_other_than_three_segments_or_with_an_empty_one_is_refused`,
  `a_name_without_three_non_empty_segments_is_refused_at_definition_and_by_the_partitioned_layout_on_both_backends`).
- `area`, `device` and `metric` join `site`, `series` and `month` as names no file column may take (`docs/export.md`
  Compatibility); `series` stays reserved for readers of the first form.
- The reading examples declare all five partition columns as strings; DuckDB, pyarrow and polars were checked on
  exported files, pruned by device and by metric across devices.

## Q-197 — The interval end in an export, and calendar-length interval series

**Scenario.** A half-hourly import series is exported for a billing tool that wants each interval's end, not its
length.
**Draft (roadmap 6.4a).** An interval series has an `interval_end` column, `t + d` (F-14, F-17), and
`kvik.interval_length_ns`. Calendar-length interval series are refused by the export, as neither backend stores them.
**Settled by the owner, 2026-11-16,** as recommended: keep `interval_end`. For calendar lengths each end is taken from
the interval zone under the tzdata version in force at export, recorded in the metadata; built with calendar-length
interval series (roadmap 7.3, settle-by row).

## Q-198 — The backup and restore interfaces ahead of their implementations

Recorded at the interfaces commit of roadmap 8.1 and 8.3, which defines the types, traits and fakes the parallel
items build against (`kvik-backup`, `files/src/{tier,restore}.rs`, `log/src/setaside.rs`,
`log/src/writer/options.rs`). Decided: (a) to (d); (e) and (f) settled at roadmap 8.1a.
**Scenario.** (a) A `put` of a sealed segment is stored but its reply is lost; the store restarts and seals the
same file again. With a random nonce the bytes differ, the sink refuses the repeat (`HashMismatch`), and the
segment can never be uploaded. (b) An operator points two stores at one bucket under different prefixes; a restore
must find the store without being told the prefix. (c) A restore is killed after its record is synced but before
every file is installed; `Store::open` cannot finish it without the sink. (d) The upload driver runs on its own
thread, so the sink it holds must outlive the call that attached it; and a partial restore needs a scope, which a
`Copy` mode cannot hold. (e) Two parallel items must agree on every object key before either is merged. (f) Two
restores race for one generation (Q-161).
**Decided.** (a) The envelope is deterministic: its nonce is derived from the key, the object key and the
plaintext hash (`Envelope::seal`), so a repeated `put` sends identical bytes. (b) There is no key prefix in the
engine: keys start at the store id, a restore finds the store id by listing the bucket, and two store ids fail it
(`RestoreError::SeveralStores`). (c) An unfinished restore is resumed by calling `Store::restore` again (Q-164 (c)).
(d) `attach_backup` and `restore` take `S: BackupSink + 'static`; `RestoreMode` is not `Copy`, defaults to `Full`
and gains `Partial(RestoreScope)`, `NotSupportedYet` until roadmap 8.3f; `RestoreScope` holds only the time range
until a series selection exists.
**Provisional at the interfaces commit.** (e) The key layout of Q-160's suggestion without the prefix, in `backup/src/keys.rs`, the only
place a key is built or parsed; kinds other than log segments under `files/<kind:2 hex>/`. (f) The claim marker is
12 bytes, the generation (`u32`) then the restore's reset-stamp nonce (`u64`), little-endian (Q-161's suggestion).
Both were to be fixed by `format.md` A.17 before anything is uploaded.
**Settled at roadmap 8.1a.** (e) and (f) as above: `format.md` A.17 takes this contract's keys (other kinds under
`files/<kind:2 hex>/`, the probe at `<sid>/probe`) and its 12-byte marker, which repeats neither the store id (in the
key, bound by the envelope) nor a version (in the envelope header). This contract was ruled authoritative over the
earlier A.17 draft (`data/` and `tree/` prefixes, a 40-byte marker with magic, version and store id); nothing
had been uploaded. A.17 was aligned to the contract's types in the same commit: a key that starts with no store id
is ignored, since `Refused::OtherStore` and `RestoreError::SeveralStores` carry store ids; an uploaded record is the
canonical encoding (`Record::encode`, as the manifest port hands out decoded records); the attach probe checks the
key against `<sid>/STORE` (`Refused::WrongKey`). Two mismatches remained, Q-213 and Q-214; roadmap 8.3b settled both.
**Model.** Not modelled.

## Q-202 — A definition waiting for its acknowledgement holds up every other decision

**Scenario.** The ingest queue holds 60,000 readings behind a slow sync. A `define_series`, `alias`, `retire`,
parameter or zone change finds room, queues its entry and waits for its acknowledgement (after its batch's sync and
the forced mark sync) with the durable backend's decision lock held, so that no later decision sees an entry that is
not yet durable and two definitions of one name cannot take two ids. Until then every `emit` and `try_emit` waits
for that lock: `try_emit` does not wait for room in the queue, but it does wait for the definition.
**Now.** No lock is held while a call waits for room (`kvik/tests/durable.rs`
`with_the_queue_full_try_emit_is_busy_at_once_while_other_calls_wait_for_room`); the wait for an entry's
acknowledgement is under the lock. A routing change (roadmap 5.2) is decided with the lock already held, so it
waits for room in a full queue with the lock held too. With the disk critical and the queue full nothing leaves the
queue, so that routing change holds the decision lock, and the read side of the log's lock, until free space
returns: `try_emit` waits for the decision lock instead of returning `Busy(Wait::DiskFull)`, and `shutdown` does not
return, since it needs the write side of the log's lock to stop the log. Calls waiting for room hold no lock and are
not affected. Every entry waited so before definitions, aliases, retirements, parameters and zones moved to waiting
for room with no lock held (`commit_entry`); routing changes still do (`commit_entry_held`). Seen by a probe, not
pinned by a test; which lock `shutdown` waits on is by reading.
**Needed.** A choice between taking an entry into the decision state when it is queued (a failure leaves the store
read-only, so nothing is decided after it) and keeping the present wait as an accepted limit of `try_emit`, measured
with the routing and delivery benchmarks of roadmap 5.5; and for routing changes, waiting for room before taking
the lock (`offer` with `wait_room`, as `commit_entry` does) or a wait that does not hold the log's guard.

## Q-203 — The identity of a loss found while the store is open that grows downward before a start records it

**Scenario.** Three acknowledged batches, ONE, TWO and THREE. THREE is damaged and found by a read: every open process
reports one loss, identity THREE, range THREE to THREE, not recorded. Then TWO, the newest batch read, is damaged:
each process reads again from ONE and grows the loss downward under its first identity, THREE, range TWO to THREE.
The next start scans one region, finds no record of it and records it under the first position of its range, TWO.
§3 Recovery rule 2 says the start records a loss found while open "under the same identity", and step 4 makes the
identity the first position of the range when it was first reported; the start cannot know that position, which no
file holds.
**Now.** Processes that found the loss before the growth report identity THREE. A reader process opened after the
growth and before the start reports identity TWO, range TWO to THREE, not recorded: it scans one region and holds no
earlier report. So two open processes can disagree on the identity before the start, and an acknowledgement given
in the store's process under the identity such a reader shows covers nothing and writes nothing (§8 Acknowledging a
loss). After the next start every process, a reader process opened before it included, reports identity TWO,
recorded, with the same range and commit times (`kvik/tests/durable_losses.rs`
`damage_below_a_found_loss_grows_it_downward_under_its_first_identity` pins the store's process and a reader opened
before the growth). Acknowledgements are held in memory (Q-123 (d)), so none is lost across the start today.
**Needed.** Either an accepted limit (a loss found while open that grows downward before it is recorded may be
recorded under its lower first position; an acknowledgement given under the old identity lapses), or a place that
carries the first-reported identity to the next start. Settled with Q-123 (d), which writes acknowledgement records.

## Q-210 — Telling a bucket that reports no checksum from another sink failure
Recorded at roadmap 8.1a. To be settled by roadmap 8.1c.
**Scenario.** The owner ruled that a bucket reporting no checksums is refused (`BackupRefusal::NoChecksums`). The
probe's `stat` meets such a bucket, and `kvik-sink` fails it with `SinkError::Failed { reason }`, the same variant as
refused credentials or a missing bucket. The driver cannot tell the refusal from a fault it should report as
`BackupError::Sink`.
**Model.** Not modelled.
**Needed.** A distinct report. Suggested: an additive variant `SinkError::ChecksumMissing { key }` in
`kvik-sink-api` (the enum is non-exhaustive), returned by `kvik-sink` where it now returns `Failed` for a missing
SHA-256, and mapped to `NoChecksums` by the probe. Until then the probe reports `Failed` as `BackupError::Sink`.

## Q-211 — An uploaded epoch written again after the last two synced records are damaged
Recorded at roadmap 8.1a. To be settled by roadmap 6.3d.
**Scenario.** Records 1 to 20 are synced; record 19 is uploaded, since 20 was synced after it (§7 Manifest). The
device then damages records 19 and 20 together. No valid record follows them, so a start reads them as a torn tail
(§7 Manifest), truncates after 18 and gives the next record epoch 19 again, with other content. Its upload under
`manifest/<generation>/<epoch>` (`format.md` A.17) meets the uploaded record 19: the key is taken, and the bucket
holds two different records 19 for one generation, one of them in no local manifest.
**Model.** Not modelled.
**Needed.** A rule that a start does not reuse an uploaded epoch. Suggested: a start that drops a tail, with deep
configured, asks deep (`stat`) for the record keys above the last valid epoch in its generation; one that is present
was uploaded, so its successor had been synced and the tail is corrupt, not torn: it is fetched and written back as
§7 repairs a corrupt record, and new epochs continue above the highest present.

## Q-212 — A key already taken by an object that is not the file
Recorded at roadmap 8.1a. To be settled by roadmap 8.1c.
**Scenario.** The envelope is deterministic (`format.md` A.17), so a repeated upload of the same file sends the same
bytes and the sink accepts it. A `put` that fails with `Exists` or `HashMismatch` therefore means the key holds
something else: another writer under `<sid>/`, damage in the bucket before the copy was verified, or Q-211. The key
can never be used for the file (no overwrite, no delete), and the restore derives keys from the file's name.
**Model.** Not modelled.
**Needed.** What the driver does. Suggested: no alternate key in format v1; the file stays local, is never evicted,
and health asks for attention (`BackupStalled` naming the key); the queue continues with other files, but no
manifest record that names the file is uploaded while it is not backed up.

## Q-220 — Where a log segment's plaintext hash lives, what it covers, and when it is computed
Recorded by roadmap 6.3b, while computing the plaintext SHA-256 at seal (`kvik-log` `writer::seal_segment_described`).
**Scenario.** §8 Hashes and the A.14 file description need a log segment's plaintext SHA-256, and A.14 says a file's
header and trailer carry everything a record says about it. The A.9 trailer has 4 reserved bytes, and no hash can sit
inside the bytes it covers. A blocked seal (A.13) runs again from step 1. A seal on `kvik-maint` would still hold up
an acknowledgement if the commit thread waited for it. Restore (Q-290) needs every sealed-segment record to follow the
lost-range records of the start before its seal.
**Decided.** (a) The hash is over every byte of the sealed file, header to end marker, and only the file description
holds it: the trailer is unchanged, so no version rule applies, and a rebuild computes the hash from the file
(`format.md` A.9, A.14; `spec.md` §7 Manifest). (b) It is computed in the pass that computes `file_crc32c`, on
`kvik-maint`; the commit thread never waits for it. (c) Once per seal: a blocked seal's retry reuses the seal blocks
and hash of its first pass. (d) Each `SealedFile` is handed over once (`LogWriter::take_sealed`, `Log::take_sealed`),
after the `.kvl` name and the directory are synced, with the segment's length, which A.14 does not hold. (e) The
sealed-segment record is appended after the seal from that description, and the seal does not wait for it. No
description leaves the writer before the start that preceded it has synced its lost-range records, and a start that
cannot write one is read-only and hands out none, so a record appended from a description has an epoch above them
(`format.md` A.14). `log/tests/seal_hash.rs`: `the_hash_is_sha256_of_the_sealed_files_bytes`,
`a_blocked_seal_reads_and_hashes_its_segment_once`, `the_commit_thread_acknowledges_while_a_seal_is_hashing`;
`log/tests/seal_describe.rs`: `no_description_leaves_the_writer_before_the_starts_lost_range_records`,
`a_start_that_cannot_record_a_loss_is_read_only_and_describes_nothing` (step 4),
`no_description_leaves_the_writer_before_the_gap_below_a_recovery_entry_is_recorded`,
`a_start_that_cannot_record_the_gap_below_a_recovery_entry_describes_nothing` (step 6),
`no_segment_is_described_after_a_fault`.
**Needed.** Roadmap 6.3c appends the record from the descriptions (through `StartOptions::sealed`, which still
refuses until then) with an epoch above the manifest's last, which keeps (e); `From<&SealedFile>` for the `kvik-files`
file description is 6.3c's.

## Q-221 — A read that fails while hashing, and sealed segments no record names
Recorded by roadmap 6.3b with Q-220. The spec names write and sync failures (§3) but no read failure outside them.
**Scenario.** (a) A read of the segment fails during the seal's pass. (b) A start finds sealed segments that no record
names (§7 Manifest: an unlisted sealed log segment is listed again); until records are appended at seal that is
every sealed segment, at every start. (c) Such a segment cannot be read whole, its header disagrees with its name,
its trailer or end marker is not valid, or its `file_crc32c` does not match.
**Decided (the conservative choice in each case).** (a) As before the hash: the seal fails as a write failure on the
log (read-only, fatal health; the next start seals it again from step 1), so no segment takes its `.kvl` name
without a hash. (b) The start does not read them; a segment is named by generation and `file_id`, so a record of
another generation does not list it. A sealed-segment, a publish-and-retire and a restore record list segments (the
restore's field 9, set-aside segments included), so a segment a restore placed is not described and listed again in
the new generation (scenario from the review of roadmap 6.3b: after a restore, earlier generations' segments are named
only by the `0x25`). The writer hands them out one at a time (`LogWriter::take_unlisted`, run by
`writer::describe_sealed` on `kvik-maint`), only while no seal is ready or running and the store is writable, outside
`seals_running` and `sealed_segments`. Once a shutdown begins none is handed out, and one in flight is dropped before
its next file operation (a read is at most 1 MiB), so neither a start nor a shutdown waits for a description beyond
the read under way; one not described when the writer stops is found again at the next start (the owner's ruling at
the review of roadmap 6.3b; scenario: a stop in the first minutes of a run, with every sealed segment still queued,
waited for a whole segment's read). (c) No description
(`DescribeError`): the segment stays unnamed, nothing is written and no fault is set; a damaged file's hash is never
taken as the hash at seal. A failure to open or read it (`DescribeError::Io`: a sharing violation, a descriptor limit,
a permission, an I/O error that may clear) is retried within the run, 10 s after the first failure and then twice as
long each time up to 10 minutes (`DESCRIBE_RETRY_INTERVAL`, `DESCRIBE_RETRY_MAX`), at the writer's timer, so never
in a busy loop: a retry time counts for the writer's next turn only while it could be handed out (none in flight, no
seal ready or running), and what closes that gate wakes the writer when it ends (scenario from the re-check of
roadmap 6.3b: a retry due while another description was reading spun the writer loop); the segment stays unlisted meanwhile, and health names it once the wait reaches its cap (Q-640). Only a failed check
(`NotSealed`, `Invalid`: header, trailer, end marker, `file_crc32c`) is damage: health is degraded by
`Reason::SegmentDamaged { file_id }` (`api.md` Health; `spec.md` §8 Corrupt block, §11) while the writer runs, and
raised again at each start until a description succeeds: a damaged unlisted segment is damaged data, held degraded
like a `data_lost` (Q-124 (a)) but ended by repair, not by an acknowledgement (the owner's rulings on (c), at the
review of roadmap 6.3b; scenario for the retry: an open refused for a sharing error was reported as a damaged segment
for the rest of the run, while the same error on a seal is `SealBlocked` and retried, A.13). `log/tests/seal_hash.rs`:
`a_read_error_while_sealing_is_a_write_failure_on_the_log`; `log/tests/seal_describe.rs`:
`a_start_describes_every_sealed_segment_no_record_names`,
`a_start_skips_every_segment_a_record_lists_by_generation_and_id`,
`no_segment_is_described_while_a_seal_is_ready_or_running`,
`a_running_log_describes_the_unlisted_segments_on_its_maintenance_thread`,
`a_shutdown_drops_a_description_in_flight_before_its_next_read`,
`a_shutdown_that_drops_a_queued_description_still_runs_the_seals_that_follow`,
`a_start_leaves_a_segment_that_fails_its_checks_unnamed_and_degrades_health_naming_it`,
`a_description_that_cannot_read_is_retried_after_the_interval_and_succeeds_once_the_fault_clears`,
`a_due_retry_that_cannot_be_handed_out_does_not_wake_the_writer_at_once`,
`a_description_that_never_opens_is_retried_with_doubling_waits_and_degrades_health_once_the_wait_reaches_the_cap`,
`describe_sealed_refuses_each_check_it_makes`,
`a_sealed_file_with_any_byte_changed_is_never_described`; `files/tests/listed_segments.rs` (the listing against
the records `kvik-files` encodes,
`listed_segments_counts_the_segments_a_restore_record_lists_set_aside_ones_included`).
(d) `kvik_log::writer::Reason` is `#[non_exhaustive]`, like `kvik::Reason`, so a reason the log adds breaks no match
outside the crate. Scenario (the review of roadmap 6.3b): a branch that matched every log reason stopped compiling
once `SegmentDamaged` merged, with no textual conflict to warn of it. `kvik` maps those it knows and reports any other
as `kvik::Reason::Other { reason }`, its debug form, which degrades health: a reason never vanishes from health (the
lead's ruling at the re-check of roadmap 6.3b; scenario: a reason added to the log but not to `kvik`'s mapping would
otherwise be dropped silently). `kvik/src/durable/mod.rs`
`a_log_reason_the_mapping_does_not_name_stays_in_health_with_its_name`, through the `kvik-log` feature
`unmapped-reason`, which only `kvik`'s tests enable.
**Needed.** The damaged case handed to §8 Corrupt block (scrub, copy from deep, quarantine), with backup.

## Q-243 — `repair_done` under manual acknowledgement, across a restart
Recorded by roadmap 8.5a.
**Scenario.** `repair.acknowledge = manual`. The application calls `repair_done(L)`: stage 2 ends and L awaits an
operator. With the automatic setting the call appends the closing record; with the manual one nothing is written,
so after a restart L is awaiting re-send again until `repair.resend_wait` passes.
**Model.** As described: under manual acknowledgement `repair_done` is held in memory only
(`tests/self_repair.rs` `s8_self_repair_manual_waits_for_an_operator`).
**Needed.** Whether `repair_done` under manual acknowledgement writes a record (a further `0x24` outcome, "re-send
ended, awaiting an operator") so that the state survives a restart. Proposed: no record; the application calls it
again, and the wait bounds the state either way.

## Q-249 — A torn closing record of a loss restored in full
Recorded by roadmap 8.5a.
**Scenario.** A copy is swapped in and the closing record "restored in full" is torn by a power loss. §8 Self-repair
says the loss is closed again with the same outcome. For "re-send ended" the next start sees the wait passed and
closes it again; for "restored in full" the start must tell that the swap happened (the swap record survived) and
count the positions again.
**Now.** The model writes manifest records atomically and cannot show it.
**Needed.** The rule a start applies: a recorded open loss all of whose positions a live file holds is closed as
restored in full with that file's count, before any re-send request is offered. It must also tell "restored in full"
from "lost again": a lost-range record written after the closing record opens the loss again (§8 Self-repair), so a
torn closing record must not be given again over a later re-loss.

## Q-250 — Unsynced faster-level batches that a remount within the boot removes
Recorded by roadmap 5.5a. **Needs the owner's ruling** (it changes what the faster level promises).
**Scenario.** A price series is at the `written` level. The writer syncs at 10:00:00.000 and the mark moves to
position 100. Batches 101 to 140 are written, acknowledged and delivered within the next 400 ms, and a consumer acts
on 120. The store's file system is unmounted and mounted again within the boot (a USB disk unplugged, a container
volume detached): the page cache is gone and none of 101 to 140 reached the disk. The next start has the same boot id
and a readable mark at 100 with nothing beyond it: rule 1 sees a clean process crash, writes no recovery entry, and
gives 101 onwards to new readings. The consumer that acted on 120 receives no reset point and no `possible_loss` is
listed: a silent loss and a reused position, which principle 5 forbids. Syncing the mark at every commit, the
existing advice for remountable media (Q-125), does not help: the batches were never synced at all. (b) Partial
write-back: before the remount the cache wrote back 101 to 110 and none of 111 to 140. Rule 1 keeps 101 to 110 as
acknowledged flagged batches and finds nothing after them, so 111 onwards are given again, also with no report.
**Now.** Stated as open in principle 5, rule 1's accepted limit and invariant 10; no rule invented. The model pins
the gap (`tests/open_questions.rs` `q250_a_remount_removing_unsynced_flagged_batches_gives_their_positions_again`,
`q250_a_remount_keeping_part_of_the_flagged_batches_gives_the_rest_again`), and the reuse property of the recovery
histories excludes only remounts that lose a flagged batch. Q-254 is the same failure with no remount.
**Options.** (a) Accept and document: the `written` level is not for media that may be remounted within a boot, and
opening such a store with a `written` series is the operator's choice. (b) Before acknowledging the first flagged
batch after a sync, sync a mark update that says flagged batches may follow (one sync per `max_unsynced` window, not
per batch); a same-boot start that finds the flag and nothing beyond the mark writes a reset point with a
`possible_loss`. This costs one mark sync of latency for the first faster-level reading of each window. (c) Refuse
the `written` level unless `log.mark_sync` is `EveryCommit` (does not close the gap; listed to rule it out).
Proposed: (b) if the latency is acceptable, else (a). (b) also closes Q-254; (a) does not.

## Q-251 — Which recovery entries report a `possible_loss` for the faster level
Recorded by roadmap 5.5a. To be settled by 5.5b. **OWNER**: (b) and (c) change what health promises.
**Scenario.** (a) A store has a `written` series. A power cut removes flagged batches 101 to 140 entirely (zeros on
disk). After the reboot rule 3 keeps up to 100 and writes an entry with an empty tail and a readable mark, which by
§11 before 5.5a was no `possible_loss`: consumers got a reset point but health listed nothing. (b) The same store
shuts down cleanly and the machine reboots: the entry is the same, though nothing was lost. (c) With the mark's
current slot damaged after a completed rule 3 start, an older slot is read that does not cover that start's entry;
the writer had resumed and acknowledged flagged batches that left no byte, and the entry is adopted.
**Decided at 5.5a (provisional).** A recovery entry written while a series is at the `written` level in the
registry at its `after` (K) is a `possible_loss` whatever its tail (`format.md` A.6 field 48; `spec.md` §3 Recovery rule 3, §11), so
(a) is reported, at the price of (b) being reported too. With the mark missing or unreadable and such a series
in force, an entry is never adopted (a doubtful case costs a second report). Reason: honest without a new mechanism;
the definitions and parameter changes that make a series `written` are synced with the mark before they are
acknowledged, so the registry at K after a reboot always holds them. The model, whose levels are fixed for life,
reads the definitions (`tests/durability.rs`
`s3_a_reboot_entry_is_a_possible_loss_while_a_written_series_is_defined`).
**Open.** (b) A clean-shutdown bit in the mark's reserved bytes, written by the final mark update and cleared by
the next, would let a start after a clean shutdown report nothing. (c) is not covered: a damaged current slot is
not detectable from the slot read; proposed with Q-322's numbering change.
**As built.** Every start after a reboot lists a `possible_loss` while any series is at the `written` level, a
clean shutdown included; (c) is unreported. **Proposed to the owner:** accept (b) until the clean-shutdown bit
removes the false reports; settle (c) with Q-322, until then an accepted limit.

## Q-252 — A per-call wait for the sync on a faster-level series, and the `max_unsynced` default
Recorded by roadmap 5.5a. **Needs the owner's ruling**; to be settled by 5.5c.
**Scenario.** An adapter for a `written` series emits a reading it must not lose (a meter reading at the end of a
billing period). It can only change the series' level (a parameter change, synced) or wait for `LogStats.durable` to
pass the position, which it must poll.
**Now.** No per-call override (Q-170 asked whether to offer one). `max_unsynced` is 1 s, the owner's example in
Q-170, at most the mark-sync interval.
**Proposed.** `Store::sync() -> Result<Position, WriteError>`: resolves once every position acknowledged before the
call is durable, forcing a sync if one is not due; no per-reading flag. And confirmation of the 1 s default.

## Q-253 — The commit-block flag in formerly reserved bytes
Recorded by roadmap 5.5a. To be settled by 5.5b.
**Scenario.** The 5.5a design note asked for "a format change, version bump of A.7". The flag is bit 0 of `flags`,
bytes 44 to 48 of the commit block, which v1 reserved and readers do not test (common rules). A build from before
5.5b that opens a store holding flagged batches reads them as unflagged: on the same boot it would truncate them as
a reset point with a `possible_loss` (reported, never silent) and its stale-mark test may misjudge the mark (rule 3,
which keeps complete batches). A set bit other than bit 0 is critical, like a fact flag bit.
**Decided at 5.5a.** The reserved field, with no major version: no release predates it, every batch written before
holds zero (unflagged), and the golden files stay valid. If a released build must refuse such a store, the
alternative is a new non-skippable commit block type (`0x0E`, same body), which an earlier reader refuses as an
unknown critical block; 5.5b chooses when it writes the flag.
**OWNER.** As built: a build from before 5.5b reads a flagged batch as unflagged, and `format.md` A.3's "readers do
not test reserved bytes" has this exception. Proposed: accept while no release predates the flag.

## Q-254 — Acknowledged faster-level batches evicted after a failed write-back, then a restart on the same boot
Recorded at the review of roadmap 5.5a. **OWNER** (it decides what the faster level promises across a failed sync).
**Scenario.** A price series is at the `written` level; the mark is at 100. Batches 101 and 102 are written,
acknowledged and delivered, and a consumer acts on 101. The `max_unsynced` sync, or the next default-level batch's,
fails: the engine goes read-only (§3 Write or sync failure). On Linux the failed write-back leaves the pages of 101
and 102 clean (§3 Recovery rule 3, Last batch written again; the 4.9 fault trial saw exactly this), so the kernel
may reclaim them at any time, and a file system that drops them at the error loses them at once. The operator
restarts the process on the same boot. The bytes beyond the mark read back as zeros: rule 1 finds nothing beyond
the mark, writes no recovery entry and gives 101 onwards to new readings. The consumer that acted on 101 gets no
reset point and nothing is listed: a silent loss and a reused position, against principle 5. For a default-level
batch the same eviction is harmless, as it was never acknowledged.
**Why Q-250 (a) does not cover it.** No remount is involved, and every medium can fail a sync: refusing the level on
remountable media leaves the case open. Q-250 (b), a synced marker before the first flagged acknowledgement of each
window, does cover it: the start finds the marker and nothing beyond the mark and writes a reset point.
**Now.** Stated as open wherever the text promised that a process restart loses no faster-level reading (principles
3 and 5, §2.1, §3 Write or sync failure, rule 1, rows 7 and 12, invariant 10, `api.md`); no rule invented. The
model injects the eviction (`Model::evict_failed_write_back`) and pins the gap (`tests/open_questions.rs`
`q254_a_failed_sync_then_eviction_and_a_same_boot_restart_gives_flagged_positions_again`).
**Options.** (a) Q-250 (b): the synced marker per `max_unsynced` window. (b) After a failed sync with flagged batches
beyond the mark, the writer, now read-only, records the failure durably (a mark slot flag written and synced on
another handle, or a small file) before anything else; the next start that finds it writes a reset point with a
`possible_loss` for the range beyond the mark, and a `data_lost` for any of it found missing before a complete
batch. A failed device may refuse that write too, so (b) narrows the case rather than closing it. (c) Refuse the
`written` level on platforms where a failed write-back drops or cleans the pages (Linux today), which leaves it to
development platforms. (d) Accept and document as a limit of the level.
**Proposed.** (a) if Q-250 (b) is chosen; otherwise (b), and (d) for what (b) cannot reach.

## Q-255 — How calls of two levels are cut into batches and acknowledged
Recorded at the review of roadmap 5.5a. **OWNER** (Q-170's note said each reading is acknowledged at its own level).
**Scenario.** (a) One call holds a reading of a `written` series and one of a `synced` series. (b) Calls W1, S1, W2
(W of `written` series only, S otherwise) queue behind a default-level sync. (c) In a store mixing levels under load,
a `written` reading arrives while a default-level batch's sync is in flight.
**As built (spec §3 Batches, Commit; `api.md` Writing).** (a) The call is acknowledged whole at the default level:
an acknowledgement is per call and the stricter level never weakens a promise. (b) The queue is cut by call into runs
of one class: W1, S1 and W2 are three batches, W2 acknowledged after S1's sync, and calls share a batch only within
a class, so default-level calls still group. (c) The `written` reading waits for that sync, so its latency is the
sync's, not a write's; writing flagged batches during a sync would change what rules 1 to 3 can find beyond the mark.
The model commits the queue as one batch, flagged only when every reading is at the `written` level.
**Proposed.** Accept (a) and (b); accept (c) for now and measure the mixed-load latency at 5.5b.

## Q-256 — The level changed at run time by `set_durability`
Recorded at the review of roadmap 5.5a. **OWNER** (the owner's request said "per series definition").
**As built (spec §2.1 `durability`; `api.md` Series).** `set_durability` appends a parameter change that takes effect
at its position, like a change of retention class; it changes no fact and is always synced with the mark before it
is acknowledged. The model fixes the level for the life of a series (Q-60).
**Proposed.** Accept.

## Q-257 — Faster-level readings lost out of order reported as `data_lost`
Recorded at the review of roadmap 5.5a. **OWNER** (it decides which report a known loss of delivered readings gets).
**Scenario.** Flagged batches 101 to 103 are written with no sync between them; a power loss leaves 101 and 103 on
disk and 102 as zeros.
**As built (spec §3 Recovery rule 2).** 102 is a lost range: `data_lost`, health degraded until acknowledged,
positions not reused, a reset point for cursors below it. Not modelled (the model writes back in order); the engine's
kill loop at 5.5b covers it on the same boot and after a reboot.
**Proposed.** Accept: it is a known loss of delivered readings. Alternative: report a range holding only flagged
batches as `possible_loss`.

## Q-258 — Default-level batches after flagged ones truncated by a remount where they were kept before
Recorded at the review of roadmap 5.5a. **OWNER.**
**Scenario.** Under the timed mark sync, flagged batches then one default-level batch are committed after the last
mark sync, and the file system is remounted within the boot. Before 5.5a two batches beyond a same-boot mark made it
stale and rule 3 kept both; now flagged batches do not count, so the default-level batch is the one complete
unflagged batch beyond the kept ones and rule 1 truncates it as a reset point (reported, the accepted limit of
invariant 10). The wording "one batch" still holds; it happens more often in a store mixing levels.
**Proposed.** Accept; `log.mark_sync = EveryCommit` avoids it, as today.

## Q-259 — What sees a faster-level reading before it is durable
Recorded at the review of roadmap 5.5a. **OWNER.**
**As built (principle 3, renamed "React after acknowledgement"; invariants 2 and 11 relaxed).** Consumers,
conditions, live state, the tree and the writer's own queries see a `written` reading once its batch is written;
reader processes, cursor files and derived files see it only once synced. A consumer that sends commands on such a
series can have its reason withdrawn by a reset point, or silently (Q-250, Q-254).
**Proposed.** Confirm.

## Q-260 — A restored generation's first mark and the first start in it
Recorded by roadmap 8.3c. Settled by the owner's acceptance of the decision below.
**Scenario.** A full restore installs its files and calls `init_generation`, which writes the generation's first mark:
generation g, the restore record's reset stamp, no position (file 0, offset 0, last sequence one below the stamp's),
and no boot identity, since the restore names none. The store then opens with `LogWriter::open_with`. Read as an
ordinary mark of another boot, rule 3 would write a recovery entry at the generation's first position, so every full
restore would put ingest one above it, and a reader would see a reset stamp change before anything was written.
**Decided (8.3c).** The model's start (`model/src/engine/recovery.rs` `recover_tail`: "the first start of a store, or
of a restored generation: no mark and nothing of the current generation in the log") and §8 Restore step 1
("Positions of the new generation start at sequence 1") both take ingest at the first position. A start in
generation g above 0 reads that first mark as no mark when nothing of generation g is in the log (no segment of g in use or sealed, no written unused segment, no lost range of g): it
writes the first mark again with its own boot and the same stamp, writes no recovery entry, and the writer resumes at
the stamp's sequence minus one. Generation 0 is unchanged (its first mark is written by its first start, with the
boot). `init_generation` is idempotent: any readable mark of g is left alone, a torn one written again, a mark of
another generation refused (`StoreError::Generation`). Safe because a segment that held an
acknowledged batch of g cannot vanish whole by a crash (its creation syncs the directory before it is taken into
use); damage to it is Q-264. Tests: `log/tests/generation/main.rs`
`a_restored_generation_takes_ingest_at_its_first_position_with_the_restores_stamp`,
`init_generation_writes_the_first_mark_once_and_refuses_another_generations`.
**Settled (owner).** Accepted as decided, matching the model. For the spec text (§8 Restore step 1): the first mark is
written without a boot and is read as no mark by the generation's first start.
**Refined (owner's ruling: Q-125 prevails over this decision).** Scenario: generation 1 with a timed mark sync; the
first start wrote the first mark again with its boot; batches were acknowledged at (1,1)..(1,3) while that mark stayed
the durable one; a power loss; the generation's only segment missing or zeroed; a reboot. The start read that mark as
no mark too (the boot was not compared), took the first start, wrote no recovery entry, skipped the cursor and
registry files, and acknowledged (1,1) again under the restore's stamp; generation 0 writes a recovery entry with a
new stamp there. Ruling: only the mark `init_generation` wrote, with no boot, is read as no mark. The mark the first
start writes carries its boot and is read as any mark, so every later start follows generation 0's rules: a recovery
entry with a new reset stamp, F from every source, a `data_lost` from the generation's first position up to what a
source names (Q-264), and no position given again under the unchanged stamp. Reason: a consumer or reader that took a
position must see a reset before that position is given again, and "the first mark counts as no mark" must not
suppress the report Q-264 owes. Nothing is acknowledged while the boot-less mark is the durable one, since the first
start syncs its own mark before it takes ingest. With no sealed segment of its own, a start counts nothing below the
generation's first position as lost (`plan`'s `kept_last`). Where no other source names a lost position, the mark synced at the segment's first batch does (Q-630). Tests:
`log/tests/generation/first_start.rs`
`losing_the_only_segment_before_the_timed_mark_sync_is_data_lost_from_the_anchor_in_every_generation`,
`a_cursor_past_the_lost_only_segment_gives_data_lost_from_the_generations_first_position`,
`a_generation_starting_above_1_loses_nothing_below_its_first_position_after_its_first_start`,
`a_first_mark_with_another_stamp_is_read_as_a_mark` and, for a crash at each operation of `init_generation` and the
first start, `a_crash_at_every_operation_of_init_generation_and_the_first_start`.

## Q-261 — Files of another generation at a start
Recorded by roadmap 8.3c. To be settled by roadmap 8.3.
**Scenario.** A restored directory holds sealed segments of generation 0 and the writer runs in generation 1. Sorted
by name, the last sealed segment is generation 0's: taken as the chain's anchor it seeds the expected position and K
from generation 0's sequences, makes the Q-116 (j) check see a sealed position above the mark at every start (a
stale mark, a recovery entry each time), and the first start is never seen as one. A restore's `Lost` step records
lost ranges of generation 0 in the manifest, which the start compared by bare sequence with its own.
**Decided (8.3c).** The chain, the last sealed segment, an empty log and "no sealed segment" are of the start's own
generation only; sealed segments of earlier generations are read only by the scan of Q-262. A mark of another
generation, an unsealed segment of another generation, or a sealed segment of a later generation refuses the start
(`StoreError::Generation { found, start }`): the caller passed the wrong generation, or the directory was not
initialised (an unfinished restore is resumed with `restore` again, Q-164 (c)). Lost ranges are compared and extended by
whole positions, and losses found are recorded in the start's generation, with the record keyed by it. Tests:
`log/tests/generation/main.rs` `earlier_generations_sealed_segments_take_no_part_in_the_chain`,
`a_start_refuses_a_mark_or_segment_of_another_generation`,
`a_loss_in_a_restored_generation_is_recorded_in_it_beside_one_of_generation_0`.

## Q-262 — The writer's scans in a restored generation: set-aside batches and the id floor
Recorded by roadmap 8.3c. Settled by the owner's acceptance of the decision below.
**Scenario.** The first mark carries no id floor and a restore installs no registry file, so the first start of a
restored generation would take id floor 0 and let a new series take an id the restored log, or a set-aside definition
(§8 Restore step 1: its id stays reserved), already holds. The routing changes and interval lengths the start takes
from sealed segments above the registry file's `through` (Q-130, Q-115 (f)) were read from generation 0 only.
**Decided (8.3c).** The start's sealed scan reads every sealed segment of its generation and earlier ones above the
registry file of its own generation (`through` compared as a position), and with no such file every sealed segment
(the cost of Q-116 (n)). Every batch there counts toward the id floor, facts, definitions and a recovery entry's
floor alike (an id a rule 1 truncation reserved lives only in its entry's floor); a batch set aside (Q-263) counts too, and gives
no consumer and no interval length; the rest give all three. Interval lengths apply in log order between the registry
file's and the unsealed log's. The first start takes this floor into its mark. Accepted by the owner.
**What changes in generation 0** (`log/src/writer/generation.rs` `scan_sealed`, `log/src/writer/recovery.rs`
`start`), three things and no others:
1. The scan of sealed segments above the registry file's `through`, which read routing changes only, now also takes
   every series id a fact or definition there names into the id floor, and every definition's interval length into
   the intervals the writer's segment index uses, applied after the registry file's and before the unsealed log's.
2. A first start (empty log, no readable mark, no lost range) took id floor 0 and no intervals and wrote floor 0 in
   its mark; it now takes both from the registry file, if any, and from that scan, and writes that floor in its mark.
   In generation 0 that scan finds no sealed segment there, so only a registry file can change the result.
3. With no registry file, a sealed segment holding no batch no longer ends the scan (the old test `first <= 0`
   held for it); every sealed segment is read. With a registry file the stop is unchanged (`through` is compared as
   the position `(0, through)`, the same order as the bare sequence in generation 0).
Tests: `log/tests/generation/main.rs`
`set_aside_batches_reserve_their_ids_but_name_no_consumer_and_give_no_interval`,
`f_and_the_id_floor_in_a_restored_generation_come_from_its_own_sources`; `log/tests/generation/first_start.rs`
`a_recovery_entrys_id_floor_in_the_sealed_log_counts_at_the_first_start`,
`the_first_start_writes_the_id_floor_in_its_mark`, `a_fact_in_a_set_aside_batch_reserves_its_id`.

## Q-263 — What is set aside: whole batches, by their first position
Recorded by roadmap 8.3c. To be settled by roadmap 8.3.
**Scenario.** §8 Restore step 1 sets aside positions; the writer's and the reader's scans skip batches.
**Decided (8.3c).** A batch is set aside when its first position is (`kvik_log::scan::set_aside`). A restored end lies
between batches by construction (the end of a gap, of a segment, or of a carried end), so no batch straddles one; one
that did would be served whole. Set-aside batches are skipped before a recovery entry in them is applied.
**Open.** An invalid region inside a set-aside segment is still reported by a reader as corruption; whether such a
report should be dropped (its positions are set aside) is for 8.3d with the health report of set-aside ranges.

## Q-264 — Position 1 as the chain's start in a restored generation
Recorded by roadmap 8.3c. Settled by the owner's ruling.
**Scenario.** Q-321 left "a restored generation, whose first position is not 1" to roadmap 8.3. A restore record's
reset stamp is always at sequence 1 (`files/src/restore.rs` `RestoreRecord::check`; spec §8 Restore step 1:
"Positions of the new generation start at sequence 1"), so that premise does not hold: in a restored generation with
no sealed segment of its own, its lowest unsealed segment damaged, zeroed or removed leaves its positions gone with no
`data_lost`, as in generation 0 before Q-321.
**Settled (owner's ruling).** The premise was wrong; the rule is extended. In any generation with no sealed segment
of its own, the generation's first position anchors the log start: position 1 in generation 0, the restore record's
first position (its reset stamp's sequence) in a restored generation. A lowest segment damaged, zeroed or missing is
then a recorded `data_lost` range from that anchor, as Q-125 and Q-321 have it (`plan`'s `log_first`). Q-321's open
part (an anchor once sealed segments are evicted) applies to every generation. Tests: `log/tests/generation/main.rs`
`a_restored_generations_lowest_segment_damaged_zeroed_or_missing_is_a_recorded_loss` (damaged, zeroed and missing,
same boot and after a reboot), `a_generation_starting_above_1_takes_ingest_at_its_stamps_sequence` (the anchor is
the stamp's sequence, not 1); generation 0 stays pinned by `log/tests/recovery.rs`
`the_lowest_unsealed_segment_damaged_zeroed_or_missing_is_a_recorded_loss`.
**Refined (owner's ruling with Q-260's refinement).** Scenario: the only segment of a restored generation is missing
or zeroed after a power loss that fell before the generation's mark first moved under a timed mark sync, so no chain
remains and the mark names no position. This report is not suppressed by Q-260's first mark: the start writes a
recovery entry with a new stamp, as in generation 0, and a source that names a lost position (a cursor file, the
registry file, a lost range) makes the range from the generation's first position a recorded `data_lost`. Reason:
Q-125's reset and this rule's report apply whatever mark the generation began with. With no such source nothing
named the lost positions in either generation; since Q-630's ruling the mark synced at the segment's first batch
names one, so the loss is `data_lost` from the anchor in both. Tests: `log/tests/generation/first_start.rs`
`losing_the_only_segment_before_the_timed_mark_sync_is_data_lost_from_the_anchor_in_every_generation`,
`a_cursor_past_the_lost_only_segment_gives_data_lost_from_the_generations_first_position`.

## Q-265 — When a writer reads the claims, and what it does on a higher one
Recorded by roadmap 8.3c (a review finding of roadmap 8.1c). To be settled by roadmap 8.1d.
**Scenario.** §8 Generations: a writer that finds a higher generation claimed stops uploading and becomes
read-only. The upload driver never read the claims and had no way to report it: a store whose bucket a restore had
claimed past went on uploading records of its generation that no later restore reads.
**Decided (8.3c).** Before each record `put` the driver reads the claims above the record's generation by `stat`,
in ascending order (claims leave no gap, Q-305, so one `stat` answers when none is higher). On a higher claim the
step fails with the additive `UploadError::Superseded { by }`, `by` the highest claimed, and every later step fails
the same way without touching the bucket, also if the marker later reads as absent. Files may still go up until the
next record is due; they are in no later generation's records, as §8 accepts. The cost is one `stat` per record
uploaded. Not done here: the store maps the failure to read-only and fatal health (8.1d), and nothing reads the
claims while the driver is idle (an idle writer uploads nothing). Tests: `backup/tests/upload_superseded.rs`.

## Q-266 — A full restore retried after a failure past its claim
Recorded by roadmap 8.3c (review finding of 8.3b's fix round). Settled by the owner's ruling for v1.
**Scenario.** A full restore claims generation 1, then fails before its record is written (a transient fault reading
the source records, a lagging listing, Q-304). `Store::restore` is called again and each retry claimed the next
generation, so a bucket with a flaky listing walked the generations, each claim never withdrawn.
**Decided (8.3c).** A `Restorer` keeps the store id, generation and nonce it claimed across failed `prepare` calls; a
retry on the same driver for the same store runs in that generation with that nonce (its marker's) and claims nothing, unless a higher
generation is claimed meanwhile, when it fails with `RestoreError::Superseded { by }` and claims nothing more (the
record would be superseded at once). A drill keeps nothing. Unchanged: a crash or a new process before the record
starts again with the next generation (§8 Restore, row 28a, Q-39 (f)); keeping the claim across that would need a
local note written before the claim, a new file. So `Store::restore` (roadmap 8.3d) closes the finding only if it
retries `prepare` on the same driver; a call that builds a new driver claims the next generation.
**Settled (owner's ruling, v1).** In-memory reuse is accepted. A crash during a restore burns a generation, which is
safe: a claim is never withdrawn and the restore already handles claim gaps (Q-305, Q-163). Follow-up: roadmap 8.3d
retries `prepare` on the same `Restorer` within one `Store::restore`; a durable note of the claim, kept across a
crash, is left for a later version. Tests: `backup/tests/restore_guards.rs`
`a_full_restore_retried_after_a_failure_past_its_claim_keeps_its_generation`,
`a_retry_past_its_claim_after_a_higher_claim_is_superseded_and_claims_nothing_more`.

## Q-267 — The start options 8.3c does not act on
Recorded by roadmap 8.3c. To be settled by roadmap 6.3c.
**Scenario.** `StartOptions` has three fields; `recorder` needs `FileRecorder` (6.3c) and `sealed` needs
`seal_segment_described` (6.3b), neither on main.
**Decided (8.3c).** `open_with` acts on `generation` and still refuses `recorder` and `sealed` with
`OpenError::NotSupportedYet` rather than ignoring them; `recovery::start` takes `&mut StartOptions`, so 6.3c changes
call sites only. Test: `log/tests/interfaces.rs`
`open_with_refuses_the_recorder_and_the_seal_listener_until_their_providers_land`.

## Q-268 — A reader on a restored directory
Recorded by roadmap 8.3c. To be settled by roadmap 8.3.
**Scenario.** A reader read only segments up to the one the mark names. A restored generation's first mark names
file 0, as a drill leaves its directory, and later the mark names a segment of the new generation above every
restored file; the restored segments of earlier generations were then never read.
**Decided (8.3c).** A reader reads every segment of a generation below the mark's whole (they are sealed), whichever
segment the mark names, and skips set-aside batches (Q-263) by the restored ends in `ReaderConfig::set_aside`, which
the caller takes from the latest restore record (§8 Restore step 1). Test: `log/tests/generation/main.rs`
`a_reader_reads_earlier_generations_past_what_is_set_aside`.

## Q-270 — Envelope damage that `EnvelopeError` has no variant for

Recorded by roadmap 8.1b, which implements the envelope (`backup/src/envelope.rs`, `ChunkedXChaCha`). Decided
there, provisionally.
**Scenario.** (a) One bit of an object's header flips in the bucket: the magic or `header_crc32c` no longer
matches. A.17 calls this a damaged object, never `WrongKey` or a version refusal, but `EnvelopeError` (fixed by
the interfaces commit, Q-198) has no damage variant other than `Authentication { chunk }`. (b) The same with valid
CRC but non-zero reserved bytes. (c) An object longer than its header's length gives (an extension). (d) A ranged
read for plaintext past the length, or an inverted range.
**Decided (provisional).** (a) and (b) are `Authentication { chunk: 0 }`: every chunk binds the header as
associated data, so chunk 0 could not authenticate anyway; a restore treats it as damage (retry, then `Lost`,
Q-82). The order of A.17 Reading holds: magic and checksum, then version and algorithm (`Unsupported`), then
reserved bytes, then the key check (`WrongKey`). (c) is `Authentication { chunk: n }`, `n` the chunk count, as for
a chunk that cannot exist; a shorter object is `Truncated`. (d) is `Truncated`, as the test envelope does. A
header whose length needs more than 2^32 chunks is `TooLarge`, before anything is allocated. An empty range opens
to nothing without reading any chunk, and `span` of it is empty. Pinned by `backup/tests/envelope.rs`
(`a_damaged_header_is_damage_never_a_wrong_key_or_a_version_refusal`, `truncation_and_extension_are_refused_by_size_before_any_chunk`,
`a_range_past_the_plaintext_or_inverted_is_refused`). A `Damaged` variant would be clearer; adding one changes the
seam, so it is the lead's call.
**Model.** Not modelled.

## Q-280 — How a rebuilt manifest queues files for upload again

Recorded and decided by roadmap 8.1c.
**Scenario.** Segments 1 to 3 are uploaded and each has a copy-verified record (`0x27`). A record is then found
corrupt and the manifest is rebuilt: the rebuild record comes first and every valid record follows it, carried
with field 100 (`format.md` A.14), the three `0x27` records included. §7 says a rebuilt manifest takes no file as
verified in another tier until it is verified there again. Folded as written, the carried `0x27` records would mark
the files verified again after the rebuild record cleared the marks; ignored, they would leave no copy, and every
file would be uploaded again.
**Decided.** A `0x27` carried into a rebuilt manifest (it holds field 100) sets the file's deep copy (`FileEntry::deep`)
and no verified mark; a `0x27` written since then sets both. The upload queue is every live file not verified in deep
(not `deep == None`). For a queued file with a copy, the driver asks the bucket for the object's reported size and
hash; if they equal the copy, it appends a fresh `0x27` with no upload, and otherwise uploads the file as for one with
no copy (the envelope is deterministic, so the same key takes the same bytes; other content under the key fails,
Q-284). A copy of a corrupt record appended later (`rebuild::resolve`) carries field 100 too, so it is checked the
same way. Reason: the copy is a fact about the bucket that the rebuild does not change, so a restore reading the
rebuilt records still finds it, while eviction and removal wait for the fresh check §7 asks for. The restore plan uses a file's recorded copy (`deep`); "verified in deep" governs the upload queue only (owner's
ruling, see Q-295). Pinned by
`files/src/catalogue.rs` `q280_a_carried_copy_verified_record_sets_the_copy_but_no_verified_mark` and
`backup/tests/upload.rs` `q280_a_rebuilt_manifest_verifies_each_copy_again_without_a_new_upload`,
`q280_a_rebuilt_copy_the_bucket_no_longer_reports_is_uploaded_again`.
**Model.** Not modelled.

## Q-281 — The copy-verified record (`0x27`): layout and fold

Recorded by roadmap 8.1c; field 1 settled by the owner's ruling.
**Scenario.** A.14 reserved `0x27` for a tier move; the backup and restore interfaces (Q-198) use it for the
copy-verified record, which a restore reads to know which files have a copy in deep. §7 Manifest and `format.md`
Part B say every record that names a file repeats its kind, position range and plaintext hash; once eviction lands,
a corrupt publishing record would otherwise leave a file in deep known by reference only.
**Decided.** `0x27` copy verified: 1 the file as a whole file description (A.14), so `CopyVerified::file` is a
`FileDescription` (owner's ruling, before anything is uploaded); 2 the tier `u8` (1 hot, 2 cold, 3 deep); 3 the
object's size `u64`; 4 its SHA-256 as the bucket reported it (32 bytes); each exactly once, other fields below
`0x8000` refused as unsupported. Written as a reserved type, so older readers carry it as a field list; decoded and
encoded strictly. The catalogue refuses one that describes a known file otherwise (`CatalogueError::Conflict`), sets
the deep copy and the verified mark (Q-280 for a carried one), and for a file it does not know changes nothing.
Pinned by `files/tests/records.rs` `a_copy_verified_record_round_trips_through_its_block`,
`the_checks_of_a_copy_verified_record`, `a_block_of_type_0x27_is_decoded_and_encoded_strictly` and
`files/src/catalogue.rs` `q281_a_copy_verified_record_describing_a_file_otherwise_is_refused`.
**Model.** Not modelled.

## Q-282 — The upload queue, the record cursor and which records wait

Recorded and decided by roadmap 8.1c.
**Scenario.** (a) The store restarts after uploading records up to epoch 40; nothing local says so, and a queue file
would be a new durable structure. (b) A compacted file is published at epoch 5 and retired by a merge at epoch 9
before it was uploaded; if record 5 waits for its file, it waits for ever. (c) Settling the last record appends an
empty record, which is then the last; settling that one too never ends. (d) Files sealed while no sink is attached
(Q-164 (b)). (e) Record 5 of 8 is damaged on disk while the store runs, after records 1 to 4 went up; epochs are
not dense, so the driver cannot see the gap itself, and a port that passes over the damaged record lets 6 to 8 go up
with 5 missing from the bucket for good.
**Decided.** (a) No queue file: the file queue is recomputed at each step from the manifest's snapshot (live files not
verified in deep, ascending position), and the record cursor is the highest epoch a listing of the generation's
records shows, read at the driver's first step. A listing that lags only makes the driver `put` records it had
uploaded, with identical bytes, which the sink accepts. (b) A record waits only for the files it publishes or lists
that are still live; a file retired before its upload is not uploaded (the files that replace it hold its facts).
So the files records wait for are exactly the queue. (c) The last record is uploaded only after `settle`; an empty
last record is never settled, so a drained queue appends at most one empty record and the driver is then `Idle`,
also after a restart, since the rule reads only the manifest. (d) They are in the snapshot, so they are queued at the
first step after attach, oldest first (input to Q-164 (b), which roadmap 8.1d settles). Only records at or below the
snapshot's last epoch are uploaded, so a record whose sync failed never goes up. (e) By the owner's authorisation
to change the frozen port: `ManifestPort::records_after` returns an error for a record after `after` that cannot be
read, never skipping it, and `DirectPort` does so; the driver then fails and uploads no later record (§8
Generations). This is the contract the store's own port (roadmap 6.3c) keeps. Pinned through `DirectPort` by
`backup/tests/upload.rs` `a_damaged_record_stops_the_upload_of_those_after_it_through_the_direct_port`.
**Model.** Not modelled.

## Q-283 — The attach probe

Recorded and decided by roadmap 8.1c, applying the owner's ruling on Q-162 (a bucket without conditional writes is
refused).
**Scenario.** §8 refuses a bucket that holds another store's files and requires a bucket that rejects a hash mismatch
and honours create-if-absent; the engine never deletes, so the probe's object stays, and a second attach meets it.
**Decided.** In this order, reads first: (1) the bucket is listed around this store's keys and any key under another
store id refuses it (`OtherStore`; a lagging listing can miss one, so this is best effort); (2) the bucket's `STORE`,
if any, must open under the key (`WrongKey`; an unreadable header is a sink failure). Then, on this store's probe
key: (3) a `put` with a wrong hash must be refused (`NoChecksums` otherwise); (4) the probe content, put with its own
hash, must be reported back with that size and hash (`NoChecksums` otherwise); (5) a `put` of other content must be
refused (`NoConditionalWrites` otherwise). At a later attach `Exists` and `HashMismatch` both count as refusals in
(3) and (5). A bucket that checks the signed payload hash refuses (3) whatever it does with the checksum header, so
it passes; that is the check the hash rule needs. A sink failure is `Refused::Sink`.
**Model.** Not modelled.

## Q-284 — Upload failures: retries, other content, an object not reported after its `put`

Recorded and decided by roadmap 8.1c; part (c) settled by the owner's rule on Q-82 (an object is lost after 3 failed
lookups over at least 10 minutes while other calls answer), which the owner confirmed applies to the read-back after
an upload.
**Scenario.** (a) Transient faults, a busy sink, and a `put` refused with `HashMismatch` (damaged in transit, or after a
lost reply). (b) Another object already under a file's key. (c) The bucket accepts a `put` and then reports no
object for the key.
**Decided.** (a) `Retry`, with delays doubling from `retry_first` to `retry_max`, reset by the next progress;
`failing_since` is the first failure of the run, which the caller turns into `BackupStalled`. (b) `Failed` (the
bucket is never overwritten; how the engine recovers is Q-85's). Other content reported after a `put` likewise fails,
and so does a `STORE` object other than the sealed local descriptor, found before the first upload.
(c) The driver asks again, by `stat` alone, without a new `put`; a lookup counts only when another call to the bucket
answers in the same step. After 3 counted lookups over at least 10 minutes the object is taken as lost and uploaded
again. Upload never records a loss: the local file is the source. The S3 sink already reads an object back before
its `put` returns, so (c) arises only when an object vanishes between two lookups.
**Model.** Not modelled.

## Q-285 — Uploading files with no local layout yet, and the hash a file is checked by

Recorded by roadmap 8.1c. Open.
**Scenario.** `format.md` A.12 lays out local paths only for log segments; compacted files, tree checkpoints and the
other kinds under `data/` and `tree/` have none yet. A compacted file published by roadmap 6.2 or 6.3 is live and
queued.
**Decided for now.** The driver reads only log segments; a queued file of another kind fails the upload by name
(`UploadError::Io`, unsupported), so the gap is visible in health, never skipped silently. A file is checked before
upload by the SHA-256 of the whole local file against its description's hash (the hash a sealed segment records,
roadmap 6.3b); a mismatch is `UploadError::LocalHash`.
**Needed.** The local path of each kind, from the item that lays it out.
**Model.** Not modelled.

## Q-291 — Recovery-entry loss reports of the generation a restore reads
Recorded by roadmap 8.3a. To be settled by roadmap 8.3d.
**Scenario.** A restore record carries the recovery-entry loss reports of the generations it restores from, so a loss
whose entry the restore sets aside stays listed (§8 Acknowledging a loss). A `possible_loss` (§3 Recovery rule 1) is
held only by its recovery entry, inside a log segment, and the record is written before any file is downloaded
(§8 Restore, Failures and end). A store restores generation 0 to `0:20`; the recovery entry at `0:25`, which reports
a possible loss, is set aside. The plan, made from the records, carries only the reports of the record it read
(`RestoreRecord::entry_losses`), so generation 1's record lists none, and with set-aside positions excluded nothing
reports that loss.
**Model.** Not modelled; the model writes no recovery entries.
**Needed.** Where the source generation's reports come from. Suggested: read from the listed segments once step 1
has installed them (set-aside ones included; they stay local) and reported at every start, as the set-aside range
is; the record keeps carrying the earlier generations' reports.

## Q-292 — A restore record larger than one block
Recorded by roadmap 8.3a, which laid out the restore record. To be settled by roadmap 8.3, before a store can list that many files.
**Scenario.** A restore record lists every file it restores from (§8 Generations), and log segments are never
retired by compaction (§7), so the list only grows: each listed log segment takes 128 bytes (field header, tier,
copy size and hash, file description), so the 16 MiB block body holds about 130,000. A store that seals a segment
every hour reaches that in about fifteen years; compacted files and their inputs make it sooner. The record can
then not be written and the restore cannot proceed.
**Model.** Not modelled; the model's restore record holds no file list.
**Needed.** A form for a longer list: several `0x25` records of one restore, each naming the same stamp, read as
one; or a list of log segments by ranges of ids. Until then such a record fails to encode (`EncodeError::BodyTooLong`).

## Q-294 — Where the model's restore differs from the plan
Recorded by roadmap 8.3a, whose plan is tested against the model (`kvik/tests/restore_plan_model.rs`). To be settled
by roadmap 8.3b, with the restore executor.
**Scenario.** (a) A store defines a series (`0:1`) and ingests `0:2` to `0:6`; the batch at `0:3` is corrupted
below the mark and the next start records it as a lost range; the segment is sealed and uploaded. The model's
restored end is `0:2`: it ends at any missing position. §8 Restore step 1 and the plan give `0:6`, since a recorded
lost range does not end the restored log (checked with `Model::corrupt_open_batch`, `restart`, `upload` and
`restore_from`). (b) A store seals and lists segments but none is uploaded yet;
its records are in the bucket. The model refuses the restore (`Error::EmptyBucket`); §8 Generations refuses only a
bucket with no manifest records, and the plan restores an empty log, restored end `0:0`, everything later set aside.
**Model.** As described; the differential test leaves both cases out by construction.
**Needed.** (a) The model's restored end to skip recorded lost ranges, as the plan does. (b) Whether a restore whose
records list no copied file is refused (the model's rule, stated in the text) or restores an empty generation (the
text as written). Suggested for (b): refuse, with a named error, since nothing would be restored.

## Q-301 — The files a resumed restore fetches
Recorded by roadmap 8.3b.
**Scenario.** A plan returns the restore record and the files to fetch; only the record is durable. A restore killed
after its record resumes and must fetch "the files its restore record lists that steps 1 and 3 bring back and are not
yet local", so that list must follow from the record alone.
**Provisional.** The executor refuses a plan whose files to fetch are not exactly the files its record lists
(`PlanError::Invalid`), and fetches, fresh or resumed, every listed file that is not installed and has no lost
range recorded in the new manifest, ascending by first position. This is exact for a log-only restore, in which every
listed file is a sealed log segment.
**Needed.** With tree checkpoints, chunk indexes and tiers, a rule from the record (its tiers, the checkpoint) to the
files steps 1 and 3 bring back, used by both the plan and a resume.

## Q-302 — The lost range of a listed file the bucket has lost, and files other than log segments
Recorded by roadmap 8.3b.
**Scenario.** §8 Restore, Failures and end: what a lost listed file held is compacted again from the other listed
files, and the positions no other listed file holds become a lost range. A lost-range record also carries a byte
offset and length, a boot id, an id floor and the commit times around the loss, which a restore record does not
give.
**Provisional.** The executor installs only log segments holding positions (a plan listing anything else is
refused: compacted files have no local layout yet, `format.md` A.12 `data/`). A lost segment's lost range is its whole
position range: identity and first its first position, last its last, its `file_id`, offset 0 and length 0, a zero
boot id, id floor 0, no commit times, cause 3 lost in the bucket. It is appended to the new manifest, in a drill too.
**Needed.** The fields for a loss found by a restore, and the split into several ranges once compacted files can hold
part of a lost segment's positions.

## Q-303 — The local restore record file
Recorded by roadmap 8.3b.
**Scenario.** §8 Generations syncs the restore record "in the manifest and in the local restore record file
(`format.md` A.1 kind 12)", and §7 Readers reads restored ends from that file or from deep. Kind 12 is reserved with
no layout, and the committed interface (`RestoreDriver::prepare`) names `MANIFEST` only.
**Provisional.** The executor syncs the record in `MANIFEST` only; a reader process can read it there through
`kvik-files`.
**Needed.** A layout for kind 12 and who writes it, or the spec text without it.

## Q-310 — The delivery budget's p50 at exactly 100 calls a second with the default bound
Found by the roadmap 4.8 run on the Linux NVMe target (`docs/measurements/4.8-commit-latency.md`, Linux NVMe target).
**Scenario.** 100 series at 1 Hz, staggered evenly, so a call of one reading every 10 ms; `WriterConfig::default()`
(`min_sync_interval` 10 ms, mark timed). `design.md` Budgets sets p50 ≤ 5 ms "while syncs are not back to back,
roughly up to 100 synced batches a second". Three runs: p50 6.95, 6.75 and 7.02 ms; p90 11.54, 11.40 and 11.50; p99
12.33, 12.33 and 12.24 (inside 20 ms); 99 syncs a second, 1.0 call per batch. The same load with the bound at 0:
p50 2.36, 0.68 and 2.24 ms. Quiet and 10 × 1 Hz loads pass at the default (p50 2.30–2.52 ms).
**Cause.** The bound counted from the previous sync's start (spec §3 Group commit, `LogWriter::sync_ready_at`).
With calls exactly one interval apart, each late timed wake of the writer (about 0.1 ms here) delays every later
sync, so the wait climbs to the whole interval before two calls share a batch, and latency spreads evenly from the
sync (2.2 ms here) to the sync plus 10 ms: p50 is the sync plus about 5 ms. Only a wake with no lateness avoids it.
`design.md`'s statement that the default adds its wait "only when syncs are back to back" does not hold at the rate
equal to one call per interval. The cause rests on the code and the fit of that spread (predicted p50 / p90 / p99
7.2 / 11.2 / 12.2 ms), not on a trace. The miss is not the bound's average wait on random arrivals: the harness
spaces calls evenly, and random arrivals would give about 73 syncs a second with 1.4 calls per batch (measured 99
and 1.0), p99 about 11 ms at 10 × 1 Hz (measured 2.71–2.79) and p50 about 7 ms on macOS (measured 4.14). So it is
not inherent in the interval and this drive's sync time; option (b) below would remove it. macOS shows a tail of
the same size (p90 12.0 ms) without a budget.
**Options.** (a) Word the p50 budget as applying below 100 synced batches a second at the default, putting this
load with the higher rates. (b) Stop the lateness compounding: after a wait on the bound, count the next interval
from when the sync was due, not when it started; two syncs could then start up to the lateness less than the
interval apart, so spec §3 Group commit's "at least `min_sync_interval` apart" would need rewording, with tests. (c)
A lower default interval moves the edge to a higher rate, at the cost of the host-write cap roadmap 2.2 measured.
**Settled** by an engineering fix, pending the next run on the Linux NVMe target. The writer thread confirms the
cause: it sleeps until `sync_ready_at` with a timed wait and stamps the sync's start when it wakes, so any lateness
moved the next deadline. Two more ways to start late give the same lasting delay at one call per interval, with no
spare slot to recover in: a turn entered late with the bound already passed (a call arriving during the timed mark
sync), and the sync that takes a segment into use at a timed seal, which took a slot of the bound at whatever phase
it fell. A rule correcting only syncs that waited on the bound misses the first; capping the correction at half an
interval leaves a stall over half an interval, and a seal late in the interval, as a lasting delay.
**Rule.** Two clocks. A batch sync is due when the previous one's due time let it start, but at most one interval
before it starts; the next may start once `min_sync_interval` has passed since that due time and half of it since
the previous log sync started, a timed seal's included. The seal's sync spaces the next batch but takes no slot.
So late wakes and stalls are caught up at half-interval spacing and delay later syncs only while the load stays at
the bound (below), under saturation batch syncs keep to a grid one interval apart, any n + 1 consecutive batch syncs
span at least n − 1 intervals, no batch sync starts less than half the interval after any log sync, and a call
during a sync still waits for the next batch. Costs: after a
quiet period a burst's first batch follows the lone first reading after half the interval rather than a whole one;
a seal adds one sync per seal interval beyond the bound and delays at most one call, by under half an interval;
spec §3 Sealing's "counts as the previous log sync for the next batch" now holds for the spacing only.
**At the bound (owner's ruling: accepted, no rule change).** The catch-up is the one interval by which a due time
may trail its start; a late start stays stored in the due time. One extra batch (a stray call between two periodic
ones) spends that interval, and at exactly one call per interval no slot is ever missed to regain it, so every later
call waits for the stored lateness, under one interval. Scenario: one call every 10 ms, call 10 starting 3 ms late,
one extra call 6 ms after call 30: calls 11 to 29 wait 20 µs, every call after the extra one waits about 3.1 ms.
Any rule that bounds the long-run rate at 1/interval has no slack when the load runs at exactly that rate, so an
extra call can delay later calls until the load drops below the bound; at a 10.1 ms period the same history drains
within 20 calls. Pinned by
`at_the_bound_a_stray_call_after_a_late_start_delays_later_calls_until_the_load_drops`, removed with the adaptive
interval when the sync budget replaced it (Q-980).
**Rejected.** (a) would have kept a known miss at the named load; (c) only moves the edge and spends host writes.
Anchoring to the oldest call's arrival avoids the burst cost but needs a monotonic stamp on every call and a change
to the call source, for one batch after a quiet period.
**Tests.** `log/tests/cadence.rs`: late wakes, stalls of 3, 8 and 30 ms, a seal early and late in the interval
after a late start, saturation with late wakes and stalls, a burst after a quiet period, a stray call at and just
below the bound. macOS before and after:
`docs/measurements/4.8-commit-latency.md`, the dated note under Results: macOS.
**Superseded** (2026-11-18) by Q-980: the two-clock grid, its half-interval spacing after any log sync and the
stored lateness accepted "at the bound" are gone with the schedule they paced. The sync budget refills by time, so a
late wake or a stall leaves the refill in the budget and stores no delay; the rate bound is kept (any window holds at
most its length over the least interval, plus four, batch syncs). `cadence.rs` is replaced by the scenarios of
`log/tests/pacing.rs` (`late_wakes_and_stalled_turns_delay_no_later_call`,
`a_timed_seal_delays_only_a_call_arriving_during_its_sync_and_by_no_more_than_it`,
`a_spent_budget_holds_arrivals_for_the_next_refill_and_they_form_one_batch`,
`under_load_batch_syncs_run_at_the_interval_or_as_the_previous_one_returns`,
`a_25_ms_stall_holds_the_ceiling_until_the_estimate_falls_and_the_floor_returns_within_45_syncs`,
`the_calls_that_arrive_during_a_sync_form_one_batch_as_it_returns_while_the_budget_holds`).

## Q-320 — Which bytes beyond a same-boot mark make a reset point

**Scenario.** Three inputs the kill loop found, rare enough that a default run fails only now and then, as each run
takes a fresh seed (`log/tests/recovery.rs`, one test each). (a) The writer takes the segment created ahead into use
and stops before its sync; a remount within the boot keeps part of the first batch but not the header rewrite, so the
segment is not in use (`base_sequence` 0, no complete batch) and holds non-zero bytes after its header
(`kill_loop_a_torn_batch_in_a_segment_never_taken_into_use_is_a_reset_point`). The start deleted it with no recovery
entry and no report. (b) Seal blocks that hold lie after the last batch of the segment the mark still names, the next
segment in use by its header alone (`kill_loop_seal_blocks_beyond_a_same_boot_mark_are_not_a_reset`). (c) A rule 1
start writes its entry in a new segment and stops before zeroing the batch beyond the mark; the next start finds the
mark stale, adopts the entry and drops that batch
(`kill_loop_an_adopted_entry_in_a_later_segment_drops_the_batch_before_it`).
**Settled by the owner's ruling Q-125, which covers a store with no readable mark too:** any non-zero byte beyond the
mark is reported before it is deleted. (a) The bytes after the header of every unused segment beyond the mark's
segment, wherever segments in use lie, or of every unused segment when no mark is readable, are part of a recovery
entry's tail: under rule 1 a reset point with a `possible_loss`, under rule 3 counted in the new entry's tail. With
no readable mark such a store is never a first start; rule 3 writes the entry. The segment is deleted only after an
entry that reports it is synced: the one written, or one adopted. With the mark lost, or its current slot damaged so
that an older slot is read, the writer may have written the segment after an entry the mark read does not cover, so
an entry followed by such a segment in a later file is never adopted, on the rule 1 or the rule 3 path, and a new
entry reports the bytes. A start cut short and then killed or remounted leaves the bytes, or a durable entry naming
them, for the next start
(`a_torn_batch_in_a_segment_never_taken_into_use_is_reported_before_it_is_deleted`,
`a_torn_batch_in_a_segment_never_taken_into_use_is_in_a_reboots_entry`,
`a_torn_batch_in_a_segment_never_taken_into_use_is_reported_with_the_mark_lost`,
`an_entry_with_the_mark_lost_is_not_adopted_over_bytes_in_an_unused_segment_after_it`,
`an_unused_segments_readable_commit_blocks_count_in_the_entry`,
`an_entry_is_not_adopted_over_bytes_in_a_later_unused_segment_with_the_marks_current_slot_damaged`). The same bytes
can be reported twice, each time by an entry that is its own reset point and `possible_loss`: when a start's entry
lies in a file below the segment and the start stops before its mark, the next start cannot tell that entry from one
the writer wrote before the segment, and writes another; and when a start removes the segment and writes its mark,
and a remount undoes the removal before the mark's directory sync, the next start finds the bytes beyond the new mark
and resets again. A second report costs a second `resync` and no position; nothing adds up the bytes of several
reports. A damaged header counts the same way when bytes
follow it; with zeros after it nothing was written and the file is only deleted
(`a_damaged_header_beyond_the_mark_is_a_reset_point_only_with_bytes_after_it`). (b) Valid seal blocks are not lost
data and are sealed again, as step 2 says. (c) The one dropped batch is the adopted entry's reported tail: the kill
loop checks that at most one batch is dropped, that the entry's tail is at least that batch's bytes, and that it
counts a readable commit block. A damaged header and damaged batches in the mark's segment are rule 2 corruption: the
positions up to the mark's are a recorded `data_lost`, with no reset point, before the file is deleted
(`a_damaged_header_and_batch_in_the_marks_segment_are_a_recorded_loss_at_every_cut`); below the mark's segment, see
Q-321. The kill loop also
remounts after a start cut short, with the mark synced at every commit. `spec.md` §3 Recovery rules 1 and 3,
Adoption and the first start, and `format.md` A.4, say so. The reference model has no headers or seal blocks; its
torn tail is a reset under rule 1 already, and its first start now also requires an empty invalid tail, a state its
interface cannot reach (a definition is committed before any reading), so no test pins it.

## Q-321 — The lowest unsealed segment damaged, zeroed or missing, with no sealed segment below it

**Scenario.** Segment 1 holds acknowledged batches and awaits its seal; no segment is sealed yet, and the mark names a
later segment. Media damage hits segment 1's header and every batch, or zeroes the file, or the file is gone. A start
found segment 1 not in use (or not at all) and began the chain at the next segment's own base, so no gap was seen:
the positions vanished with no `data_lost`, no manifest record and no recovery entry, on the same boot or after a
reboot.
**Settled after the review of Q-320:** with no sealed segment, the chain of generation 0 starts at position 1, so the
positions below the first segment kept are a gap, reported as `data_lost` and recorded as a lost range (`spec.md` §3
Recovery rule 2; `format.md` A.4). This covers a damaged, a zeroed and a missing file alike, and a start cut at any
operation, since the gap is read from the segments that remain
(`the_lowest_unsealed_segment_damaged_zeroed_or_missing_is_a_recorded_loss`). When that segment holds the mark, the
mark's position already gave the range (`the_marks_only_segment_damaged_zeroed_or_missing_is_a_recorded_loss`). A
restored generation's log starts at its own first position (Q-264). The reference model injects no damage to
headers or whole segments.
**Open part (settle by 8.1).** Position 1 as the start holds only while sealed segments stay in the log directory. If
every sealed segment left it, the start would report a false `data_lost` from position 1 up to the first segment
kept. Nothing evicts sealed segments today, so this is not live; eviction must give the log start another anchor
(for example the last evicted segment's last position, recorded before it leaves) before the last sealed segment
leaves the log directory.

## Q-322 — A torn mark update that makes a damaged older slot valid again

**Scenario.** Found by the kill loop's sweep (`kill_loop_keeps_every_acknowledged_entry`, "a complete batch above
position 3"; about once in 9,000 runs of 5,000 cases). Slot 1 of the mark holds counter 4 of boot 2; both slots lose
their first 8 bytes (the magic). After a reboot the start finds no valid slot and writes counter 1 into slot 0. The
writer's next update goes to slot 1, and a remount within the boot keeps only its first 8 bytes, which equal the old
magic: slot 1 is valid again, with counter 4, and wins. The next start reads a mark of the previous boot, takes
rule 3, and keeps the unacknowledged batch beyond the real mark with no reset owed for it.
**Now.** Not changed (it needs a change to how a start numbers the slots when none is valid). No acknowledged entry
is lost in the case found. Not yet checked: whether a slot of the same boot brought back this way can make rule 1
truncate after an older position.
**Needed.** Proposed: with no valid slot, a start takes its counter above every counter field it can read in either
slot, valid or not, or writes both slots.

## Q-341 — The attach probe at `<sid>/probe`: its wrong-hash put there, and an attach after a full restore
Recorded when roadmap 8.3b was stacked on 8.1c. To be settled by roadmap 8.1d.
**Scenario.** (a) `format.md` A.17 Probe has `attach_backup` put the checksum probe, with a wrong SHA-256, to a fresh
`<sid>/probe/<nonce>`; the attach probe (`backup/src/upload/probe.rs`, Q-283) puts it to `<sid>/probe`, the
conditional-write probe's key, and never builds `Keys::checksum_probe`. That is the folding Q-213 warned of: a bucket
that does not check stores the wrong-hash object at `<sid>/probe` and is refused; at the next attach the key is
present, the wrong-hash `put` fails with `Exists`, which cannot be told from a checked hash, and only the later
checks of the reported hash stand between the refused bucket and its acceptance. The restore's probe
(`backup/src/restore/probe.rs`) follows A.17. (b) A full restore's probe leaves at `<sid>/probe` an envelope of 16
random bytes, for ever. The restored store then attaches to the bucket it was restored from: the attach probe puts
its own fixed probe to that key with the right hash, the sink refuses it with `Exists`, and the attach fails with
`Refused::Sink(Exists)`. This is the normal path of every restored store. The other order passes: a restore's probe
accepts the object an earlier attach left.
**Model.** Not modelled.
**Needed.** One probe for both callers: the attach probe moved to A.17's form (the shared code in
`restore/probe.rs`: an existing `<sid>/probe` must fail a second put and be reported unchanged, then the checksum
probe under a fresh `<sid>/probe/<nonce>`), with tests of a second attach to a bucket refused once for
`NoChecksums` and of an attach after a full restore; or A.17 amended to the attach probe's form with the reason.
Neither path is reachable before 8.1d: `Store::attach_backup` and `Store::restore` return `NotSupportedYet`.

## Q-342 — `backed_up_through` in a restored generation
Recorded when roadmap 8.3b was stacked on 8.1c. To be settled by roadmap 8.1d.
**Scenario.** A full restore installs segments 1 to 3 of generation 0 and writes its restore record, whose fold
gives each file a deep copy and the verified mark (8.3a), so the upload queue skips them. The upload cursor counts a
file as copied only from a fresh copy-verified record (`backup/src/upload/driver.rs` `fresh_copy`, `copied_at`;
Q-280), and none is written for a file the queue skips. After the restore record and two later records of
generation 1 are uploaded, the driver reports `backed_up_through: None` with nothing queued, and the figure stays
there until a rebuild carries the restore record and the files are verified again. It under-reports, so nothing is
evicted early.
**Model.** Not modelled.
**Needed.** Suggested: the cursor takes the files a fresh (uncarried) restore record lists as copied at its epoch,
with a test of the figure after a restore; with `BackupStats` (8.1d), which reports it.

## Q-601 — A corruption named by an epoch across restore generations
Recorded at roadmap 8.5a; a technical question, settled by 8.5b.
**Scenario.** A corruption seen names a file by kind, `file_id` and the epoch of its quarantine record, and a
corrupt manifest record by its epoch (`format.md` A.14); `0x25` fields 12 and 13 carry these values into the next
generation. An epoch is unique only within one generation's manifest (records are keyed by generation and epoch),
the value holds no generation, a restore keeps file ids, and no text says which epoch a restore record takes (the
model's takes 1). (a) Generation 0: segment 7 is quarantined at epoch 105, swapped and seen. A restore claims
generation 1 and carries "seen (7, 105)". In generation 1 the restored segment 7 is found damaged and its
quarantine record takes epoch 105: it is born seen and never alerts, against the owner's ruling that every
corruption alerts. The same holds for a corrupt manifest record at the same epoch number. (b) Over generations 0,
1 and 2, a seen "(7, 105)" from generation 0 and an unseen "(7, 105)" from generation 1 are the same 24 bytes, so
fields 12 and 13 ("each named once") cannot hold both. (c) Carried values hold no time, so after a restore the
30-day count of `repair.suspect_after` (Q-247) starts from nothing unless carried corruptions are dated.
**Model.** Not shown: the model's restore carries no losses or corruptions (Q-294).
**Needed.** Proposed: the value carries its generation (in the reserved bytes, or a longer value), or,
equivalently, a generation's first epoch lies above every epoch its restore read; and a carried corruption is dated
at the restore record for the count, as Q-244 dates one at the rebuild record.

## Q-610 — The chain of epochs in uploaded manifest records: the field and where it starts
Recorded and decided by roadmap 8.3g.
**Scenario.** A restore lists the records of the generation it reads. A record the listing does not show, or the
bucket has lost, leaves no trace (Q-304): epochs may skip, since after a start the next epoch is above every epoch a
lease file names (`format.md` A.14), and a rebuilt manifest's epochs start above the old manifest's last, whose
last record may never have been uploaded.
**Decision.** The uploaded form of a manifest record (`format.md` A.17) starts with `previous: u64`, the epoch of
the record before it in the manifest it was read from, then the record's block, which stays 8-aligned and is
decoded as before. `previous` is 0 for a manifest's first record: a generation's first record (the store's first
record in generation 0, the restore record above it) and a rebuild record that opens a rebuilt manifest (the one
without field 100). A chain lives in one generation: a claim marker is no record and starts nothing, and a record
names only epochs of its own generation. Decoding refuses `previous` at or above the record's epoch, and a
non-zero `previous` on a rebuild record without field 100. The upload driver takes `previous` from the records it
reads from the manifest, as it takes them into its queue; the restore driver writes 0 for the restore record.
**Reason.** The envelope is deterministic in its plaintext and the sink takes a repeated `put` only with identical
bytes (A.17). A predecessor read from the bucket (the last record uploaded) would change with a listing that lags
at a restart, so a repeated `put` would meet `Exists` and stop uploads; one read from the manifest cannot change.
What the chain cannot show (a lost last record, the last old record before an unfinished rebuilt manifest) the
last-uploaded marker shows (Q-614); the format change itself (no version marker) is Q-960.
**Model.** Not modelled: the model uploads no manifest records.
**Tests.** `backup/src/uploaded.rs` (format vector, refusals both ways); `backup/tests/upload_chain.rs`
(consecutive records, a restart, a rebuilt manifest after a restart and under a driver that outlives it, with old
records pending, and a restart that leaves an old tail unuploaded), each bucket fed to `plan::check_chain`, as in
`check_done` and `upload_histories.rs`; `backup/tests/upload_restored.rs` (the driver over a restored directory
repeats the restore record byte for byte); the uploaded bytes in `upload_bucket.rs`, `restore.rs`.

## Q-611 — What a broken chain of uploaded records is, and what a restore does with one
Recorded and decided by roadmap 8.3g. **Owner** for (g).
**Scenario.** A restore reads a generation's records (Q-108) and each names its predecessor (Q-610). A record the
bucket has lost, or a listing does not show, makes the record after it name an epoch that is not listed.
**Decision.** (a) What is checked: the listed records from the first one the restore reads to the last listed, in
epoch order (`kvik_backup::plan::check_chain`). Records before the first read are superseded by a whole rebuilt
manifest and are not checked; those after the last read are, since a record hidden among them can change which
records are read. (b) Unbroken: each names the record listed before it, and the first, and each rebuild record
that opens a rebuilt manifest, name none. Skipped epochs are no break. (c) `Missing`: a record names an epoch
between it and the record listed before it (or any epoch, for the first). The restore asks `stat` for that record:
reported, the listing lags, `RestoreError::ListingIncomplete` (retried, as in Q-304); not reported,
`RestoreError::BrokenChain { generation, epoch, previous }`. (d) `Forked`: a record names none where one is listed
before it, or an epoch at or below the record listed before it; no listing can cause that, so `BrokenChain` without
a `stat`. (e) An epoch listed twice can only be the listing's fault, since a key names one object: the sink's
`Failed`. A record whose generation or epoch is not its key's is invalid (`RestoreError::Record`), so no chain
crosses generations. (f) A `stat` that does not answer fails the restore with its error. All of this comes before
the restore record is written, so a refused full restore uploads no record (its claim stands, as for every failure
after the claim).
`Store::restore` reports `BrokenChain` as the additive `BackupError::BrokenChain { generation, epoch }`
(`api.md`); the mapping is roadmap 8.3d's.
**Owner.** (g) Before this item a restore read whatever the listing showed: from a bucket that had lost a record it
restored fewer files or an earlier end, and a full restore then claimed a generation that every later restore
reads, so what the lost record named was lost for good with nothing reported. Now it refuses, and the refusal is
total: one record object lost in the checked range makes every restore and every drill of that store fail for good,
including what the records before the break hold. For a store that is gone (the case a restore is for) no operator
action exists: no call restores past the break. A store still running heals by uploading a whole rebuilt manifest
after the break (`a_break_before_a_whole_rebuilt_manifest_does_not_refuse_the_restore_that_reads_it`), which today
only a rebuild for another reason triggers; a regular drill is what finds the break while the store can still heal.
A healthy bucket cannot give this refusal (the engine never deletes and versioning is required); a bucket that loses
objects can, and from it a restore that used to succeed now fails. Built: the refusal, the conservative choice.
Alternatives for the owner: restore up to the record before the break and report the rest as lost, or let a store
that finds the break upload a rebuilt manifest; each needs a mechanism of its own.
**Model.** Not modelled: the model uploads no manifest records. `kvik/tests/restore_plan_model.rs` checks that the
chain of every record list it builds passes.
**Tests.** `backup/tests/restore_chain.rs` (each break, both modes, a break before and after the records read);
`backup/tests/restore_chain_properties.rs`.

## Q-613 — Files of the generation read that no record names
Recorded and decided by roadmap 8.3g. To be settled by roadmap 8.3d (how `Store::restore` reports them).
**Scenario.** A file is uploaded before the records that name it (spec §8 Continuous backup), so a disaster between
the two leaves its object in the bucket with no record naming it; so did a record lost or hidden beyond the reach
of the chain and the tail check (Q-611, Q-612) before the marker check (Q-614). The restore leaves such a file out, as spec §8 Generations says for
what is uploaded after a restore read its records, and before this item nothing said so.
**Decision.** After reading the source records, the restore lists the source generation's log segments
(`<sid>/log/<generation>-`) and the files of other kinds (`<sid>/files/`) and returns in `Prepared::unnamed` every
file of that generation that the fold of the records read does not know, in ascending order. It is a report, not
an error: the restore goes on and lists none of them. A drill reports them too; a resumed restore reports none, as
it reads no records. A listing that fails fails the restore with the sink's error.
**Needed.** Roadmap 8.3d: surface the report through `Store::restore` (health or statistics, next to the set-aside
report), so an operator learns that the bucket holds data the restore left out.
**Model.** Not modelled.
**Tests.** `backup/tests/restore_chain.rs` (`q613_*`).

## Q-615 — The source records are read after the claim
Recorded by the review of roadmap 8.3g. To be settled by roadmap 8.3d.
**Scenario.** A full restore claims its generation, then lists and reads the source records (`fresh` in
`backup/src/restore/driver.rs`). A `ListingIncomplete` there (Q-304, Q-611, Q-612), a broken chain or a sink fault
fails it after the claim, and each retry claims another generation, which is never withdrawn. With a source store
still uploading, or a listing that lags (Q-612 (d)), that can be many generations.
**Model.** Not modelled.
**Needed.** Suggested: read and check the source records before the claim, and read the claims again after it to see
that no higher generation was claimed in between; or retry the read within one claim. With `Store::restore` (8.3d).

## Q-630 — Acknowledged positions lost with the whole unsealed log when nothing names them
Recorded by roadmap 8.3c (fix round of its review). Settled by the owner's ruling (option (c)).
**Scenario.** Any generation, with a timed mark sync (the default). The durable mark names no position of the
generation yet (its first start's mark, not yet moved); batches are acknowledged at positions 1 to 3; a power loss;
then the only unsealed segment is missing or zeroed, with no sealed segment of the generation and no cursor file,
registry file or lost range naming a position above the mark. The start wrote a recovery entry at position 1 with a
new reset stamp and gave positions 2 and 3 again under it; no `data_lost` was reported, since nothing left named
positions 1 to 3. Q-264 and Q-321 promise a `data_lost` from the anchor for a lowest segment damaged, zeroed or
missing; that held only where some source named a position at or above the lost ones. The same held for any segment
whose batches were all above the durable mark (the next segment after a roll, before the timer fired).
**Options were.** (a) Accept, and say in §3 Recovery rule 2 that where nothing names the lost positions the reset is
the only report. (b) Record a lost range of unknown extent above the mark: a new manifest record form. (c) Sync the
mark at the first commit into each segment.
**Settled (owner's ruling, option (c)).** The batch that is the first commit into a segment is acknowledged only after
the mark naming it is synced: the writer forces that mark sync as it does for a batch holding an entry that is not a
fact (`log/src/writer/mod.rs` `commit`), one mark sync more per segment, none for a segment taken into use by a timed
seal with no batch. A durable mark then names a position in every segment holding acknowledged data, so losing the
only unsealed segment before the timed mark sync is a recorded `data_lost` from the generation's anchor up to the
mark's position, by the existing rule 2 path for a mark's segment lost at the tail, and positions acknowledged above
the mark are the recovery entry's reset (with a source naming them, its gap, `data_lost` of cause truncated tail).
Reason: a loss of acknowledged data must be reported, not hidden in a reset, and one sync per segment is cheap
(64 MiB or 5 minutes). `spec.md` §3 Commit, Durable mark and Recovery rule 2, `format.md` A.4 Sealing and A.10, and
`api.md` `log.mark_sync` say so. The reference model syncs its mark at the first batch of each segment
(`model/src/engine/recovery.rs` `sync_mark`), so its power loss never loses a segment's first batch. Cost on the
commit path: `docs/measurements/4.8-commit-latency.md`, Q-630 note. Tests: `log/tests/generation/first_start.rs`
`losing_the_only_segment_before_the_timed_mark_sync_is_data_lost_from_the_anchor_in_every_generation`,
`the_only_segment_lost_before_the_timed_mark_sync_is_a_recorded_lost_range_from_the_anchor`,
`the_first_batch_in_each_segment_forces_exactly_one_mark_sync` (mark syncs counted through `MemFs`) and
`a_cursor_past_the_lost_only_segment_gives_data_lost_from_the_generations_first_position` (now two ranges: up to the
mark by corruption, above it by the entry's gap), and
`losing_a_rolled_segment_before_the_timed_mark_sync_is_data_lost_at_the_marks_position_in_every_generation` (the
segment after a timed seal). Tests whose scenario needed a first batch beyond the mark now commit one batch first
(`log/tests/recovery.rs`, `log/tests/writer.rs`, `log/tests/threads.rs`). The log-against-model differentials run the
model with its mark synced at every commit (`log/tests/model_histories.rs`, header), so they do not see the rule; the
model's side rests on `model/tests/rules.rs` `s3_the_first_batch_in_a_segment_syncs_the_mark`, and matching the two
timers stays the follow-up that file names.

## Q-640 — A description that never stops failing to open or read its segment
Recorded at the review of roadmap 6.3b, with the retry of Q-221 (c); settled by the lead's ruling at its re-check.
**Scenario.** An unlisted sealed segment's permissions change, or another program holds it open for the whole run.
Every try of its description fails with `DescribeError::Io`; it is tried again every 10 minutes once the wait reaches
its cap, and the segment is not listed, so it is not uploaded and a restore ends the restored log at its range (as
for a damaged one). Silent, the store would report itself healthy throughout.
**Decided.** A description that keeps failing does not stay silent: from the failure that sets the 10-minute wait
(`DESCRIBE_RETRY_MAX`), health is degraded by `Reason::SegmentUnreadable { file_id }` (`api.md` Health; `spec.md` §8
Corrupt block, §11), not `SegmentDamaged`, which stays for failed checks (Q-221 (c)). Retries go on at the cap; the
reason clears when a read succeeds (with a description, or with a failed check, which raises `SegmentDamaged`
instead). A restart starts the waits again from 10 s and raises the reason again at the cap. Reason: an unlisted
segment is never backed up, so a silent indefinite retry is a hidden loss of backup. `log/tests/seal_describe.rs`
`a_description_that_never_opens_is_retried_with_doubling_waits_and_degrades_health_once_the_wait_reaches_the_cap`,
`segment_unreadable_clears_when_a_read_succeeds`.

## Q-650 — A manifest search that passes a window and finds no later record

**Scenario.** A crafted manifest: after a damaged record, checksum-valid record images in one 16 MiB window fail their
field checks until they have used the window's budget, so the forward search passes the rest of the window
(`format.md` A, common rules: scan bound; Q-110). No valid record of a higher epoch follows. A valid synced record
inside the passed part is never looked at. Damage cannot reach this: a counted record needs a valid checksum.
**Was (provisional).** The passed bytes were part of the torn tail, as for any invalid last record: the writer
truncated them at a start, with no report.
**Settled (owner's ruling, 2026-11-17): report corrupt.** The search did not look at those bytes, so the reader does
not know they are a torn record. Both readers now report them corrupt: the start's reader returns
`ManifestError::Corrupt` at the end of the last valid record (`log/src/manifest.rs` `read_all`), and the whole-file
reader gives a corrupt span with no later record, `CorruptSpan { before_epoch: None, end: <file end> }`, always the
last span, with the image not torn (`files/src/image.rs`). The bytes are kept and the copy or rebuild path runs, as
for any corrupt record. The rebuild record's field 7 writes such a span's `before` as 0 (only the last span may),
`Rebuild::gaps` and `appender::Gap::before` carry it as `None`, a copy of it is any valid record above the span's
`after`, and step 3 counts it as a span with room for several records (field 6), since its extent is unknown.
`spec.md` §7 Manifest and `format.md` A.14 say so. Tests: `files/src/image.rs`
`a_crafted_search_passes_the_rest_of_a_window_once_failed_reads_use_its_budget`,
`a_full_crafted_window_stays_within_the_budget_and_a_smoke_ceiling`; `log/tests/crafted_files.rs`
`a_crafted_manifest_search_passes_the_rest_of_a_window_once_failed_reads_use_its_budget`,
`a_full_crafted_manifest_window_stays_within_the_budget_and_a_smoke_ceiling`; `files/tests/open_span.rs`
`q650_a_crafted_tail_with_no_later_record_is_kept_corrupt_and_its_rebuild_names_an_open_span`;
`files/tests/records.rs` `q650_field_7_writes_a_span_with_no_later_record_as_0_and_only_as_the_last`; the files fuzz
target checks that a span with no later record holds the rest of the file. Natural damage is unchanged: it never
reaches the window rule, so a torn tail stays a torn tail. Added at the review: a copy written back for such a span
goes before it and its bytes are kept whole after the copy, never written over or dropped, as the passed part may
hold valid records (`kvik_files::rebuild::repair_with_copy`; `files/tests/open_span.rs`
`q650_a_copy_written_back_keeps_an_open_spans_bytes_after_it`); the rebuild's epochs go above what such a span may
hold (Q-872); and that it counts as several records in step 3 is pinned by
`q650_a_loss_no_valid_record_names_behind_an_open_span_counts_in_field_6`. A passed part that holds only zeros, where
no record can be, is still reported corrupt, not a torn tail: only a crafted file reaches it, and the copy or rebuild
path is safe for it.

## Q-651 — The crafted-file worst case against the 1 s restart budget

**Scenario.** A crafted 64 MiB log segment reached the scan bound's worst case (`format.md` A, common rules): about
390 MiB of bodies checked, about 2 s at 100 ns per row; 0.6 s at 16 MiB; a crafted manifest about 0.45 s per 16 MiB
at a refresh. The 1 s restart budget (`design.md` Budgets) was exceeded at 64 MiB.
**Settled (owner's ruling, 2026-11-17): tighten the scan; the published 1 s promise stays.** Both means the ruling
named, chosen by measurement (release build, one shared 4-core cloud machine, one run at a time; the harness is
`log/tests/crafted_files.rs` `crafted_worst_cases_timed` and `files/src/image.rs` `crafted_manifests_timed`, both
ignored tests):
1. Each kept body is checked once. A boundary read judged every body of a batch and the batch read then checked them
   again (`read_batch`); it now reads the judged batch without checking its bodies again
   (`kvik_log::batch::read_judged_batch`, a recovery entry found from the entry headers), so body checks total at
   most L instead of 2L, and the read that reaches a window's budget adds at most a batch instead of two:
   264 MiB of bodies for a 64 MiB segment instead of 390 MiB (`ScanStats::within_bound`; a manifest has its own,
   `within_manifest_bound`, with a body as its window and no direct read).
2. Cheaper body checks with the same verdict. The facts check keeps no row and validates text through
   `kvik_log::utf8::HeapText`: directly while the text checked stays within the heap's length, then from a table built
   in one pass, so rows that all name one 255-byte value cost a constant each (it was a decode and a copy, about
   100 ns a row). The entries check copies no text and builds no entry that needs memory; an entry's fields are
   split once into a list with an index by id (a lookup scanned the whole list, so a definition with 65,536 state
   names cost 20 lookups of 65,536), and state names are checked and told distinct in one pass (Q-870). The start's
   manifest reader reads a record's fields without keeping them and a lost range in one pass; the whole-file reader
   copies no field and gathers no ignorable field until the record's checks hold, keeps a field its type does not
   repeat at most twice, and finds an undefined field in the same pass, with the error it gave before.
Measured, before and after: nested facts images over every window of a 64 MiB segment 0.90 s and 0.22 s; 64 MiB of
batches whose rows share one text value, after damage, 1.85 s and 0.10 s; a 16 MiB manifest window of rejected records
0.15 s and 0.08 s at a start, 0.31 s and 0.10 s at a refresh (0.13 s with Q-872's pass); per byte checked, about 0.5 ns
for facts and at most about 2.9 ns for entries, every type small and large (Q-870). So against the bound a crafted 64
MiB segment costs at most about 0.8 s in body checks, 16 MiB about 0.22 s, and a crafted manifest at most about 0.09 s
per 16 MiB at a start and 0.15 s at a refresh: inside the 1 s budget. The header walk's time is bounded
by Q-873. `format.md` A common rules (scan bound), `design.md` Budgets and `SECURITY.md` state the figures.
Natural damage is unchanged: the verdicts are the same and the window rule is untouched. Tests:
`log/tests/body_check.rs` (each check against its decoder, the text table against `std::str::from_utf8`, and
`state_names_at_the_limit_are_told_distinct_and_past_it_refused`), `log/tests/batch_assembly.rs`
`after_damage_a_recovery_entry_not_alone_is_still_invalid_content` (the entry first, last or between others) and
`a_judged_batch_finds_a_recovery_entry_as_the_full_check_does`, `log/src/manifest.rs`
`a_lost_range_field_given_twice_is_read_from_its_first_occurrence`,
`log/tests/crafted_files.rs` `after_damage_each_kept_body_is_checked_once` and the crafted tests of Q-110 at the
tighter budget, `log/tests/scan_bound.rs` (the differential property against the reference scan at the new bound),
`log/src/scan/tests.rs` `the_bound_holds_body_checks_and_the_failed_read_budget_per_window`, `files/tests/records.rs`
`a_rejected_record_gives_the_same_error_with_its_fields_kept_at_most_twice`; the fuzz targets assert the new bounds.
The in-tree comparisons of each check with its decoder share the code they compare. Independent of it are the text
table against `std::str::from_utf8`, the state-name check against a sort (Q-870), the field index against a linear
search (a debug assertion, so every debug test checks it) and the reference scan; the record decoder and the start's
lost range were compared with the decoders before this change once, by a differential of about 1.8 million inputs at
the review, with no difference but Q-650's.

## Q-870 — Definitions and short state names against the 1 s restart budget

**Scenario.** After the first cut of Q-651, a definition entry cost about 460 ns to check whatever it held (some
thirty field lookups by binary search, and a set built for its names), about 3.8 ns a byte for small gauge
definitions and 5.3 for small two-name state definitions; a definition of 65,536 state names of 3 bytes (8 bytes of
body a name) cost about 5.7 ns a byte, and small routing entries 5.5. Against the bound (264 MiB of bodies at 64 MiB)
that is 1.0 to 1.5 s, above the 1 s restart budget.
**Settled (technical decision, no format change).** Requiring names in ascending order was rejected: field 13 is
in index order, the order facts and exports use. The check is made cheaper instead (`log/src/entries/decode.rs`,
`definition.rs`), with the same verdicts:
1. An entry's fields are split once, with a table of allowed and repeated ids per entry type, into a list that keeps
   an index by id, cleared per entry, so a lookup costs a constant; the check builds no boxed entry, selection list
   or consumer list.
2. State names are checked and keyed in one pass: a name of up to 7 bytes is packed into one word with its length,
   so equal keys are equal names; a longer one is hashed by `RandomState` and equal hashed keys are settled by a sort.
   Up to 8 names are compared pairwise; more go into an open-addressing table of 2n slots under a keyed mix with a
   per-process random seed and multiplier, so no input is slow on purpose. A probe run over 128 slots ends in a sort,
   so the worst case stays `n log n` whatever the keys.
Measured (release build, one shared 4-core cloud machine, the best of three runs; `log/tests/crafted_files.rs`
`crafted_worst_cases_timed` builds every entry type, small and large, short names ASCII and not, and reports the
dearest): the dearest body is now a definition of 65,536 state names of two 4-byte characters (8 bytes, hashed), 2.8 to
3.1 ns a byte; other short names 2.0 to 2.9 (one 4-byte character, a letter and a 3-byte character, and 3, 4 and 8 ASCII
bytes); small definitions about 1 ns (gauge 1.1, two-name states 0.9), small routing entries 1.0, every other entry at
most about 1.3, facts 0.4. So the body checks of a crafted segment cost at most about 0.8 s at 64 MiB and 0.22 s at 16
MiB, inside the budget; the header walk, which this does not count, is bounded by Q-873. `format.md` A common rules (scan bound),
`design.md` Budgets and `SECURITY.md` state the figures. Tests: `log/tests/body_check.rs`
`state_names_are_checked_as_a_plain_reference_checks_them` (a sort as the reference, both sides of the pairwise limit)
and `state_names_at_the_limit_are_told_distinct_and_past_it_refused`; `log/src/entries/definition.rs`
`the_sort_after_a_long_probe_run_and_after_equal_hashed_keys_gives_the_same_verdict`,
`packed_keys_tell_apart_names_that_differ_in_length_or_any_one_byte` and
`a_short_name_of_invalid_utf8_is_refused_on_both_sides_of_the_pairwise_limit`; the field index is checked against a
linear search by a debug assertion.

## Q-872 — The epochs a rebuild takes above a corrupt span with no later record
Recorded at the review of the Q-650 ruling. For the owner's acceptance.
**Scenario.** A rebuild took its first epoch above every epoch a valid record names, every closed span's later
epoch and the leases. A span with no later record (Q-650) has no later epoch, so the rebuild record took the last
valid epoch + 1: the epochs of records the span may hold, synced and perhaps uploaded under
`manifest/<generation>/<epoch>`. The rebuilt records at those epochs are then not uploaded, or meet another record
under their key, and a restore reads a chain without the rebuild.
**Decided.** Such a span bounds the rebuild's epochs from above (`kvik_files::image::CorruptSpan::epoch_ceiling`): the
higher of the last valid epoch and the highest epoch above it that a checksum-valid record image starting in the span
names, whether or not its fields pass (`named_epoch`; the search now also visits the passed part of a window for this,
with checksums and no field checks, so the window rule's bound on body checks is unchanged), plus the records of 40
bytes, the shortest record, that the span's length could hold, for records after it that damage has made unreadable. The
two add, as a record after a named one that jumped may be damaged. Epochs normally rise by one a record; after a start
they may jump above a lease (`format.md` A.14), which names an epoch a reader saw, and the record images give that case.
Not covered: a record that itself jumped above a lease and is damaged, with no whole record after it, is named by
nothing and lies above the length bound; the rebuild may then take its epoch again, as a torn tail reuses the epochs of
the records it drops, and the upload meets the earlier record under that key. A crafted image may name a very high epoch
and use up the epochs left, which a crafted valid record can do already. Cost: the passed part's headers and checksums,
about 0.13 s at a refresh for a crafted 16 MiB window (0.10 s before). `spec.md` §7 Manifest step 4 and `format.md` A.14
say so. Tests: `files/tests/open_span.rs` `q872_a_rebuild_takes_no_epoch_of_a_valid_record_hidden_in_an_open_span` (the
review's probe), `q872_a_hidden_record_far_above_the_spans_length_still_bounds_the_rebuilds_epochs`,
`q872_a_damaged_record_after_a_jumped_one_is_below_the_rebuilds_epochs` and
`q650_a_crafted_tail_with_no_later_record_is_kept_corrupt_and_its_rebuild_names_an_open_span` (the length bound);
`files/src/image.rs` `a_crafted_search_passes_the_rest_of_a_window_once_failed_reads_use_its_budget` and
`an_open_spans_epoch_ceiling_adds_its_room_to_the_higher_of_the_last_valid_and_the_named_epoch`.

## Q-873 — The time of the header walk on a crafted file
Recorded at the re-check of the Q-651 ruling; older than it (the same at `524a5cb`). Settled by roadmap 4.9e.
**Scenario.** The scan bound (`format.md` A, common rules) counts header reads and checksummed bytes, and Q-651 bounds
the time of body checks. A file can hold a valid block header every 8 bytes, each naming a plausible length and a
body checksum that does not hold (4 free bytes a header make the next one valid). No body is checked and no read
fails, so the window rule never applies, and the counts stay inside the bound (about 3 header reads per 8 bytes of the
16 allowed). Measured at the re-check (release build, one shared 4-core cloud machine): a 64 MiB segment 7.75 s
(16 MiB 1.5 to 1.7 s; 0.31 s when every block is of the commit type), a 16 MiB manifest 1.15 to 1.33 s with either
reader: about 0.3 µs a header read in the segment, each following a length to another part of the file, and 0.65 µs a
candidate in the manifest, the CRC shift of `PrefixCrc::range`. The 1 s restart budget is exceeded.
**Settled by roadmap 4.9e: the walk is bounded in header reads per byte; its time is measured, and what a start is
promised is Q-970.** Resynchronisation reads each 8-byte step's header at most once, a batch ahead of the candidates,
and keeps a 4-byte entry per step in a ring indexed by offset (8 MiB): no chain, a commit block naming a start, or the
first chain node past the step's block. The ring is filled 4 MiB of steps at a time from the highest down, a step
taking its successor's entry when the successor lies in the same block and a pass after each block giving its ends to
the earlier entries whose chains enter it. A step is a candidate exactly when its entry names it, which is the walked
condition, and distinct candidates' chains are disjoint (each ends at the one commit block that names it). A boundary
read that keeps no batch keeps a 4-byte verdict per block it walked in a ring indexed by offset (4 MiB, a batch and 64
KiB of steps) and its stop's event, so each block is judged once and remembering costs one store a block; a candidate
on such a chain is read from those verdicts: one whose read would fail at the remembered stop takes its framing only,
and is charged the facts and entries body bytes that framing and the read in full would have checked; one whose stop
is a commit block is read in full without its bodies checked again, and charged likewise if it keeps no batch. A
header found valid once damage is found is remembered in a bitmap by offset and read again without its checksum; the
tail's commit search reads a header only where the chain ends did not find it valid and its bytes name a commit block of
the commit length; `crc32c_shift` takes at most eight lookups in nibble tables below 4 GiB, so a manifest step reads one
header and takes at most one checksum. The argument: a header checksum is taken at most twice per 8-byte step, once by
the chain ends at a step whose type and length could belong to a batch and once by a torn tail's commit search at a step
whose bytes name a commit block of the commit length (a header that fails its checksum is not remembered, so a torn tail
of such steps takes two a step), plus once more for the headers of the one direct read that found the damage and for the
block where each boundary read stops (the bitmap spans 16 MiB from the lowest offset the scan still asks about, and
every read lies within 12.1 MiB of it); and a block's header is read at most eight times: once by the chain ends, three
times by a candidate (framing once, the read in full twice), twice by a boundary read and twice more by the read of the
batch it keeps, plus twice by the commit search at a commit block. Verdicts are
those of the walked chains and the reference scan, and the window rule and the charges are unchanged. Measured
(release build, one shared 4-core cloud machine, the best of five, the worst at most 40 % above it, and runs at other
hours up to 30 % slower again; a header read costs about 20 to 31 ns with the rest of the scan's work), 64 MiB: a
valid header every 8 bytes 0.25 s (16 MiB 0.06 s; 5.7 s before), commit blocks naming data headers 0.24 s (3.1 s),
candidates whose framing holds 0.25 to 0.27 s, chains of empty skippable blocks read in full 0.35 to 0.43 s, batches
of them kept after damage 0.33 to 0.57 s (at a boundary 0.52 to 0.56 s), a boundary read that fails at the end of a
chain of 500,000 such blocks the candidate then reads in full 0.35 s (2.0 s before), the same chain a batch the
candidate keeps 0.61 s, the dearest built, chains of 4,096 blocks 0.30 to 0.47 s, 8-byte bodies whose every step is a
plausible header 0.29 to 0.40 s, batches of state names whose every step is a plausible header, each body checked and
each step checksummed, 0.31 s, kept batches of the dearest entries 0.23 to 0.26 s; undamaged batches of 500,000 empty
blocks 0.25 to 0.37 s, most of it building the report; a 16 MiB manifest with a header every 8 bytes 0.19 to 0.30 s
with either reader (0.84 to 0.89 s before). Tests: `log/tests/header_walk.rs`
`files_longer_than_the_ring_take_one_header_checksum_and_a_few_reads_a_plausible_step` (header checksums within the
plausible steps and header reads per plausible step on ten shapes of 24 MiB, against the walked chains),
`a_candidate_on_a_chain_a_boundary_read_rejected_is_charged_what_reading_it_checks` (the charge equals the walked
chains', which read the candidate in full), `a_header_at_every_step_has_each_header_read_once`,
`commits_naming_data_headers_have_each_header_read_once`,
`framed_candidates_check_no_body_and_agree_with_the_walked_chains` and the ignored `crafted_header_walks_timed` and
`stacked_passes_timed`; `log/tests/scan_differential.rs` (deep shard: crafted and damaged segments against the
reference scan, and longer than the rings against the walked chains); `log/tests/scan_bound.rs` (deep shard)
`crafted_header_runs_give_the_reports_the_walked_chains_give` and `chain_ends_find_the_candidates_a_plain_walk_finds`,
with `a_critical_block_of_exactly_the_limit_found_by_resynchronisation_refuses_the_segment`,
`a_commit_block_alone_at_the_first_damage_is_a_readable_commit_of_the_tail`,
`a_header_failing_its_checksum_a_ring_above_a_valid_one_is_not_taken_as_valid` and
`a_boundary_read_a_ring_above_the_valid_headers_checks_the_header_checksum`; `log/src/scan/tests.rs`
`a_start_a_ring_above_a_candidate_takes_its_own_chain_end`,
`the_valid_header_ring_holds_only_aligned_starts_from_its_base`, `a_header_read_again_must_fit_below_the_limit_asked`
and `boundary_reads_ending_at_one_invalid_body_check_it_once`; `log/src/crc.rs`
`the_table_shift_equals_the_shift_by_squaring_for_every_digit_and_beyond_4_gib`; `files/src/image.rs`
`a_header_at_every_step_takes_one_checksum_a_step_in_both_readers` and the ignored `crafted_manifests_timed`; the fuzz
harness asserts the scan equals the walked chains on every input.

## Q-670 (c) — The pattern grammar as a public promise
Recorded by roadmap 5.6a, for the owner; (a) and (b) settled by the owner's ruling (see Settled).
**Scenario.** The narrowed grammar and the new public type `RouteError` (with `ConsumerError::Pattern`) are promises
in `spec.md` §4 and `api.md`.
**Now.** Built on both backends as `spec.md` §4 Funnel and `api.md` Consuming give them (Q-88).
**Needed.** Accept at the 5.6 gate the narrowed grammar, the additions decided at 5.6a (Q-88) and `RouteError` as
public interface; the name (`RouteError` beside `ConsumerError::Route`) is cheapest to change before then.

## Q-680 — `Outcome::Recorded` carrying the level and `LogStats.acknowledged`
Recorded at the review of roadmap 5.5a. **OWNER** (public interface).
**As built (`api.md` Writing, Health).** `Outcome::Recorded` gains `durability`, the level the reading was
acknowledged at; `LogStats` gains `acknowledged` beside `durable` ("durable up to"). Not in the model.
**Proposed.** Accept.

## Q-681 — What `max_unsynced` bounds
Recorded at the review of roadmap 5.5a. **OWNER** only if a hard bound is wanted.
**Scenario.** The writer starts the log sync 1 s after the first flagged batch, and the device takes 3 s to complete
it while flagged batches keep arriving; a power loss then removes about 4 s of acknowledged readings.
**As built.** The text says what holds: an event removes what was written since the last log sync that returned,
and the writer starts that sync at the latest `max_unsynced` after the first flagged batch (spec §2.1, invariant 10;
`api.md` Configuration). **Proposed.** Keep the wording; a hard bound would make ingest of `written` series wait
while a sync overruns.

## Q-704 — The writer's delivery reader at the writer's durable position
Recorded by the store-path quick wins. To be settled by roadmap 6.3b, the first item to write manifest records during
a run.
**Decision.** The writer process's delivery reader (`kvik/src/durable/deliver/mod.rs`, `read_new`) skips `refresh`
when its durable position is at or above the writer's (`LogStats::durable`), and calls `keep_alive` instead, so the
lease is still renewed at least every `lease_refresh`. A refresh there would read nothing: only the writer's process
changes the log while it holds the store lock, so no rewrite, void or region can appear below that position (§7
Readers), and in this build the manifest is written only at a start (`StartOptions::recorder` is refused), so the
lease already names the current epoch. Every refresh that does run follows the full reader protocol of §7 Readers.
With 16 callers acknowledged by one batch sync, one refresh runs instead of 17. A failing `keep_alive` is counted in
`DeliveryStats::refresh_errors` although nothing was read, and an idle store no longer checks its last batch again
every second, so damage to it is voided at the next batch. Any reader also skips opening the mark's segment while
the mark has not moved, so a header damaged after it was first read is reported when the mark next moves, not at the
next refresh.
**Open part.** Once manifest records are written during a run (publish and retire, tiers), an idle writer's delivery
reader would keep naming the epoch of its last refresh, which `keep_alive` renews unchanged, and so hold retired files
(§7 Retired files: "no live reader lease names an older epoch") until the next batch. That item must also require the
lease to name the manifest's current epoch before skipping (for example, the writer publishing its epoch in
`LogStats`), with a test of retirement under an idle store.
**Pinned by.** `a_reader_at_the_writers_durable_position_only_renews_its_lease_when_due` (unit test of the deliverer);
`the_delivery_reader_refreshes_at_most_once_per_batch_sync` and
`an_idle_store_writes_no_delivery_lease_before_its_renewal_is_due` (`kvik/tests/write_path.rs`).

## Q-705 — The model backend's `outranked` when a later reading of the same call outranks an earlier one
Recorded by the store-path quick wins. Settled by the lead.
**Scenario.** A versioned series. Call 1 records `t` with `changed_at` 1. Call 2 holds two readings of the same key:
`changed_at` 0, then `changed_at` 2. The durable backend reports the first as recorded and `outranked` (a fact of
higher rank, `changed_at` 1, was already recorded when it was committed: §2.3, §4 Outranked facts), the second as
recorded and not outranked; its deliverer flags them the same way. The model backend reports both as not
outranked: its adapter (`kvik/src/backend/mod.rs`, the outcomes after `commit`) calls a fact outranked only when
the key's current fact after the whole call lies at an earlier position, and here the current fact is the second
reading. The reference model's engine marks the first fact outranked at commit (`model/src/engine.rs`), as the
durable backend does, so the adapter is the odd one out; `outranked_on_arrival` differs the same way.
**Settled by the lead, as proposed:** the adapter takes `outranked`, and so `outranked_on_arrival`, from the engine's
commit-time judgement, which the model's `CommitReport::outranked` carries (the positions of the facts outranked
when committed). The differential test `the_durable_backend_decides_as_the_model_does` (`kvik/tests/durable_differential.rs`) no
longer keeps `changed_at` from rising within a call, and also generates calls on two keys of the versioned series
alone, so that an earlier reading outranked at commit and a later one that becomes current meet often.
**Pinned by.** `a_reading_outranked_at_commit_stays_outranked_when_a_later_reading_of_its_call_becomes_current`
(`kvik/tests/api.rs`, both backends); `the_durable_backend_decides_as_the_model_does`.

## Q-706 — An emit's read-back of its batch when no consumer is registered
Recorded by the store-path quick wins. Settled by the lead.
**Scenario.** After its acknowledgement every durable `emit` takes the deliverer's lock, reads its batch back
through the delivery reader (`pump`) and waits for the consumers it woke; before deciding, it takes the same lock to
run the timers. With no consumer registered none of this has an observer, and leaving it to the `kvik-deliver`
thread lets the caller decide its next call while that thread reads: a prototype measured about twice the readings
per second at 2,000 readings per call. A consumer would see nothing different if a flag were raised before a
consumer is taken: registering pumps under the decision lock before it sets the consumer up, so every batch
acknowledged before the flag was read is taken before the consumer exists, as now. Two things outside delivery do
change, both in `Health::delivery` (`DeliveryStats`):
- `refresh_errors`: today each acknowledged emit reads, so with the delivery reader's lease write failing, n emits add
  at least n; with the skip only the `kvik-deliver` thread's reads count.
- `corruptions`: today an emit returns only after the deliverer has read past its batch, so a region damaged below it
  before the mark moved (a test can do this from a sync hook) is counted when `emit` returns; with the skip it is
  counted a little later, by the `kvik-deliver` thread.
Running only the timers without the lock (they have nothing to evaluate without a consumer) is exact but measured no
gain, since the caller's own read-back still waits for the lock; it was not kept.
**Question.** May these two counts lag an emit's return, and count reads rather than calls, while no consumer is
registered? `api.md` gives them no more than their descriptions ("reads of the log that failed and were tried again
at the next one"; "invalid regions below the durable mark the delivery reader passed over"). If yes, the skip is a
small change with a test that a consumer registered after many such calls receives what its start promises; if no,
the read-back stays on the caller's thread until acknowledged batches are handed to the deliverer in memory.
**Settled by the lead: yes.** With no consumer registered, `refresh_errors` and `corruptions` may lag an emit's return;
the `kvik-deliver` thread counts them shortly after, and `refresh_errors` counts reads, not calls (`api.md` Health and
statistics). `emit` runs no timers, takes no batch and waits for no wake while the hub's consumer flag is lowered
(`kvik/src/durable/consumers.rs`, `deliver/hub.rs`). The flag is never lowered. `take_consumer` raises it before its
two pumps (under the decision lock, then under the deliverer's lock with the setup); an emit that read it lowered
therefore had its batch acknowledged before both, and either takes it before the consumer's start is computed. A take refused after the raise
leaves the flag raised for the run, which costs only the work emits would skip. The flag also starts raised when the
start found any consumer (named by a cursor file or the log); that is a precaution, since a consumer not taken in this
run is neither fed nor evaluated. Tests that count the delivery reader's work take a consumer
(`the_delivery_reader_refreshes_at_most_once_per_batch_sync`) or a tick
(`an_idle_store_writes_no_delivery_lease_before_its_renewal_is_due`, `kvik/tests/write_path.rs`) so that the emit's
read-back is done where they count. In a measurement (four writers, 500 readings per call, in-memory files) the
thread ran at most 3,500 to 6,000 readings behind the last acknowledged position.
**Pinned by.** `with_no_consumer_an_emit_returns_without_taking_its_batch`,
`with_no_consumer_an_emit_never_waits_for_the_deliverer`,
`a_consumer_taken_after_an_emit_left_its_batch_starts_at_or_after_that_batch` (the pumps of a take),
`an_emit_waits_while_a_consumer_is_set_up_instead_of_leaving_its_batch` (the raise before the setup),
`an_emit_returning_after_a_consumer_is_set_up_has_taken_its_batch`,
`an_emit_acknowledged_before_a_take_and_returning_after_it_has_taken_its_batch`,
`with_a_consumer_an_emit_returns_after_taking_its_batch_and_waking_it` (unit tests in
`kvik/src/durable/registering.rs`, with pause points);
`a_consumer_taken_during_emits_receives_every_fact_after_its_route_once_and_in_order`,
`a_consumer_taken_after_many_emits_without_one_receives_what_its_start_promises`,
`the_delivery_counters_reach_the_same_counts_with_or_without_a_consumer` (`kvik/tests/emit_without_consumers.rs`).

## Q-710 — The default floor of the batch-sync interval

**Scenario.** The batch-sync interval adapts to the device between `log.sync_interval_floor` and
`log.min_sync_interval` (spec §3 Group commit). With a floor of 2 ms a fast device waits about 2 ms instead of 10,
and a store of 1,000 unbatched 1 Hz series on it runs up to 500 batch syncs a second instead of 100: at 4 KiB a
sync, about 165 GiB of host writes a day against 33 GiB, roughly a decade of a 600 TBW drive if sustained. Spec §3
states the bound as at most 1/`min_sync_interval` per second (100 at the default), §13 Sync rate and SSD wear names
a longer `min_sync_interval` as the remedy, and `design.md` Budgets ties p50 ≤ 5 ms to about 100 synced batches a
second.
**Settled by the owner's ruling:** the default `log.sync_interval_floor` is 2 ms. Spec §3 states the bound as at
most 1/`sync_interval_floor` batch syncs per second (500 at the default), and `api.md` Configuration states the
wear ceiling; a deployment on flash that is sensitive to wear sets a longer floor, up to `min_sync_interval`, where
the interval is fixed. The interval landed with the floor at 10 ms, which changed no behaviour; the default moved
to 2 ms in a change of its own after the commit latency rows (`docs/measurements/4.8-adaptive-sync-interval.md`)
showed no regression against the fixed 10 ms rows beyond their run-to-run spread. The design's own marks were not
all met on that machine: one writer of 2,000-reading batches ran at about twice the fixed rate but far below bound 0,
since its sync of about 0.5 ms keeps the interval near 4 ms; the owner's ruling did not depend on them.
**Since.** Q-980 keeps the floor and its default and keeps the bound the ruling was given on: batch syncs run at
most 1/`sync_interval_floor` per second over time. It adds a burst of four batch syncs to any window (Q-985), which
the ruling did not cover (Q-983), and makes the floor a store key, as the ruling's remedy needs (Q-981).

## Q-711 — New fields in the log crate's `WriterConfig` and `LogStats`

**Scenario.** `kvik_log::writer::WriterConfig` gains `sync_interval_floor`, and `LogStats` gains `sync_interval`
and `sync_time`. Neither struct is `#[non_exhaustive]`, so a caller that builds one naming every field no longer
compiles.
**Decided by the lead.** The fields are added; no `#[non_exhaustive]`, which would itself break callers by
forbidding struct expressions outside the crate (every test and benchmark builds `WriterConfig` with
`..WriterConfig::default()`; no literal in the tree names every field). The log crate is not published.
`kvik::LogConfig` and `kvik::LogStats` are `#[non_exhaustive]` already and take the new key and statistics
compatibly.
**Since.** Q-981 adds the floor to `kvik::LogConfig`; `kvik::LogStats` does not carry the two statistics (Q-984).

## Q-712 — The constants of the adaptive interval

**Scenario.** The interval is eight times a smoothed batch-sync duration (`log/src/writer/pacing.rs`); the estimate
rises to a longer sample at once and falls by an eighth of the difference per shorter one. A multiplier of 4 would
halve the wait on a mid-speed device and double its sync rate up to the floor; rising at once holds a device with
frequent short outliers near the ceiling; with the mark synced at every commit each batch brings a mark sync that
could be timed with it.
**Decided by the lead.** Multiplier 8, a fall of an eighth, a rise at once; with the mark synced at every commit only
the batch sync is sampled, so device syncs are up to twice the batch-sync bound, as today. None is a configuration
key. The commit latency sweep over multipliers 4, 8 and 16 at a 2 ms floor kept 8
(`docs/measurements/4.8-adaptive-sync-interval.md`): 4 halved the wait again but ran about 1.7 times the syncs of 8
at 1,000 calls a second and the lowest saturation throughput of the three; 16 stayed near the fixed schedule.
Pinned by the unit tests of `pacing.rs` and `log/tests/pacing.rs`.
**Noted at the review.** The first sample after a start moves the interval from the ceiling to the estimate in one
step, not by an eighth, so a burst right after the first reading can start syncs half a floor apart where the fixed
schedule allows one; this stays inside the stated rate and spacing bounds, which the properties of
`log/tests/pacing.rs` check. The sample grows with the batch and the batch with the interval, so under saturation
with 1 MiB batches the interval holds itself near the ceiling, and a single closed-loop writer of large calls runs
well below bound 0 (`docs/measurements/4.8-adaptive-sync-interval.md`); the run on the Linux NVMe target decides
whether the multiplier or the floor needs tuning for single closed-loop writers.
**Since.** Q-980 keeps the constants; the interval is now the refill period of the sync budget, not a spacing.

## Q-713 — A `min_sync_interval` beyond `u64` nanoseconds

**Scenario.** `log.min_sync_interval` has no upper limit. A value near `Duration::MAX` overflowed the additions of the
sync schedule (the previous due time or sync start plus the interval) and panicked the writer thread; with the
adaptive interval, a ceiling above about 584 years also changed value at the first sample, which converts it to
`u64` nanoseconds.
**Settled with the review of the adaptive interval:** the interval is held in `u64` nanoseconds whether or not there
is an estimate, so a ceiling above about 584 years counts as 584 years from the start. No configuration that was
accepted is refused, and the schedule's additions stay far inside `Duration`
(`a_ceiling_near_the_largest_duration_schedules_without_overflow`,
`a_ceiling_beyond_u64_nanoseconds_reads_the_same_before_and_after_a_sample`).
**Since.** Q-980 keeps the interval in `u64` nanoseconds and counts the budget in `u128`; the tests are now
`the_largest_sample_and_ceiling_saturate`, `the_ready_time_saturates` and
`an_interval_near_the_largest_duration_schedules_without_overflow`.

## Q-750 — The slow path of the duplicate rule meeting damage found during a run
Recorded by roadmap 11.0. **Settled by the owner's ruling** at its review.
**Scenario.** A gauge records 1.0 at `t`; an hour later its recent window has moved on. A sealed segment holding
that fact is then damaged while the store runs (a flipped byte below the mark), and the adapter replays 1.0 at `t`.
Before roadmap 11.0 the writer held every fact in memory and dropped the replay as a duplicate. Now the key is read
from the local log (the slow path, §2.4), and the region that may hold it cannot be read.
**Ruling.** A reading whose duplicate check cannot complete because of damage found during the run is recorded
flagged unchecked (`Outcome::Recorded { unchecked: true }`) and counted in `duplicates_unchecked`, the existing flag
for a fact on no local tier, with its rank judged on the facts that can be read; ingest never stops. A delivery
decided without such data (whether a fact was outranked when committed, or the facts a condition or live state was
evaluated over) carries the same flag (`DeliveryFlags::unchecked`) and is counted (`DeliveryStats::unchecked`).
Either holds health degraded (`Reason::LocalDataUnreadable`) until the next start (spec §2.4, §11), counted on
their own, not from `duplicates_unchecked` (which a fact on no local tier also raises, D5). An evaluation made
without such data is counted whether or not it delivers (`DeliveryStats::evaluations_unchecked`). A failed lease
renewal during a read flags nothing; only data that could not be read does. A damaged region that no lost range
records is the lead's decision at the re-check, Q-755.
**As built.** An invalid region the store's start passed over is lost (§3 Recovery rule 2; the start's state never
held its facts, as before roadmap 11.0) and reads as absence; a region met later is lost only if it lies inside one
the start passed over, so damage that lengthens it is unreadable. A segment file removed while the store runs is not
listed and so is not seen, as for any reader (gaps between sealed segments are the scrub's, §8). On a series that is
not versioned, a replay of an earlier value whose key sits in a damaged batch is recorded and becomes current; the
flag and the health reason say so.
**Pinned by.** `a_key_that_cannot_be_read_is_recorded_unchecked_unless_a_known_fact_matches` (unit test of the
decision); `a_lookup_never_reads_damage_as_absence_unless_the_region_is_lost`,
`damage_that_lengthens_a_lost_region_is_not_read_as_absence`, `a_failed_lease_renewal_leaves_a_lookup_complete`
(log reader); `a_reading_outside_the_recent_window_is_checked_against_the_log_across_seals_restarts_and_damage`,
`a_fact_whose_earlier_facts_cannot_be_read_is_delivered_unchecked` (`kvik/tests/slow_path.rs`).

## Q-751 — The recent windows' memory: per series and store-wide
Recorded by roadmap 11.0. **Decided by the lead** at its review.
**Scenario.** §5.1 gives the recent window "default 15 minutes, at most 1 024 facts" per series, and §13 row "Memory
per series" asks for memory bounded and documented. Per series alone, 2,000 one-second series hold about 1.8
million facts in the writer's windows and as many in the deliverer's (some 400–440 MiB in all); 10,000 series about
2 GB, beyond the minimal hardware class (1 to 3 GB).
**Decision.** The span and per-series cap stay constants (`kvik/src/durable/history.rs`: `WINDOW_SPAN`,
`WINDOW_FACTS`), the span at least an interval series' length. One configuration key bounds the store:
`limits.window_memory` (default `WINDOW_MEMORY_DESKTOP`, 512 MiB; `WINDOW_MEMORY_MINIMAL`, 64 MiB, for the minimal
class; at least 4 KiB), half for the writer's decision windows and revised-key index, half for the deliverer's
windows. Above its half the writer drops revised-key entries the log already shows, oldest first, then each series'
oldest window key in turn; the deliverer drops each series' oldest key in turn. Decisions do not change: what leaves
memory is read from the log (the slow path, §2.4). The bound counts entries held; a window's allocation can hold up
to as much again while it grows. The deliverer's per-series cap holds whatever `now` is. The lookup's caches of
sealed segments' indexes and batch maps are bounded too (8 MiB per reader), and a segment is read a part at a time.
The resulting bound is stated in `api.md` Configuration (Memory) and measured in
`docs/measurements/store-emit-throughput.md`.

## Q-752 — A start's time and the revised-key index before tree checkpoints exist
Recorded by roadmap 11.0. To be settled by roadmap 7.1b (tree files and checkpoints).
**Scenario.** §8 has each tree checkpoint carry the revised-key index, the version index and each series' latest
recorded `t`, so a start replays only the log after the checkpoint (§13 Restart ≤ 1 s). No checkpoint exists yet. A
store holding a few million readings is opened.
**Decision.** The writer's start reads the whole log once, in parts of 65,536 items, keeping only the registry,
commit times and recovery entries, each series' latest `t` and lateness reference, its recent window, the keys
revised within the window as they leave it, and the per-series counters rebuilt from the log (`recorded`, `late`,
`future`); the deliverer's open does the same for its own state. Memory at start is bounded by those and one part of
the log; the start's time stays proportional to the log. The in-memory revised-key index is complete for each key it
holds, but holds only the keys this process saw revised in the window or read whole by the slow path: a key revised
outside the window before a restart is found by the slow path, not the index, until the index is persisted. A part
that voids what came before it (a recovery entry voiding intact batches above its `after`, which the recovery rules
leave no room for) cannot be undone from that state, so the start then reads the log again whole into memory, as
before 11.0, and says so (`IngestStats::start_whole`). Accepted by the lead until 7.1b.
**Open.** A start bounded in time needs the persisted state of §8 (the tree checkpoint), a change to the formats;
the counters rebuilt at a start need it too.

## Q-753 — What the deliverer keeps in memory and reads from the log
Recorded by roadmap 11.0. To be settled by roadmap 5.6 (delivery looked at as a whole).
**Scenario.** The deliverer kept an index of every fact for live state (§2.3), held-for history (§6 Restart and
resume), whether a fact was outranked when committed (§4) and the records of a consumer that is behind.
**Decision (the lead's).** Per series it keeps every fact taken with `t` at or above a floor: the earlier of the
series' latest `t` and `now`, less the recent window's span, so the facts a live-state fact can come from as `now`
advances (scheduled values ahead of `now` included) stay in memory; at most 1,024 facts whatever `now` is, within its
half of `limits.window_memory` (Q-751). A fact of a versioned series
below the floor is judged outranked from its key's facts read from the log (once per taken batch for all such
keys); live state and held-for history before the floor are read a span at a time, each span 16 times the last; a
consumer behind has its records read again from the log by position. Each read is counted in
`DeliveryStats::slow_path`; reads before the window take only what has been taken (positions at or below the last
taken). A read of the log for a consumer behind that fails leaves the consumer behind and is counted
(`refresh_errors`); the next call that catches it up reads again. A consumer far behind holds its records from its
cursor to the last position while it catches up, as before. The deliverer's view of a run of bad facts longer than
the window therefore costs reads of the log on every evaluation of a condition on that series. Deliveries decided
without data that could not be read are the owner's ruling, Q-750.
**Pinned by.** `the_deliverer_holds_its_recent_window_and_reads_older_facts_from_the_log`,
`a_fact_taken_below_the_window_is_judged_outranked_from_the_log` (unit tests of the deliverer);
`deliveries_follow_the_model_over_restarts` (`kvik/tests/consumer_properties.rs`), whose histories jump past the
window.

## Q-773 — The series directory's cadence against a reader's allowance
Recorded by roadmap 9.0a; decided at its review (technical). Moved to roadmap 9.0e by 9.0c, whose reader continues
any directory and builds no allowance (Q-1003).
**Scenario.** A reader covers the sealed segments its directories do not yet cover by reading them, up to 64 MiB in
all, and fails with `DirectoryUnavailable` beyond (the query design's section 4). Between a seal and the series
directory write that covers it (a wait of up to one mark sync, which the writer forces, or until the deliverer takes
the batches), a store with `segment_size` above 32 MiB is past half the allowance, and one above 64 MiB past all of
it, although the writer runs normally.
**Decision.** The series directory is written once the uncovered sealed bytes plus the largest of the last 16 seals
would pass 32 MiB, and otherwise at least every 15 minutes (`log/src/directory/cadence.rs`, `format.md` A.18
Cadence): between writes the uncovered bytes stay at most 32 MiB, and segments that seal by size (the default
64 MiB) are covered at every seal, which is what the earlier rule did too, while small timed seals follow the
15-minute rule. A reader past its allowance waits one mark-sync interval and tries again before
`DirectoryUnavailable`. A `segment_size` ceiling was not taken: it would change a documented configuration range.
The rule holds while one seal takes less time than a segment takes to fill (seals run before the directories on
`kvik-maint`).
**Pinned by.** `uncovered_bytes_stay_bounded_through_the_directories_at_any_rate`,
`a_series_write_covers_the_seals_so_the_next_small_seal_waits` (`log/tests/directories.rs`).

## Q-774 — Where the derived files are laid out, their names and generations
Recorded and decided by the lead in roadmap 9.0a (technical; the design's Q-736 makes them normative in Part A).
To be confirmed by roadmap 9.0c, the first reader of the directories (moved from 9.0b1, which reads none).
**Decision.** `format.md` A.18, kinds 15 to 17, in `derived/` (A.12). Each file is a header and one block; a family's
generations are told apart by `file_id`, and the previous generation is kept (none for the status file) so that a
power cut, which may leave the newest unsynced file invalid, falls back to it. The derived files are passed over as
sources of F when they are of a newer version, and the writer then writes none of that family, rather than refusing
to open: they never name a position above the last synced mark, so F cannot need them, and they are disposable.
The group files are numbered by ordinal in position order, so a replaced segment keeps its group number.

## Q-775 — Checking a log directory against the catalogue before the manifest lists sealed segments
Recorded by roadmap 9.0a. To be settled by roadmap 6.3b, which writes the sealed-segment records (`0x22`).
**Scenario.** A.18 checks each group record against the catalogue's description of the segment (id, generation,
last sequence, plaintext hash). No manifest record names a sealed log segment yet, so a writer's start has no hash to
compare without reading every segment whole (the whole kept log, hours on a card after a year).
**Decision.** Until then the writer's start builds its catalogue from `log/`: each sealed segment's trailer, read by
two small positional reads, gives its last sequence, length and `file_crc32c`, and those replace the hash in the
check (`log/src/directory/catalogue.rs`, `catalogue_from_disk`). A segment rewritten with the same trailer but other
content would pass; A.9's copy-and-swap writes a new `file_id`, so a replaced segment does not.
The cost is two small reads of every sealed segment at every start, in steps between the maintenance thread's
other duties (105,000 segments a year at the home rate), and a full read of each segment no valid group record
describes (after a restore, a lost `derived/`, or the first start with the derived files): background time only. A
sealed segment whose trailer cannot be read at the start ends the listing below it (A.18 Contiguity). The root's
epoch and lost ranges are those of the start: the manifest changes only at a start today (Q-704).
**Open.** With 6.3b the check uses the manifest's hash, the start stops listing `log/` for it, and the root follows
the manifest as it grows (a record per seal would otherwise make a reader parse a run's records at each call).

## Q-776 — When the writer keeps a series directory across a reset or a loss
Recorded by roadmap 9.0a. To be settled by roadmap 9.0c, the first reader of the series directory (moved from 9.0b1,
whose reader reads no directory).
**Scenario.** A series directory written at `through` = 900 under stamp S; a power cut; the start writes a recovery
entry with `after` = 950 and a new stamp S'. The file's positions are all kept, so it is still a function of the
kept log; a file with `through` above 950 would not be.
**Decision.** `spec.md` §7 Derived files states the rule as a saved-point judgement. The writer applies the part it
can judge at its own start (`DirectoryStart::keeps`): same generation, and the current stamp or a `through` at or
below the `after` of the recovery entry this start wrote or adopted. A file written under an earlier stamp and kept
across a start without a reset of its own is therefore dropped at the next start and written again: conservative,
never wrong. A lost range is not a reset (the stamp does not change), so the file also records how many positions at
or below its `through` the manifest's lost ranges covered (`format.md` A.18): a position lost since, by a new range
or by one extended under its identity, drops it, at the writer's start (`kept_series`, shared by `kvik`'s seed) and
for a reader. Positions rather than ranges, since a loss that grows keeps its identity and a reader sees the ranges
merged by identity.
**Pinned by.** `a_reset_below_the_series_directory_invalidates_it_and_one_above_keeps_it`,
`a_restore_invalidates_every_file_of_the_older_generation`, `a_lost_range_recorded_below_the_series_directory_drops_it`
(`log/tests/directories_check.rs`); `a_loss_extended_under_its_identity_below_the_series_directory_drops_it`,
`a_loss_that_grows_only_above_the_series_directory_keeps_it` (`log/tests/directories_faults.rs`).

## Q-778 — What the status file holds before the query envelope exists
Recorded by roadmap 9.0a. To be settled by roadmap 9.0g (the pressure flag, slots) and 9.0e (`kvik health`).
**Decision.** Health from what the writer holds in memory (the log's reasons); data-lost and manifest reasons are
not in it, since finding them reads the log, so the file carries the flag "partial" until 9.0e and its level covers
only the reasons it carries. A store that turns read-only writes no file more (`spec.md` §3 Write or sync failure):
a file older than twice its interval says nothing about the writer, which A.18 tells a reader. The interval is the design's 60 s as a constant: no `status_interval`
configuration key is added before 9.0e, so no user promise changes here. The pressure flag is written clear and the
slot count 0; their fields are reserved in A.18.
**Open.** The configuration key (`api.md`), and losses in the status file.

## Q-779 — Group numbers once compacted log segments are removed locally
Recorded by roadmap 9.0a; settled at its review (technical), before roadmap 9.0b1 reads groups.
**Scenario.** Groups hold the sealed segments 256n to 256n + 255 in position order. Once a compacted segment is removed
locally (7 days after its compaction, Q-745), every later segment's ordinal would move down by one, so every later
group would no longer match and be written again.
**Decision.** A full group's membership never changes: a segment removed locally keeps its record, with flag bit 1
"not local", and is checked against the catalogue's description wherever it is (`format.md` A.18 Group body).
Roadmap 6.3e writes the bit with the removal; what serves those positions then (C(E), the compacted files) stays
reserved in the root.

## Q-780 — The log directory across a generation change
Recorded by roadmap 9.0a at its recheck. To be settled by roadmap 8.3, whose restore writes the restore record.
**Scenario.** After a restore the local log may hold segments of the older generation and then the new one's. A
sealed segment the start cannot read at the end of the older generation, or at the head of the newer, would be passed
by `through` if any later generation joined whatever came before it.
**Decision.** Every generation starts at sequence 1 (§1), so a record starts its generation from 1 or after lost
ranges from 1; the earlier generation's last record reaches the lowest restored end in force for it, and no
generation between held a position (`format.md` A.18 Contiguity). The root carries the restored ends of the writer's
start, as it carries the lost ranges, so a reader checks the same. Until 8.3 the writer runs in generation 0 only
(`StartOptions::generation` is refused) and the array is empty.
**Open.** Where a restore leaves the older generation's first segments on no local tier, the first record listed is
not at sequence 1 and the listing stays empty: 8.3 says whether the restored files are local, or the rule names the
restore's first restored position instead. Two further points for 8.3, neither reachable before it: the first
listed record may be of any generation (`Root::check` and `list_ready` start from the generation before its own), so
an unreadable first segment of the older generation would let the newer generation's first record lead the listing
and `through` pass every kept older segment; the rule must require the first record to be of the lowest generation
that keeps a position. And a restore that goes back before an intermediate generation that held positions is refused
a join, so the listing ends there: safe, but 8.3 decides which carried restored ends are in force.
**Pinned by.** `a_later_generation_joins_from_its_first_position_once_the_earlier_reaches_its_restored_end`,
`a_skipped_generation_joins_only_when_it_held_no_position`,
`a_root_carries_its_restored_ends_and_its_first_group_starts_its_generation` (`log/src/directory/logdir.rs`).

## Q-754 — The lookup's batch maps across a reset stamp or generation
Recorded by roadmap 11.0. To be settled by roadmap 9.0f, which adds reader processes that look up keys.
**Scenario.** A reader in another process maps a segment's batches by lookup, then the writer restarts after a power
loss and truncates the tail of that segment (§3 Recovery rule 1, a reset point). The reader's map still names the
truncated batches.
**As built.** Both lookup readers are in the writer's process, where nothing rewrites the log while it holds the
lock; a mapped batch is read again and checked before use (a batch that no longer reads the same drops the map), and
a map whose resume point lies beyond the segment's end is dropped. A map is not dropped on a new reset stamp or
generation.
**Proposed.** 9.0a drops every map when the mark's reset stamp or generation changes, with a test across a reset point.

## Q-755 — Damage found during a run recorded durably as a loss
Recorded by roadmap 11.0 at its re-check. To be settled by roadmap 6.3b, the first item to write manifest records
during a run.
**Scenario.** A sealed segment is damaged while the store runs; the slow path meets the region and records a replay
unchecked (Q-750), and the store is restarted. The start passes over the region (§3 Recovery rule 2): its facts are
out of every decision. §11 promises a `data_lost` per lost range caused by corruption, and §8 Corrupt block records a
sealed segment's damaged region by a manifest record; neither happens here, so before this decision the restart left
health `Ok` with nothing in `losses`.
**Decision (the lead's).** Health stays honest now: a damaged region that a start passes over, or that a consumer's
read of the log meets after the open, and that no lost range of the manifest records (same segment, bytes inside the
range), holds health degraded (`Reason::LocalDataUnreadable { regions }`) at every start until the region is
repaired or recorded as lost. The decisions a run made without the data are counted for that run.
**Open.** The durable record itself: writing the lost range to the manifest when the damage is found (the record §8
Corrupt block describes for a sealed segment, with the quarantine and copy), so that `losses` reports it as
`data_lost` and it can be acknowledged. It needs manifest records written during a run, which this build refuses
(Q-704), so it is more than a small change and is left to 6.3b.
**Pinned by.** `a_reading_outside_the_recent_window_is_checked_against_the_log_across_seals_restarts_and_damage`
(health after the restart) and `a_region_a_consumers_read_meets_after_the_open_is_counted`
(`kvik/tests/slow_path.rs`).

## Q-850 — The error of a second reset during one materialised call
Recorded by roadmap 9.0b1. **Settled by the lead with roadmap 9.0f: `CutExpired`, as built** (Q-1061).
**Scenario.** The query design §3.2 rule 3: a materialised result whose reset stamp changed during the read "is
computed once more under the new cut and returned with the new state; a second change is an error". It does not name
the error. Two starts that each reset the store within one call (two page-cache losses on the same boot, or a reset
and a restore) are the only way to reach it.
**Proposal, built.** `QueryError::CutExpired`, the error the same item adds for a cut that ended: the caller's remedy is
the same (start the read again), and no second new variant is added to a public enum for a case this rare. The
alternative is a variant of its own (for example `Reset { stamp }`), which an iteration needs anyway for its error item
(roadmap 9.0f: the reset with `after` and the stamp); if 9.0f adds one, this case can move to it. The review noted
that `CutExpired` hides a reset storm behind an error that reads as "too slow", which 9.0f weighs.
**Pinned by.** `a_reset_during_a_call_is_read_again_under_the_new_cut_and_a_second_is_cut_expired`
(`kvik/src/durable/read/cut.rs`).

## Q-851 — The second read of the mark after a pass
Recorded and decided by roadmap 9.0b1 (technical). To be confirmed at its review.
**Scenario.** The query design §3.1 lists the second read of the mark among the per-call work that goes. That read is
§7 Readers' comparison of the reset stamp "before and after reading": a start that truncates while a pass reads
(a page-cache loss on the same boot) would otherwise let the pass return batches that the next call voids.
**Decision.** A refresh reads the mark again only after a pass that took something (a batch, a void, a region, or a
moved resume point); a pass that took nothing has nothing to discard. An idle call reads the mark once, and a call
with new bytes twice: constant either way, so the per-call bound holds without changing §7's promise. Removing the
read altogether would make the comparison the kvik layer's and is left to the stamp rules of the iteration kind
(roadmap 9.0f).
**Pinned by.** `a_mark_that_moves_backwards_during_a_refresh_discards_the_pass`,
`a_mark_with_a_new_reset_stamp_during_a_refresh_discards_the_pass`,
`a_pass_that_moves_on_without_a_batch_is_discarded_when_the_stamp_changes` (`log/tests/reader.rs`);
`a_calls_file_system_calls_and_writes_do_not_grow_with_the_sealed_segments`,
`an_idle_call_reads_the_mark_once_and_one_that_takes_a_batch_twice` (`log/tests/reader_calls.rs`).

## Q-852 — What a reader call reads once caught up
Recorded and decided by roadmap 9.0b1 (technical). To be revisited by roadmap 6.3b, which writes the sealed-segment
records.
**Scenario.** A refresh listed `log/` twice per call (the segments to read, and the segment of the last batch to check
again) and read and parsed the whole manifest; after a year that is 105,000 names sorted per call (query design
§3.1, review B1).
**Decision.** Once a reader has read a segment, a call opens the segments from the one it follows to the mark's by
name (`<generation>-<file_id>`, under either extension, the mark's generation for those after the followed one);
only a segment not found by name is looked up in a listing, taken once for the pass, which decides as on a first
pass: listed but not openable is `SegmentMissing`, not listed is passed over, and the mark's segment is required. The
last batch is read again from the segment it was read from, by name. A first pass, and one after a generation change,
lists. The manifest is checked by path at every call: its length, its file identity (device and inode on Unix, none
elsewhere; `FsFile::identity`) and the bytes of the last record the reader parsed, at their offset; unchanged, nothing
more is read; longer, it is parsed from where the reader stopped; shorter or changed, again from its start. A record
before the last one damaged after the reader parsed it is therefore found at the next manifest rebuild or reopen, not
at the next call. Measured through the `Fs` seam on stores with 10 and 1,000 sealed segments: an idle call made 14
calls and listed 26 and 2,006 entries before, 8 calls and no listing after; a call with one new batch 21 calls and 28
and 2,008 entries before, 18 calls and no listing after, also after a seal. Each wrote the lease once before and
not at all after (the lease rules of `spec.md` §7 Readers).
**Pinned by.** `a_calls_file_system_calls_and_writes_do_not_grow_with_the_sealed_segments`,
`an_unchanged_manifest_is_read_at_its_last_record_a_longer_one_past_it_and_a_replaced_one_whole`,
`a_caught_up_reader_takes_the_lost_ranges_of_appended_records_as_a_whole_read_does`
(`log/tests/reader_calls.rs`); `ids_start_at_one_and_never_reuse_a_floor` (`kvik/tests/durable.rs`); `an_earlier_segment_listed_but_gone_when_opened_is_an_error` (`log/tests/reader.rs`).

## Q-853 — `cut_max_age`'s default and bound, and its clock
Recorded and decided by roadmap 9.0b1 (technical). To be confirmed at its review.
**Scenario.** The query design §2.1 sketches `cut_max_age: Duration, // default lease_refresh; at most the grace hour`.
A default that follows `lease_refresh` needs an optional field or a builder that sets both.
**Decision.** `ReaderConfig.cut_max_age` defaults to 30 minutes, the default `lease_refresh`, independently of a
`lease_refresh` the caller sets; a value above an hour (`LEASE_EXPIRY`, the grace period) is taken as an hour, and
zero is refused at `Reader::open` (`OpenError::Config`; the log crate takes zero as the default). By the lead's
decision at 9.0b1's review, a cut's age runs from the last manifest check that found its epoch current, not from
`hold`: a cut taken long after the reader last read the manifest could otherwise outlive both its lease and the hour
of a file retired meanwhile. A later check that finds the epoch still current renews the age, since no file of that
epoch has been retired; a check that finds a later epoch does not. The check of a held cut (`LogReader::check_cut`)
renews the lease first when it is due, and so finds a lease the writer deleted and ends every cut. Safety then rests
on three things: a cut lives at most an hour from the last time its epoch was current, retired files go only an hour
after their retirement, and a reader reads under a held cut only after its check. The internal reads of the writer's
process (`DurableReader::with_again`: the registry file after a routing change and at shutdown, a series' latest
`t`) call once more after `CutExpired`, then report it.
**Clocks.** A reader process's and the store's own reader's lease renewal and cut ages run on the environment's
monotonic clock (`Env::clock`), so an injected clock drives them; the deliverer's reader and the writer's expiry of
leases stay on the process clock. With an injected clock that stands still, the reader's lease is renewed only when its
epoch or the highest epoch read changes.
**Open for 9.0c.** `ReaderState.epoch` is the epoch the lease names (the lowest live cut's), equal to the manifest's
epoch while no cut outlives its call; 9.0c decides which one the public field means once `Snapshot` holds cuts. A
property test over histories of cuts (take, release, time, checks, a lost lease) comparing the log reader with the
model's cuts is left for 9.0c, with `Snapshot`; the example tests cover each rule.
**Pinned by.** `a_cut_max_age_above_the_grace_period_is_taken_as_the_grace_period`,
`a_cut_counts_its_age_from_the_manifest_check_that_found_its_epoch_current`,
`a_check_that_finds_a_cuts_epoch_still_current_renews_its_age`,
`a_held_cut_whose_lease_the_writer_expired_is_ended_at_its_check`,
`a_young_cut_whose_lease_the_writer_deleted_ends_at_its_check` (`log/tests/reader_calls.rs`);
`a_call_that_outlives_cut_max_age_ends_with_cut_expired_and_releases_its_cut`,
`the_stores_own_reads_call_once_more_after_a_cut_ends_and_then_report_it` (`kvik/src/durable/read/cut.rs`);
`a_cut_max_age_of_zero_is_refused` (`kvik/tests/durable.rs`).

## Q-854 — An epoch a reader has read, under a lease naming an older one
Recorded at 9.0b1's review and decided by the lead (technical: the promise of `spec.md` §7 Manifest is unchanged).
**Scenario.** A reader holds a cut at epoch 1; a record of epoch 2 is appended and read from the page cache; the
reader takes a second cut at 2, and its lease still names 1. The record is lost before its sync and the next start
takes `max(manifest, leases) + 1` = 2: one epoch number names two file sets while the cut of the first is live.
**Decision.** The lease records the highest epoch its reader has read in bytes 104..112 of the slot (`format.md`
A.15, `highest_read`; reserved bytes until now, covered by the checksum), written before any file is read under a
higher epoch, and new epochs continue above it as above `epoch` (`highest_epoch`). A lease of an older reader holds 0
there and names its own epoch as before. The design's "the lease file's bytes do not change" is kept in size and
layout of everything else.
**Pinned by.** `an_epoch_the_reader_has_read_is_never_used_again_while_an_older_cut_holds_the_lease`
(`log/tests/reader_calls.rs`); `layout` (`log/src/lease.rs`).

## Q-900 — How a reader's published views share structure
Recorded and decided by roadmap 9.0b2 (technical). To be confirmed at its review.
**Scenario.** The query design §3.5 holds the series directory in reference-counted pages of 1,024 entries and the
tail as append-only runs, so that a refresh copies only what changed. Until roadmap 9.0c and 9.0f the reader's index
holds the registry, every kept fact by series, every batch's commit time and the recovery entries; a published view
must share them, or each refresh copies the whole index.
**Decision.** Every map of the index (the registry's series, the facts by series and each series' facts by
`(t, position)`, the entries, the commit times, the recovery entries) is one persistent B-tree with nodes of at most
64 entries, reference-counted: a published view is a clone that copies no entry, and the next refresh copies only the
nodes on the paths it changes, one node a level. A level is added per 64-fold growth, so the cost grows with the
logarithm of the number of series and of a series' length, not with either. One fact alone in a refresh after a
publish, with 20 facts a series, costs the map 104 entries copied, 13,000 bytes and 10 allocations at 128 series, and
170 entries, 14,040 bytes and 12 allocations at 16,384 (its own allocations, counted by the map: a node is allocated
once, with room for the entry an insert adds, two allocations a node); each further series touched costs about one
leaf of its facts and one of the directory. A range is read lazily from both ends, so the last fact before a time is
one descent whatever the series' length. Flat pages of 1,024 would copy a whole page per page touched under a root
that grows with the series, and late facts need ordered inserts that append-only runs do not give; one structure
serves every map. A void (a reset) builds the maps it changes again, as the index did before. Counting the nodes a
held view pins against the memory cap comes with the accountant (roadmap 9.0f).
**Pinned by.** `a_refresh_after_a_publish_costs_about_the_same_whatever_the_number_of_series`,
`a_live_read_visits_a_path_whatever_the_length_of_the_series` (`kvik/src/durable/index.rs`);
`a_map_and_its_snapshots_read_as_ordered_maps_do`,
`a_change_to_a_shared_map_copies_one_node_a_level_and_leaves_the_snapshot_as_it_was`,
`a_copied_node_takes_the_insert_that_follows_without_growing`, `a_range_read_from_either_end_visits_a_path_not_the_map`
(`kvik/src/durable/read/pmap.rs`).

## Q-901 — The model checker for the single-flight refresh
Recorded and decided by roadmap 9.0b2 (technical). To be confirmed at its review.
**Scenario.** The query design §3.5 verifies the view swap and the single-flight refresh under a model checker for
interleavings, a dev-dependency on the stable toolchain. The workspace had none.
**Decision.** loom 0.7.2 (MIT), a dev-dependency only under `cfg(kvik_loom)`: normal and test builds neither compile
nor link it (it is in `Cargo.lock`, as every target-specific dependency is). Under `--cfg kvik_loom` the flight's mutex
and condition variable are loom's, and only `durable::read::flight::loom_tests` run so, in their own CI job
(`model-check`, release build, the cfg in `RUSTFLAGS`), with at most three preemptions unless loom's environment sets
another bound; two tests run four calls, so that a round's end that wakes one waiter of three fails them. loom rather than shuttle: it explores every interleaving within the bound instead of a random sample,
which a wait-and-wake protocol needs; shuttle's randomised search is the fallback if the state space outgrows it. The
flight is generic over its view, so the checked code is the code the reader runs. 0.7.3 was nine days old when this
was built and is not taken.
**Pinned by.** `every_call_gets_a_view_read_after_it_began_and_refreshes_never_overlap`,
`four_calls_all_end_with_a_view_read_after_they_began`, `only_the_leader_of_a_failed_round_reports_its_error`, `a_follower_reports_an_error_only_after_two_failed_rounds`
(`kvik/src/durable/read/flight.rs`, under `--cfg kvik_loom`).

## Q-902 — The single-flight refresh: rounds, followers and a leader's error
Recorded and decided by roadmap 9.0b2 (technical). To be confirmed at its review.
**Scenario.** The query design §3.5: calls that arrive together share one mark read; a call that began after a refresh
started joins the next one, so every call sees the mark as of its start; a follower gets the leader's result, and on
the leader's error retries once itself.
**Decision.** Refreshes run in numbered rounds, never two at once. A call needs the first round that begins after it
does: the next one if a round runs, else one it leads at once; it returns the view of that round or of a later one.
The leader runs the refresh on its own thread, outside the flight's lock, and returns its own result, error included.
A follower whose round failed takes part in the next round to end, leading it if none runs, and reports that round's
error if it fails too: refreshes are serialised, so followers that retry together share one round as any calls do,
and "retries once itself" is read as one more round. A round that succeeds or unwinds clears the error, so a follower
never reports the error of a round that failed before it began; a leader that unwinds ends its round with no result
and its followers go on to the next. Every call that reports a round's error gets the same error, an I/O error with
its operating-system code. The view a round supersedes is freed after the flight's lock is released. The refresh is outside every budget by construction; the budgets come with roadmap 9.0g.
**Pinned by.** `calls_that_arrive_during_a_refresh_share_the_next_one`,
`a_follower_leads_once_more_after_its_leaders_error_and_the_leader_reports_it`,
`a_follower_whose_second_round_fails_too_reports_that_error`,
`a_round_whose_leader_panics_ends_and_its_follower_leads_the_next`,
`a_follower_after_unwound_rounds_reports_no_error_of_a_round_before_it_began`, and the loom tests of Q-901
(`kvik/src/durable/read/flight.rs`); `concurrent_calls_equal_sequential_ones_at_the_same_cuts`,
`many_calls_of_every_kind_from_many_threads_across_a_power_loss_and_a_restart` (`kvik/src/durable/read/tests.rs`).

## Q-903 — A call's cut when calls share views
Recorded and decided by roadmap 9.0b2 (technical). To be confirmed at its review.
**Scenario.** Roadmap 9.0b1 took a cut of the log reader per call under the call-long lock and checked it at the end
with `LogReader::check_cut`, which renews a due lease. With no lock across a call, a call that took or checked a cut of
the log reader would wait for whatever refresh runs.
**Decision.** A view holds one cut of the log reader, taken by the refresh that publishes it and released once the view
is superseded and no call holds it, by the end of the next call or the next refresh. A call's cut is its view's. Its
check needs no lock: the cut has ended if a lease was found gone or changed since the view was taken (a refresh that
finds the current view's cut ended that way ends every view), or once `cut_max_age` has passed since the refresh that
took it began. Later refreshes do not renew a call's age, so calls running together end as each would alone. A call
as old as half `lease_refresh` also checks its cut through the log reader under its lock, which renews a due lease and
finds one the writer deleted (9.0b1's review, S2): at most one wait for a refresh, for such a call only. The lease
therefore names the lowest epoch of the live views: after an epoch change it names the old one until the old view is
released and the lease next rewritten, and `ReaderState.epoch` shows that; which epoch that field means is still
9.0c's (Q-853).
**Notes for 9.0c (9.0b2's review, findings 6 and 7).** The superseded view's cut is released at the end of the call
that led the round, but the lease is rewritten only by the next refresh: a reader that answers one call just after a
retirement and then sleeps names the old epoch until the writer expires its lease, an hour later, where 9.0b1 named
the new epoch at that call. It only keeps files longer, and `highest_read` is still written before anything is read
under the new epoch. 9.0c may let the release renew the lease when the epoch it names would change. Second, a call
younger than half `lease_refresh` is not checked through the log reader, which is safe while calls read memory only:
a lone call that runs to a `cut_max_age` above 45 minutes ends with a lease up to 75 minutes old; its check then finds
the lease gone and reports `CutExpired`, but once a call's work reads files it can meet a removed file first. 9.0c
renews the lease during a call that reads files, or caps `cut_max_age` at the expiry less half `lease_refresh`.
**Pinned by.** `a_call_that_outlives_cut_max_age_ends_with_cut_expired_and_releases_its_cut`,
`the_stores_own_reads_call_once_more_after_a_cut_ends_and_then_report_it` (`kvik/src/durable/read/cut.rs`);
`a_call_runs_while_another_call_holds_its_view_and_each_view_keeps_its_cut_until_released`,
`a_lease_another_calls_refresh_finds_gone_ends_a_call_in_flight`,
`a_call_as_old_as_half_lease_refresh_renews_the_lease_at_its_check_and_finds_it_gone`,
`a_long_calls_check_that_finds_the_lease_gone_ends_the_views_of_other_calls_in_flight`
(`kvik/src/durable/read/tests.rs`); `a_calls_cut_ages_from_the_start_of_the_refresh_that_published_its_view`
(`kvik/src/durable/read/cut.rs`).

## Q-904 — The recent window's shard locks
Recorded and decided by roadmap 9.0b2 (technical). To be settled by roadmap 9.0c.
**Scenario.** The query design §3.5: the recent window is not in a view; in the writer's process a query copies the
rows it needs for one series under that series' shard lock (at most 1,024 facts, filtered to its cut), and the
decision path holds a shard lock only for its own insert. The roadmap row of 9.0b2 lists the shard locks.
**Decision.** They are built with the first query that reads the recent window: the latest values in the writer's
process (roadmap 9.0c, S1). No query reads it yet. Every read of the writer's process goes through its own reader,
and the recent window (`kvik/src/durable/history.rs`) is the decider's, under the decider's lock. It records each
fact's call, not its position, so a copy filtered to a cut needs the position recorded at queueing, and its eviction
for the store-wide budget walks every series, which locks per series must not serialise: both change the decision
path, and only a reader of the window can test them. What 9.0b2 does for the promise they serve (the commit path takes
no lock a query holds across a read): no call holds a lock during its work, so the writer's own reads (the registry
file after a routing change, read under the decider's lock, and at shutdown) never wait for another call's work. They
wait for at most two refreshes: the one running when they arrive and the one they need. Each reads the bytes appended
since the last once the store's reader has caught up, but the first refresh of a reader just opened reads the whole
log, so a routing change soon after the store's reader is first used waits for that read under the decider's lock.
**Note for 9.0c.** The shard locks are built so that no read of the decision path runs under the decider's lock.
**Pinned by.** Nothing yet; 9.0c's tests.

## Q-760 — A stored `changed_at` of kind `time` above `i64::MAX`

**Scenario.** Since Q-195 (a) ingest refuses a `changed_at` above `i64::MAX` on a series of `changed_at kind` `time`.
A log written before that rule, or by other software, holds a fact row of such a series with `changed_at` = 2^63
(`format.md` A.5 column 8). Recovery and the readers accept it as stored; the export then fails on that series with
`ChangedAtOutOfRange`.
**Now.** A reader accepts any `u64` in column 8 (`format.md` A.5); only ingest checks the range. The facts block
alone does not know the series' versioning, so a check would sit with the registry, as the Q-182 check of a
definition's state names does in its own entry.
**Needed.** Whether such a row is invalid content in the log (recovery treats the batch as damaged), is kept and
reported, or stays accepted as now. Recommendation: accept it as now, because no store has recorded one under the
rule and refusing it at recovery would turn a valid older log into a loss.

## Q-761 — Long and renamed series names in the partitioned layout

**Scenario.** (a) A series name may be up to 255 bytes (`format.md` A.6 definition field 2), so its three segments
(Q-670) hold at most 253 bytes together and one at most 251. In the partitioned layout (Q-196 as amended) each is
escaped to up to three times its length, so a folder such as `device=<segment>` can reach 760 bytes and exceed the
255-byte limit most local file systems put on one file name. The longest path is 960 bytes (a 64-byte site, 759
bytes of escaped segments and the longest partial `as_of` file name, 95 bytes), so a bucket key (1,024 bytes) holds
it only while the caller's own prefix is at most 64 bytes. (b) A series renamed in
March is filed under its new name from March on, and so is January when it is exported again, so one series' months
sit in two folders.
**Now.** The layout is as settled: the folders are the escaped segments of the display name at export time;
`kvik.series_id` in every file ties a renamed series' folders together; a path the caller cannot create fails in
`open` (`ExportError::Open`).
**Needed.** (a) Whether a long name keeps its full segment (as now), or a shorter form takes its place, such as a
truncated segment with a hash, which is not reversible. (b) Whether the path should carry the series id as well
or instead of the name, which a rename does not change but a reader cannot filter by name.
Recommendation: (a) as now, as a bucket is the target; (b) as now, with the id in the metadata.

## Q-830 — Whether every poll runs every consumer's timers
Recorded by the first profiling sweep. **Decided by the lead:** a poll runs only the timers that are due, if that is
exactly equivalent; otherwise all are kept, with the reason.
**Scenario.** `Consumer::next` runs `tick` for every consumer before it hands anything out, as the model backend runs
timers at every call (Q-701 asks the same of `emit`); with 20 condition funnels this was most of a delivery's cost.
**As built: all are kept.** A threshold or on-change funnel is already evaluated on the timer only when its live-state
value may have changed since its last evaluation (a fact not yet in force then), which is its only way to change
state without a fact. A held-for or stale-for funnel is evaluated at every poll, and skipping one that is not due is
not exactly equivalent: its evaluation reads the live-state value at `now`, which a future, scheduled or interval fact
coming into force changes without a new fact; when that read cannot reach every fact it rests on, it counts an
unchecked evaluation (`DeliveryStats::evaluations_unchecked`, counted whether or not it emits) and a read of the log
(`slow_path`), so skipping it changes those counts; and the deadline that would make it due lives in the reference
model's `Runner`, which exposes none. What was removed is the per-poll work that changed nothing: runner syncing,
the copy of each consumer's map, and any evaluation for a consumer with no condition funnel (D1), and the maps of
funnels a confirmation and a cursor write built when they did not use them (D2).
**Pinned by.** The runner sync at a taken routing entry: `threshold_condition_delivers_transitions`,
`on_change_and_held_for_conditions` and `rolling_operator_and_stale_for_timer` (`kvik/tests/api.rs`); the sync
after a void, at the next evaluation: `a_condition_a_void_brings_back_into_force_is_evaluated_at_the_next_poll`
(unit tests of the deliverer); `a_condition_routed_between_polls_is_evaluated_at_the_next`
(`kvik/tests/consumers.rs`); the consumer property
tests and the differential test against the model.

## Q-831 — A segment holding a recovery entry read whole by every lookup
Recorded by the first profiling sweep. **Settled by the lead: yes**, provided lookups stay exact.
**Scenario.** A store once stopped uncleanly holds a recovery entry in its open segment. The entry voids what precedes
it in the segment above its `after` (§7 Readers), so the lookup's batch map was not trusted and every lookup scanned
the segment whole: 22 to 32 M instructions per slow reading, under the decision lock.
**As built.** The map keeps each recovery entry it met with its place among the mapped batches and its `after`. A
lookup that reads mapped batches again goes through them in file order as a scan does: it stops at the first batch
above the mark's last position, voids at each entry what it took above the entry's `after` from the batches before
it, and reads again only the batches whose event times may hold a wanted key. A mapped batch that no longer reads the
same still drops the map.
**Pinned by.** `a_segment_holding_a_recovery_entry_keeps_its_map_and_lookups_stay_exact` (voided positions taken again
by other values of the same keys; later lookups decode fewer batches than a whole read and return what a fresh
reader's first lookup returns) and `a_recovery_entry_voids_what_precedes_it_at_every_lookup` (unit tests of the log
reader).
**Damage after mapping (the lead's decision at the re-check).** The map does not read an entry's batch again, so an
entry mapped before its batch was damaged keeps voiding for that reader until a start: the facts it voids were void,
and the mapped answer is the better one. A fresh reader passes over the damaged batch (Q-836). Pinned by
`a_recovery_entry_mapped_before_its_batch_was_damaged_keeps_voiding_for_that_reader` and
`a_reader_looking_up_again_finds_what_a_fresh_reader_and_a_refresh_find`, whose damage step also flips a byte of a
recovery entry's batch, or changes a header field the scan checks under a valid checksum, which refuses the segment
for the mapped reader too (`log/src/reader/tests/lookup_properties.rs`).

## Q-832 — A start independent of the log's length without tree checkpoints
Recorded by the first profiling sweep. **Kept with Q-752** for later (the lead's decision), to be settled by roadmap
7.1b.
**Scenario.** A sealed segment's index (series, count, least and greatest `t`) says whether it can hold facts of the
recent windows, so a start could skip old segments; but the per-series counters (`recorded`, `late`, `future`) and
each batch's commit time are rebuilt from every fact.
**Open.** Either a small derived file of counters and commit times per sealed segment before 7.1b (a new file, so a
format matter), or counters that count "since this start" until then (a change to what `series_stats` promises).

## Q-833 — Recovery taking the last sealed segment's last position from its trailer alone
Recorded by the first profiling sweep. **Declined by the lead.**
**Scenario.** A start scans the last sealed segment whole, content checks included, and uses only two fields of its
trailer: 1.7 G instructions and a 64 MiB read per start once a segment is sealed.
**Decision.** Recovery keeps reading the last sealed segment, so damage there, or an unsupported block in it, is still
noticed at a start.

## Q-834 — The seal's file checksum from the block checksums
Recorded by the first profiling sweep, recorded only (the lead's decision). To be looked at by roadmap 6.3b, which
verifies segments before it publishes compaction outputs.
**Scenario.** The seal computes `file_crc32c` over a re-read of the segment (3.9 instructions a byte, 0.65 % of the
process). Combining it from the block checksums the writer computes would remove the read, and with it the last look
at the bytes before the file is named sealed. Recorded so that it is not done as a tidy-up.

## Q-835 — How much of a start may be deferred past `open`
Recorded by the first profiling sweep. **Deferred by the lead**: the deliverer's read of the log stays inside `open`.
To be settled by roadmap 5.6 (delivery looked at as a whole).
**Scenario.** The deliverer's windows are needed only when a condition funnel or a versioned series asks; with no
consumer registered its read of the log (about 40 % of a start) could run on `kvik-deliver` after `open` returns, or
not until a consumer is taken.
**Open.** That changes when `Health` and the delivery counts become true after a start, and what `Store::consumer`
costs the first time.

## Q-836 — Facts a damaged recovery entry voided read again as live
Recorded at the re-check of the first profiling sweep. To be settled by roadmap 6.3b, with Q-755.
**Scenario.** A recovery entry voids what precedes it in its segment above its `after`. A byte of the entry's batch
is then damaged. A refresh or a lookup of a reader that has not mapped the entry meets the batch as an invalid
region: it reports the region (a `Corruption` event, health degraded by Q-755), a lookup leaves the ranges of the
segment incomplete unless the caller takes the region as lost, and no void is applied, so the facts the entry voided
are taken as live and nothing says they were void. A reader that mapped the entry before the damage keeps voiding
(Q-831).
**As built.** Unchanged: the positions between the batches around the region are lost; the facts before it are live.
**Proposal.** Report the range as corrupt, as the owner ruled for corrupt manifest spans (Q-650): a region that may
hold a recovery entry makes the positions it could void, from the start of its segment to the region, corrupt rather
than live. This is a correctness promise, so it is not changed here; the cost is that damage to any batch then
withholds the earlier facts of its segment until it is repaired.
**Pinned by.** `a_recovery_entry_mapped_before_its_batch_was_damaged_keeps_voiding_for_that_reader`
(`log/src/reader/tests/lookup_properties.rs`), which asserts today's refresh.

## Q-871 — Store directory entries a stop or a power loss can undo
Recorded by the implementation of the Q-630 ruling (the crash sweep of `log/tests/recovery.rs`
`a_crash_while_creating_the_store_or_in_its_first_start_is_recovered` found it once the commit path's operations
changed); widened at its review and re-check. A technical decision.
**Scenario.** (1) Store creation writes `STORE.tmp`, renames it to `STORE` and syncs the directory (`format.md` A.12). A
process stop after the rename and before that sync, then a restart on the same boot, found `STORE` and opened the store
without syncing the directory; the first start then wrote the manifest, log and mark. A power loss later could undo the
rename, leaving files under `log/` with no `STORE`: `NotAStore`, and the store could not be opened. (2) A rebuild
renames `MANIFEST.new` over `MANIFEST` and then syncs the directory (A.14); a stop between the two, a restart on the
same boot that appends to the rebuilt manifest, then a power loss could bring the corrupt manifest back and lose every
record appended since. (3) Creation never synced the store directory's parent, so on a file system that does not carry a
new directory's entry along with a sync inside it, a power loss could leave no store directory at all after acknowledged
data (`spec.md` §3 Sync primitive asks for a sync after a create); a restore that creates the store directory
(`backup/src/restore/local.rs` `write_manifest`) never synced it either.
**Decided.** Opening a store whose `STORE` is present syncs the store directory first (`log/src/store.rs`
`StoreDir::open`; `format.md` A.12 Open), so any rename in it cut short before its directory sync, creation's or a
rebuild's, is durable before anything else is written: one directory sync more per open. Every way a store directory
comes into existence syncs its parent right after creating it, before anything is created inside
(`kvik_log::store::sync_parent`, whose error names the parent): creation (A.12 Creation), where a stop before that sync
leaves no `STORE` and the next open creates again and syncs it then, and a restore, before it writes its manifest, where
a stop before it leaves the directory empty, so the restore starts again and syncs it then. So an open of a finished
store needs no parent sync. Directories above the parent that creation makes are the caller's to make durable. A parent
that the process cannot open for reading now makes creation and a restore fail, with the parent named. No format or
interface change. Tests: `log/tests/writer.rs` `opening_a_store_makes_the_rename_of_a_creation_stopped_after_it_durable`
(a process stop after the rename), `opening_a_store_makes_a_creations_rename_durable` (a failed last sync: covered only
as far as a later directory sync can be trusted, which `spec.md` §3 does not promise, so the stop is the case that
counts) and `creating_a_store_syncs_the_parent_of_its_directory_first` (with the error naming the parent);
`backup/src/restore/local.rs` `a_restore_syncs_the_parent_of_the_store_directory_it_creates_first`;
`files/tests/rebuild.rs` `opening_the_store_makes_the_rename_of_a_rebuild_stopped_after_it_durable`. `MemFs` has no
directories (directories exist by their files), so the loss of the store directory's own entry cannot be simulated: the
parent's sync is pinned by the operations, not by a power loss.

## Q-937 — A damaged last batch of a sealed segment reported as lost to the head of the log
Recorded by the design that reconciles the durable fixes with bounded decision history (by reading, not run).
**Scenario.** Segment 1 is sealed with batches A and B; segment 2 is open with C and D, all acknowledged. B is
damaged. A sealed segment is scanned bounded, so the region gives `first_sequence_after: None`, and the finder
ends the range at the mark's last position: the report runs from B's first position to D's last. The durable
fixes' decider takes that range (at a start as well) and forgets C and D, which are intact; their replays are
recorded again.
**Now.** The public report is built as before, wide bound included. The range a process removes from the index that
answers its queries is the closed one (Q-938), so no query rests on the bound. The writer's loss watch reads the log
in parts and ends the wide range at the last position of the part it met the region in; a reader process refreshes
whole and ends it at the mark. So `losses()` in the two can differ in `last` for one such loss, after a long gap
between polls and at a start that reads more than one part. Ending the report at the next batch read removes both.
**Needed.** Whether the report ends at the next batch read (`first − 1`), as the closed range does; proposed test
`damage_to_the_last_batch_of_a_sealed_segment_loses_only_that_batch`. A separate item.

## Q-990 — A batch kept after damage, with its content kept
Decided by the lead when the scan-bound rulings (Q-651) met the scan that keeps the content its checks decode (the
bounded decision history), for review.
**Scenario.** After damage a scan reads at block boundaries through the walk, and the boundary read checks each body
once; the batch it then keeps is read without checking its bodies again (Q-651). A scan keeping content (recovery's
scan to the end, a reader's pass) needs each kept body decoded, which that read no longer did.
**Decided.** When content is kept, the judged read decodes each facts and entries body of its batch once every
check of the batch holds (`log/src/batch.rs` `read_judged_batch`, and `read_remembered_batch` for a candidate on a
chain a boundary read walked, Q-873), so a read that keeps no batch decodes nothing; the boundary read keeps its
cheaper check. The decode is the content asked for, not a check, so `ScanStats` counts it
nowhere and a scan keeping content counts what one without counts; a body kept after damage costs one check and one
decode, as before Q-651, and failed reads stay as cheap as Q-651 made them.
A decode that fails where the check passed would be a verdict the two disagree on, which Q-651's property rules out;
it ends the batch as invalid content.
**Pinned by.** `log/src/scan/tests/kept_content.rs`
`batches_kept_after_damage_keep_their_content_with_each_body_checked_once` and
`a_boundary_read_that_keeps_no_batch_decodes_no_body_for_its_content`; the segment fuzz target and
`log/tests/scan_differential.rs` compare a scan keeping content with one that does not (findings, content and counts)
on the boundary reads their inputs reach. A remembered candidate keeping its content is pinned by
`a_candidate_a_failed_boundary_read_walked_keeps_its_content` alone: the comparisons' inputs do not reliably reach that
read.

## Q-991 — What the status file's count of failed derived-file writes counts
Decided by the lead with the follow-ups to the derived files and the sync budget, for review.
**Scenario.** While the durable backend's series fold is broken, the seal hook gives no series directory. The writer
backed off and counted each attempt as a failed write, so counter 14 of the status file (`format.md` A.18,
"derived-file writes that failed") rose about once a minute although no write failed.
**Decided.** The counter counts a derived file whose write failed or was not tried for lack of space, and a failed
step of the start's preparation of `derived/` (as before, `log/tests/directories_faults.rs`
`a_start_that_backs_off_asks_to_run_again_after_its_wait`). A series directory the hook cannot give, one ahead of
the synced mark and one that makes no progress back off as before and count nothing (`log/src/writer/directories.rs`
`back_off`).
**Pinned by.** `log/tests/directories.rs`
`a_series_directory_the_hook_cannot_give_backs_off_and_counts_no_failed_write` (the hook gives none, gives one ahead of
the synced mark, or makes no progress), `a_hook_behind_the_seals_is_asked_again_after_a_growing_wait_and_then_written`
and `uncovered_bytes_stay_bounded_through_the_directories_at_any_rate`.

## Q-810 — What the manifest's layout of a compacted file may repeat from the file
Recorded by roadmap 6.2a1. **Settled by the lead** at its review.
**Scenario.** The compaction track's plan gives the publish record's layout (field 3 of `0x21`; the owner accepted
the manifest additions inside version 1) as the file's class, level, period, run, `min_t`, `max_reach`, commit
times, `fact_count`, the entry count per generation, flags including `complete` (every input element known and
carried), and the name range, and says a rebuild restates it from the file or its kind 14 object. The first
layout held neither the count per generation nor `complete`, so a layout restated after a restore could not decide
release's rule 5 or precondition 4, nor rule 3 of a merge with a restated input.
**Ruling.** Both are in the file's description, and so in its chunk index object: field 10, the positions held per
generation (16 bytes each, generations strictly ascending, counts at least 1), and field 13, present when the file
is complete. The caller supplies field 10 because the description is the first block; the writer refuses at the end
a file whose positions per generation differ from it, and the reader checks at open that its first and last
generation are those of the trailer's range and that its counts sum to `entry_count`. A merge's output is complete
only when every input is and the reader skipped no element of any (`Ignored`). Fields 11 to 13 are flags present
with no value, an exception to A.6's reading of a zero-length field. Both join `Layout` and its comparison. The
layout type stays in `kvik-chunk`: the manifest rebuild in `kvik-files` must open chunk index objects, so
`kvik-files` will depend on `kvik-chunk` whatever holds the type.
**Pinned by.** `open_refuses_positions_per_generation_that_disagree_with_the_trailer` (`chunk/tests/file_checks.rs`),
`the_writer_refuses_series_out_of_order_two_facts_of_a_key_and_flags_or_counts_that_disagree`,
`a_file_that_disagrees_with_its_recorded_layout_is_refused_naming_the_field` (`chunk/tests/file_damage.rs`), the
golden files.

## Q-811 — Format details settled while writing `format-compacted.md`
Recorded by roadmap 6.2a1. **Decided by the implementer**, for the review; each is in
`docs/format-compacted.md`.
**Scenario.** The compaction track's plan left four points of its format text to the item that writes it, and the
text did not say some details a writer and a reader must agree on.
**Decided.**
(a) A chunk directory's `block_count` is not compared with the index entry, which has none; instead the directory is
bound by `dir_crc32c`, its block must be exactly `dir_len` bytes, and its block records must lie end to end from the
chunk's first byte to the directory, which ties the count to `chunk_len − dir_len`.
(b) The description and the entries section are bound to the trailer: the trailer grows to 144 bytes with the
description's `body_crc32c` at 132 and a CRC32C over the whole entries section at 136. The section is read whole in
any case, so one checksum over its bytes binds every marker, block and padding byte and its place, and an entries
block of another generation with the same count is refused. The entries are also checked: positions rise strictly
inside the trailer's range, each inside a batch row whose `received_at` its block carries.
(c) A reader given the manifest's layout compares the trailer's counts, ranges and commit times and the
description's fields with it (Q-810). A chunk index object's `file_crc32c` is checked at every open.
(d) The chunk index object's size and hash are fields `0x8001` and `0x8002` of the segment's copy-verified record,
appended once both objects are verified, so the object is uploaded and verified before that record.
(e) The trailer's commit times are the least and greatest over the batch rows; a commit-time directory record gives
its block's first and last row; commit times not decreasing within a generation is a reader's check here (A.7 has it
by construction and does not test it), so as-of time can pick one block per generation.
(f) `has_nan` is set only on a chunk whose valued rows are all `f64`; a mixed chunk has neither bounds nor
`has_nan`.
(g) Commit-times column 4 takes the shortest of plain, run-length and delta, a tie going to the lower id, as C.9
chooses a value column; columns 1 to 3 take delta when shorter than plain, as C.9's other 64-bit columns.
(h) A chunk index object's header carries its segment's `created_at`, so the object is a function of the segment;
its blocks follow each other in a fixed order.
(i) The description's flags 11 and 12 are fields with no value; field 7 never names the file itself; a period of
length 0 or one whose end leaves `i64`, a run count of 0, or names out of order are invalid.
(j) The reader records the ignorable elements it skips (`Ignored`), and the canonical checks do not judge a file
holding any, so a scrub by an older binary does not call a newer minor version's file damaged.
**Accepted** at the review of roadmap 6.2a1. **Owed by roadmap 6.2a2**, before its memory gate:
- the writer takes a series' facts whole and the canonical check holds every fact of the file as decoded rows (80
  bytes a fact; about 175 MB for 2,000,000 facts). Section 4's caps (32 MiB minimal, 256 MiB desktop) need a writer
  that takes a series' rows in order a block at a time and a canonical check that reads from a `ReadAt` and compares
  series by series, one chunk at a time;
- `FileWriter::new` refuses batches whose commit times decrease within a generation (e), which A.7 has by
  construction but a start that found no sealed segment readable can break after the clock steps back: the run
  stalls at such a batch, named in `compaction_behind`, or is cut there.
**Pinned by.** `chunk/tests/file_damage.rs`, `chunk/tests/file_golden.rs`, the unit tests of `chunk/src/file/`.

## Q-961 — The last-uploaded marker at `Store::restore`: the switches, the errors and the reports
Recorded when the owner's ruling on Q-614 was applied. To be settled by roadmap 8.3d.
**Scenario.** The restore driver fails with five marker errors, reports `Prepared::marker_fallback` and
`Prepared::earlier_state`, and takes `RestoreConfig::accept_unmarked` and `accept_earlier_state` (Q-614, Q-962);
`Store::restore` builds no restore driver yet, so none reaches the public call.
**Proposal.** `Store::restore` reports each marker error as the additive `BackupError::Marker { generation, reason }`
(`api.md`), keeping `ListingIncomplete` as `Sink(Transient)` (Q-304), and paces its retries of a lagging listing by
`retry_first` and `retry_max`, since `MARKER_LISTING_TRIES` counts calls, not time. What an operator may accept after a
marker failure is exactly the two switches, each an option of the restore call, off by default: `accept_unmarked`
(a generation with no marker, restored without the check) and `accept_earlier_state` (Q-962);
the fallback and the earlier state reach the operator in health next to the set-aside report (with Q-613's
`unnamed`).
**Model.** Not modelled.

## Q-980 — Batch syncs paced by a timer on an idle disk: group commit on a sync budget
Recorded by the flush-pacing change; decided by the lead (2026-11-18) after its first form failed review.
**Scenario.** The adaptive interval (Q-710 to Q-713) spaced batch syncs `clamp(8 × d, floor, min_sync_interval)`
apart, `d` the smoothed sync time, inside the Q-310 two-clock schedule. On a consumer NVMe disk with an `fdatasync`
p50 of about 1.1 ms, 16 writers each sending its next batch when the last is acknowledged reached 3,090 batches a
second at p50 4.8 ms, against 15,000 at 0.9 ms for a durable PostgreSQL on the same disk; the log harness's lone
writer had p50 5.0 ms at the default and 0.32 ms with the bound at 0. The first form of this change synced a batch at
once unless calls had waited as the previous sync returned, and kept the floor only then. Its review found that this
dropped the rate bound for callers that arrive further apart than one sync time, the load of spec §13 (N unbatched
1 Hz series): on the writer core, 4,000 staggered calls a second at a 0.13 ms sync made 4,000 batch syncs a second,
and 800 at 1.1 ms made 800 (the adaptive rule: 500 and 113), with the slow-device caps and the eighth of device time
gone too.
**Decided (lead).** A promise beats the latency target. Group commit on a sync budget, a token bucket: it holds
four batch syncs (Q-985), is full at the start and refills at one per interval, the adaptive interval of Q-712 and
Q-713 unchanged. With no log sync in flight, a batch is synced at once while the budget holds a sync, and otherwise
as soon as its refill reaches one; calls that arrive meanwhile queue, and the next batch sync takes all of them (up
to `max_batch_bytes`). A call is acknowledged only once the sync covering it has returned and the mark has moved,
with the Q-630 mark sync for the first batch of each segment, the forced mark sync for a batch that is not all facts
and the timed mark sync unchanged (it was not the bottleneck: one a second). The budget is counted in billionths of
a sync, refilled by elapsed time at the interval in force, rounded down, and the wait for the next sync rounded up,
so the refill has reached a whole sync when the wait ends (`log/src/writer/pacing.rs` `Budget`). So any window holds
at most its length over the least interval, plus four, batch syncs; a writer under the budget's rate waits for no
timer; a late wake or a stall leaves its refill in the budget and stores no delay. Durability, the format, the mark
protocol and recovery do not change; the reference model has no timing. The Q-310 schedule goes (that entry's note).
**Tests.** `log/tests/pacing.rs`: the writer against a reference schedule of the rule on generated arrival patterns
(calls also arriving while a sync is in flight, by the manual clock the fake file system advances), with and without
a clock, every start, covering sync and acknowledgement compared; the window bound on any history at the least
interval, and on a steady device at its interval; a lone writer under budget never delayed; no acknowledgement
before its covering sync returns, and every segment byte durable at each acknowledgement (with the mark synced for
the first batch, Q-630); one sync covers every call that arrived during the previous one; staggered callers at the
review's rates; slow devices at the fixed schedule's ceiling; the writer thread's burst and refill; `writer.rs`
`group_commit_syncs_at_once_within_the_budget_then_batches_until_its_refill`.

## Q-981 — The interval's bounds as store keys: `sync_interval_floor` and `min_sync_interval`
Recorded and decided by the flush-pacing change, for the lead's acceptance; the default floor is the owner's (Q-710).
**Scenario.** Spec §13 names a longer `sync_interval_floor` as the remedy for wear, but the store's `LogConfig` had
no such key: a store could only lower the interval, through `min_sync_interval`, and never raise the floor above
2 ms (review of the first form, finding 2). The first form also made a floor of 0 mean back to back.
**Decided.** `log.sync_interval_floor` is a key of the store's `LogConfig` (default 2 ms; compatible, as `LogConfig`
is `#[non_exhaustive]`), passed to the log writer with `min_sync_interval`. The bounds keep their adaptive meaning:
the interval is eight times the smoothed sync time within [min(floor, `min_sync_interval`), `min_sync_interval`],
and `min_sync_interval` before the first sample; `min_sync_interval` at 0 disables the budget (batch syncs back to
back); a floor of 0 leaves the rate bounded by eight sync times, as the adaptive rule did (the first form's "floor 0
means back to back" is withdrawn). §13's remedy is a longer floor with `min_sync_interval` at least as long, which
now bounds every load.
**Tests.** `kvik/src/durable/open.rs` `the_store_passes_both_pacing_keys_to_the_log_with_the_logs_defaults`;
`log/tests/pacing.rs` `a_zero_ceiling_starts_each_batch_sync_under_load_as_the_previous_one_returns`,
`under_load_batch_syncs_run_at_the_interval_or_as_the_previous_one_returns` (a zero floor at eight sync times),
`a_floor_above_the_ceiling_fixes_the_interval_at_the_ceiling`, `the_default_floor_is_2_ms_under_a_10_ms_ceiling`.

## Q-982 — What spends the sync budget: batch syncs only, counted at the start of their turn
Recorded and decided by the flush-pacing change, for the lead's acceptance.
**Decided.** Each batch sync spends one sync of the budget, counted at the start of the writer's turn that runs it.
The sync that takes a segment into use at a timed seal and every mark sync spend nothing (as under Q-310, they took
no slot of the bound); a turn that commits nothing (no segment yet for the batch) spends nothing; calls put back from
a batch follow at once while the budget holds a sync. Counting at the turn's start means a timed mark sync run first
in the same turn can bring two batch syncs closer than the interval, by at most that mark sync; both are still
counted, so the window bound holds.
**Tests.** `log/tests/pacing.rs` `a_seal_sync_spends_nothing_of_the_budget_and_leaves_the_estimate_alone`,
`a_mark_sync_leaves_the_estimate_alone`; `log/tests/batch_bytes.rs`
`calls_put_back_follow_at_once_while_the_budget_holds_and_a_turn_that_commits_nothing_spends_nothing`.

## Q-983 — The device-time term of the budget against the latency of loads above it (OWNER)
Recorded by the flush-pacing change. For the owner.
**What the budget settles.** The bound Q-710 was ruled on holds again for every load: batch syncs at most
1/`sync_interval_floor` per second over time (500 at the default), 1/`min_sync_interval` (100) on a device whose
sync takes 1.25 ms or more, and at most about an eighth of the device's time in between. On the writer core, 4,000
staggered calls a second at a 0.13 ms sync make 504 batch syncs in a second (500 plus the burst), 800 at 1.1 ms make
118 (113 plus the burst), 1,000 at 5 ms make 104. Slow devices keep the fixed schedule's 100 a second at 1.25, 5 and
10 ms.
**What remains.** (1) The stated bound gains a burst: any window holds up to four batch syncs more than its length
over the interval (Q-985), which the Q-710 ruling did not cover. (2) The cost of the `8 × d` term to loads above the
budget's rate. Under the budget a lone writer that waits for each acknowledgement, or several such writers, run at one
sync per interval, so on a fast device they get the adaptive rule's latency back, not the sync time. Commit latency
on the shared test machine (virtual disk; batch sync p50 about 0.1 ms when syncs run back to back and 0.3 ms when
spaced; two runs each, interleaved; adaptive rule / first form / budget):

| Load, floor 2 ms | p50 ms | Calls/s | Batch syncs/s |
|---|---|---|---|
| 1 writer, back to back | 3.03 / 0.13 / 3.42 | 306 / 6,857 / 280 | 306 / 6,857 / 280 |
| 16 writers, back to back | 4.05 / 2.32 / 3.17 | 3,734 / 8,096 / 4,348 | 234 / 780 / 272 |
| 1,000 staggered 1 Hz series | 2.16 / 0.27 / 2.08 | 1,000 each | 284 / 996 / 296 |
| 16 writers, 200 calls/s | 0.56 / 0.47 / 0.50 | 200 each | 174 / 182 / 178 |

With a floor of 0 the budget's figures are the same within the spread (the interval is eight sync times either way).
On the NVMe target (sync about 1.1 ms, interval about 8.8 ms) the rule gives, by estimate: about 114 batch syncs a
second for any closed-loop load, so one writer at p50 near 8.8 ms and 16 writers near the 3,090 calls a second at
about 5 ms that started this change; staggered loads under about 110 calls a second keep the sync time. Only loads
under the budget's rate gain.
**Options.** (a) Accept as built: the bound and the device-time cap, with closed-loop loads above the rate at the
adaptive rule's latency. (b) Drop the `8 × d` term (rate 1/floor only): by the first form's figures, one writer at
about 2 ms and 16 writers at about 8,000 calls a second at about 2.3 ms on a fast device, still at most 500 batch syncs
a second; the costs are the device time (on the NVMe target a 1.1 ms sync every 2 ms, about 55 % of the device under
sustained load, against an eighth) and the slow-device caps (a 1.25 ms device up to 800 syncs a second against 100,
2 ms up to 500, 5 ms up to 200; 10 ms and above unchanged), at 4 KiB or more a sync. (c) A smaller multiplier, between
the two (Q-712's sweep: 4 halved the wait and ran about 1.7 times the syncs of 8). (d) A larger burst, which helps
only a burst of closed-loop calls, not a steady one.
**For now.** (a), the lead's decision. Settle by gate 4.9, on the run on the Linux NVMe target, which should include
a staggered open-loop load between 400 and 2,000 calls a second and report batch syncs a second.

## Q-984 — The smoothed sync time and `sync_interval` under the budget
Recorded and decided by the flush-pacing change, for the lead's acceptance.
**Decided.** The first form kept the smoothed sync time as a statistic only; under the budget it sets the interval
again (Q-712), sampled from batch syncs that succeed only, `None` before the first sample and with no clock (then the
interval stays at `min_sync_interval`). `sync_interval` (the interval in force) and `sync_time` are fields of
`kvik_log::LogStats`, not of the store's `LogStats` in `api.md`.
**Tests.** `log/tests/pacing.rs` `the_smoothed_sync_time_stays_between_the_last_sample_and_the_largest`,
`after_a_peak_the_estimate_falls_at_least_an_eighth_of_the_way_per_sync`, `a_failed_sync_leaves_the_estimate_alone`,
`only_the_batch_sync_is_timed_not_the_batch_write_or_the_mark_update`, `log_start_times_batch_syncs_on_its_clock`.

## Q-985 — The burst of the sync budget: four batch syncs
Recorded and decided by the flush-pacing change, for the lead's acceptance.
**Scenario.** A token bucket needs a depth. One sync would delay any load just under the rate at its first jitter
and give a quiet period's first few calls a wait; a deep bucket would let the device be held in back-to-back syncs
after every quiet spell and loosen the window bound.
**Decided.** Four (`kvik_log::writer::SYNC_BURST`, a constant, not a key). It is the excess over the rate in any
window, a constant (four syncs, 16 KiB at 4 KiB a sync), not a rate; after a quiet period it covers the first call
synced alone, the batch of calls that arrived during that sync and two more short clusters (several adapters
reporting on the same second) without a wait; it holds the device at most four syncs back to back (about 4.4 ms on
the NVMe target) before the rate applies; and a closed-loop writer above the rate reaches it after four syncs.
**Tests.** `pacing.rs` `a_full_budget_lets_the_burst_through_at_once_then_one_sync_per_interval`;
`log/tests/pacing.rs` `a_lone_writer_above_budget_runs_at_one_sync_per_interval_after_the_burst`,
`the_writer_thread_syncs_a_lone_writer_at_once_within_the_budget_and_holds_the_next_for_its_refill`, and the window
properties.

## Q-1000 — The fact index of a reader that serves latest values
Decided by the lead with roadmap 9.0c, for review.
**Scenario.** A reader process held every fact in one index (roadmap 5.1) and answered every call from it. Roadmap
9.0c serves `live`, `series` and `losses` from bounded latest values (Q-1003); windows, `facts`, `at`, `buckets` and
the calendar calls still read facts until the sources of roadmap 9.0f.
**Decided.** The index is built only when a call that reads facts first comes: the reader opens a second log reader,
which reads the kept log whole into the index and is brought at every round exactly to the main reader's cut
(`LogReader::refresh_upto`), so both answer at one cut; that first call waits for the read. An index read under
another reset stamp or generation than the main reader's is dropped and read again at the next round. A reader that
reads only latest values never holds one. Cost: a reader that mixes windows with latest values holds the index as
before 9.0c, beside the latest values, until 9.0f.
**Pinned by.** `kvik/src/durable/read/latest/cases.rs` `a_readers_memory_does_not_grow_with_the_facts`;
`kvik/tests/latest_memory.rs` `a_readers_heap_does_not_grow_with_the_facts_it_folds` (the heap allocated and not
freed while a reader catches up over 280,000 facts; one that also answers windows holds no more since 9.0h);
`kvik/src/durable/reader/tests.rs` `a_first_read_in_parts_builds_the_index_one_read_whole_does`;
`log/tests/reader_start.rs` `a_refresh_upto_returns_nothing_above_it_and_resumes_there`.

## Q-1001 — The registry file before a series directory that covers an entry
Decided by the lead with roadmap 9.0c, for review.
**Scenario.** A reader continuing a series directory (`format.md` A.18) reads the log only above its `through` and
takes the registry from the registry file (A.16) and the entries above. The file is written at a start, a clean
stop and a routing change. A store restarted a month ago that defines no series since has a file through the start
and directories through the last seal: entries between them would be read from the log only if every segment of the
month were read. The query design asks for the file "at a seal whose segment held an entry" and the log directory's
`has_entries` bits for the span; the bits need the catalogue check of every segment in it (two reads each).
**Decided.** The seal hook gives no series directory whose `through` is at or above an entry taken that the registry
file the process last wrote or found current does not hold: it first writes the file up to that `through` from the
deliverer's index, under a lock that every write of the file takes (routing changes and the clean stop included), so
that the newest file is never one computed before another; a failed write is no fault, and that directory is simply
not written. Since directories are written at seals, the file follows every seal whose segment held an entry, as the
design asks. A reader then holds every entry at or below a directory's `through` in the newest registry file and
reads the rest from the log above it. No file written before this change breaks the rule: series directories are
new with roadmap 9.0a and in no release.
**Pinned by.** `kvik/tests/derived.rs` `a_series_directory_never_covers_an_entry_the_registry_file_lacks`.

## Q-1002 — No series directory over a loss no lost range records (OWNER)
Recorded by roadmap 9.0c. For the owner; built as the option that keeps what readers report.
**Scenario.** A sealed segment is damaged; the store restarts and its start passes over the region, which no lost
range of the manifest records until roadmap 6.3b (Q-755); the store reports `data_lost`. Before 9.0c a reader process
read the kept log whole and reported the loss too. A reader that continues a series directory (roadmap 9.0c) reads
the log only above the directory's `through`; a directory written in that run lies above the region, so the reader
would never meet it and would report nothing.
**Built.** A run whose start met a region that no lost range records writes no series directory and removes those on
disk (`kvik/src/durable/derived.rs` `hook`), as Q-952 does for a loss found while open; readers then read the log
whole, as before.
**Options and their costs.**
(a) As built. Every start reads the whole kept log (`kvik/src/durable/start.rs`), so a damaged region in a sealed
segment is met at every start, and nothing records it until 6.3b: one damaged block turns the series directory off
for that store until then. Every reader process, and the one-shot `latest` of roadmap 9.0e, reads the whole kept log
at its first call for as long (hours on a card after a year at the home rate), and its memory stays bounded. No loss
goes unreported anywhere.
(b) Write directories over such a region, and state that a reader process reports only the losses the manifest
records, the registry file's recovery entries and what its own reads meet, as the query design's section 4 reads.
Given up: a reader process's `losses()` and health omit a `data_lost` that the store's own process reports, for as
long as the region stays unrecorded. No wrong fact is shown: the run's fold never held the lost facts, so no
directory names one.
(c) Record the range in the manifest when the start meets it: a manifest record written during a start, which is the
part of roadmap 6.3b that writes lost ranges for damage found in a run (Q-755), brought forward; on the order of 300
lines with its fault tests, most of them 6.3b's own. Readers then see the range in the manifest, as they see any
other, and directories are written as usual.
**Pinned by.** `kvik/tests/derived.rs` `a_run_over_a_region_the_start_meets_and_no_lost_range_records_writes_no_series_directory`
and `a_query_meeting_a_region_the_start_met_adds_no_loss_and_the_run_writes_no_series_directory`, which replace the
two tests that pinned a directory written in such a run.

## Q-1003 — Which series directory a reader continues
Decided by the lead with roadmap 9.0c, for review.
**Decided.** The newest series directory whose frame and `through` are of the mark's generation, with `through` at
or below the mark's durable position, written under the mark's reset stamp, with a `lost` count not saturated and at
least the positions at or below `through` that the manifest's lost ranges cover now (Q-776), together with a
registry file of that generation through at most the durable position (Q-1001), both judged against the mark and the
manifest as re-read at the first round; a newer file that fails is passed over for an older one kept. The reader
takes the registry and
every recovery entry from the file and the entries above it from the log; each series' state from its entry, judged
at the later of the file's `judged_at` and its write time; and starts its log reader after `through`
(`LogReader::start_after`: the reader reads from the segment that holds the next position and passes over every
batch at or below it). With no such pair it folds the kept log from its start, as before. A reset between the check
and the first pass breaks the state (Q-1006). This is stricter than the writer's subset of the saved-point rule
(Q-776): a file the writer keeps across a reset was written under the earlier stamp, and the reader folds the log
until the next directory: conservative, never wrong. No reader allowance (`DirectoryUnavailable`, the query design's
section 4) is built: a reader reads whatever lies above the directory, in time that grows with it and memory that
does not; the allowance and its retry (Q-773) wait for the one-shot tool of roadmap 9.0e.
**Pinned by.** `kvik/src/durable/read/latest/histories.rs` `latest_values_equal_the_models_at_every_check` (readers that
continue a directory written as the writer's fold gives it at any batch boundary); `log/tests/reader_start.rs`
`a_reader_started_after_a_position_returns_exactly_the_batches_above_it`,
`a_started_reader_reads_only_the_segments_from_the_one_holding_the_next_position`,
`a_start_after_is_refused_after_a_refresh_and_for_another_generation_or_a_position_above_the_mark`,
`a_recovery_entry_at_or_below_the_start_voids_nothing_the_start_passes_over`,
`a_start_after_the_second_last_position_of_a_segment_reads_from_that_segment`;
`kvik/src/durable/read/latest/cases.rs` `a_series_directory_under_another_reset_stamp_is_not_continued`,
`a_reader_opened_before_the_newest_directory_continues_it`,
`a_newest_directory_a_reader_cannot_continue_falls_back_to_the_one_kept_before_it`,
`a_directory_behind_a_loss_a_later_start_records_is_not_continued`,
`a_registry_file_of_another_generation_or_above_the_mark_is_passed_over`; `kvik/src/durable/read/cut.rs`
`a_reset_between_the_directorys_check_and_the_first_pass_breaks_the_state`.

## Q-1004 — What a reader finds in the log because the state cannot tell
Decided by the lead with roadmap 9.0c, for review.
**Decided.** `live` judges the series' state at the call's `now`: facts ahead that fell due are promoted, and the
candidate is the answer when the state is sure, the candidate lies at or below `now`, an observed interval reading is
ready, and its reading is valid (spec §2.3). Where the fold met the candidate the reader holds its full fact, at most
three per series (the candidate and the earliest and latest fact ahead). A candidate continued from a directory is
read once at its position, every such candidate of the view at the first need (one lookup), and kept; each is read
from the one segment that holds its position, found by the segments' header base sequences, with no listing of
`log/` and no sealed segment's index read (`LogReader::lookup_at`). Otherwise (an
unsure mark, more pending facts than the state names, a candidate above `now` or not yet ready, a reading that is not
valid) the live fact is searched for backwards in event time from `now` at the call's cut, in spans that double from
the recent window's, through the A.8 indexes; this is the only read whose cost grows with how long the series has
been quiet. What a search finds is kept, with the first time a fact it saw after it may become live (one lookup
forward), and answers again while the series folds no further fact under the same reset stamp: a value ahead that
the state cannot name is found once until the next falls due (the query design's section 4). A lookup that cannot read a place that may hold the range is `NotAvailable`, never absence. A fact a
directory names that a complete lookup does not find is `NotAvailable` too, unless a lost range the manifest records
holds its position: a segment removed before the lookups' reader opened is passed over by it as by any reader opened
since (Q-951), and the entry is the only witness. The lookups use a log reader of their own, opened at the first
lookup and renewed only by lookups; a lookup under another reset stamp or generation than the cut's, or behind it,
ends with `CutExpired`, and a plain `live` is then computed once more under a new cut (the query design's section
3.2), a snapshot's not. A reader so holds up to three leases: its own, the fact index's while it has one (Q-1000) and
the lookups'. Each names only the cuts its log reader holds, so the writer keeps nothing older than the oldest of
them; one lease per reader comes with roadmap 9.0f's sources, which replace the fact index. The lookups' mutex is
the one lock a reader holds across a read: lookups at a cut run one at a time.
**Pinned by.** `kvik/src/durable/read/latest/cases.rs`
`a_bad_latest_fact_is_passed_over_and_a_candidate_corrected_to_bad_is_searched_for_once`,
`a_candidate_above_the_readers_now_is_not_live`,
`future_and_outranked_facts_are_live_only_when_current_and_due_with_no_search`,
`a_quiet_series_candidate_is_read_once_with_every_other_unmet_one`,
`a_value_ahead_the_state_cannot_name_is_found_once_until_the_next_falls_due`; `log/tests/reader_start.rs`
`a_lookup_at_positions_reads_one_segment_each_and_finds_what_a_lookup_by_time_finds`,
`a_lookup_at_a_position_whose_segment_is_gone_never_finds_it_elsewhere`; and in `cases.rs`
`a_text_candidate_in_a_maximal_batch_is_read_whole_from_its_position`,
`a_candidate_whose_segment_is_gone_is_reported_at_every_call_never_passed_over`,
`a_directory_candidate_inside_a_recorded_lost_range_is_searched_for_not_unavailable`,
`a_candidate_read_at_its_position_is_never_taken_for_another_fact_at_its_time`; `kvik/src/durable/read/cut.rs`
`a_lookup_under_another_reset_stamp_than_its_cut_is_cut_expired`,
`a_live_value_looked_up_under_a_reset_is_read_once_more_under_the_new_cut`.

## Q-1005 — `Snapshot`: what it answers at one cut
Decided by the lead with roadmap 9.0c, for review.
**Decided.** `Reader::snapshot` (`api.md` Reading) answers `live`, `series` and `losses` at one cut and one `now`,
both taken when it is made; `state` and `now` say which. It is owned, `Send + 'static` and cheap to clone (Q-722).
On the durable backend it holds a published view and checks the cut once after each call, so it never returns a
result computed under a cut that ended and ends with `CutExpired` once the cut is `cut_max_age` old or its lease was
found gone; a `live` its state cannot tell is looked
up at its cut (Q-1004). On the model backend every result is taken when it is made. Windows and facts join it with
the sources of roadmap 9.0f; until then a dashboard's latest values and its windows do not share one cut.
**Pinned by.** `kvik/src/durable/read/latest/histories.rs` `latest_values_equal_the_models_at_every_check` (a fresh
reader's snapshot against the model at every check); `kvik/src/durable/read/latest/snapshots.rs`
`a_snapshot_answers_at_its_cut_and_now_while_facts_arrive_and_ends_once_its_cut_is_old`; `kvik/tests/threads.rs`
`handles_and_results_are_send_and_sync`.

## Q-1006 — A reader whose latest values no longer follow the log
Decided by the lead with roadmap 9.0c, for review.
**Decided.** The state breaks on a void below what the reader folded; on a range its process found lost (the
latch's closed ranges, Q-938) at or below it that no lost range of the manifest holds; on a lost range the manifest
records since the state was seeded or last compared that starts at or below it (a start found it after the reader
read past it, and the log reader never reads again what it returned); on damage its first pass meets
before any batch above the directory, which may hold positions the directory covers; and on a reset between the
directory's check and the first pass. The reader then answers `live` and `series` from its fact index (Q-1000),
which forgets the ranges found, until it is reopened, and `losses` merges both finders' reports. A range a pass finds
itself holds no fact it folded and breaks nothing. So a reader never shows a fact found lost and never hides a loss,
the rule Q-952 sets for the writer's fold. Cost: after such a fault the reader holds every fact until reopened.
**Pinned by.** `kvik/src/durable/read/latest/cases.rs`
`a_loss_found_while_open_breaks_the_state_shows_the_loss_and_never_the_lost_fact`,
`a_loss_found_in_a_batch_already_folded_breaks_the_state_and_one_the_pass_finds_does_not`,
`damage_under_a_directorys_candidate_never_shows_the_lost_fact`,
`a_continuing_reader_never_shows_a_fact_a_start_recorded_lost`; `kvik/src/durable/read/cut.rs`
`a_reset_between_the_directorys_check_and_the_first_pass_breaks_the_state`; `kvik/src/durable/read/latest/histories.rs`
`latest_values_equal_the_models_at_every_check` (histories with power losses and damaged batches, every reader's
`live`, `series` and `losses` against a fresh reader's fact index after damage); `log/tests/reader_start.rs`
`a_range_a_restart_records_below_what_was_read_reaches_a_started_reader_only_as_a_lost_range`;
`kvik/tests/durable_losses_open.rs` `a_loss_found_while_open_decides_as_a_restart_over_it_does`.

## Q-1007 — Where S1 stands: the writer's own latest values
Decided by the lead at the review of roadmap 9.0c (technical).
**Scenario.** The query design's S1 is 400 latest values from one snapshot in the writer's process, from live state
in memory: 0.2 ms on the desktop class, 2 ms on the minimal class (two targets, one per class). The writer's own
`live` and `snapshot` go through the same reader as another process's, which reads the mark file, checks the
manifest and reads the new bytes of the open segment at each round.
**Decided.** Now: in the writer's process a round whose current view is at the writer's durable position, with every
closed range of the latch taken, no fact index wanted that it lacks and no lease renewal due, publishes that view
again with no file read (the manifest changes only at a start, Q-704); and the per-call cost of `live` was cut (one
cut check per snapshot call, the state judged without a copy, the fact borrowed, the reading checked without being
built). With roadmap 9.0d's follow-up the writer's reader takes its new batches from the shared tail hub in memory
instead of the segment file, which removes the refresh that remains after a write.
**Measured** (release, best of five with the spread, a shared four-core virtual machine, 400 gauges, 1,108 sealed
segments; `kvik/tests/latest_budgets.rs`): S1 with nothing written since 0.127 ms (spread 0.049); S1 after a round of
400 readings 0.406 ms (spread 0.031), of which the refresh is about 0.3 ms; S2 0.202 ms and 0.406 ms. The first
snapshot of a reader just opened is about 0.1 s here, folding the log since the only directory. The figures on the
two hardware classes are roadmap 9.0e's.
**Known effect.** A round that reads no file does not re-check the last batch its log reader returned, so a segment
removed under an idle writer (Q-951) reaches a call that reads only latest values at the next durable write or once
the view is half a lease old, not at the next call; the loss watch and every call that reads facts check at once, and
no fact is shown that the view did not hold (`reader.rs` `view_latest`).
**Pinned by.** `kvik/src/durable/read/latest/cases.rs`
`the_writers_own_reader_reads_no_file_while_the_writer_has_made_nothing_durable_since`.

## Q-1010 — What a live session sends and when
Decided by the lead with roadmap 9.0d, for review.
**Scenario.** The query design gives `LiveSession::open`, `cancel` and `next(timeout)` and leaves the shapes of the
options, the opening cut and an empty batch open.
**Decided.** `open` registers the query; its `Opened` comes with the next batch, at that batch's cut, so a session
opens several reads at one cut. `next` computes a batch when called and returns one only when some event changed: an
idle session returns `None`. The pacing is the session's (`SessionOptions`, given to `Reader::session_with`;
`min_batch_interval` 200 ms, at least 20 ms; `poll_interval` 100 ms, at least 10 ms; less is `Invalid`), the limits
are each read's (`LiveOptions`). `Tail` opens at the cut of its `Opened` only: from a time it waits for the as-of time
of roadmap 9.0f, from a saved point for 5.3. `UpdateBatch.durable` is one `Position`, as `ReaderState.durable` is,
while the store has one log; several logs (roadmap 11.2) add the per-log form beside it (the struct is
`#[non_exhaustive]`). `Appended`, `Updated` and `Part` are enums with one variant each in 9.0d (`Facts`, `Live`,
`Live`), so that 9.0j adds its shapes without changing these.
**Pinned by.** `kvik/src/durable/read/live/tests.rs` `a_session_opens_then_brings_a_new_reading_to_latest_and_tail`;
`kvik/src/durable/read/live/cases.rs`
`a_reading_is_in_a_batch_within_the_poll_interval_in_a_reader_process_and_at_once_in_the_writers`,
`intervals_below_their_least_and_a_pattern_outside_the_grammar_are_refused`.

## Q-1011 — Resets and losses in a live session
Decided by the lead with roadmap 9.0d, for review.
**Scenario.** A `Tail` delivered facts that a reset then voids, or whose bytes are later found damaged; a `Latest`
shows a value from a batch found lost after it was shown.
**Decided.** A void the reader's refresh meets is `Reset { above }` to every live read of the session, followed in
the same batch by `Opened` (a `Latest`'s whole answer, a `Tail`'s empty one); the consumer drops what it holds above
`above`, and the `Tail` goes on with the positions assigned again. Positions found lost are `Gap { first, last }` to
every live read: a region the refresh's own pass meets (its closed range, Q-938; a region whose closed range is empty
lost no position a query shows, so no gap), a range another read of the process found since (the latch's closed
ranges, as the views forget them), and a lost range the manifest records that the session had not seen (a start
recorded it). A batch never carries a fact inside a gap or above a reset it names, a `Tail`'s consumer drops facts
it already holds inside a gap, and a `Latest` is opened again after any reset or gap, so it never shows a lost
fact. Gaps are not narrowed to what a `Tail` delivered: one below its opening drops nothing. A batch a row limit
stops (Q-1014) names every gap among what the session took, past the stop too, and takes no fact a later void or
gap drops, so its cut lies below every void after it, which the next batch applies; a round the shared tail trims
keeps each void and gap where it stood. A range another read found reaches the shared tail by the session's own
refresh before a batch could carry its fact. The `Opened` after a `Reset` is a `Latest`'s whole answer; a `Tail`'s
says only that it goes on from `above`, its consumer keeping what it holds at or below it. The model backend has
neither resets nor losses, so these kinds are checked against the history a reader opened afterwards reads, the
losses the writer and a reader opened afterwards report (each named to the `Tail`, none of its facts inside one)
and the static read of a reader opened afterwards.
**Pinned by.** `kvik/src/durable/read/live/cases.rs`
`a_reset_voids_what_the_tail_holds_above_it_and_opens_latest_again`,
`a_reset_that_voids_an_alias_takes_it_from_what_a_tail_selects_by`,
`damage_a_tail_meets_is_a_gap_and_its_fact_is_never_shown`,
`a_loss_another_read_finds_after_delivery_is_a_gap_that_takes_the_fact_back`,
`a_loss_the_start_records_is_a_gap_and_its_fact_leaves_the_tail`; `kvik/src/durable/read/latest/cases.rs`
`a_live_latest_on_a_continuing_reader_after_a_recorded_loss_is_what_a_fresh_reader_reads` (a range a start records
at or below what was folded breaks the reader's state, Q-1006, and reaches a live `Latest` as `Gap` and `Opened`);
`kvik/src/durable/read/live/stops.rs`
`a_row_limited_batch_never_carries_a_fact_the_reader_already_reports_lost`,
`a_gap_after_a_row_limit_stop_is_named_in_that_batch_and_survives_the_round_s_trim`;
`kvik/src/durable/read/live/lags.rs`
`a_loss_found_after_delivery_in_a_round_since_trimmed_is_still_named_to_a_lagging_tail`.

## Q-1012 — A live read's selection, membership and canonical order
Decided by the lead with roadmap 9.0d, for review.
**Decided.** A query selects with `Selection { select: Vec<Select> }` (`#[non_exhaustive]`; the labels design adds
`when`). A series is selected when it is not retired and some entry selects it: listed by id, a name or alias
matching a pattern of the routing grammar (a pattern outside it is `Invalid` at `open`), or `All`. A retired series
leaves under every entry, ids included, as the query design's `Left` says, so `Latest` and `Tail` agree. The
canonical order, which `Joined.index` and `Answer::Latest` follow, is the entries in order: listed ids in their order,
a pattern's or `All`'s matches by id, each series once. A `Latest` resolves the selection again when a batch brings a
registry entry, from the registry at its cut; a `Tail` takes each fact by the registry as of the fact's position
(Q-895), kept per read and rebuilt as of `above` at a reset. `Snapshot::select` gives the static members, by the same
rule.
**Pinned by.** `kvik/src/durable/read/live/differential.rs`
`every_batch_brings_each_consumer_to_the_static_read_at_its_cut_and_to_the_models` (definitions, aliases and
retirements under six selections, against the model's session, which replays the entries in position order).

## Q-1013 — The shared tail of a reader and slow consumers
Decided by the lead with roadmap 9.0d, for review.
**Scenario.** Two sessions on one reader; one is polled every 100 ms, the other not for a minute while 10,000 readings
a second arrive.
**Decided.** A reader keeps what each refresh read (facts, entries, voids and gaps, numbered as the view it
publishes) only while a session is subscribed, and only until every active session has taken it, so one tail read
serves every session and an idle reader keeps nothing. What it keeps is bounded by the largest backlog a subscribed
session allows (`max_backlog_rows`, and `max_backlog_bytes` counted as the bytes held in memory, a little above the
log's); past it the oldest refreshes keep only their voids and gaps, and a refresh that alone reads more keeps none of
its facts. A session that needs facts no longer kept, or whose backlog passes the limit of any of its `Tail`s, lags:
each `Tail` gets `Lagged { from, to }` up to the batch's cut and continues there, and each `Latest` judges every member
again (it coalesces by construction). `Lagged.from` is the position after the last one the `Tail` stood at. The lag
is the session's, not a read's, so that one batch keeps one cut. Refreshes that read nothing keep one round for a run
of them, so a subscriber not called holds a round per refresh that read something, not per refresh. A session whose
`next` was not called for `cut_max_age` (any call counts, whatever it found) is marked idle by the next refresh from
any call, holds nothing more, and its next call returns `Closed(Idle)` for each read; a read opened afterwards starts
afresh.
**Pinned by.** `kvik/src/durable/read/live/cases.rs`
`a_slow_tail_gets_lagged_to_the_mark_and_the_shared_tail_stays_within_its_backlog`,
`a_session_not_polled_for_the_cut_age_is_closed_as_idle`; `kvik/src/durable/read/live/lags.rs`; and
`kvik/src/durable/read/live/waits.rs`
`a_session_polled_through_a_spell_longer_than_the_cut_age_is_not_closed_in_either_process`,
`refreshes_that_read_nothing_keep_one_round_for_a_session_not_called`.

## Q-1014 — A row limit met inside one refresh's read
Decided by the lead with roadmap 9.0d, for review.
**Scenario.** A `Tail` with `max_rows` 10,000 and a `Latest` share a session; one refresh reads 26,000 facts. The
views a reader publishes are at the ends of its refreshes, not at the position where the row limit falls.
**Decided.** The batch's cut is the position before the fact that would pass a `Tail`'s limit, and every live read of
the session stops there; the next batch continues from it. A `Latest` at such a cut is judged as of it: its live fact
is searched for in the log among the facts at or below the cut (the search of Q-1004), with the registry as of the
cut for its members. The info a `Joined` carries at such a cut is what the view at the end of that refresh holds, so
its `latest_t` may be later than the cut; only a joining series' metadata is affected.
**Pinned by.** `kvik/src/durable/read/live/cases.rs`
`a_row_limit_stops_every_live_read_of_the_session_at_one_cut_inside_the_read`,
`a_maximal_batch_reaches_the_tail_whole_in_row_limited_batches`.

## Q-1015 — Waking, pacing and due times of a live session
Decided by the lead with roadmap 9.0d, for review.
**Decided.** In the writer's process a session waits on the writer's progress (the notification its delivery thread
waits on, raised at every writer turn that published something, the mark's moves among them) and reads the mark only
when it moved; in a reader process it sleeps and reads the mark once `poll_interval` has passed on the environment's
monotonic clock, so an injected clock decides freshness in tests. Within a second of its last full refresh such a
poll reads the mark alone and asks nothing more when it is unchanged and nothing is due; a change to the manifest
alone (a range recorded while the writer runs) waits for the next full refresh, at most a second. `next` sleeps out
the rest of `min_batch_interval`, measured on the same clock, and the whole timeout with no live read, so it never
turns while it waits.
A `Latest` member is due at the earliest of: a fact ahead falling due, a candidate above `now` or an interval reading
becoming ready, and its staleness passing (in event time, or from the refreshing commit in the writer's process,
spec §5.4); at once, and so at every batch interval, when its state cannot tell. A batch judges again only the
members it brought facts or entries for, those due, and those whose live staleness a call of the writer's process
refreshed since the last batch (its acknowledgement can follow the batch that brought its fact); so an idle `Latest`
with nothing due costs nothing and one with a due time costs one judgement per due time.
**Pinned by.** `kvik/src/durable/read/live/cases.rs`
`staleness_and_a_value_falling_due_update_latest_with_no_new_fact`,
`a_reading_is_in_a_batch_within_the_poll_interval_in_a_reader_process_and_at_once_in_the_writers`;
`kvik/src/durable/read/live/waits.rs` `next_sleeps_out_the_least_batch_interval_and_a_session_with_no_live_read`,
`a_waiting_next_in_the_writer_s_process_returns_soon_after_a_reading`,
`an_acknowledgement_after_the_batch_that_carried_its_fact_makes_latest_fresh_again`; the budget S13 in
`kvik/tests/live_budgets.rs` (Q-1017).

## Q-1016 — A live read and an unavailable segment or tier
Decided by the lead with roadmap 9.0d, for review.
**Decided.** A session reads nothing but the shared tail: the segments from the reader's position to the mark. A
lagging `Tail` never reads back; its consumer reads the skipped span statically and meets the stated errors there. An
error of the shared read is returned by `next` at every call it lasts, never passed over (a failed refresh leaves the
next poll due, in a reader process too): `NotAvailable` for a
segment the reader stands in that is missing (Q-951, until a restart), and the tier-unavailable error once cold
segments exist (roadmap 8.1g).
**Pinned by.** `kvik/src/durable/read/live/cases.rs` `a_missing_segment_under_the_tail_is_an_error_at_every_next`.

## Q-1017 — The empty poll of a reader process against S13
Decided by the lead with roadmap 9.0d, for review; a performance target is the lead's.
**Scenario.** S13 asks 50 µs for an empty poll of a live session (200 µs on the minimal class). A reader process's
full refresh finding nothing costs about that alone: 54 to 60 µs on a shared four-core machine in
`kvik/tests/live_budgets.rs`, 300 rounds of 400 series with a seal every few. Built on it, the empty poll measured 56
µs best and 89 to 101 µs in the review (99 to 128 µs with a `Latest` of 400 series).
**Decided.** A reader process's poll reads the mark alone within a second of its last full refresh and asks nothing
more when it is unchanged, nothing is due and no loss was found since (Q-1015); refreshes that read nothing with a
session subscribed keep no round of their own (Q-1013). Measured once afterwards on the same machine (best mean of
five runs of 50 polls 21 ms apart, so one full refresh per run): `Tail` 31 µs (31 to 40), `Tail` and a `Latest` of
400 series 36 µs (36 to 68); the writer's process 4 and 12 µs. What remains over 50 µs is the full refresh a poll runs
once a second, 60 µs here and up to 108 µs for the slowest single poll, and the run disturbed by other work (68 µs).
These are figures of one shared machine, not of the minimal class, which was not measured; the year of seals the
design's budget test asks for is roadmap 9.0e's, which measures again.
**Measured by 9.0e** on a year of seals: 64–111 µs for a reader process's `Tail`, which is this machine's wake-up
cost rather than work that grows with the log (Q-1054).
**Pinned by.** `kvik/tests/live_budgets.rs` (a measurement, ignored by default);
`log/src/reader/tests/parts_and_lookup.rs` `the_mark_alone_shows_whether_a_refresh_would_find_a_new_batch`.

## Q-880 — The label rules and limits as a promise (OWNER)
Recorded by the series labels design; applied by the labels spec-and-model step (L1) with the design's proposal.
For the owner.
**Scenario.** Keys `[a-z][a-z0-9_]*` of 1 to 63 bytes, values of 1 to 255 bytes compared byte for byte, at most 64
keys and `limits.label_versions_per_series` (1,024) versions a series, `limits.label_versions` (1,000,000; 100,000 on
the minimal class) and `limits.label_bytes` (32 MiB; 4 MiB) a store. Nothing is deleted, so a limit is a ceiling until
configuration raises it, and a series at its cap cannot be corrected; the store limit costs up to about 85 MB (9 MB
minimal) in each process that uses labels (design §3.2).
**Consequences the owner is asked to accept** (both permanent for a user, both following from the design):
(1) the 64 keys a series may ever use are a format constant, not configuration (Q-1029), and every version counts
its key, so a key that was only ever removed or withdrawn, such as a mistyped key taken back at once, uses one of
the 64 for good; (2) orphaned label entries count towards `limits.label_versions` and `limits.label_bytes`
(Q-1022), so after a lost definition the lost series' labels take room that nothing gives back short of raising the
limit.
**Options.** (a) Accept, the per-series cap configurable (the design's proposal, applied); costs: (1) and (2) above.
(b) Higher defaults, at the memory cost per process. (c) A way to fold old versions away, which breaks "nothing is
deleted" and history. (d) For (1): count only keys that were ever set, so a key taken back costs nothing; the index
then tracks keys apart from versions, and a series may hold more distinct keys in its history than 64. (e) For (2):
leave orphaned entries out of the limits; the memory figure per process then no longer bounds what a process holds.
**Model.** (a): `labels::LabelLimits`, `MAX_KEYS_PER_SERIES`; `tests/labels.rs`
`s2_1_labels_the_series_and_store_limits_refuse_a_call_that_adds_past_them`,
`s2_1_labels_a_series_uses_at_most_64_keys_ever` (keys only removed or withdrawn included),
`s2_1_labels_an_orphaned_entry_is_kept_and_counted_outside_every_view`.

## Q-881 — Reserved label keys (OWNER)
Recorded by the series labels design; applied by the labels spec-and-model step (L1) with the design's proposal.
For the owner.
**Scenario.** `site`, `area`, `device`, `metric`, `month` and `series` are the export layout's folder names, and
`area`, `device` and `metric` are the keys a user reaches for first. Every key starting `kvik_` is kept for the engine.
**Options.** (a) Reserve them and refuse a call that sets one (`LabelError::Reserved`), and from the static label
selection step (L3) read the three name segments in selectors as fixed labels under `area`, `device` and `metric` (the proposal, applied for the
refusal). (b) Allow them, and rename the export folders, which the owner's ruling on the layout fixed. (c) Allow
them, and let the export layout win in a conflict, so a label could disagree with the folder it is exported under.
**Model.** (a): `labels::RESERVED_KEYS`; `tests/labels.rs`
`s2_1_labels_a_bad_item_refuses_the_whole_call_and_appends_nothing`. The fixed labels are L3's.

## Q-884 — Labels in a definition are initial only (OWNER)
Recorded by the series labels design; applied by the labels spec-and-model step (L1) with the design's proposal
(spec and `api.md`; the labels engine step, L2, builds it). For the owner.
**Scenario.** An adapter defines its series at every start with `SeriesSpec::label`. Defining an existing series
again with other labels either changes them or does not.
**Options.** (a) Initial only: the definition's labels are written as an entry effective *always* in the
definition's batch; defining again returns the id, leaves the labels alone and says so (`labels_ignored`), and
`declare_labels` is the start-up form (the proposal, applied). (b) A definition again sets the labels it carries from
now, as `declare_labels` does; every start then writes entries whenever two adapters disagree.
**Consequences of (a) as the interface stands.** (1) Only `define_many` reports ignored labels (`Defined {
labels_ignored }`); `define_series` returns a bare `SeriesId` and cannot say so, so the spec's "says so" holds for
the batch call only. Options: (i) leave `define_series` as it is and document that it does not report (applied);
(ii) give it a result carrying `labels_ignored`, a breaking change to a call accepted at gate 3.3; (iii) log a
warning through `tracing`, which a program cannot test. (2) Whether `labels_ignored` is set when the labels carried
equal those in force: (i) set whenever the call carried labels (applied), simple and true of the call; (ii) set only
when they differ from those in force now, which tells an adapter whether its labels drifted but makes the result
depend on the clock and on earlier label calls.
**Model.** `LabelLog::declare`; `tests/labels.rs` `s2_1_labels_declare_sets_only_the_keys_whose_value_now_differs`.
Definition labels are a `set_labels_many` call with `Effective::Always` in the model.

## Q-1020 — Label entries in the model kept apart from registry events
Recorded and decided by the lead with the labels spec-and-model step (L1).
**Scenario.** Type 10 entries are log entries of the series' own log, but the registry file does not hold them
(spec §2.1 Labels, Q-892). A `RegistryEvent` variant for them would also change every exhaustive conversion of
registry events (the log crate's model differential) ahead of L2's encoding.
**Decided.** The model folds label entries in `labels::LabelLog`, a list of the kept log's label entries in position
order, beside the `Registry` built from the same kept log; every label view takes the registry to tell orphaned
entries apart. The engine's model (`engine::Model`) gains its label calls with the labels engine step (L2), which compares both
backends with `LabelLog`. **Tests.** `tests/labels.rs` throughout.

## Q-1021 — A label call that appends nothing, and the outcome's position
Recorded and decided by the lead with the labels spec-and-model step (L1).
**Scenario.** An entry holds 1 to 64 items (`format.md` A.6 type 10). A call with an empty change, or whose every
item is a duplicate, has nothing to write.
**Decided.** Such a change writes no entry and the call succeeds: `LabelOutcome { position: None, recorded: 0, .. }`.
A call that writes entries reports the position of its last entry, the position at which the whole change is known.
Each change of `set_labels_many` that keeps any item is one entry, at consecutive positions in one batch.
When it resolves is Q-1040.
**Tests.** `tests/labels.rs` `s2_1_labels_an_empty_change_appends_nothing`,
`s2_1_labels_a_duplicate_is_only_the_same_item_at_the_same_key_and_effective_time`,
`s2_1_labels_a_duplicate_counts_earlier_entries_of_the_same_batch`.

## Q-1022 — What `limits.label_bytes` and `limits.label_versions` count
Recorded and decided by the lead with the labels spec-and-model step (L1).
**Scenario.** The design states the byte limit as "the bytes of all label entries" without a measure.
**Decided.** Each entry counts its encoded length in an entries block (`format.md` A.6): its 8-byte header and its
fields with their headers and padding to 4, the entry padded to 8 (a set of `room` = `hall` effective at a time is 48
bytes). A version is one item. Both count every label entry the kept log holds, orphaned ones included, since the
engine keeps them in its label parts and index structures alike. The measure does not depend on how a part or a
block is laid out, so both backends and every process agree. **Tests.** `tests/labels.rs`
`s2_1_labels_an_entry_is_measured_by_its_encoded_length`,
`s2_1_labels_an_orphaned_entry_is_kept_and_counted_outside_every_view`.

## Q-1023 — The order of a label call's checks
Recorded and decided by the lead with the labels spec-and-model step (L1).
**Decided.** A call is checked change by change in the order given: the series (`UnknownSeries`), then each item in
order for the key grammar (`BadKey`), a reserved key (`Reserved`), the value (`BadValue`) and a key given twice
(`Duplicate`), then the item count (`TooManyItems`). Only then does the duplicate rule drop items, and the limits are
checked on what remains, for the whole call: the keys of each series it adds to (`TooManyKeys`), their versions
(`SeriesLimit`), then the store's versions and bytes (`StoreLimit`). The first failure is reported and nothing is
appended. **Tests.** `tests/labels.rs` `s2_1_labels_a_bad_item_refuses_the_whole_call_and_appends_nothing`,
`s2_1_labels_the_series_and_store_limits_refuse_a_call_that_adds_past_them`.

## Q-1024 — A limit refuses only a call that adds versions
Recorded and decided by the lead with the labels spec-and-model step (L1).
**Scenario.** Limits are checked at the call only, so after a limit is lowered the kept log can hold more than it
allows (spec §2.1 Labels). A call whose items are all duplicates adds nothing.
**Decided.** A limit refuses a call only when the versions it would add take a count above the limit; the
per-series limits apply to the series the call adds to. A call that appends nothing succeeds whatever the counts,
so re-asserting what is recorded never fails. **Tests.** `tests/labels.rs`
`s2_1_labels_replay_under_a_lowered_limit_applies_every_entry`.

## Q-1025 — Duplicates within one batch of label changes
Recorded and decided by the lead with the labels spec-and-model step (L1).
**Decided.** As for facts (spec §2.4), the duplicate rule sees the earlier entries of the same batch: two changes of
one `set_labels_many` call with the same item at the same `(series, key, from)` record it once. Two changes for one
series and effective time are two entries; a key given twice within one change is refused (`Duplicate`).
**Tests.** `tests/labels.rs` `s2_1_labels_a_duplicate_counts_earlier_entries_of_the_same_batch`.

## Q-1026 — A withdrawal with nothing to withdraw
Recorded and decided by the lead with the labels spec-and-model step (L1).
**Decided.** A withdrawal at a `(series, key, from)` with no version is recorded, as the caller asserted it, and reads
as no change; a withdrawal whose current version is already a withdrawal is a duplicate. A later set or remove at
the same key and time reinstates a change (spec §2.3 Label views). **Tests.** `tests/labels.rs`
`s2_1_labels_a_withdrawal_reads_as_no_change_whatever_is_corrected_before_it`,
`s2_1_labels_a_withdrawal_reads_as_if_no_change_was_made_and_a_later_set_reinstates_it`.

## Q-1027 — What `declare_labels` checks and reports
Recorded and decided by the lead with the labels spec-and-model step (L1).
**Decided.** Every listed pair is checked as a set item (grammar, reservation, a key once, at most 64) before any is
compared, so a bad list appends nothing even where its keys already hold their values. The sets are effective at
`now`, so a `declare_labels` at the effective time of an existing version is a correction of it. Keys already in force
with the listed value count as duplicates in the outcome. **Tests.** `tests/labels.rs`
`s2_1_labels_declare_sets_only_the_keys_whose_value_now_differs`.

## Q-1028 — Label views of orphaned and retired series, and as of a time
Recorded and decided by the lead with the labels spec-and-model step (L1).
**Decided.** A series with no definition in the kept log has no labels in any view, no history, and is in no count of
`label_keys` or `label_values`; its entries are kept and counted (`orphaned_label_entries`). A retired series keeps
its labels, takes corrections and is counted. `labels_at` under `AsOfTime(T)` takes the versions at or below the last
label position whose batch commit time is at most T: the label versions at or below spec §2.3's P are the same, as
commit times never decrease. Versions of one entry are listed in the order of its items. **Tests.**
`tests/labels.rs` `s2_1_labels_an_orphaned_entry_is_kept_and_counted_outside_every_view`,
`s2_1_labels_a_retired_series_keeps_and_still_takes_labels`,
`s2_3_label_views_as_of_time_takes_the_versions_committed_by_then`.

## Q-1029 — The 80 % notices and the fixed key limit
Recorded and decided by the lead with the labels spec-and-model step (L1).
**Decided.** A notice is given when a count is at least four fifths of its limit (count × 5 ≥ limit × 4, in integers),
for the store's versions and bytes and for each defined series' versions; a start reports them and never refuses its
own log. The 64 keys a series may ever use are a format constant, not configuration: every version counts its key,
removals and withdrawals included. **Tests.** `tests/labels.rs` `s2_1_labels_notices_name_limits_at_80_percent`,
`s2_1_labels_a_series_uses_at_most_64_keys_ever`.

## Q-1040 — A call that writes nothing resolves once what it duplicates is synced
Recorded with the fix round of the labels spec-and-model step (L1); decided by the lead.
**Scenario.** A label call whose every item is a duplicate writes nothing, so it has no position to wait for. If
the version it duplicates is in a batch still being synced (a second caller, or a retry after `OutcomeUnknown`),
reporting success at once could tell a caller a version is recorded that a power loss then removes, against "what
a caller was told succeeded is recorded" and "resolves once it is durable".
**Decided.** A label call resolves only once the synced durable mark is at or above both its own last entry and
every version one of its items was dropped against; a `declare_labels` also waits for the versions in force it
matched. This keeps the existing promise; nothing is written for a duplicate. The model gives the position in
`LabelOutcome::resolves_at` (`None` only for a call with no item at all). **Tests.** `tests/labels.rs`
`s2_1_labels_a_call_resolves_at_its_last_entry_or_the_versions_it_duplicates`,
`s2_1_labels_a_duplicate_is_only_the_same_item_at_the_same_key_and_effective_time`.

## Q-1041 — A definition whose labels pass a limit
Recorded with the fix round of the labels spec-and-model step (L1); decided by the lead.
**Scenario.** A definition carrying labels appends a label change, which can pass `limits.label_versions`,
`limits.label_bytes` or the per-series limit; only a grammar refusal was named (`DefineError::Invalid { field:
"labels" }`).
**Decided.** The definition is refused whole and appends nothing, as one bad item refuses a label call: a grammar
or reservation refusal is `DefineError::Invalid { field: "labels", .. }`, a limit `DefineError::LabelLimit(
LabelError)` carrying the label error. `define_many` is refused whole likewise. A definition is never written
without the labels it carried. **Tests.** None in the model, which has no definition with labels; the labels engine
step (L2) pins it on both backends.

## Q-1042 — Label part file ids are never reused
Recorded with the fix round of the labels spec-and-model step (L1); decided by the lead (`format.md` A.19).
**Scenario.** With "the highest present plus one", an invalid part that is the highest present could be removed
before its replacement is written; the replacement then takes the same id and range, so the same name and bucket
key with other content, where the bucket creates a key only when absent and the tie rule cannot tell them apart.
**Decided.** Two rules, the simplest that keep names and keys unique for ever. (1) The id is A.12's for every kind:
1 + the highest of kind 18 among local names, ids named by any manifest record (each upload is recorded there) and
any rebuild record's floor of kind 18. (2) A part is removed locally, whatever the reason (invalid, merged, set
aside), only once a part with a higher `file_id` is durable (synced, its directory synced). So the highest local id
never falls and no local name is used twice; with a backup, every uploaded id is named by the manifest, so no key
is taken twice, and a restore starts a new generation, which is in every name. When nothing replaces a removed part
(every label change of its range lost, or an in-place restore to a position below every label change), a part with
no entries is written to carry the higher id first; the layout allows zero entries blocks. A directory emptied by
hand on a store without a backup may start the ids again, which no name outside the store can meet. **Residual, a
double fault:** a part uploaded, its copy-verified record (`format.md` A.14 `0x27`) not yet synced, and the
`registry/labels/` directory then lost or emptied while the store stays in its generation: the id can be taken
again. Rebuilt from the same kept log the bytes are the same and the upload matches; where they differ the bucket
refuses the second create (it creates a key only where absent), so the fault is loud, never silent.

## Q-1043 — A label part's `through` at a start
Recorded with the fix round of the labels spec-and-model step (L1). Open, for the labels engine step (L2).
**Scenario.** A.16 makes the registry file's `through` a source of F (Recovery rule 3): the file never names a
position above the last synced mark, so one that does shows a loss. A label part obeys the same bound, but A.19
said nothing about it at a start.
**Proposal.** Treat it as the registry file is treated: a part's `through` is a source of F, and a part above the
recovered log is then invalid and rebuilt from what is kept. Three conditions for the engine step, or the evidence
can be false: (1) only parts of the current generation count, since a part of an older generation above its
restored end is set aside, not lost; (2) a part that waits for its replacement under Q-1042 and overlaps a recorded
lost range names positions already reported, and must not report them again; (3) the start opens no part, and a
`through` read from a file name alone is unauthenticated, where the registry file's is read from a checked file:
either open and check the highest part of the generation at a start (one open), or do not use the name as
evidence. Until L2 decides, nothing depends on it: the model has no parts.

## Q-1031 — A recovery entry in a segment that cannot be read while a copy may still come
Recorded and decided by the recovery-history fix, for the lead's acceptance.
**Scenario.** A power loss truncates acknowledged facts at positions 3 and 4 that a consumer has received; the start
writes a recovery entry at 3 (`after` = 2) and assigns 4 again. The segment holding the entry is sealed and uploaded,
the bucket stops answering for its object, and a scrub finds the hot copy damaged. The segment is quarantined, and
no lost range is recorded, since the lost-object rule has not judged the object lost (§8 Corrupt block, Self-repair
stage 1). The model built reset points from readable entries only, so the entry's reset point vanished, and the
consumer was given a different fact at 4 with no `forget_above` below it, against principle 5 and §4 Resync.
**Decided.** A recovery entry is a reset point unless a recorded lost range holds it (that range is then the reset
point, as §4 Resync says for a range that may have held an entry); whether its segment can be read does not matter,
since the registry file and each tree checkpoint keep every recovery entry (§11 Health). No promise changes: §4
Resync now says so in a clause. The durable backend had the same defect at a start that cannot read the entry's
segment; Q-1033 mends it.
**Tests.** `tests/recovery_histories.rs`
`a_recovery_entry_in_a_quarantined_segment_awaiting_its_copy_stays_a_reset_point`,
`a_recovery_entry_in_a_recorded_lost_range_gives_only_the_range_reset_point`,
`a_recovery_entry_in_a_damaged_sealed_segment_stays_a_reset_point`,
`a_recovery_entry_in_a_removed_sealed_segment_stays_a_reset_point`, and the reuse property
`s4_a_position_is_given_again_only_after_a_forget_above_below_it`.

## Q-1032 — The next position after a start that cannot read a published segment
Recorded and decided by the self-repair history fix, for the lead's acceptance.
**Scenario.** A sealed segment holding positions 8 to 10 leaves the fast disk and the bucket stops answering for its
object while it cannot be reached, so no lookup counts and the lost-object rule has not spoken. The engine restarts.
The model took the next position from the readable entries and the lost ranges only, so the next batch was given
8 to 10 again; the segment then answered, so two files held different facts at the same positions, against §3
(positions are assigned once, except above K after a truncation, rule 3) and principle 5.
**Decided.** A start never assigns a position that a published segment holds, whether or not it can be read now: the
manifest lists the segment and its range. No promise changes; the model now does what §3 already says. The durable
log does not have the defect: the review's probe of 48 combinations of stop, harm to the sealed segment (damaged,
removed, zeroed), mark kept or lost and a later segment found no position assigned again; where the mark is lost
and the segment gone, the other sources of F (§3 Recovery rule 3, Q-116 (b)) hold the positions. Not covered: the
segment, the mark, the manifest record, the registry file and every cursor file all lost at once, where §3's F has
no source.
**Tests.** `tests/self_repair/properties.rs`
`a_start_does_not_assign_again_the_positions_of_a_segment_awaiting_its_copy`,
`self_repair_history_a_segment_found_and_lost_again_after_a_start_keeps_the_rulings` (the shrunk case), and
`s8_self_repair_histories_keep_the_rulings`.

## Q-1033 — The durable engine's reset points from recovery entries it cannot read
Decided by the lead (a technical decision: it keeps the promise of §4 Resync and §11 Health).
**Scenario.** A consumer holds unconfirmed facts at 5 and 6; a power loss takes them with the mark's updates; the
start writes a recovery entry at 5 (`after` = 4) and assigns 6 again; the segment is sealed and the store shut down
cleanly. The sealed segment is then damaged in the entry's batch, removed, or its body zeroed. At the next start the
durable deliverer took reset points from the entries it could read and the recorded lost ranges only; the damaged
region is passed over and not recorded (Q-755), and A.16 rewrote the registry file from the kept log, so the entry
left the file. The consumer was given the new fact at 6 with no `forget_above` (damaged), or never told to forget 5
and 6 (removed, zeroed).
**Decided.** No layout changes; only the rule for what a rewrite keeps. (a) Every write of the registry file keeps
the recovery entries of the latest valid file of the same generation that the new content lacks, up to its
`through` (`kvik_log::registry::write`, `RegistryFile::carry_recovery`), and the derived check at a start compares
the file with the kept log's entries plus those carried (`format.md` A.16). (b) The deliverer takes a reset point
from every recovery entry in the registry file of the current generation that no recorded lost range holds, as from
one in the log, with its consumers' lost cursors (`kvik/src/durable/deliver/mod.rs`). The generation compared is the
writer's own, not that of the newest batch read, so a restored generation whose readable log holds only earlier
generations' batches still takes its carried entries. An unreadable registry file
loses the carried entries with it, as it loses the id floor today.
**Health.** With the segment removed the start reports no loss and health `Ok`: that is Q-951's limit, ruled by the
owner to stand until roadmap 6.3b. The zeroed body is Q-1034.
**Tests.** `kvik/tests/durable_reset_points.rs` (the review's reproduction: control, damaged, removed, zeroed; the
registry file keeps the entry and passes the derived check, and a confirmed reset point is not sent again at a
later start); `kvik/src/durable/deliver/tests.rs`
`kept_resets_are_the_registry_files_recovery_entries_of_the_writers_generation_outside_lost_ranges`;
`log/src/registry.rs`
`a_rewrite_keeps_recovery_entries_the_kept_log_no_longer_holds`,
`write_carries_the_recovery_entries_of_the_latest_file`; the model's side in Q-1031.

## Q-1034 — A sealed segment whose body is zeroed, passed over with health `Ok`
Recorded by the durable reset-point fix, from the review's probe; to be settled by roadmap 6.3b with Q-755.
**Scenario.** A sealed segment's body is zeroed after its header while the store is stopped. The next start passes
over it with no loss listed and health `Ok`, as for a removed segment (Q-951). §11 Health says a sealed segment that
no manifest record lists and that fails its checks degrades health (`segment_damaged`); a zeroed body fails them.
**For now.** As built; the reset points it held are kept by Q-1033. Proposed: report it as `segment_damaged`, or
rule it a removal under Q-951; 6.3b's removal records decide which.

## Q-1050 — The tool's command line
Decided by the lead with roadmap 9.0e, for review.
**Scenario.** The query design's section 6 gives the commands and `--format` and `--zone`, and leaves open how the
store is named, how a live command ends, and `tail --since` and `--once`, while a `Tail` opens at its cut only until
the as-of time of roadmap 9.0f (Q-1010).
**Decided.** `--store <dir>`, or `KVIK_STORE`; patterns are the `Select::Names` patterns. A live command runs until
stopped, or for `--for <duration>`; `--interval` (default 200 ms, at least 20 ms) is the session's least batch
interval, and its poll interval is half of it (at least 10 ms). `tail --since` and `--once` are refused with the
reason until 9.0f gives a tail from a time; without them `--once` would have no recent facts to stop after. A
usage error exits with status 2 and the usage text, a malformed query (a pattern outside the grammar) with status 2,
output closed by its reader (a broken pipe) with status 0, any other error with status 1; an error is never printed
as an empty answer. `series` shows every name of a series (the defining name first) in place of a separate alias
column. Tables, CSV with a header line, and JSON lines with numbers and booleans bare; times in RFC 3339. A live
`left` row names the series in `id` and `name`.
**Pinned by.** `cli/src/args.rs` tests; `cli/tests/output.rs`.

## Q-1051 — What `health` and `info` read besides the reader
Decided by the lead with roadmap 9.0e, for review.
**Scenario.** The design builds the tool "on `Reader` alone", but `health` shows segments, leases, the series
directory and the writer's status file, and `info` the descriptor and files by kind, none of which `Reader` gives.
**Decided.** Facts, latest values, losses and the reader's position come from `Reader`; the file inventory comes from
listing the store directory, and the descriptor, leases (A.15), newest series directory and status file (A.18) and
the newest registry file (A.16) from `kvik-log`'s read-only decoders. None opens a file for writing; the tool's only
write is the reader's lease (Q-727). A missing or invalid status file is shown as such, the file's age beside it.
The site zone and tzdata version in force are the newest registry file's last entries of each, else the
descriptor's. Query slots and compaction join `health` with roadmaps 9.0g and 6.2a2.
**Pinned by.** `cli/tests/writes.rs` `the_tool_writes_only_under_leases_with_the_writer_running_and_shut` (every
file-system call that can change a file or take a lock names a path under `leases/`); `cli/tests/output.rs`.

## Q-1052 — How S1 to S3 and S13 are measured on a year of seals
Decided by the lead with roadmap 9.0e, for review.
**Scenario.** The design asks for S1 to S3 and S13 on a store a year old, on both hardware classes; a year's bytes
at the home rate (about 261 GB) cannot be written on a development machine, and the work runs on one shared cloud
machine.
**Decided.** The store holds a year's count of seals (105,120 at the five-minute default; 105,687 built), one reading
each, and an open segment at full size (five minutes at 45 readings a second, one a batch): the reader's costs that
grow with age grow with the number of segments, not their bytes, until compaction. Another `Reader` handle in the
same process stands for a second process (it reads only files); S3 is timed both as a reader opened, read and closed
and as the `kvik` binary started as a process. Each figure is reported as the median and the worst of its runs over
three runs, never a best case. A machine is recorded as the class it stands for, or as neither; a class not measured
is left "not measured here" (`docs/measurements/9.0e-latest-and-tail.md`). Neither class was measured by 9.0e, so its
acceptance "both hardware classes recorded" waits for the hardware run of roadmap 9.2.
**Pinned by.** `kvik/benches/read.rs`.

## Q-1053 — S3 on a year of seals: a reader's start listed the log directory
Decided by the lead with roadmap 9.0e's fix round: a defect against the design, fixed.
**Scenario.** S3, 400 latest values as a one-shot command, targets 30 ms on the desktop class and 200 ms on the
minimal class. On a year of seals it measured 195–213 ms median: a reader starting after the series directory's
`through` (`LogReader::start_after`) listed `log/` to find the segment holding the next position, one directory
entry and its metadata per sealed segment, about 1.7 µs a segment here. The design says a reader's system calls per
call do not depend on the sealed-segment count (9.0b1) and that a reader starts from recorded positions.
**Decided.** `start_after` tries the mark's segment and the two before it by name, then a binary search over headers
opened by name (`segment_holding`); it lists no directory. The search runs from file id 1, so it costs about 17
header reads at a year while the mark's generation holds the low file ids. A restored generation's ids start above
every earlier one's (`format.md` A.12), and the search then passes over each absent id with two failed opens; no
record the reader holds yet names a generation's first segment (no writer appends sealed-segment or restore records
today). To settle before the restore step. Measured again on 41,299 sealed segments: 22.0 ms median in
process and 25.5 ms as a process after the run's own first rounds, rising to 40.3 and 55.0 ms
as the runs' own writes doubled the tail read after the series directory (`docs/measurements/9.0e-latest-and-tail.md`).
**Pinned by.** `log/tests/reader_start.rs` `a_start_after_lists_no_directory_and_opens_few_headers`.

## Q-1054 — The empty poll of a reader process against S13's 50 µs
Decided by the lead with roadmap 9.0e's fix round; the desktop-class figure waits for roadmap 9.2.
**Scenario.** S13 targets an empty poll of 50 µs on the desktop class (200 µs on the minimal class). On a year of
seals the harness measured 64–111 µs for a reader handle's `Tail`, against 31–40 µs on about 1,100 sealed segments
(Q-1017). No part of a poll depends on the number of sealed segments: a mark-only poll is one open and one read of
4,224 bytes at both 10,669 and 43,026 sealed segments, under 2 µs back to back. The harness polls after a 21 ms
sleep, and on this machine the first system calls after a wake cost 32–47 µs whatever they do: a bare read of a small
file after the same sleep costs that much. The rise between the two figures is the machine's load on two days.
**Decided.** The engineering is done: a mark-only poll reads the mark through the handle the last full refresh
opened by path (the query design's section 3.1), one positioned read and no open; the full refresh once a second
opens it by path again. The harness prints the reference read beside each poll figure. Measured again on 41,299 sealed
segments: 41.4–45.4 µs median for a reader handle's `Tail`, beside 56.3–59.7 µs for the reference read.
What remains is this machine's wake-up cost, not engine work; the desktop-class figure waits for the
hardware run of roadmap 9.2.
**Pinned by.** `log/tests/reader_calls.rs` `a_mark_only_poll_reads_the_held_mark_with_no_open_and_sees_it_move`,
`a_replaced_mark_is_followed_by_the_held_read_after_one_full_refresh`,
`a_held_mark_that_reads_no_valid_mark_is_read_again_by_path`; `kvik/benches/read.rs`.

## Q-1055 — S1 after a write misses its desktop target
Decided by the lead with roadmap 9.0e's fix round: a known miss, scheduled.
**Scenario.** S1 targets 0.2 ms on the desktop class. With nothing written since, the writer's process measured
0.16–0.18 ms median (worst of 50: 0.34 ms); after a round of 400 readings 0.55–0.67 ms median, worst 1.0–3.7 ms, as
the writer's own reader reads its new batches back from the file rather than from live state in memory.
**Decided.** The design serves S1 from memory and Q-1007 names the fix, the shared tail hub feeding the writer's
reader. It lands with roadmap 9.0j (Q-1007) and is measured again there; the target stands.

## Q-1056 — Per-series counts in `kvik series`, and `tail --since` and `--once`
Decided by the lead with roadmap 9.0e's fix round.
**Scenario.** The query design's section 6 gives `kvik series` counts per series and `kvik tail` `--since` and
`--once` with 9.0e. A series directory entry holds the facts it folded (`fact_count`, `format.md` A.18), and a
reader continues that count over the facts it reads after it; a `Tail` opens at its cut only until the as-of time of
roadmap 9.0f (Q-1010).
**Decided.** `kvik series` shows a `facts` column from `Snapshot::fact_count` (the fold's count; the fact index's
when the fold is broken, Q-1006; the model backend's recorded count), no format change. `--since` and `--once` stay
refused with the reason and follow 9.0f.
**Pinned by.** `cli/tests/output.rs` (the `series` snapshots); `kvik/tests/api.rs` `define_emit_and_read` (both
backends' counts agree, and an unknown series is refused).

## Q-1060 — The log tail as the one source of `facts` and `at`
Decided by the lead with roadmap 9.0f, for review.
**Scenario.** The query design §2.2 runs every read over sources behind one trait: the recent window (the writer's
process), the log tail above C(E), compacted chunks below it. No compacted file exists yet (6.2b, 6.3e), and the
reader of the writer's process reads durable data only.
**Decided.** One source is built, the log tail, with C(E) the origin, so the tail is the whole kept log. It reads
through the key-history lookup of roadmap 11.0 (`LogReader::lookup_within`, A.8 indexes and a scan of segments without
one) on the lookups' log reader of Q-1004, at the view's cut: regions the manifest records or the process found lost
are passed over, positions above the cut are dropped. A pass that meets other damage first has the process's scanning
read find it, as a reader holding every fact did before (it builds the fact index once, which the reader then keeps,
Q-1006's cost), and reads once more: a region that read reports lost is passed over, and the losses reach every read
and live session of the process as before (Q-938); damage it does not report is `NotAvailable`. A segment gone from
under the sources' reads follows Q-951 as Q-1066 applies it. Finding the damage builds the fact index, and the reader
keeps it, outside the memory cap of Q-1063 (`api.md` says so). `facts` and `at` no longer
build the fact index; windows, `buckets` and calendar results did until 9.0h folded them from the stream (Q-1130),
and a broken state's latest values still answer from it (Q-1006). The merge drops a position two sources hold, for the
recent window when it becomes a source. No new lease: the lookups' log reader already holds one, so a reader keeps up
to three leases while it holds the fact index. The lookups' mutex is held for one pass's lookup, not across a
stream, so passes of concurrent calls on one reader run one at a time.
**Pinned by.** `kvik/src/durable/read/source/tests.rs`
`facts_and_at_equal_the_fact_index_and_the_model_at_every_cap` (generated histories of late facts, revisions of a
versioned series, restarts, power losses, damaged batches and seals; every view, `AsOf` and `AsOfTime` among them,
against a fresh reader's fact index and the model); `kvik/tests/durable_differential.rs`; `kvik/tests/durable_losses.rs`
`damage_to_a_batch_read_after_a_found_loss_extends_it_in_every_process`,
`a_watch_whose_segment_is_removed_reports_losses_unknown` (Q-1066).

## Q-1061 — An iteration whose cut ends or whose reset stamp changes
Decided by the lead with roadmap 9.0f, for review; settles Q-850.
**Scenario.** The query design §3.2 rule 3: an iteration ends with an error item carrying the reset (`after`, stamp)
once the stamp changes; Q-850 left the error of a second change in a materialised call to this item.
**Decided.** One error for both, `QueryError::CutExpired`, with no new variant: the caller's remedy is the same (start
the read again), and `api.md` already says that nothing of the read is to be kept. A stream checks its cut before
each pass (age and lease, Q-853) and each pass's lookup compares the mark's reset stamp and its positions' generation
with the cut's, so a new reset stamp (a reset, which a recovery starts), a restore (the only step that starts a new
generation, which the store does not support yet) or an aged cut ends it with that item, the last it yields; a
writer's restart that starts neither does not end it. A materialised `at` is computed once more under a new cut, and a
second `CutExpired` is returned. Cost: a reset storm reads as an aged cut, which the review of 9.0b1 noted. Options left for later: a `Reset { after, stamp }` variant (additive, `#[non_exhaustive]`).
**Pinned by.** `kvik/src/durable/read/source/tests.rs` `a_stream_held_past_its_cuts_age_ends_with_cut_expired`,
`at_reads_once_more_under_a_new_cut_when_the_first_ends_and_not_twice`;
`kvik/src/durable/read/cut.rs` `a_reset_during_a_call_is_read_again_under_the_new_cut_and_a_second_is_cut_expired`.

## Q-1062 — The order of several series read in one pass
Decided by the lead with roadmap 9.0f, for review.
**Scenario.** "Several series in one pass" (query design §2.2) reads each file once for all of them. A read that does
not fit the cap narrows its event-time window, so a stream in `(series, t)` order would read every file again for each
series.
**Decided.** Sources and the merge order rows by `(t, series, position)`: for one series that is the `facts` order of
spec §2.3; for several, event time first, so that passes partition the range and each file is read once per pass for
all of them. At one event time that holds more than a pass's grant, the series are split, then one key's positions
(Q-1068). The call is crate-internal
(`reader::facts::facts_of`); it becomes public with the `Query` read of §2.1 (`Query::Facts { sel, .. }`).
**Pinned by.** `kvik/src/durable/read/source/tests.rs`
`several_series_read_in_one_pass_come_in_event_time_order_then_series`;
`kvik/src/durable/read/merge.rs` `a_merge_of_overlapping_sources_equals_one_sorted_run_of_their_union`.

## Q-1063 — The memory accountant and the cap with multi-pass
Decided by the lead with roadmap 9.0f, for review; its fix round changed what a grant holds (Q-1067, Q-1069).
**Scenario.** The query design §1 Memory: one cap per reader (`ReaderConfig::memory`, 64 MiB default, 32 MiB least),
hard; §3.5: a minimum grant at admission, the cache giving way down to a quarter of the cap, a reserve for the largest
log block.
**Decided.** A pass holds a grant: it reserves 16 MiB for a log batch read and checked (the largest body, flat until
the A.8 index names block sizes) plus at least 1 MiB of rows, taking everything free when that is under two such grants
and half of it otherwise; once read, its rows keep the grant, shrunk to what they take, until the caller has taken
them (Q-1067). The cache's share is a quarter of the cap and at least the lookups' log reader's 8 MiB (segment indexes
and batch maps), which no pass takes. Decoded passes are not kept: a lookup verifies every batch it reads, so damage
after an earlier read is found at the next read, as the fact index's re-check of the last batch found it, where a kept
pass would show the damaged fact. A lookup decodes only the rows it keeps and counts them as they are kept: the records
its vector has room for, its growth before it happens (the old buffer and the new), and each text (Q-1069); past the
pass's rows it stops, and the pass halves its event-time window, then the series at one time, then one key's positions
(Q-1068), so results are equal at any cap from 32 MiB. With 32 MiB one pass runs at a time with 8 MiB of rows; with
64 MiB two run at once. A cap under 32 MiB is refused with `OpenError::Config`. Not built: growth past the admission
grant, and the cap of the writer's own reader, which takes the default.
**Pinned by.** `kvik/src/durable/read/cache.rs` `the_default_cap_admits_two_passes_and_the_least_one`,
`a_pass_waits_until_a_grant_is_given_back`,
`rows_held_for_delivery_are_counted_and_an_admission_needing_them_empties_their_shelf`;
`kvik/tests/query_memory.rs` `facts_and_at_allocate_within_the_memory_cap` (Q-1069);
`kvik/src/durable/read/source/tests.rs`
`a_maximal_batch_reads_in_several_passes_at_the_least_cap_with_the_fact_indexs_result`,
`a_reader_below_the_least_memory_cap_is_refused`, `reads_in_many_passes_equal_one_pass_in_every_view_and_at_every_time`,
and the generated histories at caps of a few facts a pass;
`log/src/reader/tests/parts_and_lookup.rs`
`a_lookup_within_a_budget_below_what_it_keeps_overflows_and_one_with_room_to_grow_finds_everything`,
`a_lookup_within_positions_keeps_only_the_facts_between_them`.

## Q-1064 — As of a time from the log's commit times
Decided by the lead with roadmap 9.0f, for review; the restore row stays open (settle-by 6.2b).
**Scenario.** Spec §2.3: as of time T is as of P, the last position whose batch's commit time is at or before T. Commit
times are non-decreasing within a generation only (query design §3.2); the fact index held every batch's commit time.
**Decided.** `LogReader::position_at` takes segments newest first up to the mark: a sealed segment's trailer passes it
over when its first commit time is after T and answers its last sequence when its last commit time is at or before T;
otherwise the segment's batches, mapped as a lookup maps them, answer the last one committed by T. The first segment
that holds such a batch decides. Lost batches are counted with their commit times: they hold no fact, so the facts as
of their position equal those as of the last batch the fact index kept. Voided batches are not: spec §3's recovery
entry keeps positions up to its `after` and writing goes on from there, so a voided batch's positions may be given
again to batches committed after T; a batch a later recovery entry of its segment voids answers at most that `after`,
and one it voids whole answers nothing. Batch maps a reader cached before such a reset are not re-read
(`may_hold` is false with no ranges), so the recovery entry, which the next read of the segment maps, is what the
cached map is checked against. A segment that may hold the answer and cannot be read is met as a pass meets damage:
the process's scanning read finds it and the search runs once more (Q-1060); damage it does not report is
`NotAvailable`. A position above the cut is taken as the cut. The log directory's commit-time ranges, and the
compacted files' tables for segments removed locally, are not used yet. Open: a T that falls in two generations,
which only a restore makes and the store does not support yet; the newest-first order takes the later generation's
position, and no test can make two generations at the store's level until restore exists. The test named for a
restart pins one generation whose commit times the writer keeps from going back across a restart with the clock set
back.
**Pinned by.** `log/src/reader/tests/parts_and_lookup.rs`
`a_position_at_a_time_is_the_last_batch_committed_by_then_in_every_segment`,
`a_position_at_a_time_inside_a_damaged_segment_is_incomplete_never_an_earlier_one`,
`a_position_at_a_time_passes_over_positions_a_later_recovery_entry_voids`;
`kvik/src/durable/read/source/tests.rs`
`as_of_a_time_across_a_restart_with_the_clock_set_back_is_the_last_position_committed_by_then`,
`as_of_a_time_over_damage_its_trailer_does_not_decide_reads_from_a_reader_that_has_not_scanned`.

## Q-1065 — `at` searched back from its time
Decided by the lead with roadmap 9.0f, for review.
**Decided.** `at` reads back from `t` in spans that double from one hour, each in passes that fit the cap, later first,
and stops at the first span holding a fact of the view not of bad quality; the later position wins a tie in time.
After the first span that holds none, one lookup that keeps at most 4 KiB of facts tells whether the series holds any
fact at or before the span; when it holds none, `at` answers none at once. A series quiet for a long time, or whose
earlier facts are all of bad quality, costs about log₂ of the quiet span in lookups, each pruned by the A.8 indexes;
6.2b's chunk index gives it one block. `at(i64::MAX)` reads up to the last time and answers its fact, where the fact
index's `at` returned none (it took the end of the range as exclusive and could not add one).
**Pinned by.** `kvik/src/durable/read/source/tests.rs` `facts_and_at_equal_the_fact_index_and_the_model_at_every_cap`
(`at` at several times in every view), `at_before_a_series_first_fact_ends_after_one_probe_and_finds_a_fact_far_back`.

## Q-1066 — A segment gone from under the sources' reads
Decided by the lead with roadmap 9.0f's fix round, applying the owner's ruling on Q-951.
**Scenario.** Sealed segments A, B and C; a reader's `facts` or `at` has read facts of B; B disappears with no
manifest record naming its removal. Since 9.0f these calls read through the lookups' log reader, not the scanning read
that Q-951's check sits in, so they read on and return the other segments' facts with no error; within one stream at a
small cap, part of B's facts can be yielded before it goes and the rest silently missed.
**Decided.** Q-951 holds on the new read path. The lookups' log reader notes each segment a lookup took a fact from;
once one of them is unlisted, every later lookup of that reader is incomplete, so `facts` and `at` fail with
`NotAvailable` at every call until the reader is opened again (the store or the reader process restarted), as Q-951
rules, until the removal records of roadmap 6.3b. A segment only listed, or one a lookup read and took nothing from,
does not count: recovery deletes segments that hold no batch in use, which a reader may have listed. A reader that
never took a fact from B passes over it, as Q-951's limit says. Within one read, a stream or `at` keeps the segments
its first pass listed and ends with `NotAvailable` when one is gone at a later pass, so no read yields part of a
segment silently. For `at` the first listing holds for the whole call: every span it searches back over and the
probe for earlier facts are checked against it (a second attempt under a new cut lists afresh).
**Pinned by.** `kvik/tests/durable_losses.rs` `a_watch_whose_segment_is_removed_reports_losses_unknown`,
`a_reader_that_took_facts_from_a_removed_segment_fails_every_read_after_reading_past_it`,
`a_segment_a_read_took_nothing_from_is_passed_over_once_removed`,
`a_removed_segment_no_reader_stands_in_is_passed_over_and_later_damage_still_found`;
`kvik/src/durable/read/source/tests.rs` `a_stream_ends_not_available_when_a_segment_its_passes_listed_is_gone`,
`at_ends_not_available_when_a_segment_its_first_span_listed_is_gone_before_the_probe`,
`at_ends_not_available_when_a_segment_an_earlier_span_listed_is_gone_at_a_later_span`.

## Q-1067 — Rows waiting for delivery are held under their grant
Decided by the lead with roadmap 9.0f's fix round.
**Scenario.** The cap is hard (query design §1) and `api.md` says it caps what queries allocate; a pass's rows wait in
the stream until the caller takes them. Holding the grant until then makes a caller that holds one stream and starts
another read on the same thread wait on itself.
**Decided.** A pass's rows go onto a shelf with their grant, shrunk to what they take; the grant is given back as the
last row is taken or the stream is dropped. An admission that finds less than the least grant free waits for running
passes when their grants would make room (they wait on nothing), and otherwise empties shelves, freeing their grants.
A stream whose shelf was emptied reads that part again at the same cut and passes over every row up to the last it
yielded (Q-1096), so its results are unchanged; one key's fold and the stream's own state stay outside the cap
(Q-1069). One admission empties at a time (Q-1095).
**Cost.** At the least cap the passes share 24 MiB and the least grant is 17 MiB, so a held shelf above about 7 MiB
leaves too little free for any other read on the reader: a second stream, an `at` or a probe. Each such read empties
the shelf, and the stream then reads its part again from its start and passes over what it yielded; a caller that
takes one row and then reads once more, row after row, does work that grows with the square of a part's rows. Two
threads each holding a stream deliver only what each takes before the other is admitted. Progress is certain with any
number of threads: a stream takes the first row it has not yielded off a part it has read before it shelves the rest,
so every read of a part yields a new row or ends the part, whatever other admissions empty; that one row is held in
the stream's own state until it is yielded (Q-1069). The last row taken gives the shelf's grant back. At the default
cap one held pass (at most 8 MiB of rows) leaves a least grant free. Not built: capping a pass's rows at the passes'
share less the least grant, which would let one held stream and point reads live together at the least cap at the
price of more passes.
**Pinned by.** `kvik/src/durable/read/cache.rs`
`rows_held_for_delivery_are_counted_and_an_admission_needing_them_empties_their_shelf`;
`kvik/src/durable/read/source/tests/shelves.rs`
`a_stream_whose_rows_another_read_needed_reads_them_again_and_yields_each_once`,
`a_stream_whose_later_shelf_is_emptied_yields_each_row_once_in_both_views`,
`a_stream_whose_every_shelf_is_emptied_as_it_is_made_yields_every_row_once_in_both_views`,
`two_threads_streaming_from_one_reader_that_empty_each_others_shelves_both_end_with_every_row`,
`a_stream_gives_its_shelf_back_with_its_last_row`;
`kvik/tests/query_memory.rs` `facts_and_at_allocate_within_the_memory_cap`.

## Q-1068 — One event time holding more than a pass
Decided by the lead with roadmap 9.0f's fix round; replaces the `NotAvailable` of 9.0f.
**Scenario.** About 24,000 versions of 255-byte text (90,000 of numbers) at one `(series, t)` fill a pass's rows at
32 MiB and at 64 MiB; narrowing the event-time window cannot split them, and `api.md` promises equal results at any
cap from 32 MiB.
**Decided.** A part of one key that does not fit is halved by position, lower positions first (`at`: upper first);
history yields each half's rows in turn. The current view folds the halves: each half's fact of highest rank (spec
§2.2) against the best so far, the key's fact delivered after its last half. So any number of versions at one time
reads in passes; only one fact larger than a pass's rows is `NotAvailable`, which no cap from 32 MiB can meet.
**Pinned by.** `kvik/src/durable/read/source/tests.rs`
`one_event_time_with_more_versions_than_a_pass_holds_reads_in_passes_at_the_least_cap` (40,000 texts at one time,
against the fact index), `a_versioned_key_read_in_passes_by_position_keeps_its_highest_version`.

## Q-1069 — What the memory cap counts, measured
Decided by the lead with roadmap 9.0f's fix round.
**Scenario.** The review measured 45.9 MiB above the baseline while `facts` read 104,000 text facts at a 32 MiB cap:
the grant was given back while the pass's rows were held, rows existed as three copies, and a batch decoded every row
whatever the read wanted.
**Decided.** A lookup checks a batch's bodies keeping no row, then walks them again building only the rows the pass
wants (A.5's checks run in both), so the 16 MiB reserve holds the batch's bytes and the checks' working space, and
rows are counted as kept; the facts' vector becomes the pass's rows in place, the view is kept in place, and each row
moves into the fact yielded. Outside the cap, measured as a fixed 1 MiB allowance: the stream, its plan and cut, one
key's fold, and an `at` probe's own state. Also outside it, and said in `api.md`: the fact index a reader builds once
to find damage (Q-1060). Measured with a counting allocator (`peak_alloc`) over four 8 MiB batches whose times
interleave: 19.1 MiB above the baseline for `facts` in history and the current view and for `at`, at 32 MiB and at
64 MiB. The test bounds each later call by the grant one pass takes alone (24 MiB at both caps) plus the allowance.
The reader's first `facts` measured 32.7 MiB: its 13.7 MiB above the later calls is not the lookups' cache (a few KiB
here: two segments' maps) but the reader's first refresh, whose log reader keeps the bytes of the last segment it read
between refreshes so that the next reads only what is new: 13.6 MiB, the sealed segment holding the four batches. It
is kept while the reader follows that segment, is outside the cap like the rest of the reader's own state, and is up
to `log.segment_size` (64 MiB by default); the refresh's peak above it (9.9 MiB) is over before the first pass is
admitted. So the first call is bounded by that segment's bytes plus the later calls' bound, and another 8 MiB copy of
the rows fails at either cap. Not built: giving those bytes back after a refresh, which costs the next refresh a read
of the segment from its start. A batch larger than the reserve, which this writer does not write, is read above the
cap.
**Pinned by.** `kvik/tests/query_memory.rs` `facts_and_at_allocate_within_the_memory_cap`.

## Q-1070 — A batch's commit time read before its calls were taken
Decided by the lead (a technical decision: it keeps the promise of §2.2 `received_at` and §2.3 As-of time).
**Scenario.** The writer thread read the wall clock for a turn, at 52 s, then took the queued calls for that turn's
batch. A call queued between the two, at 157 s by the same clock, was committed with `received_at` and `commit_time`
52 s. As of a time between 52 s and 157 s, the durable fact index then held a fact whose `emit` had not begun by that
time, where the model (whose receive time is the call's) did not. A property comparing the fact index with the model
in the as-of-time view found it after a power loss and restart; the timing, not the damage, was the cause.
**Decided.** The batch's `received_at` is at least the wall clock read after its calls are taken
(`CallSource::taken_at`), so no call is stamped earlier than it was queued; `commit_time` stays
`max(received_at, previous commit_time)`. The read follows the take and is not made under the queue's lock: any read
after the take is after every taken call was queued, and a caller's clock must not stall submitters. A source fed
from other threads must give `taken_at`; one that does not keeps the step's own read, the old gap. No promise
changes: §2.2 says `received_at` is the host clock at commit.
**Remaining gaps, one-sided.** `emit` decides the `future` and lateness flags against its own read, so a fact within
the gap of the 60 s bound can carry a flag its stored, later times do not show. Between a call's read and the take,
the model, whose receive time is the call's, holds a fact the durable index does not as of a time in that gap; the
rigs move the clock only between calls, so no property meets it, and a later one that does should set it aside.
**Tests.** `log/src/writer/threads.rs`
`a_call_queued_after_the_writer_read_the_clock_commits_no_earlier_than_it_was_queued` (holds the writer between its
read of the clock and its take); `log/tests/writer.rs`
`a_batch_is_received_at_the_later_of_the_steps_wall_clock_and_its_taking` (a source's time of taking above, then
below, the step's read).

## Q-1071 — The commit time before a loss, as a reader that continues a series directory knows it
Decided by the lead (a technical decision on a test: no engine or promise changes).
**Scenario.** A reader folded the log through batch A before a series directory through A was written; batch B,
just above A, is then damaged, and nothing records it (no start has met it). That reader reports the loss with A's
commit time before it; a reader opened after the directory continues it, reads the log only above A (Q-1001) and
reports the same loss with no commit time before it. The latest values' property found it in another item's rig,
comparing every reader's `losses` with a fresh reader's whole (Q-1006).
**Decided.** The engine keeps what §11 Health promises: each report carries what is known of its range. A continuing
reader knows no commit time at the directory's `through`, as a finder after a void past its kept commits knows none
(Q-941); a start's record, once written, gives every reader the same times. The property sets aside the commit time
before an unrecorded loss only where the side that lacks it continues a series directory and the loss's first
position is just above that directory's `through`; everything else is compared, recorded reports whole, and a reader
that read the whole log keeps the time compared. Until a start records the loss, readers may differ in it. For every
reader to know it, either the series directory carries its `through`'s commit time (a `format.md` change, not
proposed) or a continuing reader reads the one batch at `through` when it first reports such a loss (Q-1090). The
property reaches the scenario by damaging a batch after a clean stop, the first above the last directory for half the
draws, and checking before the start meets it: a reader of the whole log (the series directories moved out of its way
while it seeds) against a fresh reader and its snapshot. About three histories in 256 reach the set-aside, so a fixed
history pins it. Aiming at the batch just above the directory whatever it holds is not done: a batch there without a
fact is a definition or a recovery entry, whose loss is a different scenario from this one, and the fixed history gives
the reach. The damage always lies above any directory the rig writes; a directory the store writes itself can lie over
it where segments seal within a run, which the latest values' rig does not do and the sources property does, and the
check then skips (Q-1104). A reader continuing a directory that lies over unrecorded damage, before any start meets
it, need not know of the loss or its commit time before; that window is a consequence of Q-1002 as built (the next
start removes the directories), not something Q-1002 states, and nothing is promised for it. Damage in a sealed
segment would also meet Q-937, which stays with that question's own item.
**Tests.** `kvik/src/durable/read/latest/histories.rs`
`a_reader_seeded_from_a_directory_knows_no_commit_time_before_a_loss_just_above_it`,
`a_history_damaging_the_batch_just_above_a_directory_sets_aside_the_commit_time_before` and
`latest_values_equal_the_models_at_every_check`.

## Q-1090 — A continuing reader that reads the batch at its directory's `through` for a loss just above it
Recorded at the review of Q-1071 (by reading, not run). Open, for the lead.
**Scenario.** As Q-1071: a reader continuing a series directory through A reports an unrecorded loss of B, just above
A, with no commit time before it, where a reader of the whole log reports A's.
**Proposed.** When a continuing reader first reports a loss whose first position is just above its directory and it
knows no commit time before it, it reads the one batch at `through` and takes its commit time; one lookup, only on
damage, and still unknown where that segment is gone. No format change; readers then agree before a start records the
loss, and the property's set-aside (Q-1071) narrows to a missing segment, which the latest values' property shows.
**Now.** Not built; the report carries what is known (§11 Health).

## Q-1095 — Admissions that empty shelves at once
Decided by the lead with roadmap 9.0f's second fix round.
**Scenario.** Two threads call `at` or advance streams on one reader at the least cap while two idle streams hold
4 MiB each. The first admission took the list of shelves out of the shared state and released the lock to empty one;
the second, in that window, saw shelved bytes, no running pass and no listed shelf, and its check that shelved rows
are on a shelf panicked (44 times in 30,000 rounds).
**Decided.** Shelves stay listed while one is emptied, and one admission empties at a time: another that needs room
meanwhile waits until the emptying ends and looks again, so no shelf is emptied that one admission's room did not
need. An admission that finds nothing to empty and nothing given back since waits for the next grant given back
rather than spinning (a stream dropping its shelf gives its grant back just after the shelf is gone); what that wait
counts as a change, and the check that shelved rows are on a shelf, are Q-1097.
**Pinned by.** `kvik/src/durable/read/cache.rs` `two_admissions_emptying_shelves_at_once_are_both_admitted`,
`an_admission_empties_as_many_listed_shelves_as_its_room_needs`.

## Q-1096 — Where a stream resumes after its shelf is emptied
Decided by the lead with roadmap 9.0f's second fix round.
**Scenario.** A stream whose shelf was emptied read the part again and passed over as many rows as it had yielded.
Damage found between the two reads makes the second shorter, so the count ran into rows not yet yielded: 300 facts,
15 yielded, the first batch then damaged and found by an `at` that empties the shelf, and rows 15 to 24 were silently
missing.
**Decided.** A stream yields rows in strictly ascending keys and remembers the last key it yielded: `(t, series,
position)` in history, `(t, series)` in the current view, where a part read again may give a key's fact at another
position. Every row not beyond it is passed over, whatever the second read returns; the count is gone.
**Pinned by.** `kvik/src/durable/read/source/tests/shelves.rs`
`a_stream_whose_part_lost_a_batch_before_it_was_read_again_yields_every_row_after_its_last`,
`a_stream_whose_later_shelf_is_emptied_yields_each_row_once_in_both_views`,
`a_versioned_key_whose_yielded_fact_is_lost_before_its_part_is_read_again_is_not_yielded_twice` (the current view's
key).

## Q-1097 — An admission with nothing to empty, and shelved rows no shelf holds
Decided by the lead with roadmap 9.0f's follow-up.
**Scenario.** Two admissions need room that only shelved bytes no listed shelf holds could give. Each emptying pass
that took nothing counted as a change and woke the others, so two such admissions woke each other for ever and
neither waited. The check that shelved rows are on a shelf panicked after 60 seconds of no change: library code that
panics on a clock, though a caller's pace cannot reach it (a held or leaked stream's shelf is listed and full).
**Decided.** A change is a grant given back, a shelf made or a shelf emptied; a pass that empties nothing still wakes
admissions waiting for the emptying to end, but one waiting for a change waits on until a change comes. The check is
a `debug_assert!`: in a build with debug assertions, 60 seconds with no change, no running pass and shelved bytes is
an accounting fault and panics; in a release build the admission waits on. Not an error returned to the caller: it
would turn an internal fault into a failure every caller must handle, for a state no caller can cause.
**Pinned by.** `kvik/src/durable/read/cache.rs`
`two_admissions_with_nothing_to_empty_wait_without_waking_each_other`.

## Q-1100 — A shelf made counts as a change
Decided by the lead (a technical decision on a test: no engine or promise changes).
**Scenario.** With `changes += 1` taken out of `Accountant::shelve`, every test passed. An admission waits for a change
only after a pass over the listed shelves emptied nothing, and it makes that pass only with no grant running (with
one, that grant's return would make room, and it waits for that instead). While it waits, no pass can be admitted to
make a shelf until a grant is given back, which is a change of its own. So the count for a shelf made is not needed
today for an admission to wake.
**Decided.** Kept, as Q-1097 defines a change, so that the wait does not rest on that argument as admission changes,
and pinned by a test that reads the count after each kind of change.
**Pinned by.** `kvik/src/durable/read/cache.rs`
`a_shelf_made_a_grant_given_back_and_a_shelf_emptied_each_count_as_one_change`.

## Q-1101 — A far reader that built its fact index for a broken state, and damage below what it read
Decided by the lead (a technical decision on a test: no engine or promise changes).
**Scenario.** The live differential's shrunk case: a note is damaged before a clean restart, whose start records it
(position 5). The far reader's next refresh meets the recorded range, its latest values break (Q-1006) and it builds
its fact index, reading positions 6 to 8 whole. The note at 7 is then damaged. The far reader's `facts` call meets it
in a lookup, but `find_losses` has nothing left to scan, since the index read those batches before the damage, so the
call ends `NotAvailable` and the far process never finds the loss; a fresh reader does. The property asserted that
the far session names every loss a fresh reader reports, and failed on every replay, before 9.0f too.
**Decided.** The engine keeps Q-1011 and Q-1071: a session names the losses its process found, a scanning read never
reads a batch again (Q-1060), and until a start records a loss, readers may differ in it (§11 Health, "what is
known"). The test's expectation was wrong: its rig knew only the fact index it made the far reader build, not one
the engine builds for a broken state. It now asks the far reader whether it holds one, and while it does damages only
above what that reader read. Not changed: such a reader's `facts` and `at` over the damaged batch end `NotAvailable`
until a start records the range; finding it would need `find_losses` to scan again when an index is held, a new read
not proposed here.
**Pinned by.** `kvik/src/durable/read/live/differential.rs`
`a_far_reader_that_indexed_for_a_recorded_loss_is_damaged_only_above_what_it_read` (fails with the rig's old check),
`an_indexed_reader_meets_damage_below_what_it_read_as_not_available_until_a_start_records_it` (the limit as built:
`NotAvailable` and no loss in its process, the loss from a fresh reader, agreement once a start records it) and
`every_batch_brings_each_consumer_to_the_static_read_at_its_cut_and_to_the_models`.

## Q-1102 — A start's lost range across two recorded losses (ruled by the owner)
Recorded by the loss-reports item; ruled by the owner on 2026-11-21: option (a).
**Scenario.** The latest values' shrunk history: batches at positions 10, 8 and 9 are damaged in turn, each found by
a start after a power loss. The first two starts record losses 10 (10..10) and 8 (8..8). The third finds 8..10,
across both; as first built, its step 4 extended the first recorded loss it overlapped, writing a record under
identity 10 for 8..10 (`log/src/writer/recovery.rs` `record_loss`). The manifest then held two identities whose
ranges overlap. The writer's process reported one loss: identity 10, 8..10, recorded. A reader of the whole log
reported two: identity 10, 8..10, recorded, and identity 8, grown to 8..10 by its own find and marked not recorded,
though records hold every position. The property meets it at about one check in 200 of those after damage (2 of 404
in three runs at 1,024 cases).
**The gap.** §3 Order of a start step 4 says how a range that extends one recorded loss is recorded (one loss,
counted once), not one that overlaps two; §11 Health counts each loss once; Q-940 makes a find across two losses not
yet recorded one loss under the lower identity. Whichever rule is chosen, the writer and readers then agree; a
reader's report of a recorded range as not recorded is wrong under all three.
**Options.**
(a) One loss under the lower identity, as Q-940: the start writes the bridge as an extension of the lowest identity
it overlaps, and readers and the writer fold records of two identities whose ranges overlap into the lower one
(records already written under the higher identity are folded the same way). Acknowledgement, re-send requests and
the corruption count then name 8. About 40 lines with tests. The lead's recommendation.
(b) One loss under the identity the bridging record names (10 here), the others folded into it: the writer's report
as now; loss 8 leaves the list though a start recorded it, and its earlier re-send request names an identity no
report carries.
(c) Two losses: each recorded identity stays a loss, a find counts against a record that covers it, and the writer
reports two as well; the positions of loss 8 are then in two losses, against "counted once".
**Ruling.** (a): one loss under the lower identity, as Q-940. A start writes exactly one record for a range across
recorded losses, under the lowest identity it overlaps, spanning every loss it bridges, and nothing at later starts
that find the range again (`log/src/writer/recovery.rs` `record_loss`; spec §3 Order of a start step 4). The writer
and every reader fold records of identities whose ranges overlap into the lowest identity
(`kvik/src/durable/loss.rs` `fold_recorded`), so records written under the higher identity before the ruling read
the same (a start that finds such a range again writes the record under the lowest identity once where the range
reaches the lower identity's own records, and nothing where it lies within the higher one's, which the fold reads the
same); ranges that only touch stay two losses. A found report over several losses makes them one under the lowest
identity (`merge`), and the model's losses, found ranges included, and its start records follow the same rule
(`model/src/recovery.rs` `losses`, `model/src/engine/recovery.rs` `record_lost_ranges`). Each end of the bridging
record, and of a loss's report, takes the commit time of the range or record that sets it, so a bridge does not keep
the time of a batch now lost. A start counts the records it wrote before each range, so two ranges of one start over
one recorded loss leave nothing for the next start. The latest values' property compares every loss report whole
again, its set-aside for bridged records removed; only the set-aside of Q-1071 remains.
**What the fold leaves unchanged.** The root of the log directory carries one lost range per identity as before
(`format.md` A.18): it is read for the positions lost ranges cover, never as a list of losses, so overlapping entries
change nothing. The `files` crate's catalogue keeps one entry per identity a record names, with no fold: backup,
restore and the manifest rebuild use it for the positions records cover and to carry records as written, never as a
list of losses for a user (its per-identity `acknowledged` has no reader outside its tests), and the engine folds
what they carry when it reads it; a rebuild restates a range under the lowest identity it overlaps, which the fold
reads the same. Acknowledgements, closing records, the listing of a folded loss and its corruptions are Q-1108.
The model's starts never meet a range across two identities (corrupting a batch clears it, and a scrub passes
positions already lost), so the lowest-identity choice in `record_lost_ranges` and in self-repair's records is kept
for agreement but no model history reaches it; the fold in `losses` is pinned.
**Pinned by.** `kvik/src/durable/read/latest/histories.rs`
`a_start_s_range_across_two_recorded_losses_is_one_loss_under_the_lower_identity` (the records, the writer's and a
whole reader's reports, after a power loss and a restart too); `kvik/src/durable/loss/tests.rs`
`records_of_two_identities_that_overlap_are_one_loss_under_the_lower_identity`,
`records_of_one_identity_or_of_two_that_only_touch_are_not_folded`,
`a_found_range_over_two_recorded_losses_is_one_loss_under_the_lower_identity_not_recorded`;
`a_folded_loss_takes_each_end_s_commit_time_from_the_record_that_sets_it_and_the_largest_byte_count`,
`a_record_that_extends_a_loss_gives_it_the_commit_time_of_the_end_it_moves`,
`equal_and_nested_ranges_of_two_identities_fold_into_the_lower`; `log/tests/loss_bridge.rs` (one record under the
lower identity, three losses bridged, one identity extended once, a range beside a loss, a bridge recorded under the
higher identity, durability across power losses and restarts, a torn bridging record, the commit times of a bridging
record, two ranges of one start over one loss); `log/tests/recovery.rs`
`a_recovery_entry_s_gap_across_two_recorded_losses_is_one_record_under_the_lower_identity`; `model/src/recovery.rs`
`records_of_identities_whose_ranges_overlap_are_one_loss_under_the_lowest`,
`a_found_range_over_two_recorded_losses_folds_them_as_the_engine_does`.

## Q-1103 — The first call's memory bound and the refresh within it
Decided by the lead (a technical decision on a test: no engine or promise changes).
**Scenario.** `kvik/tests/query_memory.rs` bounded a reader's first `facts` call by the followed segment plus the
larger of one pass's grant with its overhead (25 MiB at both caps measured) and the first refresh's 12 MiB, so the
refresh's term was never the larger and its 9.9 MiB figure (Q-1069) was not tested.
**Decided.** A second reader's first `live` call, which runs the first refresh and the latest values' fold and no
pass, is measured alone against the followed segment plus 12 MiB at both caps: 10,404,672 bytes above a 14,300,808-byte
segment, the 9.9 MiB figure. The first `facts` bound is the followed segment plus the grant and its overhead, which
holds since the refresh is over before the first pass is admitted and its allowance is under the grant (asserted).
**Pinned by.** `kvik/tests/query_memory.rs` `facts_and_at_allocate_within_the_memory_cap`.

## Q-1104 — A series directory the store wrote over damage no start has met
Decided by the lead (a technical decision on a test: no engine or promise changes).
**Scenario.** The sources property (`kvik/src/durable/read/source/tests.rs`
`facts_and_at_equal_the_fact_index_and_the_model_at_every_cap`) seals segments every 20 ms, so the clean stop of
`Step::Damage(_, true)` can write the store's own series directory through the newest batch: the first directory of a
run is due as soon as a sealed segment is uncovered (`format.md` A.18). The batch then damaged lies at or below that
directory's `through`. A fresh reader continues it and reports the loss with no commit time before (seen: positions
13 to 16 lost under a directory through 16), a reader of the whole log with one, and the check before the start
failed on the difference in about two runs of three at 1,024 cases. The latest values' rig keeps the default seal
interval, so no segment seals within its runs and the store writes no directory there; the rig's own directories
always lie below the damage.
**Decided.** The check before the start skips, and counts, a fresh reader that continues a directory at or above the
damage, only where the series directories through that position are the store's own (at least one, none the rig
wrote); otherwise it fails. That is the window Q-1071 leaves to Q-1002 as built: nothing is promised for a reader
continuing a directory over damage before a start meets it. Everything else is compared as before, and the latest
values' fixed history still pins the set-aside. The fixed test pauses for two seal intervals before the clean stop,
so a seal runs however fast the host is: its first history then reached the skip at every attempt seen, its second
at about half; a pause of ten intervals stopped the second reaching it, as its directory then lay below the damage,
so the pause is kept short and the attempts stay.
**Tests.** `kvik/src/durable/read/source/tests.rs`
`a_store_directory_over_damage_no_start_has_met_is_skipped_before_the_start` (the shrunk history, and one whose newest
batch holds one reading so that the directory's `through` can equal the damaged position; seals run on real time, so
each pauses past the seal interval before the clean stop and repeats until an attempt reaches the skip, and every
attempt must pass), `kvik/src/durable/read/latest/histories.rs`
`only_a_directory_the_store_wrote_counts_as_the_store_s_own`, and `latest_values_equal_the_models_at_every_check`,
whose histories must never skip.

## Q-1107 — The first refresh's bound and the series directory written at the seal
Decided by the lead (a technical decision on a test: no engine or promise changes).
**Scenario.** `kvik/tests/query_memory.rs` `facts_and_at_allocate_within_the_memory_cap` failed in most runs on a busy
host: the first refresh measured 33,000,184 bytes against the followed segment (14,300,808) plus Q-1103's 12 MiB. When
the segment holding the four batches seals, the store writes its series directory through what the deliverer has
taken by then, which depends on thread timing: seen through the second batch and through the third. A reader continues
it, so its first refresh folds only the batches above it, each held within a part as 8,294,704 bytes of rows. Q-1069's
9.9 MiB was measured over a directory through the third; one batch fewer covered is one batch more held, the failure.
No cap is exceeded: the cap does not count the refresh (`api.md` Reader, Q-1069), which is over before the first pass
is admitted.
**Decided.** The test removes the series directory after the clean shutdown, so every reader's first refresh folds the
log from its start. A refresh holds one part's batches at a time (`take` in `kvik/src/durable/reader.rs`), and a part
ends at the batch that brings it to `durable::start::PART` (65,536) items: three batches of 26,000 here. The refresh
is bounded by the followed segment plus three batches' rows (each text's 255 bytes and its 64-byte item) plus 3 MiB for
one batch's scan and the fold: measured 26,981,000 bytes above the segment in every run (2,096,888 plus three times
8,294,704). That allowance is above one pass's grant with its overhead, so the first `facts` call is again bounded by
the followed segment plus the larger of the two (Q-1103's form before it). With `PART` doubled the part holds all four
batches and the refresh assertion fails; with the cap doubled or ignored the first `facts` assertion fails.
**Pinned by.** `kvik/tests/query_memory.rs` `facts_and_at_allocate_within_the_memory_cap`.

## Q-1108 — Acknowledgements, listing and corruption counts across a fold
Recorded by the lead with the Q-1102 fix round; for the owner. Built as option (a), the lead's decision until ruled.
**Scenario.** Losses 8 (8..9) and 10 (10..11) are recorded, each acknowledged or closed (say 10 restored in full) and
each counted as a corruption. A start finds 9..10 and, under Q-1102, writes 8: 8..11, after which 10 is folded into 8.
So is a bridge 10: 8..10 written under the higher identity before Q-1102 and acknowledged there for its whole range.
As first built (3e3a4153), an acknowledgement or closing record naming 10 matched no loss once folded, so loss 8 was
degraded again though no position was newly lost; loss 10 left the list, against §8 "stays in the list of losses for
ever", its closing outcome with it; and its corruption was read as loss 8's, so the repeated-corruption count fell, and
`storage_suspect` could clear, at the moment more damage was found.
**Options.**
(a) Carry over. A folded loss lists the identities it absorbed that have a record (`LossReport::absorbed`, `api.md`),
whatever the order of the finds that folded them; a range found and folded in with no record is not listed. The latest
acknowledgement or closing record of each identity folded in, its own included, closes it while together they cover
its present range (positions follow one another within a generation); a fold that adds a position none of them covers
needs a new acknowledgement, as a loss that grows does. The outcome is that of the latest of those records; one
"restored in full" closes nothing once a later lost-range record of an identity folded in overlaps it. Each absorbed
identity whose records are a corruption keeps its own corruption, named by its own first record, so the count and
`storage_suspect` never fall at a fold; `acknowledge_losses` naming the folded loss marks them all seen. About 250
lines with tests. Its costs: the closed state rests on several records read together, not one; the loss shows one
outcome, the latest, where the records folded in had different ones (by an operator and restored in full, say); an
absorbed loss stays listed by identity only, without its own range, state or outcome, which is less than §8's "with
its state"; and acknowledged ranges that meet across a generation boundary are not joined, so such a loss asks for a
new acknowledgement.
(b) Lapse, as in 3e3a4153: records naming the higher identity close nothing, the higher loss leaves the list and its
corruption is the folded loss's. For it: one rule (a record closes the loss it names and no other), no new public
field, and the person is asked again whenever the listed loss changes shape, which some would want. Against it: the
loss is degraded again with no position newly lost, the higher loss and its outcome leave the list, and the
corruption count falls when more damage is found.
**Within (a).** An identity in `absorbed` given to `acknowledge_losses` does nothing and reports success, as an
identity with no loss does (built so). The other course reads it as naming the loss that absorbed it, which is kinder
to a caller holding an identity from an earlier listing, at the cost of one identity naming two losses.
**Recommendation.** (a): a fold is a change in how records are read, not new damage, so it should take nothing away
that a user already had. Built so (`kvik/src/durable/loss.rs` `absorb`, `acknowledged`; `kvik/src/durable/status.rs`
`loss_reports`; `model/src/recovery.rs` `losses`; `model/src/engine/repair.rs` `corruptions`). In the engine the
acknowledgements are held in memory for the process until roadmap 8.5c writes their record; the model holds them as
records.
**Pinned by.** `kvik/src/durable/loss/tests.rs` `a_folded_loss_names_every_recorded_identity_it_absorbed`,
`acknowledgements_of_the_identities_folded_in_close_the_loss_only_while_they_cover_it`,
`a_folded_loss_absorbs_every_recorded_identity_whatever_the_order_of_the_finds`; `kvik/tests/durable_losses.rs`
`acknowledgements_of_two_recorded_losses_carry_over_a_bridge_that_adds_no_position`,
`an_absorbed_identity_given_to_acknowledge_losses_acknowledges_nothing`; `log/tests/loss_bridge.rs`
`a_bridging_record_spans_only_the_losses_its_range_overlaps`; `model/src/recovery.rs`
`closing_records_of_the_identities_folded_in_carry_over_while_they_cover_the_loss`,
`a_folded_loss_absorbs_every_recorded_identity_whatever_the_order_of_the_finds`,
`a_record_of_an_identity_folded_in_reopens_a_restore_only_where_it_overlaps_it`; `model/tests/self_repair.rs`
`a_fold_keeps_each_absorbed_loss_s_corruption_and_the_count`,
`acknowledgements_of_two_losses_carry_over_a_bridge_that_adds_no_position`,
`an_absorbed_identity_given_to_acknowledge_losses_acknowledges_nothing`.

## Q-1109 — Delivery before durability: an automation may act on a reading later withdrawn
Recorded by the lead with the fast path. Ruled by the owner on 22 November: option (b).
**Scenario.** The log syncs once a second to spare the disk (`sync_interval_floor` and `min_sync_interval` at 1 s).
An automation that waits for durability reacts up to a second late. Delivered at acceptance instead, it reacts
within a millisecond, but a reading it acted on can then be withdrawn: the sync fails, the writer stops or the
power goes before the mark covers it. Until now every delivery kvik made was durable or, for a series at
`Durability::Written`, acknowledged once written; nothing told a reader that a reading it had seen was gone.
**Options.**
(a) None: every delivery waits for durability. The promise stays simple; reaction is bounded below by the gathering
window, which the purpose of the store (reaction must not wait for the disk) rules out for automations.
(b) Opt-in provisional streams beside the durable ones (as built): a stream asks for it; each call is delivered
marked durable or provisional, a watermark later covers the provisional ones, and any that will not become durable
are withdrawn by range with the writer's own outcome (unknown or not stored). Consumers, cursors, live reads and
acknowledgements keep every promise they had; a provisional fact has no position, so no cursor can cover one. The
cost is a second kind of delivery to explain, and that an action taken on a withdrawn reading cannot be undone by
the store; the automation decides whether a reading is worth acting on before it is durable.
(c) Provisional delivery as the default for live reads and consumers, with durability opt-in. Fastest for every
reader, but it changes the meaning of every existing delivery, and of a cursor that covers one.
**Within (b).** Withdrawal is told only within the open: after a restart `seq` starts afresh, and a stream that kept
a provisional reading outside the process treats what no watermark covered as withdrawn by the stop. A withdrawn
reading whose sync failed may still be found after a reboot (§9 row 7a); it then arrives through every durable
path as any recorded fact does. A slow stream lags rather than holding the ring: it is told the calls it missed and
a position below them, as a lagging live read is. A withdrawal names the outcome the writer gave the call: a call the
writer acknowledged as written (`Durability::Written`) and lost before its sync is withdrawn as unknown, although
its writer was told it was stored; a call the writer had not yet settled when it failed waits for its outcome, which
the log gives every queued call after a fault; only a stopped writer leaves a call unknown without one. The engine has
no `Durability::Written` yet (roadmap 5.5b), so the recheck for such a call is pinned on the ring with a scripted
writer and in the model.
**Ruling.** Acceptable: an automation acts on the information it has at the time, and a reading later withdrawn is
reported as withdrawn, which is enough. The fast path stays opt-in and labelled.
**Recommendation (the lead's, before the ruling).** (b): the fast path is for reaction and the durable paths are for record, and only the reader
knows which a reading is for. Built so (`kvik/src/durable/fast.rs`, `kvik/src/provisional.rs`,
`model/src/provisional.rs`).
**Pinned by.** `kvik/tests/provisional.rs`
`every_provisional_reading_ends_durable_or_withdrawn_across_crashes_and_failed_syncs`,
`a_provisional_stream_changes_nothing_a_durable_consumer_receives`,
`with_a_one_second_gathering_window_readings_arrive_within_milliseconds`; `kvik/src/durable/fast/tests.rs`
`the_stream_says_what_the_reference_model_says`; `model/tests/provisional.rs`
`s4_every_provisional_call_ends_durable_or_withdrawn_and_durable_ones_survive`,
`s4_provisional_streams_change_nothing_a_consumer_receives`; the writer's outcome: `kvik/src/durable/fast/tests.rs`
`a_failed_writer_leaves_a_call_open_until_its_writer_is_given_an_outcome`,
`an_acknowledged_call_the_failed_writer_never_synced_is_withdrawn_unknown`,
`a_call_acknowledged_ahead_of_its_sync_is_rechecked_without_a_bell`; `log/tests/threads.rs`
`a_failed_sync_resolves_every_queued_call_while_the_writer_still_runs`; opening with a call in flight:
`kvik/src/durable/emit.rs` `a_stream_opened_with_a_call_in_flight_never_names_a_lag_floor_above_a_call_it_missed`,
`a_stream_opened_while_a_call_is_checked_waits_for_its_offer`.

## Q-1110 — A stream's end after a writer panic, a call larger than `max_behind`, the watermark's rule and the ring's hold
Recorded and decided by the lead with the fast path's fix round (Q-1109). Numbered Q-1115 on the fast path's branch; renumbered where it met the seeded start, which holds Q-1115. **Ruled by the owner, 23 November: as built.**
**Scenario.** (a) The writer thread panics: the log fails every queued call as unknown and the writer stops, but
the store is neither shut down nor dropped. Every call a stream delivered is withdrawn; should `Closed` follow? A
`Closed` pushed on the writer's stop alone could precede the publication of a call offered just before the stop,
which the stream would then never see. (b) A call holds more facts than a stream's `max_behind`: no call in the
ring lies within the bound, and the stream either lags past the call or takes it whole. (c) A stream receives a call
already durable (`durable()` true), or skips calls in a `Lagged`, and a later watermark covers only those. (d) A
stream far behind takes every call past its cursor while holding the ring's lock, which the writer takes to publish
each call.
**Options.** (a) `Closed` on a shutdown or drop only, once the writer has stopped; or on the writer's stop alone,
with a count of calls between their offer and publication. (b) Deliver it whole; or refuse a stream whose
`max_behind` is below the largest call (calls hold up to `limits.max_call_readings` facts, set per store, so a
stream could not choose its bound alone). (c) Send a watermark whenever it rises; or only when it covers a call
delivered provisional. (d) Take at most a fixed number of calls per hold; or copy the ring's head and clone outside
the lock.
**Decision.** (a) A shutdown or drop, once the writer has stopped: a shutdown takes the decision lock after the log
stops, so every call offered before it is in the ring before the streams are told; nothing new is needed for the
panic case, where every delivered call is still withdrawn and `Closed` waits for the shutdown or drop. (b) Whole:
the lag ends before it and `Lagged` never names an empty range. (c) Only when it covers a call delivered
provisional. (d) At most 4,096 calls per hold, and the oldest call within bound found by bisection; the measured
hold is in `docs/measurements/fast-path.md`. A call wakes only the streams whose selection holds one of its series.
**Pinned by.** `kvik/src/durable/fast/tests.rs` `a_writer_that_stopped_without_a_shutdown_closes_no_stream`,
`closed_waits_for_the_writer_to_stop_after_a_shutdown`,
`a_call_larger_than_max_behind_is_delivered_whole_without_a_lag`,
`a_lag_ends_at_a_call_larger_than_max_behind_and_delivers_it_whole`, `a_slow_stream_lags_to_within_max_behind_and_others_miss_nothing`,
`a_stream_takes_at_most_one_batch_per_hold_of_the_ring_and_misses_nothing`,
`a_call_rings_only_the_streams_that_select_its_series`, `next_reads_past_a_batch_the_selection_filters_out_in_one_call`,
`a_waiting_stream_is_closed_at_once_by_a_shutdown`; `kvik/tests/provisional.rs`
`a_stream_that_reads_after_the_sync_gets_the_calls_flagged_durable_and_no_watermark`.

## Q-1114 — The damage scan below a seed starts again at every open
Recorded with the seeded start's fix round (Q-1115 as ruled).
**Scenario.** A seeded start's background scan reads the log below the seed from its start, and a shutdown stops
it. A process that runs for less than one scan (seconds at 1 GB: a crash loop, a short-lived tool) never finishes
it, so damage below the seed stays unfound across any number of such opens; and every open reads the whole log below
the seed once more in the background, O(store) I/O per open.
**Options.** (a) As built: the scan starts again at every open; a start from the whole log, after the next fold
break or an upgrade, finds the damage. (b) Record how far the scan got (status file) and continue from there at the
next open: one more durable write. (c) Record whether the last scan finished; if not, scan before the next open once
(`ScanOrder::BeforeOpen` for that open).
Measured: at 7 days of the home workload the scan ends 12–16 s after the open, longer than a start from the whole
log takes to read the same store (7–8 s); why it reads more slowly is not yet known.
**Provisional default.** (a): both other options add a durable record, which a fix round does not. (c) is the
smaller change if short-lived processes turn out to matter.

## Q-1115 — A seeded start scans for damage below its seed after the open returns
Recorded with the seeded start, as Q-1110; with Q-1116 and Q-1117 (recorded as Q-1111 and Q-1112) renumbered where
it met the fast path, which holds Q-1110. **Settled by the owner, 23 November: (b), the background scan as built**;
`ScanOrder::BeforeOpen` stays as the one-constant way back, and under it the scan runs once, before the open, and no
scan runs after it.
**Scenario.** A store a week old opens. Until now the start read the whole log, so a segment damaged since it was
written (a bit flip in a sealed segment, below every mark) was found before `Store::open` returned: the loss was in
`losses()` and `health()` from the first call, the series directory was set aside, and the decisions and counts
were built without the lost facts. A seeded start reads only the log above its series directory's `through`; the
damage below it is found only by reading that part of the log, which is what makes the open cost O(store).
**Options.**
(a) Scan before the open returns (`ScanOrder::BeforeOpen`, built and tested): the open still reads the whole log
once, though only for damage, without decoding every fact into the decision and delivery state; damage below the
seed makes the start read the whole log as before. Every promise stays as it was; the open stays O(store), about
half of today's cost by the profile in the reopen research (the two replays' decode and window work go, the read
stays).
(b) Scan after the open returns, on a thread of its own (`ScanOrder::Background`, the default as built): the open is
O(recent data). A loss the scan finds is reported through `losses()` and `health()` as a loss found while the store
is open is (Recovery rule 2): the writer's decisions forget the facts the log shows (Q-921) and decide those keys
from the log, which skips the region; the deliverer's summaries are rebuilt (Q-930); the series directory fold
breaks and the directories on disk are removed (Q-1002), so the next start reads the whole log and finds the
damage before it returns. Until the scan reaches the region, a read or a decision that meets it reports it as any
read does (Q-928), and `IngestStats::start_scanning` says the scan runs. What differs from (a): the counts of §11
(`recorded`, `late`, `future`) of a series with facts in the region still count them until the next start, and a
reading decided before the scan reached the region was decided with the region still read as intact.
(c) Scan in the background but hold writes until it ends: O(recent) to readable, O(store) to writable. It keeps the
decisions' promise but not the open's time for an adapter, which writes at once.
**Recommendation.** (b), as built: the purpose of the store is reaction (decision of 22 November), a restart must
resume within a second at any age, and the damage it delays finding is rare (it needs media that corrupt data after
a successful sync) and still reported, with the store setting itself right at the next start. The seam is one
constant (`kvik/src/durable/scan.rs` `SCAN_ORDER`); setting it to `BeforeOpen` restores the old order with the
seeded start's other savings, and both orders are pinned by the equivalence test.
**Limit (as built).** The scan starts again from the log's start at every open and stops at shutdown, so it reads the
log below the seed once per open, in the background, and a process that runs for less than one scan (seconds at
1 GB) never finishes it: damage below the seed stays unfound across any number of such opens, until a process runs
long enough or a start reads the whole log. Carrying the scan's progress across opens needs a new durable record
(Q-1114). Before the scan reaches the region, a query that meets it reports it (Q-928), a replay of a lost fact
whose key's segment holds it is recorded unchecked (Q-750), and a consumer behind the seed that meets it passes
over the lost facts, counted in `LocalDataUnreadable` and not reported until the scan or a query finds it; the
scan then completes the report with the commit times around the region, as the start from the whole log gives
them. A region the scan finds just below a loss found at the open (the pass above the seed) grows that loss downward
under the identity it was found under (Q-200), where the start from the whole log, meeting the lower region first,
reports the lower identity; which identity the next start records is Q-203.
**Pinned by.** `kvik/src/durable/seed/tests.rs`
`a_seeded_start_gives_what_the_start_from_the_whole_log_gives` (both orders, over sealed segments; under `Background`
a loss found after the open is compared by its reports and the series without a fact in the damage),
`a_store_with_a_directory_starts_seeded_and_gives_what_the_whole_log_gives`; `kvik/src/durable/seed/tests/damage.rs`
(damage below the seed found after the open, and reads, decisions and consumers that meet it first).

## Q-1116 — What a seeded start continues from, and when it reads the whole log
Decided by the lead with the seeded start.
**Scenario.** A start continues from the newest series directory that is still a function of the kept log (A.18
Validation, as the writer judges it, `kept_series`) and the registry file, then reads the log after the directory's
`through` S once, for the writer and the deliverer together. What else must the files hold, and when does it give
way to the whole log?
**Decision.** The directory's entries carry the start counts (A.18 fields 96 to 120: late and future counts and the
last receive time, with bit 7 when the last fact carries a `changed_at`); a v1 directory, or one continued from a v1
directory, has none, and the writer continues no fold from one without them, so after an upgrade the first start
reads the whole log and the next directory carries them. S is at or below the mark, in the mark's generation, above
0, and no series' latest `t` is `i64::MAX` (no floor lies above it). The registry file of S's generation supplies
the entries up to S, and its `through` must be at or above S (or the start reads the whole log); Q-1001 makes them
every one, so the start trusts the file up to S, and the Derived check (Q-33) compares the file with the file's
entries up to S plus the log's after it. Each series' decision window and recent window start empty with a floor
above its latest `t`, so keys at or below it are decided from the log, as keys that left the windows are; counts,
lateness reference and last arrival continue from the entry; a series whose last fact may have been outranked has
its last arrival read from the log when a stale-for condition first needs it; commit times below S, for `start:
time`, are read from the log when asked; a consumer below S reads the log below S as one behind does. The pass reads
the batch ending at S first, for the commit time a loss just above S reports (Q-1090), and gives way to the whole
log when that batch is not first, on a void, on a region holding a position at or below S, or on a read error.
The manifest's recorded lost ranges below S count as regions the deliverer passed over, as the whole read meets
them; otherwise its lookups below S report a key over one unreadable and deliver the fact unchecked where the whole
read's is not (`a_consumer_starting_below_the_seed_passes_over_a_recorded_loss_as_the_whole_log_does`). The writer's
decisions over one already match the whole read's
(`a_replay_below_the_seed_over_a_recorded_loss_is_decided_as_the_whole_log_decides_it`).
Neither a void nor a region at or below S reaches the pass through the seed's own rules: a reset below a
directory's `through` makes it no seed (`kept_series`), and recovery truncates what a reboot leaves above the mark;
both give-ways stay as a guard. The two reads below S (a last arrival that may have been outranked, a commit time for
`start: time`) read back from S a window at a time, doubling until they find their answer or reach the log's start,
so an answer near the seed reads little of the log; one far back still reads the log up to it, under the
deliverer's lock.
**Why.** Each rule keeps the start's result equal to the whole read's, which the equivalence test pins over random
histories; where the files cannot say (a revision outranked, a commit time below S) the log is read, as the bounded
history already does for what left memory (roadmap 11.0).
**Found on the way.** The deliverer decided whether a late revision was outranked against a window that an earlier
fact of the same batch had just moved past the revision's key: it then delivered it unchecked although its key
could be read. It now reads the key when the window no longer holds it
(`a_late_revision_behind_a_fact_that_moves_the_window_is_checked_against_its_whole_key`).
**Cadence.** A clean shutdown also writes the series directory, after the last mark sync, through what the deliverer
took, when more than 65,536 positions (one part of a start's read) follow the last one's `through` (A.18 Cadence), so
the next start's seed is the mark; a store whose segments seal by size alone, or none (a
day of a replayed build fits one unsealed segment), would otherwise have no seed near the head.
**Pinned by.** `kvik/src/durable/seed/tests.rs` (all, among them
`a_stale_for_condition_on_a_last_fact_tied_with_an_earlier_one_resumes_from_the_tied_arrival`,
`a_stale_for_condition_on_an_outranked_last_fact_resumes_from_the_last_arrival_that_counts` and
`a_clean_shutdown_leaves_a_series_directory_through_the_mark`), `log/src/directory/series.rs`
`a_v1_entry_carries_no_start_counts_whatever_its_flags_say`, `log/tests/directory_golden.rs`
`a_v1_series_directory_reads_as_before_with_no_start_counts`.

## Q-1117 — A power loss can take the seed with the unsynced series directory
Decided by the lead with the seeded start, provisional.
**Scenario.** Derived files are renamed into place and never synced (A.18 Writing). After a power loss the newest
series directory may be absent or invalid, and the previous one may be behind the start's reset (Q-776), so the
start finds no seed and reads the whole log: the restart after a power loss can take seconds on a large store,
where one after a process crash or a clean stop takes under one.
**Decision (provisional).** Leave the files unsynced: on Linux the page cache writes a renamed file back within
about 30 seconds, so only a directory written in the last moments before the loss is at risk, and the previous one
is then still a seed unless the start's reset falls below its `through`. If restarts after power losses must be fast
too, the cheap change is to sync the series directory and its directory once per write (at most every 15 minutes or
32 MiB of seals, about a hundred syncs a day), which costs no measurable wear; it is one line in
`log/src/writer/directories.rs` and changes no format.
**Pinned by.** `kvik/src/durable/seed/tests.rs`
`a_store_with_a_directory_starts_seeded_and_gives_what_the_whole_log_gives` (a power loss reads the whole log and
gives the same result).

## Q-1118 — Selective fan-out of live sessions: what a session passed over no longer counts
Decided by the lead with the fan-out change, provisional.
**Scenario.** Fifty automations each watch a few of 37 series at 1 Hz. Every commit woke every session in the
writer's process (the writer's `notify_all`), and each ran a turn whatever it selects: CPU grew with sessions times
commits. The fix wakes only the sessions a round reaches. A session not woken does not take the rounds that hold
nothing it selects, so either the shared tail keeps them for it (memory, and a lag once they pass its backlog) or
the session is moved past them.
**Decision (provisional).** In the writer's process one `kvik-fanout` thread, woken by the writer's turns and by
refreshes of live staleness, runs one refresh round for what the writer published; each session keeps its own
`min_batch_interval` (one rung inside it sleeps out the rest; one whose batch stopped inside a round at a row limit,
Q-1014, takes the rest once the interval passes, with no ring). Each session states what can give it a batch: the members of its opened `Latest`s and the series its
opened `Tail`s select by the registry at its last batch or at the `Tail`'s position; anything from a read's `open`
until the batch that opens it is taken, so that a round ending between that batch's cut and its advance is never
passed over by what the session reached before (otherwise a `Tail` could miss a fact above its `Opened` cut with
nothing said, against Q-1011). A round reaches a session if it holds a fact of such a series, or anything else (an entry, a void, a gap,
or more facts than the backlog keeps). A round rings the sessions it reaches and those with an earlier round still to
take; a session that took every earlier round and is not reached passes over it, its cursor moved past without a
turn. Consequences: (a) the backlog of Q-1013 counts the rounds a session must take, so a session whose series are
quiet no longer lags, nor holds the shared tail, because of other series' readings; (b) a session waiting in `next`
counts as polled, so it is never closed as idle while it waits; (c) a refresh of live staleness rings only the
sessions that select the series; (d) `next` waits on the session's bell with no re-poll, or until a `Latest` falls
due (rechecked at least once a second while one is due, as `now` follows the wall clock); (e) a loss another read of the process found reaches a
waiting session at the thread's next pass, within a second, where the 100 ms re-poll found it before; (f) the shared
tail is read when the writer commits, not at a session's call, so an error of that read reaches a session when the
thread meets it. Reader processes are unchanged. Batches, their contents, order and cuts are otherwise as before.
A fan-out thread that panics is noticed: every session is then rung by every round, as when the thread could not be
started, and none waits on a bell nobody rings.
**Lead rulings (23 November, provisional).** On what users can now observe: (a) fewer `Lagged` events, rounds of
other series no longer counting to a session's backlog, are accepted as the intended behaviour; (e) a loss found
by another read reaching a waiting session within 1 s rather than 100 ms is accepted; (f) an error of the shared
read that the thread meets and that is gone by the session's own turn is not returned to that session, which
`api.md`'s "returned by every `next` it lasts" allows. The pass-over rule is invisible to users only with the
widening at `open` above.
**Pinned by.** `kvik/src/durable/read/live/fanout.rs`
`sessions_polled_only_when_rung_receive_what_sessions_rung_by_every_round_and_the_model_receive` (against sessions
rung by every round, the old path kept as the oracle, and the model backend); `kvik/src/durable/read/hub.rs`
`a_round_rings_only_the_subscribers_it_reaches_and_the_others_caught_up_pass_over_it`,
`a_waiting_session_is_not_marked_idle_and_a_refresh_rings_only_its_series`,
`a_round_ending_between_an_open_and_the_batch_that_answers_it_is_not_passed_over`;
`kvik/src/durable/read/live/fanout.rs` `a_round_ending_inside_the_turn_that_opens_a_tail_is_delivered_to_it`;
`kvik/src/durable/read/live/races.rs` `tails_opened_beside_a_busy_writer_hold_every_fact_above_their_opened_cut`;
`kvik/src/durable/read/live/cases.rs`
`a_missing_segment_under_the_tail_is_an_error_at_every_next_with_the_fan_out_thread_running`;
`kvik/src/durable/read/live/waits.rs`
`a_ring_inside_the_batch_interval_is_taken_once_the_interval_passes`,
`the_rest_of_a_round_a_row_limit_cut_is_taken_once_the_interval_passes_with_no_new_ring`,
`a_fan_out_thread_that_panics_rings_every_session_and_leaves_them_rung_by_every_round`,
`a_refresh_rings_a_session_with_a_latest_of_the_series_and_not_a_tail_alone`. Measured in
`docs/measurements/fanout.md`.

## Q-1119 — Consumer deliveries taken off the emit's path, and wakes per consumer
Decided by the lead with the fan-out change, provisional.
**Scenario.** With a consumer registered, every `emit` read its batch back through the delivery reader and waited for
the consumers it woke, on the caller's thread; every delivery or confirmation then woke every waiting consumer, each
of which polled, found nothing and waited again: wakes grew as consumers times deliveries.
**Decision (provisional).** An acknowledged `emit` notes the clocks at its acknowledgement and returns; the
`kvik-deliver` thread, woken by the writer's turn, or the next call that takes batches, reads the batch back and
takes it as of the earliest acknowledgement inside it (never before an instant already taken as of), so what
consumers receive, conditions included, is what they received when the emit read it back itself. Every call on the
deliverer keeps the acknowledgements noted before it, whether or not it takes batches. The `kvik-deliver` thread can
take a batch a moment before its emitter notes the acknowledgement, which then has nothing left to time: "as of its
acknowledgement" means as of the acknowledgement or the deliverer's read of the clocks, whichever came first (with a
manual clock the same instant). A consumer's
waiting poll is woken only when something it may take changed for it: a fact queued or a catch-up running, a
condition transition, a resync, an entry in a batch (any consumer), or its own confirmation. Consequences: (a) the
Q-706 counts `refresh_errors` and `corruptions` may lag an emit's return with consumers registered too; (b) an
`emit` returns before the consumers it feeds are woken, which the `kvik-deliver` thread does shortly after;
(c) a confirmation wakes nobody but its own consumer. Timers pump first: the timers due when a call is decided run after the acknowledged batches are taken, so that a condition fires at the same position as on the model backend whatever the deliverer's thread has taken; this gives up the small saving of leaving those batches to it. One cursor-file sync per tick for all consumers is not built:
each consumer's file is still synced up to once a second.
**Lead rulings (23 November, provisional).** Accepted: an `emit` returns before the consumers it fed are woken; the
durable backend wakes them shortly after, from `kvik-deliver`, while the model backend still wakes them inside
`emit`, which `api.md` Consuming now says; a consumer that polls after `emit` returns still sees the delivery,
because `poll` takes the acknowledged batches first; the Q-706 counts may lag an emit's return with consumers
registered too.
**Pinned by.** `kvik/src/durable/registering.rs`
`with_a_consumer_an_emit_returns_before_its_batch_is_taken_and_the_delivery_thread_wakes_it`,
`a_batch_read_back_after_the_clock_moved_is_taken_as_of_its_acknowledgement`,
`an_acknowledgement_noted_before_a_call_that_does_not_take_batches_still_times_its_batch`,
`a_batch_acknowledged_earlier_than_one_before_it_is_not_taken_as_of_an_earlier_instant`,
`a_confirm_wakes_its_own_consumer_and_not_another_waiting`, `an_entry_wakes_a_consumer_with_nothing_queued`; the
consumer
properties against the model backend (`kvik/tests/consumer_properties.rs`), whose manual clock moves between steps.

## Q-1130 — Windows, chart buckets and calendar results from the sources
Decided by the lead with roadmap 9.0h, for review.
**Scenario.** Windows, `buckets` and calendar results built the reader's fact index at their first use and kept it
(about 150 bytes a fact, Q-1000), so a reader's memory and its first such call's time grew with the store's age:
47 MiB held over 320,000 facts, 268 MiB over 2.2 million (`docs/measurements/9.0h-bucket-reads.md`).
**Decided.** Each call takes one view and reads the series' current facts with `t` from the reach of its span to its
end through the sources of Q-1060, in passes under the reader's cap and admission (Q-1063, Q-1067, Q-1068); the
reading before the span comes from `at`'s backward search (Q-1065). A cut that a reset or its age ends is read once
more under a new cut, and a second end is `CutExpired`, as for `at` (Q-1061). A window's core is folded 256 readings
at a time and merged (`kvik_tree::merge`, associative and exact: spec §5.3, invariant 6), so a window or bucket holds
one chunk whatever it covers; an interval series' readings are credited as they are taken and kept only while their
interval reaches past the bucket. Buckets fold left to right from one stream of the range, `Resolution::Auto`'s
first `max_points` + 1 facts included (Q-1131). A tiny cap refuses as `facts` does. The fact
index is now built only to find damage (Q-1060) and for a broken state (Q-1006). `Bucket` gains `sum`, `increase`,
`resets` and `regressions` from the bucket's node on both backends (the query design's Q-744), additive since
`Bucket` is `#[non_exhaustive]`. Cost left: a fresh reader's first read fills the lookups' cache (Q-1069), reading
every sealed segment's index (`format.md` A.8) and scanning unsealed ones, so its time grows with the number of
segments, and its memory is bounded by the cache's share; a span of event times per segment would let a lookup pass
over segments outside the range (with compaction, 6.2).
**Pinned by.** `kvik/src/durable/read/source/tests/windows.rs`
`windows_buckets_and_calendar_results_equal_the_fact_index_and_the_model_at_every_cap` (revisions, forecasts and their
issue times, late readings, gaps, restarts, power losses and damaged batches, a change to summer time; against a fresh
reader's fact index, the evaluation before 9.0h, and the model); `kvik/src/durable/windows/tests.rs`
`a_streamed_fold_equals_the_one_shot_fold_and_the_model`; `kvik/tests/windows.rs`
`buckets_carry_a_counters_increase_resets_and_regressions_and_a_gauges_sum`; `kvik/tests/window_memory.rs`
`bucket_window_and_calendar_reads_stay_flat_with_the_stores_age`.

## Q-1131 — Several series' windows, buckets and facts from one read
Decided by the lead with roadmap 9.0h, for review.
**Scenario.** A dashboard reads every series over the same range. One call per series reads the log once per series,
and when every batch holds every series, so a query over 37 series decoded each batch 37 times: 23 s for a
day's charts of a one-day store, 45 s for a week's (`docs/measurements/9.0h-bucket-reads.md`). `Resolution::Auto` read
the range twice, once for its first `max_points` + 1 facts and once to fold.
**Decided.** `Reader::buckets_of`, `window_of` and `facts_of` take several series (in any order, repeats allowed) and
read them in one stream at one cut, in event-time order then series (Q-1062); each result equals the one-series call's,
and the one-series calls are the same code with one series. Checks run series by series in id order, each as its own
call orders them (the series, then the range, resolution, aggregate or window), so the call fails as the first series
in id order that would fail alone. The reading before the range is found for every series in one backward search
(Q-1065): spans that double, each read once for the series still unfound, then one probe per series. Each series
folds from a push-driven core that holds one chunk (Q-1130), so a read holds one chunk per series; with
`Resolution::Auto` it keeps a series' facts until they pass `max_points`, then folds them and the rest, so the range
is read once. The model backend answers series by series. `kvik_export::export_partitioned_of` writes several series'
partitioned files from one `facts_of` read, each series' month file open while the read is in that month, and the
one-series `export_partitioned` is the same code, reading its range once, at one cut, where it read it twice. Provisional: a call over many series holds each series'
results until it ends; a caller wanting fewer at once splits the call.
**Pinned by.** `kvik/src/durable/read/source/tests/windows.rs`
`windows_buckets_and_calendar_results_equal_the_fact_index_and_the_model_at_every_cap` (every series at once, in
reverse with one repeated, against each series' own result and its first failure); `kvik/tests/windows.rs`
`many_series_calls_equal_one_call_per_series_and_the_model`; `export/tests/layout.rs`
`a_partitioned_export_of_several_series_writes_each_series_files_from_one_read_on_both_backends`.

## Q-1132 — Passes sized from counts before they are read
Decided by the lead with roadmap 9.0h, for review.
**Scenario.** A pass that would pass its grant was read until its rows filled it, then halved and read again (Q-1063),
so on a week-old store of 37 series at a reading a second, a week of one series read the log 4.3 times, and an
unsealed segment, with no index, was read again at each narrowed pass. Rows were sorted in place, about 100 bytes moved at each
step, and each row a lookup decoded was looked up in an ordered map of the wanted series.
**Decided.** The lookups' log reader keeps, for each segment it maps, a tally of what it mapped: per series the facts,
their least and greatest `t` (the fields of an A.8 record), in its cache's share. `LogReader::estimate` sums, over the
segments its last lookup listed, the sealed segments' indexes and the tallies, taking each record's facts as spread
evenly over its span; `None` before a lookup has listed them or when a listed segment has neither. A part not narrowed
to one key's positions whose estimate takes more than half the rows an admission would grant is halved before it is
read; a pass takes room for its estimate and an eighth at once (`Within::expect`), so its vector does not grow by
doubling. An estimate is a guide: a pass that still passes its grant halves and reads again as before (Q-1068), and
results are unchanged. Texts' bytes are not estimated, so a text series meets the fallback more. Windows and buckets
take each pass's rows as it is read (`each_row`), with the grant held while they fold, rather than through a stream's
shelf, whose rows wait on a caller (Q-1067). A pass's rows are sorted by their keys `(t, series, position)` with each
row then moved once, when the room left in its grant holds 36 bytes a row; else in place. A lookup finds a row's wanted
ranges in a table by series id when the greatest id is at most 4,096. Every batch a pass reads is still verified
(Q-1063): checking each once per reader would let damage after a first read go unseen, and once passes stop reading
again CRC32C is about 3% of a read; a hardware CRC32C, a new dependency, would take most of that (not built). Not built
either: a span of event times per segment for a fresh reader's first read (Q-1130). Provisional: half a grant as the
target and an eighth as the margin, which keep a wrong estimate of up to a third from meeting the fallback.
**Pinned by.** `log/src/reader/tests/parts_and_lookup.rs`
`an_estimate_counts_what_a_lookup_finds_from_seal_indexes_and_the_tallies_of_unsealed_segments`,
`room_for_the_expected_facts_is_taken_at_once_so_a_budget_doubling_overflows_holds_them`;
`kvik/src/durable/read/source/tests.rs` `a_read_again_is_sized_from_counts_so_no_pass_passes_its_grant`;
`kvik/src/durable/read/merge.rs` `a_sort_by_keys_equals_a_sort_in_place_and_holds_to_the_room_given`; the facts' and
the windows' differentials at every cap (Q-1060, Q-1130, Q-1131).

## Q-1133 — A reader call on more than one thread
Recorded by the lead with roadmap 9.0h; for the owner. Built as option (a) until ruled.
**Scenario.** A long chart read of many series spends about three fifths of its time reading and decoding passes and
two fifths folding them (37 series over a week: 3.8 s and 2.5 s, `docs/measurements/9.0h-bucket-reads.md`). Folding
one pass while the next is read would take the read close to the reading alone, within the cap: the accountant
admits two passes at the default cap. Every reader call so far runs on the caller's thread.
**Options.** (a) Calls stay on the caller's thread. (b) A call whose estimate passes some millions of facts folds on a
scoped thread of its own while the caller's thread reads the next pass; the call still blocks until it ends, and a
reader's lookups still run one at a time. (c) As (b), on a pool the reader owns, sized by its configuration.
**Recommended.** (a) until stored levels (7.1) are built, since they shorten exactly these reads; then (b) if long
many-series reads still matter, as it keeps the call's contract and adds no configuration.
**Pinned by.** Nothing yet.

## Q-1150 — One series' facts from its own batches: series batches at seal
Decided by the lead, for review.
**Scenario.** Without the fact index (Q-1130) a one-series chart read read and checked every batch in its range
whatever series it held. At the home load most batches hold one reading, so a dashboard charting 37 series one call at
a time read a day's log 37 times and more: 55.8 s for a day and 89.5 s for a week, against 2.5 s and 2.9 s for one
`buckets_of` (`docs/measurements/series-reads.md`). A segment's index (A.8) names the series it holds, but every
segment of a day holds every series.
**Decided.** (a) Format: a sealed segment written by the writer that wrote its batches may carry a series batches block
after its index (A.8a): for each series the complete batches holding one of its facts, by offset and length, 8 bytes a
reference. Written when the (series, batch) pairs are at most half the batches times the series, no batch holds a
recovery entry (a read must meet it), and there are at most 262,144 pairs; a segment sealed by a start carries none.
New code reads stores without the block as before; a reader without A.8a skips the block (skippable) and reads the
segment index as before. (b) Reads: a lookup reads a sealed segment's wanted series through the block when the index's
counts put those series under half the segment's facts and their batches under half its batch bytes; each listed batch
is read and verified as a complete batch at its place (Q-1063); a batch that does not read as listed, or a block that
does not check or does not list exactly the index's series, sends the segment to the whole read, which judges its
regions. A batch holding none of the series read is not read, so damage confined to it leaves that read complete, as a
segment whose index lists none of them is not read (A.8). (c) The lookup plans these reads under the lookups' lock and
reads them without it (`Planned::run`), then ends under the lock (`finish_planned`): calls from several threads read
their batches at once; each planned read opens its segment when it runs, so a lookup holds one segment file open at a
time, and reads no lists block longer than a piece (4 MiB), reading that segment whole. A segment taken from reaches
`stood` (Q-951) at `finish_planned`, so a lookup in between does not yet count it; the next does. Passes and folds
stay apart for Q-1133. (d) Batches of unsealed segments a lookup has mapped
carry their sole series, and a lookup skips those of other series. (e) A lookup's batch read leaves the bodies' checks
to its walk of the rows, which makes every check of A.5 and A.6 (`read_batch_in_unjudged`), so each row is checked
once. Results: 37 one-series calls over a day 3.9 s at the 1-day store and 4.2 s at the 7-day one, 1.4 to 1.8 times one
`buckets_of`; three such dashboards at once 1.6 to 2.2 times one. Provisional: the two halves, the 4 KiB gap under which listed
batches are read together, 262,144 pairs, and 8-byte references, which add about 5% to a home store's sealed bytes and
make about three fifths of a one-series read's bytes (delta-coded references would take about a third of that).
`format_minor` stays 0: additions are judged per element (A.3).
**Pinned by.** `log/src/series_batches.rs` (round trip and checks); `log/src/writer/seg_index.rs`
`the_lists_name_each_batch_once_for_every_series_it_holds_while_a_series_is_in_at_most_half`,
`an_index_rebuilt_without_offsets_carries_no_lists`, `a_segment_past_the_most_references_carries_no_lists`;
`log/src/reader/tests/lookup_properties.rs`
`a_sealed_segment_lists_each_series_batches_as_a_scan_finds_them_while_a_series_is_in_at_most_half`,
`a_listed_lookup_finds_what_a_whole_read_finds_and_is_left_incomplete_only_by_damage_to_its_own_batches`,
`a_one_series_lookup_checks_only_its_own_batches_and_the_lists`,
`damage_in_a_listed_batch_leaves_its_series_incomplete_and_spares_the_others`; `log/tests/golden.rs`
(`sealed-listed.kvl`);
`kvik/src/durable/read/source/tests/listed.rs`
`a_one_series_read_of_sealed_segments_reads_them_through_their_series_batches`; the windows' differential against the
fact index, the model and `buckets_of` (Q-1130, Q-1131), whose sealed segments now carry the block.

## Q-1151 — Several callers' many-series reads at once
Recorded by the lead with Q-1150, for review.
**Scenario.** Three dashboards charting 37 series each with one `buckets_of` at once take about three times one (6.6 s
against 2.1 s at a day of the home load): a many-series pass reads most of each segment, so it reads it whole under the lookups'
lock, and the three calls' passes take turns. One-series calls read their batches without the lock (Q-1150 (c)).
**Options.** (a) As built. (b) Concurrent calls over the same range and cut share one pass: a call joins a pass already
reading and folds its series from the same rows. (c) Whole reads of sealed segments also leave the lock, with a second
map cache or none.
**Recommended.** (b) once a dashboard's calls arrive together in practice, as it cuts bytes as well as waits; (a) until
then. Provisional default: (a).
**Pinned by.** Nothing yet.
## Q-1140 — Gates beside today's condition funnels, and what the model leaves to later steps
Decided by the lead with the gates model (conditions step 1), provisional.
**Scenario.** Gates (spec §6 Gates) are specified and judged by the model's `gate` module, but no backend delivers
them yet, and today's condition funnels (§6 Conditions) keep their rules, identities `(funnel, sequence)` and
resume. A reset point or lost range met during a gate's replay, correction notices for superseded cause facts,
provisional evaluation and calendar windows are in the design but not in this step.
**Decision (provisional).** The model is the definition of gates until the durable engine (conditions step 2)
compares itself with it. Losses during a replay: the consumer forgets transition ids whose `at` is above the reset
point's `forget_above`, as it forgets positions, and the instance's state is rebuilt from the view now; recorded
here, specified and modelled with exact resume in the durable backend (conditions step 4). Corrections, provisional
evaluation and calendar windows follow their own steps (7, 8, 5).
Every resync carries gate state, not only the resume's: reset points, `generation_changed` and overflow catch-up
send each instance's state now and the id that set it (spec §6 Gates, Resume); §4 Resync is unchanged for funnels. The
model has only the resume's resync; the others are specified and built in step 4. The latest value with its event
time of a watched raw series at attach (`api.md` Gates, Raw series) is built with the public API, step 3.
**For step 2, the durable engine.** (1) A timer falls due on the writer's progress, not only the clock: fire a hold
or staleness timer due at `d` only once no batch with commit time below `d` can still arrive between the writer and
the deliverer (as the fan-out fix runs acknowledged batches before timers), with a test that holds the delivery
thread across a hold's due instant; the same watermark gives the last position before `d` that a timer's id needs.
(2) Window values must equal the model's bit for bit: the model judges `oracle::window` over all facts and the
engine will fold per bucket, so a level compare on a float sum or average can differ at the boundary unless the
engine sums in the oracle's order; the differential compares the value exactly, or records a tolerance at the
boundary and which.
**Pinned by.** Nothing yet: the `gate` module has no reset point in its histories.

## Q-1141 — When a gate's input has no value
Decided by the lead with the gates model, provisional.
**Scenario.** A gate is watched on a series with no live-state fact yet; a Levels gate meets a code; a window holds
no reading its aggregate can use (min, max, sum and count over an empty window, spec §6 Aggregates), or covers less
than `min_coverage` of its width.
**Options.** (a) `Unknown`, its own state, as a restore's unknown (§6 Conditions); (b) the lowest level or no match;
(c) `Stale`.
**Decision (provisional).** (a): a hub combining gates must not read "no data" as "low"; `Stale` stays for silence
after readings, which is what the owner asked it to mean. Pending holds break (Q-1146).
**Pinned by.** `tests/gates.rs` `unknown_until_the_first_reading`,
`a_window_below_its_min_coverage_is_unknown_and_known_once_covered`; `tests/gate_edges.rs`
`a_value_a_levels_gate_cannot_compare_breaks_a_pending_hold_which_restarts_after_it`.

## Q-1142 — A transition's identity names the instance's series, not an instance number
Decided by the lead with the gates model, provisional.
**Scenario.** The design gave `TransitionId` an `instance: u16`. Under `Each`, instance numbers depend on the order
series joined, so a series that leaves and joins again, or a definition committed in a different order, renumbers
instances, and an id must be a function of the log alone.
**Decision (provisional).** `TransitionId = (at, due, condition, series)`: the series id is stable for the series'
life and is the same for a single-series gate's one instance. Four more bytes per id.
**Pinned by.** `tests/gate_properties.rs` `judging_a_prefix_gives_the_same_transitions_as_the_whole_history_up_to_its_horizon`.

## Q-1143 — An `Each` past its 256 instances
Decided by the lead with the gates model, provisional.
**Scenario.** A pattern matches a 257th series.
**Decision (provisional).** The series is refused, not instantiated, and counted once (`instances_refused`) each
time it starts to match while the gate is full; it joins at the first definition entry or watch that finds room,
including one where another series leaves. Refusing the watch would make a running gate fail when an unrelated
series is defined.
**Pinned by.** `tests/gates.rs` `each_takes_at_most_its_instance_limit_and_counts_the_series_refused`; `tests/gate_edges.rs`
`a_series_refused_by_a_full_each_joins_with_a_start_transition_when_room_frees`.

## Q-1144 — A timer due at an entry's commit time
Decided by the lead with the gates model, provisional.
**Scenario.** A hold completes at 6 s and a fact that breaks it commits at 6 s.
**Options.** (a) Timers due at or before an entry's commit time are judged first; (b) the entry first.
**Decision (provisional).** (a): the value held for the whole hold, `[since, since + hold]` closed at its end as the
staleness limit is (a gate is `Stale` once silent for `stale_after`, not longer), and the timer's id then names the
last position before the entry, so it does not depend on what the entry holds.
**Pinned by.** `tests/gates.rs` `a_timer_due_at_an_entrys_commit_time_fires_before_that_entry`.

## Q-1145 — A gate's state when it starts
Settled by the owner, 26 November: option (b).
**Scenario.** A Levels gate with `hold_up` 5 min is watched (or its series joins an `Each`) while the value has been
above its level for an hour.
**Options.** (a) Boundaries start uncrossed and holds count from the start, so the state at the watch is the lower
side and rises after the hold; (b) judge the holds over the facts before the start (the history is in the store).
**Decision.** (b): a gate added mid-event seeds its holds from history, by the rule of Q-1390. (a) was the lead's
provisional choice; it remains only for gates with no hold, which read no history.
**Pinned by.** `tests/gate_seed.rs` `a_hold_is_seeded_from_before_the_watch_so_a_value_long_above_starts_crossed`,
`a_gate_watched_after_its_series_has_data_starts_from_none_at_its_state_now_judged_from_history`,
`a_seeded_start_judges_as_the_same_gate_watched_at_the_look_backs_start`.

## Q-1146 — Holds across `Unknown` and `Stale`
Decided by the lead with the gates model, provisional.
**Scenario.** A hold is pending when the series falls stale (or a window drops below its coverage); readings
return with the same value.
**Decision (provisional).** Entering `Unknown` or `Stale` breaks every pending hold, which starts again at the
first evaluation with a value; crossings already made are kept, so hysteresis continues from the last known level
and the transition out of `Stale` goes to the level the value gives from there.
**Pinned by.** `tests/gates.rs` `stale_breaks_a_pending_hold_which_starts_again_after_the_arrival`,
`stale_after_silence_on_a_timer_and_left_at_the_next_arrival`.

## Q-1147 — Window inputs in the model: sampling and the reading at `now`
Decided by the lead with the gates model, provisional.
**Scenario.** A window `[now − w, now)` is evaluated as a fact commits at `now` with `t = now`.
**Decision (provisional).** The window is the §6 window evaluated at the instant (`oracle::window`), so a reading
at `now` is outside it until the next evaluation; windows are sampled on each fact of their series and on ticks at
whole multiples of the cadence (default 1 s) on the commit clock. A time-weighted aggregate between ticks is not
judged. Calendar windows and their boundary timers come with the window step on the live-bucket module.
**Pinned by.** `tests/gates.rs`
`a_window_sum_rises_on_the_tick_its_readings_enter_and_falls_on_the_tick_they_leave`.

## Q-1148 — What counts as an arrival for a gate's staleness
Decided by the lead with the gates model, provisional.
**Scenario.** A bad-quality reading, an outranked one, and a future one arrive while a gate's series is quiet.
**Decision (provisional).** An arrival is a fact not outranked at commit (§4 Outranked facts) and neither bad
quality nor without a value (§2.5, §5.4: such a fact "does not refresh staleness"), at its commit time, or for a
future fact at its `t` (§4 Future facts: ignored by staleness until `t ≤ now`). A future fact outranked before its `t` by a fact
that is not an arrival is not an arrival at `t` (Q-1200). A sensor that keeps sending bad
readings therefore goes `Stale`; a `Value::Absent` reading is bad whatever quality was supplied, so no reading of
Q-16 (e) covers it. A series with no arrival yet stays `Unknown`, however long since the watch: `Unknown` means no
data yet, `Stale` silence after data. Today's stale-for conditions are unchanged.
**Pinned by.** `tests/gates.rs` `a_bad_quality_fact_neither_moves_the_value_nor_refreshes_staleness`;
`tests/gate_edges.rs` `a_reading_without_a_value_does_not_take_a_silent_series_out_of_stale`,
`a_series_with_no_arrival_is_unknown_never_stale`,
`an_outranked_fact_neither_moves_the_value_nor_refreshes_staleness`,
`a_future_fact_refreshes_staleness_only_once_it_takes_effect`.

## Q-1149 — The longest duration of a gate
Decided by the lead with the conditions step 1 fix round, provisional.
**Scenario.** A hold, `stale_after`, a window width or a cadence of `i64::MAX` nanoseconds would overflow the
deadlines the judgement computes from the caller's numbers.
**Options.** (a) Refuse beyond a stated maximum; (b) accept any value and saturate.
**Decision (provisional).** (a), ten years (`MAX_DURATION`, 365-day years): no gate needs longer, and a refusal at
`watch` is visible where saturation would hide a misconfiguration. Deadlines are also computed with checked
arithmetic, so a late commit clock plus the maximum never panics: a deadline beyond the clock's range never falls due.
**Pinned by.** `tests/gate_edges.rs` `durations_beyond_the_maximum_are_refused_and_the_maximum_is_accepted`,
`the_longest_durations_late_on_the_commit_clock_judge_without_overflow`.

## Q-1200 — A future fact outranked before its `t`
Settled by the owner on 2026-11-26.
**Scenario.** `stale_after` 10 s; a reading at 1 s; a future fact for 30 s commits at 2 s and is outranked at 3 s by
a valueless revision. Counting the future fact at 30 s takes a silent series out of `Stale` on a reading withdrawn.
**Decision.** It is not an arrival at `t`: an arrival not yet due is dropped when its `t` is outranked by a fact that
is not an arrival (sentence added to Q-1148).
**Pinned by.** `tests/gates.rs` `a_future_fact_outranked_before_its_t_does_not_take_a_silent_series_out_of_stale`.

## Q-1201 — Records held for a consumer not taken in this run
Settled by the owner on 2026-11-26.
**Scenario.** A consumer with a cursor file that no process takes pinned every record above its cursor for the life
of the process, and no call removes a consumer.
**Decision.** No records are held for a consumer not taken in this run; its take reads the log again. A call to
forget a consumer's name is later work.
**Pinned by.** `kvik/src/durable/deliver/tests.rs` `a_consumer_not_taken_in_this_run_holds_no_records`.

## Q-1202 — `CursorBehindLog` on the durable backend
Settled by the owner on 2026-11-26.
**Scenario.** Spec §4 names the cause; the durable backend never trims the log below a cursor, so never issues it.
**Decision.** Model only until the log trims, said in `api.md`; built with the first trimming path.

## Q-1203 — A fan-out round that fails after taking events
Settled by the owner on 2026-11-26.
**Scenario.** A round longer than one part takes events from the log reader, then a read fails; the reader never
returns those events again, so dropping the round's collector lost facts for every `Tail` with nothing said.
**Decision.** The round finishes as not whole, so every session is told `Lagged`; its collector is never carried
over, since a carried collector grows without bound while the fault lasts. A round that took nothing is dropped.
**Pinned by.** `kvik/src/durable/read/live/lags.rs` `a_round_failing_after_taking_events_lags_the_tail_it_took_them_for`.

## Q-1390 — The seed at a gate's start
Decided by the lead with cond2 step 1, provisional, to the design's default; what the seed reads amended by Q-1560.
**Scenario.** Q-1145 (b) seeds a starting gate's holds from history; the rule must be a function of the log, cheap
at a watch, and say what the value was before the first reading it reads.
**Decision.** With `L` the gate's longest hold, the look-back is `[T − min(2L, 24 h), T)`, `T` the start's commit
time. The instance is judged, emitting nothing, as if watched at the look-back's start with boundaries uncrossed,
over the series' state then and its facts committed inside the look-back before the start entry (Q-1560). Then the start transition (from none to the state at
`T`, evaluated with the live-state value then) is emitted at the start's position. `2L`, so that a series
reporting at least once per `L` completes a hold that truly held; a value first read inside the look-back later
than `T − L` completes its hold late, never early. Gates with no hold read nothing. Staleness is judged at `T` from
the series' arrivals; inside the seed the facts before the look-back count as arriving at its start (Q-1561). A
window input in the seed sees the series' facts as the seed reads them.
**Pinned by.** `tests/gate_seed.rs` `a_seed_reads_twice_the_longest_hold_back_and_counts_from_the_first_reading_inside`,
`a_value_that_rose_shortly_before_the_watch_completes_its_hold_late_never_early`, `the_look_back_is_at_most_a_day`,
`a_gate_without_holds_reads_no_history_so_its_boundaries_start_uncrossed`,
`a_series_joining_an_each_is_seeded_from_its_history_at_the_join`, `a_fact_committed_at_the_look_backs_start_is_read`,
`the_longest_hold_counts_hold_down`, `a_sets_longest_hold_counts_hold_off`,
`a_seeded_start_judges_as_the_same_gate_watched_at_the_look_backs_start`.

## Q-1391 — Durable resume state for gates
Decided by the lead with cond2 step 1, provisional, to the design's default.
**Scenario.** A consumer's gates must resume exactly after a crash. Per-instance latches cost a record per instance
on every confirmation, and a bound measured from the cursor refuses the replay of a consumer whose gates have been
quiet for longer than `replay_max.span`.
**Decision.** One gate cursor per consumer, a point in id order (the last confirmed transition id, or the frontier
of the last confirmed resync), plus a checkpoint frontier at or after it and never after an unconfirmed transition.
Confirmation is cumulative, so the cursor filters exactly as per-instance latches would. Resume replays every
transition after both; the bound (`replay_max`) is measured from the checkpoint, so a quiet consumer still replays
exactly. `kvik_model::gate::resume` takes the checkpoint and the gate cursor.
**Pinned by.** `tests/gate_properties.rs` `a_restart_resumes_every_transition_once_after_dedup_by_id` (the replay
equals the per-instance latch filter); `tests/gates.rs`
`the_replay_bound_counts_facts_and_time_from_the_checkpoint_not_the_gate_cursor`,
`a_frontier_gate_cursor_covers_timer_transitions_due_by_its_instant_only`; `tests/gate_edges.rs`
`the_replay_bound_runs_from_the_checkpoint_not_from_an_old_gate_cursor`.

## Q-1393 — The clock live gate timers run on
Open, for the owner.
**Scenario.** A gate's holds and staleness are timers. On the commit clock a forward step of the host clock
completes holds and fires staleness at once, and a quiet store fires no timer until its writer's commit clock
reaches the deadline; on the host clock live and replay can disagree.
**Options.** (a) The commit clock, as for replay: a timer due at `d` fires once every batch committed before `d` is
judged and the writer's commit clock has reached `d`, a backward step pausing timers; (b) the host clock live and
the commit clock on replay, accepting that they can differ.
**Provisional default.** (a), the commit clock, so live and replay agree (spec §6 Gates, Clock).
**Pinned by.** none yet: the model judges on the commit clock only; the engine's live timers are cond2's.

## Q-1560 — Facts inside the seed's look-back that were never the live value
Decided by the lead with cond2 step 1's fix round, provisional, to the review's default; amends Q-1390.
**Scenario.** Level 10, holds 5 s both ways; the live value is 5 (`t` 50, committed at 50); a backfill for `t` 40
with value 50 commits at 92; the gate is watched at 100. Seeded from the facts committed inside the look-back
alone, on an empty series, the backfill is the live value and the gate starts crossed, then falls at 105; the same
gate watched at 90 stays at `Level(0)`. A fact outranked at its `t` (lower `changed_at`) does the same.
**Decision (provisional).** The seed judges the series' true live state over the look-back: every fact of the series
committed before the look-back counts as committed at its start, in position order, then the facts committed inside
it before the start entry. A backfilled or outranked fact is never the live value, and the stand-in of Q-1390 is
no longer needed. The engine then needs the live fact as of the look-back's start (one read by position), to be
costed in cond2 step 2. **Fallback** if that read is too dear: count an inside fact only when it is not outranked
and its `t` is at or after the look-back's start; that stays late, never early, except against future facts.
**Pinned by.** `tests/gate_seed.rs` `a_backfill_committed_inside_the_look_back_is_not_the_live_value_in_the_seed`,
`an_outranked_fact_committed_inside_the_look_back_is_not_the_live_value_in_the_seed`,
`a_backfill_before_a_late_rise_does_not_start_the_hold_early`,
`a_bad_fact_inside_the_look_back_leaves_the_earlier_live_value_in_the_seed`,
`a_future_fact_taking_effect_inside_the_look_back_holds_from_its_t`,
`a_seeded_start_judges_as_the_same_gate_watched_at_the_look_backs_start`.

## Q-1880 — What the gates core's history reads must return, and what they cost
Decided by the lead with cond2 step 2's fix round, provisional, to the review's default; moves Q-1560's costing.
**Scenario.** The core holds one live fact per series (G9's bound), so three things it cannot answer go to
`History`: the series' state at a watch, the fallback when a bad or valueless fact replaces the live one at its
`t` (spec §2.3), and the seed's look-back (Q-1390, Q-1560). The rules that matter were only in the tests' double.
**Decision (provisional).** (1) `series(now, through)` is the state over the log through a position, not at its
head: live, `through` is the item being applied, so the read sees the batch in flight, the triggering fact and
earlier facts of the same batch included. (2) Its `last_arrival` counts the commit time of a usable fact even when
a later fact replaced it at its `t`, and a fact committed ahead of its `t` counts at its `t` unless replaced before
then: commit times are needed, which a latest-value index does not hold. (3) `at_look_back` is the state as of a
commit time, every earlier usable fact in effect by then arriving at `from`; whether one still ahead at `from`
counts there is Q-1882. (4) The fallback runs on the commit path, once per bad fact that lands on the live `t`.
Costed in cond2 step 4 against the hot log and compacted segments, with the real `History` checked against the
tests' double over the same generated logs. If `series` through a past position is too dear for replay, step 5
replays from the checkpoint's held series states and reads only facts ahead.
**Pinned by.** `kvik/src/gates/tests.rs` `a_bad_fact_replacing_the_live_one_falls_back_to_the_older_value`,
`the_seed_starts_from_the_live_fact_in_force_at_the_look_back_start`; the core's differential
(`kvik/src/gates/differential.rs`).

## Q-1881 — How a failed or short history read reaches the gates core
Decided by the lead with cond2 step 2's fix round, provisional.
**Scenario.** The seed's read may not reach the look-back's start (compacted away), and any read may fail (damage,
I/O). The design seeds over what was read and counts `seeds_shortened`, and abandons a replay whose read fails.
**Decision (provisional).** `History`'s reads are fallible: a failed read is `CoreError::History`, the call fails,
and the core is not used again (any `CoreError` part-way through a batch is fatal to it; the caller restores from
its checkpoint). A watch reads before it changes anything, so a failed read there adds nothing of the gate. A
short read is not an error: `History` returns what it reached and its caller counts `seeds_shortened`.
**Pinned by.** `kvik/src/gates/tests.rs` `a_failed_history_read_fails_the_call_and_a_watch_adds_nothing`.

## Q-1882 — Whether the seed's start state counts a fact still ahead as an arrival at the look-back's start
Decided by the lead with cond2 step 2's fix round, provisional.
**Scenario.** A usable fact committed before the look-back's start with `t` beyond it. `at_look_back` may count it
as an arrival at `from` or not.
**Decision (provisional).** Either: a seeded instance's state is never read (the start evaluation recomputes it),
and with no live fact at `from` its holds and crossings stay empty either way, so transitions and states are the
same. The real `History` takes whichever read is cheaper.
**Pinned by.** Nothing can: the tests' double's choice is an equivalent mutant.

## Q-1561 — Arrivals inside the seed
Decided by the lead with cond2 step 1's fix round, provisional, to the review's default.
**Scenario.** A series silent since commit 1 (value 50), `stale_after` 8 s, hold 5 s, watched at 100: inside the
seed its facts count as arriving at the look-back's start, so the hold completes in the seed; the start is `Stale`
and the next reading gives `Level(1)` at once. A gate watched at the look-back's start would hold again from that
reading; one watched all along gives `Level(1)` at once, as the seed does.
**Decision (provisional).** Keep: inside the seed the facts before the look-back arrive at its start. It agrees with
a gate watched all along; spec §6 Gates, Transitions says so.
**Pinned by.** `tests/gate_seed.rs` `a_series_stale_throughout_the_look_back_still_completes_its_hold_in_the_seed`.

## Q-1580 — The routing fields of a watch and an unwatch
Decided by the lead, provisional (settle with conditions step 4).
**Scenario.** Routing changes 4 and 5 need field ids after 66. Ids are per entry type,
but `format.md` A.6 names field 70 as the label item of type 10, and one id meaning two things across types invites
mistakes. A set gate's members need a value form in the entry codec, which had none.
**Decision.** 67 gate id, 68 name, 69 selector, 71 input, 72 kind, 73 level, 74 member, 75 holds, 76 dead-band,
77 stale after; 70 is skipped and stays an unknown critical field of a routing change. A member is a tag byte then
the value: 1 `f64`, 2 decimal (`i64` units, `u8` scale), 3 named state index `u32`, 4 boolean byte, 5 text of 1 to
255 bytes. Window inputs (71 = 2) are refused when read until their fold exists.
**Pinned by.** `log/tests/entries_gates.rs` (every field check, the byte vector of `format.md` A.6),
`log/tests/batches.rs` round trip.

## Q-1581 — The gate interface as built
Decided by the lead, provisional (settle with conditions step 4).
**Scenario.** `api.md` Gates sketched `Transition.flags`, a `Cause` enum and no error for an unknown name or a name
the grammar refuses.
**Decision.** A transition's flags are its delivery's (`Delivery.flags`, as for facts and triggers); the cause enum is
`TransitionCause`; `Consumer::gates()` returns `(GateId, String, Gate)` with the series by id; `GateError::Name` (not
1 to 64 bytes, or holding `/`) and `GateError::NoSuchGate` (unwatch of a name not in force) are added;
`GateInstanceState.set_by` is an `Option` (Q-1392). An equal spec is compared after resolving a series name, so a
watch by name and one by id of the same series are equal. A builder method of another kind (`deadband` on a levels
gate) has no effect.
**Pinned by.** `kvik/tests/gates.rs`, `kvik/src/gate.rs` tests, `kvik/src/durable/gates/tests.rs`.

## Q-1582 — Every entry moves a gate's frontier
Decided by the lead, provisional (settle with conditions step 4).
**Scenario.** A timer transition's `at` is the last position of the whole log before its instant, but `kvik_model::gate::Entry` could carry only facts, definitions and the consumer's own gates, so the
model backend could not hand `judge` a funnel change, another consumer's watch or a site-zone entry.
**Decision.** `gate::Entry::Other { position, commit_time }` carries any other entry; it changes no state.
`Model::gate_history` maps the whole log so.
**Pinned by.** `kvik/src/backend/gates/tests.rs`
`a_watch_starts_from_none_and_a_crossing_after_its_hold_is_a_timer_transition`,
`a_timer_after_a_funnel_change_takes_that_entry_as_its_at`.

## Q-1583 — The model backend's gates for now
Decided by the lead, provisional (settle with conditions step 5).
**Scenario.** The model backend serves gates from `judge` so that the durable engine has an oracle; some parts of the
interface have no model counterpart yet.
**Decision.** A decimal member is compared as the `f64` it denotes; a named-state member on a gate whose series has
no named states (every `each` gate) is `NotSupportedYet`. The gate cursor is the last confirmed transition, kept in
the process with the store; after a restart every transition after it comes again flagged `replayed`, with no
replay bound, and a resync carries each instance's state at the model's now, as it is handed out. A confirmation is
cumulative across transitions and the model's other deliveries (cond2 step 4). The durable backend refuses window
inputs and `each` gates with `NotSupportedYet` (until cond2 step 6 for `each`).
**Pinned by.** `kvik/src/backend/gates/tests.rs`
`unconfirmed_transitions_come_again_flagged_replayed_after_a_restart_and_a_resync_carries_gate_states`,
`kvik/src/durable/gates/tests.rs` `an_each_gate_is_refused_not_supported_yet_and_writes_nothing`.

## Q-1810 — The bounds of a set member
Decided at the review of conditions step 3, provisional.
**Scenario.** Field 74 took any `u8` decimal scale and any `u32` state index, where every other decimal in the
format is held to scale 18, a series has at most 65,536 names and `Value::State` is `u16`. Neither could match a
value, and once stores hold such entries the format cannot be tightened.
**Decision.** Scale at most 18 and index below 65,536, refused when read (field 74) and the scale at `watch`
(`GateError::Set`; the index cannot exceed `u16`).
**Pinned by.** `log/tests/entries_gates.rs` `gate_field_values_are_range_checked`, `kvik/src/gate.rs`
`a_set_holds_one_to_sixty_four_values_none_absent`.

## Q-1811 — Member order in an equal spec
Decided at the review of conditions step 3, provisional.
**Scenario.** A set gate watched again with the same members in another order.
**Decision.** Not an equal spec: members compare in order, as written, so the second watch is `NameInUse`.
`api.md` Gates, Naming says so.
**Pinned by.** By reading (the spec comparison is the entry's equality).

## Q-1812 — A named-state member on the durable backend
Decided at the review of conditions step 3, provisional.
**Scenario.** The model backend checks a named-state member against its series' names at `watch`; the durable
backend wrote the index unchecked, so the two disagreed once durable gates are live.
**Decision.** Both call one check: an index past the names is `GateError::Set`; a state member on a series without
named states, every `each` gate included, is `NotSupportedYet` until `each` gates carry state names.
**Pinned by.** `kvik/src/durable/gates/tests.rs`
`a_named_state_member_is_checked_against_the_series_names_as_on_the_model`.

## Q-1800 — A fact at position `0:0` in a history given to `judge`
Decided by the lead at the re-check of cond2 step 1.
**Scenario.** The seed's inner run starts after `Position::ORIGIN`, so a fact at `0:0` committed inside the
look-back made `judge` panic; the outer run accepted it. No log holds one (positions start at `0:1`).
**Decision.** `judge` refuses a fact at the origin as `GateError::Order`, so the seed's invariant holds for every
history it accepts. A watch or other entry at the origin stays accepted (the tests' early-watch oracle uses one).
**Pinned by.** `model/tests/gate_seed.rs`
`a_fact_at_the_origin_position_is_refused_as_out_of_order_not_a_panic_in_the_seed`.

## Q-2000 — The durable gate cursor in memory until the cursor file holds it
Decided by the lead with cond2 step 4, provisional; settle with cond2 step 5.
**Scenario.** Gates are judged live before the cursor file has a gate cursor field or a checkpoint block.
**Decision (provisional).** The gate cursor and the held snapshot (the core at or before the oldest unconfirmed
transition, G5) live in the process. A second take in the same run queues the unconfirmed transitions again under
their ids (flagged `replayed` if handed out before) as its catch-up passes their positions, then the resync with state
now: the same transitions a replay from the held snapshot gives, without reading the log. The first take after a start
reseeds the gates in force at the head by the seed rule, emits no start transitions and reports `set_by` `None`
(Q-1392), counted `gates_reseeded`; nothing is replayed.
**Pinned by.** `kvik/tests/consumer_properties.rs` `gate_deliveries_follow_the_model_without_restarts`,
`a_gate_watched_after_a_second_take_starts_behind_the_resync_that_take_sends`; `kvik/src/durable/gates/tests.rs`
`a_confirmed_transition_moves_the_gate_cursor_in_memory_and_drops_the_held_snapshot`; `kvik/tests/gates.rs`
`every_start_reseeds_the_gates_with_no_replay_and_set_by_none_until_the_cursor_file_holds_the_gate_cursor`.

## Q-2001 — What the durable backend's gate history reads cost
Decided by the lead with cond2 step 4, narrowed at its review, provisional; settle with cond2 step 5.
**Scenario.** Q-1880 asks for the real `History` to be costed and checked against the tests' double. The first
decision scanned the whole log per read, twice per gate at every reseed, under the deliverer lock.
**Decision (provisional).** A read takes the series' facts at or before `through` through the segment indexes
(`format.md` A.8; a segment without one is scanned) and judges them by the double's own rules (`Replay`), so the two
cannot differ; the gate differential checks it end to end. One read per series and position serves a whole core
call, so a reseed reads each watched series once for all its gates. A read that cannot read every place that may
hold the series fails (Q-1881). Cost: memory and I/O proportional to the series' facts, not the log's; still
unbounded in time. The bounded read of Q-1880 (from `commit_at(T − 2L)`, with the series' state there from the
summaries) is a merge condition on cond2 step 5, which builds it with replay.
**Pinned by.** `kvik/tests/consumer_properties.rs` `gate_deliveries_follow_the_model_without_restarts`;
`kvik/src/durable/gates/tests.rs` `a_reseed_reads_a_series_from_the_log_once_for_all_its_gates`.

## Q-2002 — Gate state after an overflow catch-up
Decided by the lead with cond2 step 4; replaced at its review, provisional.
**Scenario.** Spec §6 Gates, Resume says the overflow catch-up's resync carries gate states, but §4 Overflow ends a
catch-up with no resync: condition funnels re-attach by sending each changed state. The first decision re-sent, on
re-attaching, the transition that set each changed state. That transition was judged before any snapshot the run
still holds, so G5 cannot hold for it, and after a second take it came again unflagged though confirmed.
**Decision (provisional).** Transitions judged while a consumer is behind wait in a buffer as large as its queue and
are queued as the catch-up passes their positions, so a short catch-up loses none and needs no resync. A buffer that
fills makes the gates quiet: judged, counted `transitions_not_delivered`, not queued. Any resync queued while quiet
ends it (its states are taken as it is handed out, Q-2003); a catch-up still quiet at its end issues one at the last
position taken, as a resume does: cause `Resume`, funnel states included, so funnels re-attach by it (§4 Overflow, a
resync counts as emitting the states it carries). No new resync cause.
**Pinned by.** `kvik/tests/gates.rs`
`a_gate_that_overflows_its_queue_counts_what_it_drops_and_ends_its_catch_up_with_a_resync_of_the_present_state` and
`a_transition_confirmed_before_an_overflow_does_not_come_again_after_a_second_take_while_quiet` (the model backend
has no overflow: its transitions are computed when handed out).

## Q-2003 — When a resync's gate states are taken, and the gate cursor its confirmation sets
Decided by the lead with cond2 step 4, provisional.
**Scenario.** A resync is queued at one moment and handed out later; transitions may be judged in between.
**Decision (provisional).** Its gate states are taken as it is handed out, as the model backend's are. Confirming it
sets the gate cursor to the frontier at the resync's position: every transition at or below it is queued before the
resync and none above, so the transitions queued behind it stay unconfirmed.
**Pinned by.** `kvik/tests/consumer_properties.rs`
`a_gate_watched_after_a_second_take_starts_behind_the_resync_that_take_sends`.

## Q-2250 — When a run takes the snapshot that becomes `held`
Decided by the lead at the cond2 live fix round, provisional.
**Scenario.** A consumer that is behind with nothing outstanding can hold transitions in `deferred`, judged in an
earlier batch. A snapshot taken at the next batch is after their judgement, and the catch-up then makes it `held`
under them (G5).
**Decision (provisional).** A run takes the snapshot that becomes `held` only when `outstanding` and `deferred` are
both empty: in `gate_batch` and in `ready`. `gate_confirm` takes none; it clears `pre`, and the next `ready` retakes it
under the same condition. Settle with cond2 step 5, which writes `held` as the checkpoint.
**Pinned by.** `kvik/tests/gates_snapshot.rs`
`a_transition_deferred_while_behind_is_delivered_once_after_later_batches_judge_nothing`; the S5 test
`the_snapshot_held_for_a_transition_is_from_no_earlier_than_the_batch_before_its_judgement` still holds.

## Q-2251 — Deferred transitions older than a dropped one, after the resync that ended the quiet
Decided by the lead at the cond2 live fix round, provisional.
**Scenario.** A resync that ends a quiet clears `quiet` while `deferred` may hold transitions above the resync's
position and older than the ones dropped; they arrive after it with a state it superseded.
**Decision (provisional).** Keep, as Q-2003 accepts (`set_by` names the newer id). `api.md` is not changed here: a
transition whose id is below a resync's `set_by` for its instance is superseded. Not probed in this round; settle
by step 5's merge.

## Open at v0.2

The specification was tagged v0.2 with these questions still open, by the owner's decision. Each is to be settled
by the roadmap item named here, before that item is done. Until then the model's choice, where a test pins it, is the
one named in the question. Settling Q-33 or Q-88 may add fields to `docs/format.md` or calls to `docs/api.md`
after gate 3.3; such a change is made by the item that settles the question, in its own pull request, and noted in
the question. Q-34 (c), Q-47, Q-48 and Q-79, and parts of Q-33 and Q-52, were settled before gate 3.3, with their
changes to both documents (see Settled). Q-97 to Q-99, and parts of Q-52, Q-86 and Q-93, were settled at gate 3.3;
Q-87 was reopened there. Q-87 and Q-100 to Q-107 were settled after gate 3.3 by the owner's rulings; Q-108 to Q-111 were recorded then. Q-117 was settled by roadmap 4.5 as an extension of the Q-87 ruling. Q-118 and Q-119 were recorded by roadmap 4.6; Q-119, Q-46 and parts (e) and (l) of Q-118 were settled after it by
the owner's rulings. Q-121 and Q-122 were recorded by roadmap 4.7, which settled Q-122. Gate 4.9 settled Q-109 and parts
of Q-115, Q-116
and Q-118, recorded Q-115 (n), and added the parts of Q-115, Q-116 and Q-118 still open to this table. The first run of
the `fault-trials` workflow settled Q-86 (b) on Linux, and its third run Q-52 (b) on Linux (ext4 and XFS). Roadmap 5.1 settled Q-121 and recorded Q-123; roadmap 5.1a recorded Q-124 and Q-125, both settled after its review. Roadmap 5.2 recorded Q-130 to Q-132, and its review settled them with Q-91, Q-95 and Q-116 (b) and (m). Roadmap 5.4 recorded Q-140 to Q-142. Roadmap 6.1 recorded Q-150, settled at its review. Roadmap 8.2 recorded Q-160 to Q-164; the interfaces commit of roadmap 8.1 and 8.3 recorded Q-198. Roadmap 6.4a recorded Q-190 to Q-197, the provisional Parquet export schema (`docs/export.md`). Roadmap 6.3a recorded Q-171 to Q-175, settled Q-171 and Q-174 at its review, withdrew Q-172 and moved the parts of 6.3 it does not build to 6.3b. Roadmap 7.1a recorded Q-180 to Q-182, settled at its review. Roadmap 8.1a settled Q-160 and Q-162, settled Q-198 (e) and (f), and recorded Q-210 to Q-214. Roadmap 8.1b recorded Q-270 and Q-271. The Linux NVMe run of roadmap 4.8 recorded Q-310. Roadmap 8.1c recorded Q-280 to Q-285. Roadmap 6.3b recorded Q-220 and Q-221, and its review recorded and settled Q-640. Roadmap 8.3a recorded Q-290 to Q-295, settled Q-108, Q-290 and Q-293, and Q-295 by the owner's ruling at its review, and left the claim points of Q-39 (f), Q-161 and Q-163 to the restore executor. Roadmap 8.3b recorded Q-300 to Q-305 and settled Q-39 (f), Q-82, Q-213, Q-214 and Q-305. Stacking 8.3b on 8.3a settled Q-300, settled Q-271 as superseded, and recorded and settled Q-340, both by the owner's rulings, and recorded Q-341 and Q-342. Roadmap 4.9d settled Q-110, and recorded Q-230 and Q-231, which the owner then ruled on; its fix round recorded Q-650, and its re-check Q-651; the owner ruled on both on 2026-11-17, and Q-870 was recorded by that work and settled by its fix round, whose re-check recorded Q-873. Roadmap 4.9e settled Q-873 and recorded Q-970. Roadmap 5.5a settled Q-170 and recorded Q-250 to Q-253; its review recorded Q-254 to Q-259, Q-680 and Q-681. Roadmap 5.6a settled Q-88 and recorded Q-670, whose parts (a) and (b) the owner's ruling settled after it. Roadmap 8.3g recorded Q-610 to Q-613 and settled Q-304; its review recorded Q-614 and Q-615; the owner's ruling on its part (d) settled Q-612. The owner's rulings of 2026-11-16 settled Q-190 to Q-197, leaving Q-191 (b) to 8.3, Q-192 (b) to 6.4b, Q-196 (c) to 8.3 and the calendar part of Q-197 to 7.3; applying them recorded Q-760 and Q-761. The adaptive interval of the log writer recorded Q-710 to Q-713. The flush-pacing change recorded Q-980 to Q-985 and superseded the schedule of Q-310. The owner's ruling of 2026-11-17 settled Q-614 (the last-uploaded marker); applying it recorded Q-960 and Q-961, and its review Q-962. The store-path quick wins recorded Q-704 to Q-706; the lead settled Q-705 and Q-706. The adaptive batch-sync interval recorded Q-710 to Q-713; the owner settled Q-710, the lead decided Q-711 and Q-712, and its review settled Q-713. Roadmap 11.0 (the bounded decision history) recorded Q-750 to Q-755; the owner settled Q-750 at its review. The first profiling sweep recorded Q-830 to Q-835; the lead decided Q-830, settled Q-831, declined Q-833, kept Q-832 with Q-752, recorded Q-834 only and deferred Q-835. Its re-check recorded Q-836. Roadmap 9.0a (the derived files) recorded Q-773 to Q-779 and decided Q-774; its review decided Q-773 and settled Q-779, and its recheck recorded Q-780. Roadmap 6.2a1 recorded Q-810 and Q-811 and applied Q-797, Q-802, Q-803 and Q-805; its review settled Q-810 by the lead and accepted Q-811. Roadmap 9.0b1 (bounded refresh, the cut and the lease rules) recorded Q-850 to Q-853 and moved Q-773, Q-774 and Q-776 to 9.0c; its review recorded Q-854, decided by the lead, and moved Q-850 to 9.0f. Roadmap 9.0b2 (published views and the single-flight refresh) recorded and decided Q-900 to Q-903 and recorded Q-904. Integrating the durable fixes with bounded decision history recorded Q-925 and Q-937 and settled Q-921 to Q-924, Q-926, Q-927, Q-938 and Q-940 to Q-945; merging it into the engine recorded Q-928 and settled Q-929, Q-930, Q-946 and Q-947, merging roadmap 9.0b1 into it settled Q-948 and Q-949, and merging roadmap 6.2a1 settled Q-950. The owner's rulings of 2026-11-18 settled Q-925, Q-928, Q-951, Q-960, Q-962 and Q-970. Roadmap 9.0c (latest values from the series directory and the log tail) recorded Q-1000 to Q-1006, decided all but Q-1002 by the lead and left Q-1002 to the owner, settled Q-777, and confirmed Q-774, Q-776 and Q-991 for review and moved Q-773 to 9.0e. Roadmap 9.0d (live reads, first part)
recorded and decided Q-1010 to Q-1016 by the lead, and its review Q-1017. Roadmap 9.0f (sources, the merge and the memory cap) recorded and decided Q-1060 to Q-1065 by the lead and settled Q-850; its fix round recorded and decided Q-1066 to Q-1069 by the lead, and its second fix round Q-1095 and Q-1096. The follow-ups of Q-614's re-check recorded Q-1030, decided by the lead. The labels spec-and-model step of the series labels design (L1) recorded Q-880, Q-881 and Q-884 for the owner with the design's proposals applied, applied the design's settled Q-882 and Q-890 to Q-893, and recorded and decided Q-1020 to Q-1029; its review recorded Q-1040 to Q-1043, the lead deciding Q-1040 to Q-1042. The owner's rulings of 2026-11-21 settled Q-1102. The gates model (in-store conditions, step 1) recorded and decided Q-1140 to Q-1148 by the lead, provisional; its fix round changed Q-1148 (bad and valueless readings are not arrivals) and recorded Q-1149. The owner's rulings of 2026-11-26 settled Q-1200 to Q-1203, recorded by the review of conditions and fan-out.

| Question | Settle by roadmap item | Note |
|---|---|---|
| Q-16 | 5.4 | Live state and staleness are built there; built by roadmap 5.4 as the model backend has them (see the question), not settled: needs the owner's ruling on the proposal and its objections |
| Q-33 | 6.2 | Moved from 4.5, which has no derived file to check, to 5.1, which did the registry file's check and requirement 5, then to 6.2 at the review of 5.1 (indexes at 6.2, trees at 7.1): the rest of the Derived check (requirements 1–4) and invariants 5 and 8 across a lost range. Recovery entries, reset points, rows 7b and 7c and invariants 25–27 are modelled since roadmap 5.1a |
| Q-44 (b) and (c) | 5.1 | Series registry and timing; not settled by roadmap 5.1, which needs the owner's ruling on the proposal and its objections (see the question) |
| Q-52 (b) | 4.9 | Settled on Linux (ext4 and XFS) by the rewrite job of the `fault-trials` workflow (`docs/measurements/4.9-fault-trials.md`); open only for the owner's decision on the macOS and Windows wording |
| Q-83 | 7.2 | Late and corrected data; recorded after the tag, from the Q-60 ruling |
| Q-84 | 8.3 | Restore, with 5.3 (resync) |
| Q-86 (b) | 4.9 | Settled on Linux by the Btrfs job of the `fault-trials` workflow (`docs/measurements/4.9-fault-trials.md`); open only for the owner's decisions on the `chattr +C` sentence and the macOS and Windows wording |
| Q-90 | 8.3 | Restore |
| Q-92 | 6.2 | Revised-key index; recorded after the tag, from the review of the Q-72 ruling |
| Q-93 (b) and (c) | 7.3 | Calendar; (a) settled at gate 3.3 by the owner |
| Q-94 | 6.3b | Compaction order; recorded after the tag, from the review of the Q-71 ruling; with 6.3b, which publishes compaction outputs (6.3a lays out the records only) |
| Q-96 | 5.3 | Catch-up, resync and replay; recorded after the tag |
| Q-111 | 4.9 | Gate; recorded at the review of roadmap 4.3; owner decision pending on the call-limit wording in `api.md` |
| Q-115 (e), (j), (n) | 4.9 | Gate; owner decisions pending: the `space()` seam, the Windows wording, and bounds on the ingest queue's bytes and on blocked seals |
| Q-115 (f), (k) | 5.1 | Series registry (the segment index's reach) and the interface (waits and statistics); deferred by gate 4.9; both implemented by roadmap 5.1 (see the question) |
| Q-116 (a) | 6.3b | Deep fetch or manifest rebuild for a corrupt record; deferred by gate 4.9; roadmap 6.3a builds both in `kvik-files` (see the question), the start's call into them is 6.3b |
| Q-116 (c) | 5.1 | The interface reads loss history; deferred by gate 4.9; implemented by roadmap 5.1 (see the question) |
| Q-116 (k), (n) | 4.9 | Gate; (k) owner decision pending on the availability wording; (n) the Linux NVMe run measured a warm cache (0.39–0.74 s); a cold start is still to measure |
| Q-118 (c) | 8.3 | Restore: a reader that sees a new generation; deferred by gate 4.9 |
| Q-118 (f), (k) | 6.3b | Compaction and retirement: which epoch a refresh takes, and what removal honours; deferred by gate 4.9; with 6.3b, which retires files (6.3a gives `Catalogue::at_epoch` and the settled and provisional holds) |
| Q-123 (a), (c)–(g) | 5.6 | Gate; recorded by roadmap 5.1; (b) settled at its review |
| Q-140 | 5.6 | Gate, with the ruling on Q-16; recorded by roadmap 5.4 |
| Q-141 | 5.6 | Gate; recorded by roadmap 5.4 |
| Q-142 | 5.6 | Gate; recorded by roadmap 5.4 |
| Q-161 | 8.3 | Restore's own claim marker; recorded by roadmap 8.2; the marker's layout, with the nonce, is in `format.md` A.17 (roadmap 8.1a) |
| Q-163 | 8.3 | Reading the claims on a lagging bucket; recorded by roadmap 8.2 |
| Q-164 | 8.1, 8.3 | Interface points deferred to 8.2: (a) and (b) with 8.1, (c) with 8.3; recorded by roadmap 8.2 |
| Q-173 | 6.3b | The operator call `rebuild_manifest`; recorded by roadmap 6.3a; its signature and finality come with the call in `api.md`, which 6.3b adds |
| Q-175 | 6.3b | An intermediate output no record and no local file describes; recorded at the review of roadmap 6.3a; needs two corrupt merges, so it arises only once 6.3b publishes compaction outputs |
| Q-191 (b) | 8.3 | Parquet form of orphaned and set-aside facts; settled by the owner, 2026-11-16 (the export schema, one file per call); built with the export call at 8.3 |
| Q-192 (b) | 6.4b | The scale in force on `SeriesInfo` or the export call; settled by the owner, 2026-11-16; comes with decimal series on the durable backend, `ScaleAboveExport` until then |
| Q-196 (c) | 8.3 | Store-level metadata beyond the store id (tzdata version, durable position, reset stamp) with the export call in the store's interface; the saved point of a continued export after 5.3; settled by the owner, 2026-11-16; the store id is in every file already, from `Reader::store_id` |
| Q-197 (calendar lengths) | 7.3 | Interval ends of calendar-length series from the interval zone, tzdata version in the metadata; settled by the owner, 2026-11-16; built with calendar-length interval series |
| Q-198 | 8.1b, 8.3 | Backup and restore interfaces; recorded at the interfaces commit; (a) to (d) decided and built by 8.1b to 8.3d; (e) and (f) settled by `format.md` A.17 at 8.1a; the remaining mismatches, Q-213 and Q-214, settled by 8.3b |
| Q-202 | 5.5 | Entries acknowledged under the decision lock; recorded by the review of the durable backend |
| Q-203 | 5.6 | Gate, with Q-123 (d); recorded by the review of the durable backend |
| Q-210 | 8.1c | A sink's report of a missing checksum; recorded by roadmap 8.1a |
| Q-211 | 6.3d | An uploaded epoch written again after a damaged tail; recorded by roadmap 8.1a |
| Q-212 | 8.1c | A key taken by other content; recorded by roadmap 8.1a |
| Q-220 | 6.3c | Plaintext hash at seal; recorded and decided by roadmap 6.3b; settled when the sealed-segment record is appended at seal |
| Q-221 | 8.1 | Failures while hashing and unlisted segments; recorded and decided by roadmap 6.3b, health for (c) by the owner's ruling; repair of a damaged unlisted segment with backup |
| Q-270 | 8.1 | Envelope damage mapped onto the existing error variants; recorded by roadmap 8.1b; a `Damaged` variant is the lead's call |
| Q-280 | 8.1 | Rebuilt manifest and the upload queue; decided by roadmap 8.1c, accepted by the owner |
| Q-281 | 8.1 | Copy-verified record layout; decided by roadmap 8.1c, field 1 a whole file description by the owner's ruling |
| Q-282 | 8.1 | Upload queue and record cursor; decided by roadmap 8.1c, accepted by the owner |
| Q-283 | 8.1 | Attach probe; decided by roadmap 8.1c under the owner's Q-162 ruling, accepted by the owner |
| Q-284 | 8.1 | Upload failures; decided by roadmap 8.1c, (c) settled by the owner's Q-82 rule |
| Q-285 | 6.2 | Local layout of kinds other than log segments; the upload driver fails on them until then |
| Q-291 | 8.3d | Recovery-entry loss reports at a restore; recorded by roadmap 8.3a |
| Q-292 | 8.3 | Restore record size; recorded by roadmap 8.3a |
| Q-294 | 8.3b | Model restore against the plan; recorded by roadmap 8.3a |
| Q-301 | 8.3 | The files a resumed restore fetches; recorded by roadmap 8.3b; exact for a log-only restore |
| Q-302 | 8.3 | A restore's lost range and files other than log segments; recorded by roadmap 8.3b |
| Q-303 | 8.3 | The local restore record file (kind 12); recorded by roadmap 8.3b |
| Q-310 | 4.9 | Gate; recorded by the Linux NVMe run of roadmap 4.8; settled by the engineering fix to the sync schedule, whose schedule Q-980 supersedes; the next NVMe run before gate 4.9 confirms the budget under Q-980 |
| Q-341 | 8.1d | The attach probe at `<sid>/probe`, also after a full restore; recorded when 8.3b was stacked on 8.1c |
| Q-342 | 8.1d | `backed_up_through` in a restored generation; recorded when 8.3b was stacked on 8.1c |
| Q-321 | 8.1 | Tiers and eviction: the log start needs another anchor than position 1 before the last sealed segment leaves the log directory, or a start reports a false `data_lost` from 1; settled otherwise after the review of Q-320 |
| Q-322 | 5.6 | Gate; recorded from the kill loop's sweep at the review of Q-320; the kill loop fails on it rarely until settled |
| Q-243 | 8.5c | Self-repair wired; recorded by roadmap 8.5a |
| Q-249 | 8.5d | The fault matrix with a kill loop; recorded by roadmap 8.5a |
| Q-250 | 5.5b | Unsynced faster-level batches lost to a remount; recorded by roadmap 5.5a; owner decision pending |
| Q-251 | 5.5b | `possible_loss` for the faster level; recorded by roadmap 5.5a; decided provisionally, the clean-shutdown refinement open; owner confirmation pending |
| Q-252 | 5.5c | A per-call sync and the `max_unsynced` default; recorded by roadmap 5.5a; owner decision pending |
| Q-253 | 5.5b | The commit-block flag in reserved bytes; recorded and decided by roadmap 5.5a, the block type left to 5.5b; owner confirmation pending |
| Q-254 | 5.5b | Faster-level batches evicted after a failed write-back; recorded at the review of 5.5a; owner decision pending |
| Q-255 | 5.5b | Cutting calls of two levels into batches; recorded at the review of 5.5a; owner confirmation pending |
| Q-256 | 5.5b | `set_durability` at run time; recorded at the review of 5.5a; owner confirmation pending |
| Q-257 | 5.5b | Out-of-order loss as `data_lost`; recorded at the review of 5.5a; owner confirmation pending |
| Q-258 | 5.5b | Default-level batches truncated after flagged ones on a remount; recorded at the review of 5.5a; owner confirmation pending |
| Q-259 | 5.5b | Visibility before durability; recorded at the review of 5.5a; owner confirmation pending |
| Q-261 | 8.3 | Decided by roadmap 8.3c; for the owner's acceptance |
| Q-263 | 8.3 | Decided by roadmap 8.3c; the corruption-report part open |
| Q-265 | 8.1d | Decided by roadmap 8.3c (the driver's side); the store's read-only state and fatal health are 8.1d's |
| Q-267 | 6.3c | `recorder` with 6.3c's `FileRecorder`, `sealed` with 6.3b's `seal_segment_described` |
| Q-268 | 8.3 | Decided by roadmap 8.3c; for the owner's acceptance |
| Q-601 | 8.5b | Strict decoding of the corruption-seen values; recorded at the re-check of roadmap 8.5a |
| Q-610 | 8.3 | The chain of epochs in uploaded manifest records; decided by roadmap 8.3g; what it cannot show, the last-uploaded marker shows (Q-614) |
| Q-611 | 8.3 | A broken chain of uploaded records; decided by roadmap 8.3g; owner: one record object lost makes every restore and drill of the store fail for good, with no way out once the store is gone |
| Q-613 | 8.3d | Files of the generation read that no record names; reported by roadmap 8.3g in `Prepared::unnamed`; surfaced by `Store::restore` at 8.3d |
| Q-615 | 8.3d | The source records read after the claim, so each retry takes a generation; recorded by the review of 8.3g |
| Q-670 (c) | 5.6 | Gate, owner; the pattern grammar and `RouteError` as public interface; recorded by roadmap 5.6a, (a) and (b) settled by the owner's ruling |
| Q-680 | 5.5b | `Outcome` level and `LogStats.acknowledged`; recorded at the review of 5.5a; owner confirmation pending |
| Q-681 | 5.5b | What `max_unsynced` bounds; recorded at the review of 5.5a; owner decision only for a hard bound |
| Q-704 | 6.3b | The delivery reader's skipped refresh once the manifest changes during a run; recorded by the store-path quick wins |
| Q-710 | 4.9 | Gate; the default `log.sync_interval_floor`: 2 ms by the owner's ruling, applied after the commit latency rows (indicative, a shared virtual machine); to be confirmed by the run on the Linux NVMe target before gate 4.9; its bound kept by Q-980 |
| Q-711 | 4.9 | Gate; new fields in the log crate's `WriterConfig` and `LogStats`; decided by the lead with the first change, awaiting the owner's acceptance |
| Q-712 | 4.9 | Gate; the adaptive interval's constants; decided by the lead (8, an eighth, rise at once, batch syncs only); the sweep kept 8; awaiting the owner's acceptance; the budget's rate under Q-980 |
| Q-751 | 5.6 | The recent windows' memory: constants per series and `limits.window_memory` store-wide, decided by the lead at the review of roadmap 11.0 |
| Q-752 | 7.1b | A start bounded in memory but not in time until tree checkpoints carry the decision state; recorded by roadmap 11.0 |
| Q-753 | 5.6 | The deliverer's recent window and its reads of the log; decided by the lead with roadmap 11.0 |
| Q-773 | 9.0e | The series directory's cadence against a reader's allowance; decided at 9.0a's review (recent seals, reader retry); 9.0c's reader continues any directory and builds no allowance (Q-1003), so the allowance and its retry are confirmed with the one-shot tool |
| Q-774 | 9.0c | The derived files' layout, names and generations; decided by the lead with roadmap 9.0a; read as laid out by 9.0c's reader (Q-1003); for review |
| Q-775 | 6.3b | The log directory checked by trailers until the manifest lists sealed segments; recorded by roadmap 9.0a |
| Q-776 | 9.0c | The writer's subset of the saved-point rule for a series directory across a reset; recorded by roadmap 9.0a; 9.0c's reader continues only a file of the current stamp, and checks the lost positions (Q-1003); for review |
| Q-778 | 9.0g | The status file's health, interval and reserved pressure fields; recorded by roadmap 9.0a |
| Q-779 | 6.3e | Group numbers once compacted log segments are removed locally; settled at 9.0a's review ("not local" bit); 6.3e writes the bit |
| Q-780 | 8.3 | The log directory across a generation change: restored ends in the root; recorded by roadmap 9.0a |
| Q-754 | 9.0f | The lookup's batch maps across a reset stamp or generation, for reader processes; recorded by roadmap 11.0 |
| Q-755 | 6.3b | Damage found during a run recorded durably as a loss; health held degraded meanwhile by the lead's decision at the re-check of roadmap 11.0 |
| Q-980 | 4.9 | Decided by the lead (the sync budget); confirmed by the next NVMe run |
| Q-981 | 4.9 | Decided by the flush-pacing change (the floor a store key; the default stays the owner's 2 ms); for the lead's acceptance |
| Q-982 | 4.9 | Decided by the flush-pacing change; for the lead's acceptance |
| Q-983 | 4.9 | OWNER. Gate; the `8 × d` term against closed-loop latency above the budget, and the burst in the stated bound; (a) for now |
| Q-984 | 4.9 | Decided by the flush-pacing change; for the lead's acceptance |
| Q-985 | 4.9 | Decided by the flush-pacing change (a burst of four); for the lead's acceptance |
| Q-880 | labels engine (L2) | OWNER. The label rules and limits; the design's proposal (a) applied by L1 |
| Q-881 | label selection (L3) | OWNER. Reserved keys refused since L1; the fixed segment labels are L3's |
| Q-884 | labels engine (L2) | OWNER. Definition labels initial only; the design's proposal (a) in the spec since L1 |
| Q-1020 | labels engine (L2) | Decided by the lead with the labels spec-and-model step (L1); L2 compares both backends with `LabelLog` |
| Q-1021 | labels engine (L2) | Decided by the lead with the labels spec-and-model step (L1); for review |
| Q-1022 | labels engine (L2) | Decided by the lead with the labels spec-and-model step (L1); for review |
| Q-1023 | labels engine (L2) | Decided by the lead with the labels spec-and-model step (L1); for review |
| Q-1024 | labels engine (L2) | Decided by the lead with the labels spec-and-model step (L1); for review |
| Q-1025 | labels engine (L2) | Decided by the lead with the labels spec-and-model step (L1); for review |
| Q-1026 | labels engine (L2) | Decided by the lead with the labels spec-and-model step (L1); for review |
| Q-1027 | labels engine (L2) | Decided by the lead with the labels spec-and-model step (L1); for review |
| Q-1028 | label selection (L3) | Decided by the lead with the labels spec-and-model step (L1); for review |
| Q-1029 | labels engine (L2) | Decided by the lead with the labels spec-and-model step (L1); for review |
| Q-1040 | labels engine (L2) | Decided by the lead at L1's review; L2 pins it on both backends |
| Q-1041 | labels engine (L2) | Decided by the lead at L1's review |
| Q-1042 | labels engine (L2) | Decided by the lead at L1's review (`format.md` A.19) |
| Q-1043 | labels engine (L2) | Open; proposal: a source of F, as the registry file's `through` |
| Q-1031 | 5.3 | Decided by the lead; for review |
| Q-1032 | 5.5 | Decided by the lead; the durable log checked by the review's probe; for review |
| Q-1033 | 5.3 | Decided by the lead (no layout change); for review |
| Q-1034 | 6.3b | Recorded only; with Q-755 and Q-951 |
| Q-760 | 5.6 | Gate; recorded when the owner's ruling on Q-195 (a) was applied |
| Q-761 | 6.4b | Partitioned export layout; recorded when the owner's ruling on Q-196 was applied |
| Q-832 | 7.1b | A start that skips old segments; kept with Q-752 by the lead; recorded by the first profiling sweep |
| Q-834 | 6.3b | The seal's file checksum without the re-read; recorded only, by the lead's decision |
| Q-835 | 5.6 | The deliverer's read of the log past `open`; deferred by the lead |
| Q-836 | 6.3b | Facts a damaged recovery entry voided read again as live; with Q-755; recorded at the re-check of the first profiling sweep |
| Q-871 | 8.3 | Decided with the Q-630 work and widened at its review and re-check: opening a store syncs its directory; creation and a restore sync its parent |
| Q-937 | 5.1 | A suspected defect of the durable fixes' loss range, by reading; to be reproduced and ruled as its own item |
| Q-851 | 9.0b2 | The second read of the mark kept after a pass that took something; decided by roadmap 9.0b1, awaiting review |
| Q-852 | 6.3b | A caught-up reader opens segments by name and checks the manifest by path; decided by roadmap 9.0b1, awaiting review |
| Q-853 | 9.0b2 | `cut_max_age` 30 minutes by default, at most an hour, zero refused, aged from the last check that found its epoch current, on the environment's clock; decided by roadmap 9.0b1 and its review |
| Q-854 | 9.0b2 | The lease records the highest epoch read; decided by the lead at 9.0b1's review, to be confirmed at the re-check |
| Q-811 | 6.2a2 | Format details of `format-compacted.md`, accepted at the review of roadmap 6.2a1; open for the two obligations it names for 6.2a2 (memory of the writer and the canonical check; the stall on decreasing commit times) |
| Q-872 | 4.9 | Decided with the review of the Q-650 ruling: a rebuild's epochs go above what an open span may hold; for the owner's acceptance |
| Q-990 | 4.9 | Decided by the lead: a batch kept after damage has its bodies decoded for its content outside the counted checks; for review |
| Q-991 | 9.0c | Decided by the lead: counter 14 counts failed writes and failed start steps, not a series directory the hook cannot give, which 9.0c adds a case of (a registry file that could not be written, Q-1001); for review |
| Q-961 | 8.3d | The marker errors, switches and reports at `Store::restore`, and pacing the retries; recorded when the owner's ruling on Q-614 was applied |
| Q-1001 | 9.0c | Decided by the lead: the registry file written before a series directory that covers an entry it lacks; for review |
| Q-1002 | 6.3b | OWNER. No series directory in a run over a loss no lost range records, so that readers keep reporting it; (a) as built |
| Q-1000 | 9.0h | Decided by the lead with 9.0c: the fact index built only for calls that read facts, at the main reader's cut; since 9.0h only finding damage and a broken state build it (Q-1060, Q-1130) |
| Q-1003 | 9.0c | Decided by the lead: which series directory and registry file a reader continues; for review |
| Q-1004 | 9.0c | Decided by the lead: what the reader finds by lookup at the cut, and a named fact not found reported unavailable; for review |
| Q-1005 | 9.0h | Decided by the lead with 9.0c: `Snapshot` answers latest values, series and losses; facts and windows join it once 9.0h folds windows from the sources (Q-1060) |
| Q-1006 | 9.0h | Decided by the lead with 9.0c: a broken state answers from the fact index until reopened; its facts and `at` read the sources since 9.0f (Q-1060) |
| Q-1007 | 9.0d | Decided by the lead at 9.0c's review: no file read while the writer has made nothing durable since; the in-memory feed from 9.0d's hub; figures per class with 9.0e |
| Q-1010 | 9.0d | Decided by the lead: a batch only with events, `Opened` at the next batch's cut, `Tail` from its cut only, one `durable` position while there is one log; for review |
| Q-1011 | 9.0d | Decided by the lead: `Reset` and `Gap` for every reset and every loss found, a `Latest` opened again after either; for review |
| Q-1012 | 9.0d | Decided by the lead: retirement leaves every selection; the canonical order of the entries, matches by id; for review |
| Q-1013 | 9.0d | Decided by the lead: the shared tail bounded by the largest backlog, a lag for the whole session, idle sessions closed; for review |
| Q-1014 | 9.0j | Decided by the lead: a cut inside a refresh's read judges `Latest` by search; a `Joined`'s info from the refresh's end; revisited with 9.0j's shapes |
| Q-1015 | 9.0d | Decided by the lead: the writer's progress wakes, the poll and batch intervals on the environment's clock, due times per member; for review |
| Q-1016 | 8.1g | Decided by the lead: a session reads only the shared tail; its errors at every `next`; the tier-unavailable error with cold segments |
| Q-1017 | 9.0e | Decided by the lead: a reader process's poll reads the mark alone within a second of a full refresh; one shared machine's figures, measured again by 9.0e |
| Q-1050 | 9.0f | Decided by the lead: the tool's store option, `--for`, and `tail --since`/`--once` refused until 9.0f; for review |
| Q-1051 | 9.0g | Decided by the lead: `health` and `info` read files through `kvik-log`'s decoders, writing only the lease; slots and compaction join with 9.0g and 6.2a2 |
| Q-1052 | 9.2 | Decided by the lead: a year's count of seals, medians and worst over three runs, the class recorded honestly; both classes measured by 9.2 |
| Q-1053 | 9.0e | Decided by the lead: a defect, fixed; a reader's start lists no directory; S3 measured again |
| Q-1054 | 9.2 | Decided by the lead: the mark read through a held handle; the rest is this machine's wake-up cost; the desktop-class figure waits for 9.2 |
| Q-1055 | 9.0j | Decided by the lead: a known miss; the hub feed for the writer's reader lands with 9.0j (Q-1007) |
| Q-1056 | 9.0f | Decided by the lead: `kvik series` counts from `fact_count` now; `tail --since`/`--once` follow 9.0f |
| Q-1060 | 9.0h | Decided by the lead with 9.0f: `facts` and `at` from the log tail through the 11.0 lookup at the cut; windows too since 9.0h (Q-1130); for review |
| Q-1061 | 9.0g | Decided by the lead with 9.0f: an iteration ends with `CutExpired` when its cut ends, its reset stamp or generation changes; for review |
| Q-1062 | 9.0g | Decided by the lead with 9.0f: several series in one pass in event-time order, then series; public with the `Query` read; for review |
| Q-1063 | 9.0g | Decided by the lead with 9.0f: the accountant's shares, grants held by passes and their rows until taken (Q-1067), the flat block reserve; the envelope's admission builds on it |
| Q-1064 | 6.2b | Decided by the lead with 9.0f: as of a time newest segment first through trailers and batch commit times, voided positions passed over; compacted files' tables with 6.2b. Open: a time in two generations, testable once restore exists (settle by 6.2b or the restore step, whichever comes first) |
| Q-1065 | 6.2b | Decided by the lead with 9.0f: `at` searches back in doubling spans, one probe ending it before a series' first fact; the chunk index gives it one block with 6.2b |
| Q-1066 | 6.3b | Decided by the lead with 9.0f's fix round under Q-951: a reader that took facts from a removed segment fails every call; a read ends when a segment it listed is gone; for review |
| Q-1067 | 9.0g | Decided by the lead with 9.0f's fix round: rows waiting for delivery hold their grant; an admission empties shelves rather than wait on an iteration; for review |
| Q-1068 | 9.0g | Decided by the lead with 9.0f's fix round: one key splits by position, the current view folded; for review |
| Q-1069 | 9.0g | Decided by the lead with 9.0f's fix round: only wanted rows decoded, counted as kept; measured peak and fixed allowance; for review |
| Q-1090 | 9.0f | Open, for the lead: a continuing reader reads the batch at its directory's `through` for a loss just above it, so readers agree on the commit time before |
| Q-1108 | 8.5c | Open, for the owner: records naming a loss folded into a lower identity carry over (as built, recommended) or lapse |
| Q-1109 | 9.0d | Settled by the owner, 22 November: provisional delivery is acceptable, opt-in per stream and labelled, withdrawn when lost (as built) |
| Q-1110 | 9.0d | Ruled by the owner, 23 November, as built: `Closed` after a shutdown or drop; a call larger than `max_behind` delivered whole; a watermark only over a provisional call; a bounded ring hold |
| Q-1113 | 9.0e | Withdrawn: a number given at a merge to the fast path's Q-1110, which keeps its number; not reused |
| Q-1114 | 9.0e | Open, for the lead: provisional default as built (the scan starts again at every open); carry its progress across opens if short-lived processes matter |
| Q-1115 | 9.0e | Settled by the owner, 23 November: the damage scan below the seed runs after the open returns (as built); `ScanOrder::BeforeOpen` stays the way back |
| Q-1116 | 9.0e | Decided by the lead with the seeded start: the start counts in A.18, the rules for giving way to the whole log; for review |
| Q-1117 | 9.0e | Decided by the lead, provisional: derived files stay unsynced, so a power loss may cost a whole read; sync the series directory if that matters |
| Q-1118 | 9.0d | Decided by the lead, provisional: sessions passed over rounds they select nothing of; those rounds no longer count to their backlog; a waiting session is polled; losses found elsewhere within a second; a read reaches anything from its `open` until answered; the lead accepted fewer `Lagged`, the 1 s loss reach and an error gone by a session's turn not being returned |
| Q-1119 | 5.2 | Decided by the lead, provisional: an emit leaves its batch to the `kvik-deliver` thread, taken as of its acknowledgement; wakes per consumer; Q-706's counts may lag with consumers too; the lead accepted that the backends differ on when a consumer is woken after `emit` (`api.md` Consuming) |
| Q-1130 | 6.2 | Decided by the lead with 9.0h: windows, buckets and calendar results from the sources, chunked fold, `Bucket` sum, increase, resets and regressions; the first read's segment index reads wait on per-segment spans; for review |
| Q-1131 | 6.2 | Decided by the lead with 9.0h: several series' windows, buckets and facts from one read; results held per series until the call ends; for review |
| Q-1132 | 6.2 | Decided by the lead with 9.0h: passes sized from the lookups' counts, rows sorted by keys; half a grant as the target; for review |
| Q-1133 | 7.1 | OWNER. A reader call on more than one thread; (a) as built: every call on the caller's thread |
| Q-1150 | 6.2 | Decided by the lead: series batches at seal (A.8a), one-series lookups from their own batches, read without the lookups' lock; the halves, gap, pair limit and reference size provisional; for review |
| Q-1151 | 6.2 | For review: concurrent many-series reads; (a) as built until callers' reads arrive together |

| Q-1140 | conditions step 4 | Decided by the lead, provisional: the model defines gates; losses during a replay forget ids above `forget_above`, specified with exact resume |
| Q-1141 | conditions step 3 | Decided by the lead, provisional: `Unknown` with no value; for review with the public API |
| Q-1142 | conditions step 3 | Decided by the lead, provisional: ids name the series; for review with the public API |
| Q-1143 | conditions step 6 | Decided by the lead, provisional: refused and counted, joining when there is room |
| Q-1144 | conditions step 2 | Decided by the lead, provisional: timers first; the engine is compared with the model on it |
| Q-1145 | cond2 step 1 | Settled by the owner: (b), seeded from history by the rule of Q-1390 |
| Q-1146 | conditions step 2 | Decided by the lead, provisional: `Unknown` and `Stale` break holds, crossings kept |
| Q-1147 | conditions step 5 | Decided by the lead, provisional: the §6 window at the instant, sampled on facts and ticks |
| Q-1148 | conditions step 2 | Decided by the lead, provisional: arrivals as built in the model; with Q-16 |
| Q-1149 | conditions step 3 | Decided by the lead, provisional: durations at most ten years, refused beyond |
| Q-1390 | cond2 step 1 | Decided by the lead, provisional: look-back `min(2L, 24 h)`, the series' state at its start (Q-1560) |
| Q-1391 | cond2 step 1 | Decided by the lead, provisional: one gate cursor and a checkpoint; the bound from the checkpoint |
| Q-1393 | cond2 step 2 | Open, for the owner: the commit clock as the provisional default |
| Q-1560 | cond2 step 4 | Decided by the lead, provisional: the series' true live state; the read costed in step 4 (Q-1880), else the fallback |
| Q-1561 | cond2 step 2 | Decided by the lead, provisional: facts before the look-back arrive at its start (kept) |
| Q-1880 | cond2 step 5 | Decided by the lead, provisional: the four rules as the contract; a log scan per read for now (Q-2001), the bounded read with replay |
| Q-1881 | cond2 step 5 | Decided by the lead, provisional: reads fallible and a failure fatal to the core; a short read counted by the caller |
| Q-1882 | cond2 step 4 | Decided by the lead, provisional: either; the real read takes the cheaper |
| Q-2000 | cond2 step 5 | Decided by the lead, provisional: gate cursor in memory; every start reseeds |
| Q-2001 | cond2 step 5 | Decided at review, provisional: one indexed read per series per core call; the bounded read with replay, before step 5 merges |
| Q-2002 | cond2 step 6 | Decided at review, provisional: buffered while behind; a quiet catch-up ends with a resync |
| Q-2003 | cond2 step 5 | Decided by the lead, provisional: states as handed out; the cursor at the resync's position |

Not a question: the figures behind §3 Sync primitive. The 2.2 spike measured sync costs on macOS only; roadmap 4.8
measured the engine's commit path on macOS and added a harness and a workflow that run it on Linux and Windows cloud
runners. The cloud-runner figures are recorded there and are not durability-representative.
The figures on the Linux NVMe target are the owner's run, recorded there too
(`docs/measurements/4.8-commit-latency.md`); the budget point they raise is Q-310.

## For the format and interface documents
The specification wins where the documents differ. These follow from it, for the checks of `docs/format.md` and
`docs/api.md`. Gate 3.3 resolved or deferred each of them; `docs/format-check.md` and `docs/api-check.md` say where.
- **F-1.** `format.md` A.5: the fact row has one `quality` column, and a `no_value` row is forced to quality bad.
  Duplicate identity (§2.4) compares the adapter-supplied quality, which §2.2 keeps with the fact. Without it, the
  duplicate check and the revised-key index cannot be rebuilt from the log (invariants 8, 15).
- **F-2.** `api.md`: back-pressure is listed as a refusal; §3 says ingest waits. `try_emit` returning `Busy` fits.
- **F-3.** `api.md`: windowed results need a place for the rate flag (§6).
- **F-4.** `format.md` A.5 and the engine-assigned quality: the engine only lowers a quality (§2.2). The quality
  column must hold the adapter-supplied quality, or §2.4 identity is lost for rows without a value and `future` rows;
  the engine quality is then derived from it: bad for a row without a value, else the worse of the supplied quality
  and suspect for a `future` fact. The forced "a `no_value` row has quality bad" applies to the engine quality only.
- **F-5.** `format.md` recovery (§3): scan an unsealed segment to its end, not only to the first invalid block, which
  needs a way to find the next batch after a corrupt region (a batch sync marker, or scanning at 8-byte steps); a
  sealed segment with a lost range counts its surviving entries, and "consecutive" positions allow a recorded lost
  range; the manifest needs a "lost range" record (bytes and positions). A missing, unreadable or half-updated mark
  is a different boot, and recovery after a reboot writes the mark; A.10's "always truncates" on a platform without
  a boot identifier yields to that rule when the mark is missing. Part B, where the lost-range record would go, is
  not yet normative. A.11 needs golden files with corruption inside a segment. A.2 ("the first failure ends the
  readable part") and A.10 (sealed segments read whole) must allow kept batches after a lost range, and A.4's "a
  batch's first sequence follows the previous batch's last" needs a lost-range exception. A lease's boot id has no
  value on a platform without a boot identifier. The lease file needs a format and a
  file kind in A.1: epoch, boot id and refresh counter (§7).
- **F-6.** `api.md`: a fault during `emit` leaves the outcome unknown (§3 Write or sync failure); health reports the
  size of system clock jumps (§6) and `data_lost` per lost range (§3); readers need an explicit lease refresh, with
  its counter, and `Reader::open` a source of boot identity (§7); `WindowResult.value` must allow empty (rate,
  `min`, `max`, §6).
- **F-7.** `format.md`: a segment's first sequence is at least 1, and a last sequence of 0 means empty (§2.2). Part B
  needs a "restore" manifest record (generation, checkpoint, restored end, the restored ends carried from the record
  it read, each with its generation, the lost ranges, acknowledgements and recovery-entry loss reports of the
  generations it restores from (since Q-34 (c)), and every file it restores from, set-aside segments included, each with hash,
  tier and position range), uploaded before any other file of its generation, and manifest records keyed by
  generation; the deep tier holds generation claim markers; a tree checkpoint bundles the registry file, each series'
  latest recorded `t` and lateness reference (F-17), the revised-key index and the version index (§8 Continuous
  backup, Generations).
- **F-8.** `api.md`: the backup sink needs a create-if-absent write for generation claims, and a list that sees a
  completed put; restore needs a read-only drill mode and refuses a directory that holds files and no restore record;
  a consumer can register with its last confirmed position after a restore, so the application must persist its
  confirmed positions; health names a writer superseded by a higher generation, and a restore that finds a higher
  claim after uploading its record; re-ingest needs an entry point; a store's identity across drills (a restore keeps
  the store id, §8 Generations).
- **F-9.** `format.md`: a fact row in a versioned series carries `changed_at` (a u64 column with an id, present per
  series), also in compacted chunks; A's rule that times are i64 needs an exception for a `changed_at` of kind `time`;
  a series definition carries the `versioned` flag and `changed_at kind`; the revised-key index carries `changed_at`;
  A.1 needs file kinds for the version index and the revised-key index; exports carry `changed_at` (§2.1–§2.4, §7).
- **F-10.** `api.md`: `changed_at` belongs on `Reading`; `SeriesSpec` gains `versioned` and `changed_at kind`, and a
  change of either is an error like a change of unit; refusals gain `changed_at` missing, `changed_at` unexpected and
  `revise` on a versioned series; fact deliveries and history results carry `changed_at`; deliveries gain the flag
  `outranked`; `Ack` and health gain an outranked count (§2.2, §2.4, §4, §11).
- **F-11.** `format.md` A.10: the mark file is synced on a timer (`mark_sync_interval`), at once after a batch holding
  an entry that is not a fact, at clean shutdown and by recovery; recovery reads the highest position named by cursor
  files, manifest records and derived files before it writes the recovery entry. `api.md`: the `mark_sync_interval`
  configuration key and `possible_loss` among the losses with its counts; acknowledgement of a batch of facts does not
  wait for the mark sync, while a batch holding any other entry does, and the writer forces that mark sync at once
  (§3 Commit); test seams can inject a failed mark sync (§3). A.6, A.10, A.11 and Part B now carry the
  recovery entry, the mark's highest series id, and the lost-range record's identity and cause; since Q-34 (c)
  `possible_loss` never holds health degraded.
- **F-12.** `format.md`: the parameter-change entry carries the retention class and period, taking effect at its position,
  not by event time. `api.md`: `SeriesSpec` gains the retention class and period, which do not count towards "same
  specification" and may change without a new series; a call to change them; the default period as a configuration key;
  statistics for bytes moved and fetches (§2.1, §8).
- **F-13.** `format.md`: a series definition carries its timing (observed, scheduled or forecast). `api.md`: `SeriesSpec`
  gains timing, fixed for life; window bounds may be relative to `now` with either sign, and calendar windows relative
  to the current one; a fact delivery from a scheduled or forecast series is a notice, not the value in force; live
  state of a forecast series is none; a stale-for condition on a forecast series, or any condition over one without a
  windowed operator, is a compile error; timing joins the list of fields whose change is an error; a forecast series
  that is not versioned, or whose `changed_at kind` is not `time`, is refused; `Window::rolling` needs offsets from
  `now`; exports label issue time and valid time. `format.md`'s series definition entry needs the timing field (and the
  versioned flag and `changed_at kind`, F-9), outside the fields a parameter change may carry (§2.1, §4, §6).
- **F-14.** `format.md`: a series definition carries the kind `interval` with its length and phase. `api.md`: `SeriesSpec`
  gains them; a reading off its phase is refused; avg, integral, increase and time-in-state on an interval series are
  refused, also when a name pattern later matches an interval series, and so is a calendar integral; window results
  and buckets gain the flag `apportioned`; refusals gain an off-phase reason, counted per series; `live()` carries the
  interval end; exports carry d or the interval end. `format.md`: the definition entry gains interval length and phase
  (i64 nanoseconds), which a parameter change may not carry; record on change is refused for interval series; A.8's
  `min_t` and `max_t` must allow for a reading's reach of d, or of one calendar interval (F-17) (§2.1, §6).
- **F-15.** `format.md`: a log segment may follow, or be a copy that omits, a lost range (§8 Corrupt block); the
  lost-range record carries the commit times of the batches on either side (§11 Health).
- **F-16.** `format.md` A.6 has the entry types for the site zone (zone name, effective time) and the tzdata version
  (§5.7, Q-76); the calendar file needs a format recording its tzdata version and, per stored node, the zone of each
  day it covers. `api.md`: `calendar` and `calendar_in` take a day or a month, calendar windows (since local
  midnight, this month, a relative day or month, §6) need a `Window` constructor, and the site zone needs a call that
  records an entry. The engine compares zones by the tzdata link table (`backward`), not by the model's fingerprint
  of a zone's rules; a calendar file records zones by canonical name, and a release that splits a zone (2022g made
  America/Ciudad_Juarez from part of America/Ojinaga) or merges one into a link (2022b made Europe/Amsterdam a link
  to Europe/Brussels) changes which stored nodes match a named-zone query, so the rebuild after a tzdata change
  re-checks the zones of stored nodes as well as their spans.
- **F-17.** From Q-45 and Q-74; `api.md` has the minimum (interval kind and length, `interval`, `displaced`,
  `interval_displaced`, `OffPhase`, `SeriesInfo`). `format.md` A.6, when the interval fields of F-14 are given field
  ids: interval length as a `u8` tag (0 fixed, 1 `day`, 2 `month`), with `i64` d and `i64` phase for tag 0; interval
  zone as a UTF-8 string, as given at definition; day start as a local clock time in `i64` nanoseconds (05:00 is 5 h
  whatever the offset does before it; 0 ≤ day start < 24 h), tag 1 only. None of them may appear in a parameter-change
  entry. No change to fact columns or flags: a reading carries no length, and `displaced` is derived, not a fact flag.
  A.8's `min_t` and `max_t` must not depend on tzdata: a sealed segment bounds a calendar-length reading's reach by a
  fixed upper bound on its length, 50 h for `day` and 33 days for `month` (a date-line change can repeat a date:
  America/Sitka, 1867, has a local day of about 48 h). The tree checkpoint (layout not yet in `format.md`, F-7)
  carries per series the lateness reference (`i64`, absent when none) beside the latest recorded `t`. `api.md` still
  needs: a change of interval length, zone or day start is an error like a change of unit; a day start on a fixed or
  `month` length, or a zone the tzdata version in force does not know, is refused at definition; exports and `live()`
  carry the interval end.
- **F-18.** From Q-60 to Q-81. `format.md`: the tzdata entry (A.6, type 7) and the first site-zone entry are in a
  store's first batch (§5.7; since gate 3.3 the store descriptor, `format.md` A.12, holds them), and `api.md` needs how
  a store is created with them; the routing-change entry (A.6,
  type 5) needs its fields, the consumer's `emits` selection among them; the registry file (Part B) holds routing
  changes, site-zone and tzdata entries with their positions, so that every tree checkpoint carries them (§2.1, §4
  Routing, F-7); a revised-key index entry needs one bit, incomplete (§2.4; the index's file kind, F-9); a compacted
  segment's index can be fetched without its chunk data (Part B, §8 Restore step 1). The cursor file is unchanged: a
  condition delivery's sequence is not persisted (§4 Order). `api.md`: the pattern grammar (Q-88); the `Select` value
  for `emits` that selects nothing.

## Settled
| ID | Question | Settled in |
|---|---|---|
| Q-01 | Corruption before backup | §7 (inputs kept until verified), §8 Corrupt block, invariant 10 |
| Q-02 | Catch-up position on overflow | §4 Overflow |
| Q-03 | Low mark and tiering | §3 Disk space |
| Q-04 | Day nodes and edge stretches | §5.7 |
| Q-05 | Rate across a gap | §6 Aggregates |
| Q-06 | Days outside 92–100 quarter-hours | §5.7 |
| Q-07 | Forward clock jumps and staleness | §5.4 |
| Q-08 | Age of a delivery | §4 Replay |
| Q-09 | Quality and future stamping | §2.2, §2.4 |
| Q-10 | Coverage for text series | §5.2 |
| Q-11 | Counters and grace across reboots | §7, §11 |
| Q-12 | Invariant 9 and disaster loss | invariant 9 |
| Q-13 | Negative counter values | §2.5, §5.5 |
| Q-14 | State transition at a window's leading edge | §6 Edge stretches, invariant 17 |
| Q-15 | Rolling-window coverage after a forward clock jump | §6 Windows (event time against `now`), §11 Health (clock jumps) |
| Q-17 | Identity of a reading recorded without a value | §2.4 |
| Q-18 | Rate flagged in windows without a gap | §6 Aggregates (replaces the Q-05 flag condition) |
| Q-19 | A corrupt compacted file whose inputs are still local | §8 Corrupt block, row 11 |
| Q-20 | A corrupt block inside the open segment after a reboot | §3 Recovery, §8 Corrupt block, §11 Health, rows 11 and 11a, invariants 1, 9 and 10 (open part: Q-33, the Derived check) |
| Q-21 | Which clock measures lease expiry | §7 Leases (refresh counter and boot id; grace period on the writer's monotonic clock), row 26 |
| Q-22 | Negative counter values if D1 changes | §2.5 |
| Q-23 | Stored calendar nodes for days off 15-minute boundaries | §5.7 |
| Q-24 | A failed sync followed by a power loss | §3 Write or sync failure, row 12 |
| Q-25 | When recovery writes the mark; a missing or invalid mark | §3 Recovery |
| Q-26 | The first sequence of a generation, and "no position yet" | §2.2, §8 Restore step 1 |
| Q-27 | Where consumer cursors come from after a restore | §8 Continuous backup, §8 Restore step 2, §4 Resync |
| Q-28 | Two restores from one bucket; a restore from an empty bucket | §8 Generations, §8 Restore drill, row 28b |
| Q-29 | The restored end, and what a continuing cursor receives | §8 Restore steps 1–2, §8 Continuous backup, invariants 4 and 9, row 28 (Q-39 (f) settled since) |
| Q-30 | `duplicates_unchecked` during a restore compared a time with a position | §8 Restore step 2; versioned series (§2.1–§2.4, §4, §5.6, §6 Cadence, §7, §11, invariants 4, 15 and 19, rows 1, 2c and 2d), so that a replay never reverts a correction |
| Q-31 | Accepting ingest before the registry is back | §8 Continuous backup, §8 Restore step 1 |
| Q-32 | A crash or reboot during a restore | §8 Restore, row 28a (Q-39 (f) settled since) |
| Q-33 | Registry entries after a lost range, in part: the kept log, orphaned facts and id floors | §2.1 (only the kept log applies; a lost registry entry is issued again through the normal calls, a definition issued again being a new series; orphaned facts kept, outside views, trees, indexes, routing and delivery, counted and exported; id floors; the residual on a disk that loses synced data), §2.1 `id`, §3 Commit (a batch holding an entry that is not a fact is acknowledged only after a mark sync), §3 Durable mark (highest series id assigned), §11 Health, invariants 2 and 4, `format.md` A.6 and A.10, `api.md` `export_orphaned` and `orphaned_facts`, as the owner decided (open part: Q-33, the Derived check, with invariants 5 and 8 across a lost range) |
| Q-34 | What `data_lost` and `possible_loss` mean beyond the report, parts (a) and (b) | §8 Corrupt block (only the damaged region is lost; the valid batches of a sealed segment are copied to a new segment by one manifest record, which ends the error), §11 Health (each report carries what is known of its range; no flag on query results), row 11 (Q-34 (c) since settled; open part: Q-52, rebuilding a corrupt manifest record never uploaded) |
| Q-34 (c) | Whether loss reports survive a restart, and whether they are latched | §11 Health (one report per loss, read from the manifest and the recovery entries at every start, never cleared, each counted once; `data_lost` holds health degraded until acknowledged, until closed since roadmap 8.5a (§8 Self-repair), while `possible_loss` is listed and counted and does not), §8 Acknowledging a loss (an acknowledgement record; the list of losses kept for ever; a range that grows needs a new acknowledgement; carried by a restore record), §3 Order of a start step 4 (a loss keeps one identity, the first position of its range when first reported, extended upward or downward, not repeated; recovery-entry loss reports carried by a restore record), invariant 27, `format.md` Part B, `api.md` `losses` and `acknowledge_losses`, as the owner decided (open part: Q-90, the set-aside report) |
| Q-35 | Loss above a stale mark after a power loss is not reported | §3 Durable mark (timed sync), §3 Recovery rule 3 (`possible_loss`, no reuse of positions), §11 Health, rows 7 and 7a, invariant 10 |
| Q-36 | Calendar nodes for a series without a stored 15-minute level | §5.1 (calendar row and density rule), §5.4, §5.7: no calendar node without a stored 15-minute level, the result being a window over facts; row 20a, §13 (the levels fixed by the definition since Q-60) |
| Q-37 | Which manifest records a restore reads, and how generations begin | §8 Generations (generation 0 counts as claimed; each generation above 0 begins with a self-contained restore record listing the files it restores from and carrying the earlier restored ends, uploaded before any other file of its generation, after which the claims are read again; a restore reads the highest generation below its own that has records, and keeps the store id), §8 Restore step 1 (a position set aside once stays so) and the paragraph after step 4, invariant 9, row 28b |
| Q-38 | Segments beyond the restored end | §8 Restore step 1 (positions set aside, a term distinct from Corrupt block's quarantine: excluded by position everywhere, also in reader processes, with no query error; a set-aside definition keeps its id reserved; the set-aside range reported in health from the record at every start; re-ingested only by the operator, through export and Re-ingest), §2.1 `id`, §11 Health, invariants 4 and 9, row 28, `api.md` Backup and restore (open part: Q-90, whether the report holds health degraded) |
| Q-39 | Failures during a restore, and when it ends, parts (a)–(d) | §8 Restore, Failures and end (any failure before step 1 is complete is fatal and resumed at restart, and later failures are retried with degraded health, except a listed file the bucket has lost or corrupted, part (e); the record precedes every download; `restoring` is derived and ends when every file step 3 brings back is installed, or since (e) replaced or recorded lost), §8 Restore steps 1–4, §8 Continuous backup and Deep (each tree checkpoint carries the revised-key index), §2.4, §11 Health, §13, `api.md` (part (f) settled since) |
| Q-39 (e) | What ends a restore when the bucket has lost or corrupted a listed object | §8 Restore, Failures and end (a listed file that the bucket no longer holds, or has corrupted, in step 1 or later, is treated as Corrupt block with deep as the failed tier: compacted again from the other listed files, the positions no other listed file holds becoming a lost range reported as `data_lost`, and the restore completes; facts of a lost log segment held by other listed files fetched in step 1; a lost tree checkpoint replaced by an earlier one or the whole sealed log; `restoring` ends when every file step 3 brings back is installed, replaced or recorded lost), §8 Corrupt block and No deletion (the exception for a restore), invariants 2, 9 and 10, §11 Health and the set-aside report (open parts: Q-85, a loss in the bucket found outside a restore; Q-33, the Derived check after a definition entry lost this way; Q-82, how a loss in the bucket is told from a transient fault, a damaged download or a wrong key; a content conflict between two listed files still waits for an operator) |
| Q-40 | Ranking facts of versioned series during a restore | §2.4 Version index, §8 Continuous backup and Deep, §8 Restore steps 1–2 (each tree checkpoint carries the version index, and step 1's replay brings it to the restored end before ingest opens), §13 |
| Q-41 | Forecasts: values issued now about future times | §2.1 `timing`, §2.2 flags, §2.3 Live state, §4 Future facts and Timers, §5.4, §6 Windows, Evaluation and Edge stretches (credit limit), invariants 18, 20 and 21, rows 4 and 36–39, §13 (open part: Q-44 (b) and (c)) |
| Q-42 | Readings that cover an interval, not an instant | §2.1 `kind` `interval`, `interval length` and `phase`; §2.2; §5.2; §5.4; §6 Aggregates (`apportioned`); invariants 18 and 22; rows 40–42 (calendar lengths: Q-45) |
| Q-43 | Positions delivered but not confirmed, after a truncated tail | §4 At-least-once (a `resync` carrying `forget_above` = X: the consumer drops every position above X from its de-duplication memory and confirms only when that is durable), §4 Resync (reset points: a recovery entry and a lost range not described by one, each applying to a consumer whose position is below its id), §3 Recovery rule 3 (a recovery entry at F, positions above K assigned again), row 7a; no margin of skipped positions (as refined by Q-47 and Q-48; open part: Q-46, positions kept by readers) |
| Q-44 | Gaps left by scheduled and forecast series, part (a): a silent feed | §2.1 `timing` (a silent forecast source is reported by stale-for on the observed series its adapter emits on each successful fetch), row 37b (open parts: Q-44 (b) and (c)) |
| Q-44 (d) | Forecast accuracy | Not served by the engine in v1: history keeps every issue with its `changed_at`, so a reader computes "known as of issue X" from `View::History` and compares it with the outcome series; `api.md` Not in v1 (a view as of an issue time, forecast-accuracy operators) (open parts: Q-44 (b) and (c)) |
| Q-45 | Intervals of varying length | §2.1 `interval length`, `interval zone` and `day start` (a calendar length, `day` with a day start or `month`, in a zone fixed for the series, its interval starts taken under the tzdata version in force), §2.2, §5.1, §5.4 (no stored level; displaced readings), §5.7, §6 Aggregates, invariants 21–23, rows 20a, 40 and 43–46, `api.md` Series and Not in v1 (irregular billing periods) (the tzdata version in force, a day start that occurs twice, an interval zone unknown or become a link, and count, min, max and last when an edge cuts an interval were settled later by Q-76, Q-77, Q-80 and Q-81) |
| Q-47 | How a consumer learns its recovered cursor, and what forgetting means | §4 At-least-once (`resync` carries `forget_above`; the consumer confirms it only when the forgetting is durable; records kept outside the engine are the consumer's duty, never deleted or protected by the engine, as the owner decided), §4 Resync (reset points; consecutive ones delivered as one; a consumer with no cursor file; what `start: position` forgets), §4 Consumer state, §11 Consumers (the `cursor` event removed), `api.md` Consuming |
| Q-48 | The resync owed after a truncated tail is not durable | §3 Recovery rule 3 (a recovery entry at F in the log, adopted on a repeat; nothing owed as state), §3 Order of a start, §3 Durable mark (no file written naming a position above the last synced mark), §4 Resync and Overflow (reset points on resume, in a session and in catch-up), §6 Restart and resume, §8 Behind consumers, §2.2, §8 Restore step 1 (a gap followed by its recovery entry is not a gap), rows 7a and 7c, invariants 1, 16, 25 and 26, `format.md` A.1, A.4, A.6, A.7, A.10 and A.11 |
| Q-49 | What a series with no stored level serves during a restore, part (a): queries | §8 Restore step 2 and §6 Old spans (window edges move to where detail is local, the error `detail_not_local` only when no span is left, never an empty result; reads of facts fetch on demand; no priority download), row 28 |
| Q-49 (b) | Conditions and live state with no local detail during a restore | §6 Conditions (the state is unknown, not false: the evaluation is skipped and counted, `evaluations_unknown`, the condition keeps its latched state, and the first evaluation with detail is delivered as a transition if it differs from the latched state, or there is none), §2.3 Live state (unknown, not none, for a series whose latest recorded `t` no local fact or stored node holds), §4 Resync and §6 Restart and resume (`resync` reports "unknown" for that funnel, no transition), §8 Restore step 2, invariant 16, §11 Health, `api.md` Consuming, Reading and statistics (`ConditionState::Unknown`, `Live::Unknown`) (open part: Q-84, a state that stays unknown) |
| Q-50 | Window edges over moved detail of a series with no stored 1-minute level | §6 Old spans (an edge whose detail is not local moves inward to a node boundary of the finest stored level, or with none to where its detail is local, flagged `approximate_edges`; with no span left, `detail_not_local`; no fetch on the evaluation path), invariants 7, 17 and 22, `api.md` Reading |
| Q-52 | Manifest records as single copies, in part | §7 Manifest (a record appended only after the previous record's sync; the last valid record written again in place at every start; a torn tail, dropped, told from a corrupt record by a valid successor; the last record neither uploaded nor acted on irreversibly until a later record is synced; a corrupt record fetched from deep or the mirror path, else read-only with `manifest_corrupt` and no fact lost), §8 Corrupt block (a manifest record not yet uploaded is a single copy; `mirror_path` covers the manifest), invariant 10, `format.md` Part B (open part: Q-52, rebuilding a corrupt record never uploaded, and checking the in-place rewrite on each platform); the read-only stop for a record with no copy is superseded by Q-52 (a), the automatic rebuild) |
| Q-52 (a), (c) | Rebuilding a corrupt manifest record never uploaded; a mark without a checksum | §7 Manifest (every file self-describing; with no copy in the mirror path or deep, the writer rebuilds the manifest from the local files by the procedure of `rebuild_manifest`, continues, and reports `manifest_rebuilt` until acknowledged; what a rebuild cannot recover from local files: Q-103, settled), §11 Health, `format.md` A.10 (two checksummed slots), A.14 (rebuild record; open part: Q-52 (b)), `api.md` Health |
| Q-60 | Calendar nodes when the expected interval changes | §5.1 density rule (the levels a series stores are fixed by the expected interval its definition gives; a later change moves the staleness limit and keep-alive, never the levels), §5.1 table and §5.7 (calendar nodes for a series that stores the 15-minute level), §5.6 Canonical order (1-minute nodes computed for the merge where they are not stored), row 20a (open part: Q-83, node contents after a past-effective change of the staleness limit) |
| Q-61 | Identity of a condition delivery, and where its sequence is kept | §4 Order (every condition delivery, on a fact or on the timer, is `(funnel, sequence)`; the sequence is in memory, from 1 at every engine start, and a consumer resuming at a start receives none before that start's `resync`), §4 At-least-once (never delivered twice; the identity holds until the consumer's next `resync`), `api.md` Consuming; no field in `format.md` |
| Q-62 | Condition transitions while a consumer catches up after overflow | §4 Overflow (funnels evaluated but silent; the present state sent on re-attaching where it differs; transitions in between counted, `transitions_not_delivered`; catch-up reads hot and cold only and ends with `resync` where its next entry is on no local tier), §4 Resync, §8 Behind consumers, `api.md` Health and statistics |
| Q-63 | Condition state now, for conditions with memory | §6 Restart and resume (state now: a threshold between its levels keeps the latched state, false with no latch; on change compared with the latch by the dead-band), §6 Latch (a latch belongs to one incarnation of a funnel; a funnel added at run time has none and sends no `resync`), invariant 16 |
| Q-64 | Routing changes before a restore's checkpoint | §2.1 (routing changes, site-zone and tzdata entries are kept with their positions in the registry file, which every tree checkpoint carries), §4 Routing (built at start from the registry, so a route is in force from step 1 of a restore and after its log segment has left hot), `format.md` Part B Registry (open part: Q-91, how a funnel declared in configuration is matched with the registry, and what removing or changing one does) |
| Q-70 | Chunk indexes after a restore | §8 Restore steps 1 and 3 (step 1 downloads the chunk index of every compacted file the restore record lists, before ingest is accepted), §5.6, `format.md` Part B Compacted segment; the loss of such an index in the bucket follows §8 Restore, Failures and end (Q-39 (e)) |
| Q-71 | A log segment and the chunks compacted from it hold the same positions in one epoch | §7 (sealed segments compacted in position order, each once: a segment at or below the last position of a live compacted file is not compacted again, except to replace a quarantined file), §7 Manifest, invariant 14 (live compacted files) |
| Q-72 | Whether the revised-key index is consulted when the current fact is off the machine | §2.4 (a match with the key's entry in the revised-key index is a duplicate wherever the facts are; unchecked only for a key with a single fact off the machine and for an incomplete entry), §5.4 Displaced readings, §8 Retention class and Restore step 2, §12 D5, rows 2b, 2c and 40 |
| Q-73 | Who leaves out the unchanged readings of a record-on-change series | §2.6 (the adapter leaves out unchanged readings between keep-alive facts; the engine records every reading it is given), as §2.4 identity including `t`, principle 5 and §11 already required |
| Q-74 | One reading far ahead makes later on-time readings `late` | §2.2 `late` and the lateness reference (facts flagged neither `future` nor `unsynced_clock`; a reading up to 60 s ahead still raises it, an accepted limit; the latest recorded `t`, over all facts, stays for the §2.4 fast path, §8 Restore step 2 and statistics), §2.4 Fast path, §4 Future facts, §8 Deep, Continuous backup and Restore step 1 (each tree checkpoint carries both), invariant 24, rows 4 and 4a, `api.md` `SeriesInfo` (open part: Q-44 (c), far-future facts and the latest recorded `t`) |
| Q-75 | Local days around a site-zone change to another offset | §5.7 (the local date is the date in the zone in force at each instant; the day of D starts at the first instant whose local date is D or later, so days and months tile; "today" is the latest date whose day has started; a change rebuilds from the start, under the old zone, of the local day containing the effective time), row 21, `api.md` Reading |
| Q-76 | Whether a tzdata change is a log entry | §5.7 (the version in force is named by the latest tzdata entry, the first in a store's first batch; the engine reads one release at open, and a writer whose release differs writes a tzdata entry before ingest, newer or older; readers and drills of another release are refused with `tzdata_mismatch`), §2.1, §2.2, invariant 8, row 21, `format.md` A.4 and A.6, `api.md` Opening, Environment seams and Health (the refusal of readers and drills, and adoption only at open, superseded by Q-93 (a); the first batch's entries replaced at gate 3.3 by the store descriptor, `format.md` A.12) |
| Q-77 | A day whose midnight occurs twice | §5.7 (the day starts at the earlier instant), §2.1 `interval length` (a day start that occurs twice likewise; a reading at the later instant is refused off phase) |
| Q-78 | How routing compilation knows the series a consumer emits | §4 Routing (a consumer declares `emits`, recorded with its routing changes, and emits through its own handle; the cycle test compares selections, at start, on every routing change and on an alias), §4 Concurrency, row 33, `api.md` Consuming, `format.md` A.6 (the pattern grammar: Q-88, settled at roadmap 5.6a) |
| Q-79 | A lost range found by a recovery whose sync fails | §3 Order of a start, steps 3 and 4 (the kept segments are synced before anything is written; a failed sync writes nothing, and the range is reported as found and recorded once by the next start that can write), §3 Recovery rule 2 |
| Q-80 | An interval zone that the tzdata version in force does not know, or that has become a link | §2.1 `interval length` (the name is resolved by the version in force each time; a link is followed to its target; an unknown name has no interval starts, so recorded readings are displaced, new ones refused off phase and health degraded until a version that knows it is in force), row 45, `api.md` Reading and Health |
| Q-81 | Count, min, max and last over readings whose `t` lies in the window, on a calendar-length series in another zone | §6 Aggregates (the readings whose `t` lies in the window, each with its whole amount; flagged `apportioned` where an edge lies inside an interval), invariant 22 (counts over adjacent windows add up), row 41 |
| Q-86 (a) | No space when a log segment is created | §3 Segments (the partial file deleted, health critical, ingest waits, creation retried), `format.md` A.4 (open part: Q-86 (b)) |
| Q-52 (b), on Linux | Whether writing the last manifest record, the last log batch or a seal again in place makes the next sync write it | §7 Manifest, §3 Recovery rule 3 and Order of a start, `format.md` A.4 and A.10 unchanged; checked on ext4 and XFS at gate 4.9 with a device that fails one file's sectors (`docs/measurements/4.9-fault-trials.md`; open part: the owner's decision on the macOS and Windows wording) |
| Q-86 (b), on Linux | No space at a log write before any sync, on a copy-on-write file system | §3 Segments and `format.md` A.4 Creation unchanged (a write failure on the log), checked on Btrfs at gate 4.9 (`docs/measurements/4.9-fault-trials.md`; open part: the owner's decisions on the `chattr +C` sentence and the macOS and Windows wording) |
| Q-87 | A mark lost on the same boot after a failed sync | §3 Recovery rule 3, Last batch written again (with the mark missing, unreadable or stale, the start writes the last complete batch, and its segment's header when it is the segment's first, again in place before the sync of step 3; kept only if that sync succeeds, else read-only; rests on the Q-52 (b) check, moved from roadmap 4.8 to gate 4.9), Order of a start step 3, row 12a, `format.md` A.10 (No usable mark) and A.4 (a valid trailer found in an `.open` file is sealed again from step 1, the same hole after a failed seal sync), `api.md` Environment seams (the test file system reports per page whether a sync wrote it). Engine fault tests (roadmap 4.5, page-tracking file system): after a failed sync and a lost mark every position delivered after the restart is on disk at a power loss; the rewrite's sync fails; the same with an adopted recovery entry whose sync failed; a kill-loop property for invariant 2 over failed syncs, mark damage, restarts and power losses; the seal state found with a trailer after a failed `sync_all` |
| Q-93 (a) | A store nobody can open after the host's tzdata changes | §5.7 (readers and drills never refuse; calendar results computed with the reader's own release; a stored calendar node used only where its recorded release is the reader's; the writer checks the host release about once a minute and at each local midnight and adopts a new one without a restart, by a tzdata entry and a rebuild of the affected days), `api.md` Opening and Environment seams (open part: Q-93 (b), (c)) |
| Q-97 | A memory-mapped mark in a project that forbids unsafe code | §3 Durable mark, Sync primitive, §7 Readers (the mark updated by positional writes and synced like any file; no file is mapped), `format.md` A.10, A.13, `api.md` Environment seams |
| Q-98 | Same boot identity after the page cache was lost | §3 Recovery rule 1 (a batch written only after the previous batch's sync and mark update; two or more complete batches beyond the mark on the same boot mean the mark is stale and rule 3 applies; accepted limit: one batch committed since the last mark sync can be lost unreported), invariant 10, `format.md` A.10, `api.md` `log.mark_sync` |
| Q-99 | An exclusive `flock` on the store directory on Windows | §3 Single writer (an operating-system lock on the file `LOCK`, released when the process ends; network file systems not supported), `format.md` A.12, `api.md` Handles |
| Q-100 | Where the largest stored magnitude is kept, whether the policy may change, stored nodes across a scale raise, and a lost raise | §2.1 `decimal scale` ((a) the largest magnitude a per-series function of the log, carried by tree checkpoints with the highest stored scale and brought up to date by replay; units limited to ±(2^63 − 1), `i64::MIN` out of range; (d) the scale in force is the highest of the defined scale, the kept raise entries and the highest stored scale among kept facts, and a lost raise is not written again), `excess precision` and the paragraph under the table ((b) the policy and its maximum changed by a parameter change effective at its position), §5.6 ((c) a node records the highest stored scale among the current facts it covers that are not bad; a merge takes the higher), §8 Deep, Continuous backup and Restore step 1 (checkpoint contents), `format.md` A.5 (`i64::MIN`), A.6 (fields 23–26 in a parameter change) and Part B (the scale of a chunk summary and a tree node), `api.md` Series (`set_excess_precision`). Model property tests (the tracked magnitude equals the maximum over the log after any history, restart and restore, a raise refused exactly when a fact would not fit; each value handled by the policy in force at its position; rebuild equals live and merges in any grouping bitwise equal across raises; after any lost range every kept fact rescales exactly to the scale in force) wait for decimal values in the model (before roadmap 5.1; (c) before 7.1); engine: a node spanning a raise, the batch holding a raise corrupted |
| Q-101 | A replay across a scale raise, rounded or not | §2.4 (decimal values compare by exact numeric value; a fact flagged `rounded` also matches a value that rounds to it at its stored scale; the check runs on the value as submitted, before the excess-precision policy; a correction inside a rounding is dropped as a duplicate unless sent with `revise` or a higher `changed_at`; the revised-key index holds each fact's stored scale and `rounded` flag), `api.md` Core types. Model property test (replaying any recorded history after any raises and policy changes records nothing, writes no raise entry, counts no refusal and changes no current fact) and unit tests (reading, correction, raise, replay; an unflagged 0.1234 then 0.12345 is recorded) wait for decimal values in the model (before roadmap 5.1) |
| Q-102 | A finite value too large for `i64` at the series' scale | §2.5 (out of range, `i64::MIN` units included: recorded as quality bad with no value whatever the policy, counted `out_of_range`, and reported in the reading's outcome while the call succeeds; `auto-raise` never raises for it), §2.1 `excess precision`, §2.4 (compares as "no value"), row 31, §11 Ingest, `api.md` Core types, Writing (`Outcome::Recorded.out_of_range`) and `series_stats` (`out_of_range`). Model property test (values across the `i64` boundary at every scale 0 to 18: none panics, each recorded exactly or out of range) and a unit test (`auto-raise` does not move the scale for 1e300) wait for decimal values in the model (before roadmap 5.1) |
| Q-103 | What an automatic manifest rebuild cannot recover from local files | §7 Manifest (the rebuilt manifest carries every valid record with its old epoch and reconstructs only the corrupt record's effect; no fact about a file or a loss held by one record alone; (a) liveness from valid records and named inputs, a file found in deep never live for that reason, an unlocated file not a lost range until cold, the mirror path and deep have answered; (b) a restore record never rebuilt, read from the local restore record file or deep, else read-only, and a drill never rebuilds; (c) identities kept from valid records, cause unknown where none is given, and a double fault listed under its first missing position and counted; (d) a provisional rebuild after `manifest.rebuild_wait`, uploads, evictions and removals held, deep's copy winning when it answers (the wait and acknowledgement: Q-107, settled)), §8 Eviction at 90 days (a file holding a lost range leaves the local tiers only after that loss's record is uploaded or mirrored), §8 Generations ((f) uploads in epoch order, a rebuilt manifest self-contained, uploaded once final; the local restore record file), §11 Health, invariants 10 and 27 (the counted double-fault exception), `format.md` A.12, A.14 (cause 5 unknown, rebuild record fields 5 ((g) the `file_id` floor) and 6, field 100 old epoch; (e) no fact held only by a quarantine or tier-move record) and Part B (chunk index and local restore record), `api.md` `LossCause::Unknown`, `Reason::ManifestRebuilt.provisional`, `manifest.rebuild_wait`. Engine fault tests with roadmap 6.3, 8.2 and 8.3: corrupt in turn a tier-move record of an evicted file, a publish-and-retire record whose inputs are in deep, a lost-range record, its extension and its acknowledgement, the restore record (with and without its local copy); a sink that does not answer (ingest resumes after the wait, nothing uploaded or removed, the manifest equal to a direct repair once it answers); a restore from records, a rebuild record and carried records; ids issued after a rebuild above the floor; a property that no evictions and one corrupt record shorten `losses()` |
| Q-104 | A supplied `Absent` value with a quality other than bad | §2.5 (recorded as NaN is: quality bad, no value, flagged and counted as `absent_supplied`, the supplied quality kept for the duplicate rule; never live state, so on a state or text series the previous value stays live, and "unknown" is a declared state), §2.4, row 31, `api.md` Core types and `series_stats` (`absent_supplied`). Model: `Model::commit`; `tests/rules.rs` `s2_5_absent_recorded_bad_whatever_quality_supplied`, `s2_4_absent_compared_by_supplied_quality`, `s2_5_absent_changes_no_summary_or_live_state`. Engine: round trip of `no_value` rows at each supplied quality with roadmap 4.3 |
| Q-105 | Reading a segment: zeros below the mark, unknown skippable blocks in a batch, refusals while resynchronising, the first batch against `base_sequence`, a header that disagrees with its name | `format.md` A.2 ((a) sixteen zero bytes at a block boundary are end of data only when the limit is the end of the file, an `.open` segment scanned without a mark, and every byte to it is zero; below the mark's offset or before the seal blocks zeros start an invalid region, also when they run to that limit), A.7 ((b) `block_count`, `batch_bytes` and `batch_crc32c` cover every block before the commit block; a skipped block takes no sequence and carries no `received_at`; a batch of skipped blocks alone is not complete; every block before the seal blocks belongs to a batch; (d) the first batch starts at or above `base_sequence`, higher only after an invalid region or with a recovery entry, checked by recovery at roadmap 4.5), common rules ((c) a version refusal only from a valid position or in an otherwise complete batch at a resynchronisation candidate; any other block image is not a block), A.12 ((e) a header disagreeing with its name is refused, never scanned or deleted). As implemented in `kvik-log`; `log/tests/segment.rs` pins each part |
| Q-106 | Decoding batch bodies: canonical form against the checks, the text heap, run-length choice, entry field presence, entries block splitting, reserved ids, the parameter change, the recovery entry alone | `format.md` common rules ((a) a reader applies only the listed checks; a canonical check also requires the canonical bytes), A.5 ((b) canonical heap in row order, none shared, a reader accepting any offsets inside the heap, shared bytes included; (c) unpadded payload lengths, plain wins a tie), A.6 ((d) presence checked as invalid content, a named-state series needs a name, kind and value type not compared; (e) entries share a block up to 16 MiB; (f) entry type 5 refused until roadmap 5.2, field 41 skipped (superseded by Q-46: field 41 is the reset stamp); (g) fields 25 and 26 allowed in a parameter change since Q-100, fields without a range take any value, a change with only field 1 valid; (h) a recovery entry not alone in its batch is invalid content), A.11 (nothing allocated from a declared count or length before it is checked; output bounded by the row and entry limits). As implemented in `kvik-log`, with (g) changed for Q-100; `log/tests/batches.rs`, `facts_checks.rs`, `entries_checks.rs` and `batch_assembly.rs` pin each part |
| Q-107 | The wait before a provisional manifest rebuild, leaving the read-only state, and acknowledging a provisional rebuild | §7 Manifest, Deep unreachable (owner: the wait is counted on the monotonic clock from each start, once deep has been asked and failed, and a restart starts it again; accepted limit: a process restarting more often than the wait stays read-only, `rebuild_manifest` being the way out; the store leaves read-only by itself when the wait ends or deep answers, the start resuming from step 4, the one exception to §3 Write or sync failure; a provisional rebuild cannot be acknowledged until it is final), §3 Write or sync failure, §11 Health, `api.md` Health. Engine fault tests with roadmap 6.3: the wait across restarts; leaving read-only when deep answers during the wait; the acknowledgement refused while provisional |
| Q-109 | A forged seal in an `.open` file | `format.md` A.4, the states recovery may find (settled at gate 4.9 by a structural rule, with no checksum pass at open and no change to A.9: an `.open` file counts as seal-written only if its seal blocks start exactly at the end of its last complete batch, the trailer's `batch_count` and `last_sequence` equal the scan's, and it is not the last segment in use; otherwise the trailer and end marker are bytes of what precedes them, and recovery reads the file again with them ignored, so a torn tail's commit blocks and definitions count; `log/tests/recovery.rs` `a_forged_seal_after_a_torn_batch_in_the_last_segment_hides_nothing`, `a_forged_seal_in_a_segment_that_is_not_the_last_in_use_is_ignored`, `a_forged_seal_in_the_last_segment_is_a_torn_tail`) |
| Q-117 | A recovery entry adopted after its own sync failed | §3 Recovery rule 3, Last batch written again, and Order of a start step 3 (an adopted entry is written again in place before the sync of step 3, whatever the mark; extends the Q-87 ruling). Scenario: a readable mark of the old boot; the start's sync of its recovery entry fails, so the entry is in the page cache only; the next start on the same boot adopts it, and its sync writes nothing; the mark then names F, a batch is acknowledged at F + 1, and a power loss removes the entry, which the following start reports as `data_lost` with nothing damaged (`log/tests/recovery.rs`, `an_adopted_entry_whose_own_sync_failed_is_written_again`) |
| Q-115 (a)–(d), (g)–(i), (l), (m) | The commit path, in part: closing a batch, the seal clock, the first mark, retrying a creation, the first manifest record, failures creating ahead, the receive time, memory at seal, a blocked seal | Settled at gate 4.9: `format.md` A.4 Sizes ((a) a batch takes whole calls in order while the sum of their data blocks, each encoded alone, stays within `max_batch_bytes`, and at least one; calls that do not encode together are committed one by one); `spec.md` §3 Sealing ((b) the 5 minutes run from when the segment is taken into use; the sync that takes the next segment into use at a timed seal carries no batch, is not bound by `min_sync_interval`, and counts as the previous log sync for the next batch); `format.md` A.10 First mark ((c) slot 0, counter 1, all position fields 0, the current boot id); `spec.md` §3 Segments ((d) retried when free space is at least `disk_critical` + `segment_size`, checked every second, and every 30 seconds where free space is unknown, `log/tests/writer.rs` `with_free_space_unknown_a_failed_creation_is_retried_after_30_seconds`; (h) any failure of a creation other than lack of space at the create or a write, a failed sync included, is a write failure on the log); `format.md` A.14 ((g) the first record takes epoch 1); A.7 ((i) `received_at` and the commit time are the wall clock when the batch is formed, raised to the previous batch's commit time); `design.md` Memory ((l) sealing uses memory independent of `segment_size`, the checksum streamed in 1 MiB reads, `log/src/writer/maint.rs` `sealing_reads_at_most_1_mib_at_a_time`); `format.md` A.13 ((m) a sharing error — on Windows the system errors 5, 32, 33 and 1224, on any platform 'resource busy' — at the rename or length change, and lack of space at the rename, are retried every 10 seconds with health degraded; any other failure of either is a write failure on the log; a rename that fails with the `.open` name gone and the `.kvl` name present is a failed sync, never retried; `log/tests/writer.rs` `a_seal_blocked_by_a_sharing_error_is_retried`, `no_space_at_a_seal_rename_is_retried`, `a_failed_rename_or_length_change_of_a_seal_is_a_write_failure`, `a_rename_whose_flush_fails_is_a_failed_sync_not_retried`, and on Windows `a_seal_blocked_by_another_handle_finishes_after_its_release`) (open part: Q-115 (e), (f), (j), (k), (n)) |
| Q-116 (d)–(j), (l), (o), (p) | Recovery at a start, in part: a first start cut short, rule 1 and later segments, the tail's extent, epochs above leases, gaps between sealed segments, an empty segment in use, a batch beyond the mark in a sealed file, unused segments with bytes, a mark from the page cache, a header put in use by a failed sync | Settled at gate 4.9: `spec.md` §3 Recovery ((d) a mark with no valid slot beside a log with no segment in use and a manifest naming no position is a first start, and an empty `log/` stops being a safe test at roadmap 8.1; (f) the tail reported is that of the last segment in use; (j) a sealed segment holding positions above the mark makes the mark stale); `format.md` A.4 ((e) under rule 1 the headers of later segments stay and the writer resumes in the last segment in use; (h) a start checks the gap rule for unsealed segments and against the last sealed one, and the scrub checks gaps between sealed segments; (i) a segment in use by its header alone, not the last in use, is left as it is and never sealed, at the cost of being read whole at every start; how the state arises is a follow-up for 5.1; (l) an unused segment is kept as created ahead only if every byte after its header is zero); (g) by roadmap 4.6 (`format.md` A.14: a start's new epochs are above every epoch a lease file names); (o) by Q-122, its residual (a reader may read a mark not yet synced; the reset stamp voids what it took) accepted in `spec.md` §7 Readers; (p) `spec.md` §3 Order of a start step 2 and `format.md` A.4 In use (a start writes the header of the segment it resumes in again, in place, before its first sync; `log/tests/recovery.rs` `a_header_put_in_use_by_a_failed_sync_is_written_again_by_a_same_boot_start`) (open part: Q-116 (a)–(c), (k), (m), (n)) |
| Q-118 (a), (b), (d), (e), (g)–(j), (l) | The reader process and leases, in part: no mark, retry pace, a refused file, truncation without a reset point, a failed lease write, when the writer looks at leases, a corrupt manifest record, the lease header, a start during a refresh | `format.md` A.10 Read ((a) a missing mark file is `MarkUnreadable`; (b) the 8 retries are back to back); `spec.md` §7 Readers ((d) nothing at or past a refused file is read; (g) no read without a renewed lease; (i) a manifest record a reader cannot take an epoch from is an error, `ReaderError::Manifest`); `spec.md` §7 Leases ((h) the writer looks at leases at its own turns, at most once a minute, and a lease is void only when both boot ids are non-zero and differ); `format.md` A.15 ((j) the lease header has `file_id` 0 and the generation at creation): settled at gate 4.9 as implemented. (e) and (l) by the owner's rulings after roadmap 4.6: rule 1 writes a recovery entry with a new reset stamp whenever it truncates a complete batch, amended by Q-125 to a complete batch or any non-zero byte beyond the mark (`spec.md` §3 Recovery rule 1; `log/tests/recovery.rs` `a_kill_on_the_same_boot_truncates_the_batch_beyond_the_mark`), and a reader compares the stamps of its two reads of the mark and discards the pass when they differ (`log/tests/reader.rs` `a_mark_with_a_new_reset_stamp_during_a_refresh_discards_the_pass`); the residual of (l), corruption events of a pass that a start overtook may be wrong, is accepted and stated in `spec.md` §7 Readers (open part: Q-118 (c), (f), (k)) |
| Q-119 | `batch_crc32c` did not depend on the blocks it was said to cover | `format.md` A.7 and A.2 (owner's ruling after roadmap 4.6: the sum covers bytes 0..12 of each block header, without `header_crc32c`, so every body is reached through its `body_crc32c`; the candidate check stays O(blocks)). Scenario: each 16-byte header ends with the CRC32C of its first 12 bytes, and a CRC fed a message and then that message's own CRC returns to a fixed state, so every set of valid headers gave the same sum for one store, file, offset and block count; a batch assigned again at the same offset after a remount had a byte-identical commit block (`log/tests/segment.rs`, `batch_crc_tells_apart_blocks_that_differ_only_in_a_header`; golden files regenerated) |
| Q-46 | Checking a kept position against a later truncation or restore | §7 Readers, Reset stamp and saved points; §3 Recovery rules 1 and 3; §4 Resync (`start: position`); §8 Generations; row 7b; `format.md` A.6 field 41, A.10 bytes 84..100, A.14 `0x25`; `api.md` `ReaderState.stamp`, `SavedPoint`, `Start::Position`, `check` (owner's ruling after roadmap 4.6: a reset stamp, a reset sequence and a random nonce, in the mark, every recovery entry and every restore record; a saved point is (position, stamp); kept when the generation and stamp are the same, else void above the lowest of the later entries' `after`, restored ends and the positions before later lost ranges, and unknown when the reset the mark names cannot be found). This answers the review's three objections: the lower bound combines entries, restored ends and lost ranges; a restore takes a fresh nonce, so a reset stamp is never reused; a lost entry gives unknown. The writer writes the stamp and the reader reports it (`log/tests/recovery.rs`, `log/tests/reader.rs`); the check itself, `start: position` and row 7b's export are built with roadmap 5.3 |
| Q-122 | A torn mark update could leave a mark older than the last one synced | `format.md` A.10 Update and Read, `spec.md` §3 Order of a start step 2 (settled by roadmap 4.7: every update between two mark syncs goes to the slot that does not hold the last synced mark, and a start writes the mark file again in place, as read, and syncs it before its own mark write, or, keeping the mark it read, writes that slot again and syncs it before the first update). Scenario: updates alternated between the slots whether or not the last was synced; with a timed mark sync, A held counter c, synced, and B c − 1; c + 1 went to B and c + 2 to A, neither synced, and a power loss that tore the write of c + 2 left A invalid and B at c − 1, older than the mark synced last. Rule 3 kept every complete batch, but the start took its id floor from that mark and the unsealed segments only, so an id defined after c − 1 in a batch then in a sealed segment was above the floor; the low floor was durable, in the recovery entry and in the new mark's highest series id, so ids would have been assigned again once roadmap 5.1 assigns above floors (`log/tests/recovery.rs`, `a_torn_mark_update_never_falls_back_past_the_last_synced_mark`) |
| Q-150 | Canonical bytes of the zstd-compressed XOR fallback | `format-chunk.md` C.8 and C.10 (settled at the review of roadmap 6.1 by option (2): encodings 4 and 5 are one canonical class, compared by the XOR stream they hold; the class is the canonical choice when the XOR payload is strictly shorter than every other candidate, never by a compressed length, and an encoding 5 payload is canonical only when shorter than its stream; cost over 2,000 rows, in bytes of XOR / best other / compressed XOR / in the class: a °F→°C walk changing every row 9,767 / 12,141 / 3,591 / yes, a two-level flicker 6,284 / 9,045 / 1,057 / yes, a cycle of five ninths 13,255 / 13,146 / 191 / no, so written in delta; rejected: the class winning within 1/16 of the best other candidate, which recovers such cycles but makes blocks up to about 1.5 % larger on common data where zstd finds nothing (a sine: 16,243 against 16,000), and common data matters more than that shape; the payload is exactly one frame, no dictionary, no checksum, no skippable frame; no level or library is pinned; `chunk/src/block/tests.rs` `xor_class_membership_ignores_compressed_length`, `xor_class_is_canonical_in_either_encoding_and_any_frame`); roadmap 6.5 measures a pure-Rust codec |
| Q-121 | Series id 0 against an id floor of 0 | §2.1 `id`, `format.md` A.6 field 46 and A.10 `max_series_id` (settled by roadmap 5.1, which assigns ids: ids start at 1 and 0 names no series, so a highest id assigned or an id floor of 0 means none; the model's registry assigns from 1). Scenario: the model assigned ids from 0, so the mark's `max_series_id` and a recovery entry's id floor were 0 both in a store that had assigned no id and in one whose only series was id 0; read literally, "above every id floor" then forbade id 0 after a power loss in an empty store, and no reader of the floor could tell id 0 from none. Decoders do not refuse id 0, so files written before stay readable (`kvik/tests/durable.rs`, `ids_start_at_one_and_never_reuse_a_floor`) |
| Q-124 | Loss reports of recovery entries, and the order of lost ranges against saved points | §3 Recovery rule 3, Order of a start step 4, §7 Readers, §11 Health, invariant 10, rows 7, 7a and 12a, `format.md` A.14 field 13 (settled after roadmap 5.1a: (a) by the owner's ruling, (b)–(d) at its review). (a) The gap of a recovery entry (positions between K and F) is a `data_lost` with cause truncated tail, which holds health degraded until acknowledged (until closed since roadmap 8.5a, by self-repair or acknowledgement, §8 Self-repair); a `possible_loss` is one per recovery entry that names a non-empty tail, a mark not readable or stale, or a missing or unreadable cursor file. (b) A `possible_loss` has its entry's position, F, as identity; identities are unique within a kind. (c) A lost-range record carries the reset stamp in force when it is written; with none known it counts as later than every reset the start found. (d) A loss reported as found by a start that could not write is not lost when a later start finds the damaged batch as a torn tail: this adds a behaviour, that start keeps reporting it as a `data_lost` under the same identity, the entry's gap (positions named above K), now recorded. Scenario: §11 listed the gap as a `possible_loss` while the engine reported it as a `data_lost`; the engine named a `possible_loss` by K + 1, which two entries can share (an entry at K + 1 lost with the mark, the next written at K + 2 with `after` = K); a lost range found later below an entry could not be told from one recorded before it, so a saved point could not be checked against it; and a found loss could vanish from the list, against invariant 27 (`tests/rules.rs` `s3_lost_mark_and_tail_leave_a_gap_described_by_the_entry`, `s7_saved_point_and_a_lost_range_recorded_with_the_mark_lost`, `s7_saved_points_across_a_gap_and_its_lost_entry`, `s11_a_found_loss_stays_reported_after_the_mark_is_lost`; `log/tests/model_histories.rs`) |
| Q-125 | Bytes damaged beyond a same-boot mark | §3 Recovery rule 1, Order of a start steps 5 and 6, row 7, invariant 10 (settled after roadmap 5.1a by the owner's ruling: a complete batch or any non-zero byte beyond the mark is a reset point, with a recovery entry, a new reset stamp and a `possible_loss`; damaged batches beyond the mark go into that entry's tail, so one entry at K + 1 describes them). This amends the Q-118 (e) ruling: "whenever it truncates a complete batch" becomes "whenever it truncates a complete batch or any non-zero byte beyond the mark"; a write cut short on the same boot now also costs a recovery entry and a report. Scenario: with a timed mark sync, a batch was acknowledged and delivered, the file system was remounted within the boot, and that batch, now beyond the mark, was then damaged; it looked like the garbage of a write cut short, so rule 1 truncated it with no recovery entry, no loss and no reset point, and its positions were given again (`tests/rules.rs` `s3_damaged_bytes_beyond_a_same_boot_mark_are_a_reset_point`, `s3_damaged_and_complete_batches_beyond_a_same_boot_mark_are_one_reset`; `log/tests/recovery.rs` `damaged_bytes_beyond_the_mark_after_a_remount_are_a_reset_point`, which fails on the earlier rule). See Q-170: its faster durability level (acknowledged once written) promises that a reading survives a crash of the process, but rule 1 truncates everything beyond the mark on the same boot; rule 1, principle 3 and invariants 2 and 11 were rewritten for that level at roadmap 5.5a, keeping complete flagged batches beyond the mark while this ruling still governs what lies after them |
| Q-170 | Durability levels chosen per source | `spec.md` §1 principles 3 and 5, §2.1 `durability`, §3 Batches, Commit, Group commit, Recovery rules 1–3, Order of a start steps 2 and 3, §4 Overflow, §9 rows 7, 7a and 12, invariants 2, 10 and 11, §11; `format.md` A.6 fields 28 and 48, A.7 `flags`, A.10; `api.md` Series, Writing, Configuration, Health (settled at roadmap 5.5a from the owner's decision; open parts Q-250 to Q-253) |
| Q-171 | A provisional manifest rebuild across a restart | `spec.md` §7 Deep unreachable, `format.md` A.14 (decided at the review of roadmap 6.3a, the restart point decided by the owner: at open, a rebuild record that no acknowledgement record names is provisional, so the holds survive a restart, until the mirror path and deep answer in that run for every corrupt span it stands for, or are not configured; a provisional rebuild's acknowledgement is refused, also at append; after a restart with deep unreachable, a rebuild that was final but not yet acknowledged is provisional again, and health stays degraded until deep answers). The format change is rebuild record field 7 in `format.md` A.14, an optional repeated field; a rebuild record without it still reads. The residual found at that review, that after a restart only the first corrupt record (fields 1 and 2) was known, so a later span whose loss only deep's copy holds could be dropped once the first answered, is closed by rebuild record field 7 (`format.md` A.14), which lists every corrupt span with the epochs around it and is read at every start (a rebuild record without it falls back to fields 1 and 2, bounded above by the first record it carried past them). `q171_every_corrupt_record_is_asked_for_again_after_a_restart`, `q171_after_a_restart_a_corrupt_record_whose_epoch_jumped_is_still_taken_from_deep`, `files/tests/rebuild_copies.rs`, `q171_a_rebuild_no_acknowledgement_names_stays_provisional_across_a_restart`, `q171_a_final_rebuild_not_yet_acknowledged_holds_again_after_a_restart`, `q171_a_final_unacknowledged_rebuild_restarted_with_deep_unreachable_is_provisional_until_deep_answers`, `q171_a_rebuild_of_two_corrupt_records_is_final_only_once_both_have_their_answers` |
| Q-174 | Adjacent corrupt records read as one | `spec.md` §7 Deep unreachable, `format.md` A.14 field 6 (decided at the review of roadmap 6.3a: a corrupt span whose next valid epoch is more than 2 above the last valid one before it counts as two corrupt records for the field-6 count, accepting a count that may include a loss whose identity is right after a legitimate epoch jump) |
| Q-91 | Funnel identity across configuration, run-time routes and restarts | `api.md` Consuming (a funnel equal to one in force is that funnel), §4 Funnel, Routing, §6 Latch, row 16 (owner's rulings at the review of roadmap 5.2 and its audit: a funnel is identified by its consumer's name and its funnel id, both in the routing change; a new funnel's id is one above the highest any routing change of the consumer named, removed funnels included, so an id is never given again; the routing table is rebuilt from the log at every start; a latch belongs to the incarnation that wrote it, the position of the routing change that added the funnel, and is dropped with the funnel. (c) `Consumer::route` with a funnel equal to one in force adds nothing: it returns that funnel's id, writes no routing change and keeps the latch, so an application may route its funnels at every start, as `api.md`'s example does; on both backends. (a) and (b) do not arise while the interface declares no funnels in configuration; the item that adds them records them again). Scenario: an application routes `above 10` at every start; the value rises to 20 while it is down; after the restart its route returns the same id, the funnel is not doubled, and the resync that ends the resume reports the condition true as a transition, once; at the next start, routing again, it is no transition (`kvik/tests/consumers.rs` `routing_again_at_every_start_keeps_the_funnel_and_delivers_a_change_once`) |
| Q-95 | A consumer with no cursor file at a same-boot restart, and one that registers again during a run | §4 Resync, New consumer, Consumer state, §6 Latch, Restart and resume (owner's rulings at the review of roadmap 5.2 and its audit: (a) a consumer whose cursor file is missing or unreadable at a start while routing changes name it, on the same boot or after a reboot, whether or not a recovery entry names it, has lost its cursor: it starts with no latches and receives the `NoCursor` resync of Q-131; a consumer that no routing change or cursor file names is new. (b) A consumer taken again during a run, its process having restarted and not the engine, resumes from its confirmed cursor with its confirmed latches, by a replay and a `resync`, as after downtime; what it was handed and had not confirmed is dropped; its condition delivery sequences continue within the run and start at 1 at the next engine start (§4 Order)). Scenario: (a) a cursor file is damaged while the engine is down after a clean shutdown, so no recovery entry names it; the consumer's next handle first receives the `NoCursor` resync, each condition a transition (`kvik/tests/consumers.rs` `a_lost_cursor_file_gives_a_resync_with_funnel_states_by_start_kind`); (b) the consumer process restarts: it is replayed what it had not confirmed, then a resync (`kvik/tests/api.rs` `a_consumer_taken_again_replays_then_resyncs`, both backends) |
| Q-116 (b), (m) | Recovery at a start, in part: F's sources, and the consumers in the recovery entry | §3 Recovery rule 3, `format.md` A.6 field 47, A.15, A.16 (owner's ruling at the review of roadmap 5.2, as implemented: (b) F is above every position named by the manifest's lost-range records, the registry file's `through` and every local cursor file (its cursor, its latches' positions and its last confirmed reset point), each kind of file contributing through the list of position sources a start reads (`log/src/writer/sources.rs`), to which roadmap 6.2 and 7.1 add derived files, each with a test; (m) field 47 names every consumer whose cursor file is unreadable, and every consumer named by a routing change in the registry file, an unsealed segment or a segment sealed above the registry file's `through` (Q-130) whose cursor file is missing; `losses()` reports such an entry as a `PossibleLoss` naming them (`spec.md` §11 Health)). Scenario: after a power loss the kept log ends at position 4 while a cursor file names position 12; the recovery entry is written at 13, so position 12 is never assigned again, and names the consumer whose cursor file is unreadable and the one, known from its routing change, that never wrote one (`log/tests/recovery.rs` `cursor_files_raise_f_and_missing_or_unreadable_ones_are_named`) |
| Q-130 | A consumer's missing cursor file that a start cannot tell | §3 Recovery rule 3, §4 Consumer state, §2.1, `format.md` A.6 field 47, A.16 (owner's ruling at the audit of roadmap 5.2: the registry file is written after each routing change, before that change's position can be sealed). Implemented: the routing call commits the change, and once its batch is acknowledged (with the mark synced, so the file names no position above it, invariant 26) writes the registry file before it returns, under the lock that orders routing changes; a write that fails turns the store read-only. A seal may still complete between the acknowledgement and the file, so a start also reads the routing changes of the segments sealed above the registry file's `through`, newest first, stopping at the first segment that begins at or below it. The registry file is written at a start, a clean shutdown and each routing change, so after a crash the start reads every segment sealed since the last of those, which can be the whole previous run, and with no registry file every sealed segment; that restart cost is tracked by Q-116 (n). Scenario: a consumer's only routing change lies in a segment sealed since the last registry file was written, the process stops, and the consumer's cursor file is lost; the start names the consumer in the recovery entry (`log/tests/recovery.rs` `a_routing_change_sealed_above_the_registry_file_names_its_consumer`); in the interface, the file holds a routing change as soon as `route` returns, and after a power loss with the cursor file unreadable the consumer is reported and sent the `NoCursor` resync (`kvik/tests/consumers.rs` `the_registry_file_holds_each_routing_change_when_the_call_returns`) |
| Q-131 | The resync of a consumer with no cursor file | §4 Resync, New consumer, `api.md` Consuming (owner's ruling at the audit of roadmap 5.2): the position before a start is the position before the first fact at or after T for `start: time`, P for `start: position` P (a saved point: kept, P; void above X, X; unknown, the registration is refused with `ConsumerError::UnknownPoint`), and the mark's last position otherwise. A new consumer receives no resync: its cursor file is written at once naming that position and it starts after it, reset points above it applying as to a cursor there. A consumer whose cursor file is lost (Q-95 (a)) receives, before its first delivery, a resync of cause `NoCursor` with `forget_above` = that position, carrying each funnel's current live and condition state, every condition a transition since its latches are lost; §4's "any other `resync` carries no funnel state" names this exception, and `api.md` lists `NoCursor` among the causes. Scenario: facts at 0, 1, 2 and 3 s; a consumer started at 2 s starts after the fact at 1 s, one started now after the last position, one at a kept point at it; a point above what a power loss kept is void above the recovery entry's `after`, and that entry is then a reset point for it; with their cursor files damaged, three consumers latched true each receive the `NoCursor` resync with `forget_above` by their start kind and the condition true as a transition (`kvik/tests/consumers.rs` `a_new_consumer_starts_after_the_position_before_its_start_with_no_resync`, `a_lost_cursor_file_gives_a_resync_with_funnel_states_by_start_kind`; `kvik/tests/api.rs`: a new consumer receives nothing on either backend) |
| Q-132 | A confirmed latch lost with the last second of a cursor file | §4 Consumer state, At-least-once, §6 Latch, invariant 26 (owner's ruling at the review of roadmap 5.2 and its audit: a confirmation that changes a latch writes the cursor file at once and returns once it is synced, so a confirmed condition delivery is never reported again as a transition; a state acted on but not confirmed before a stop is reported again by the resync that ends the next resume if it differs from the latch, so actions on conditions are at least once, as `spec.md` §4 At-least-once now says; the immediate write never names a position above the last synced mark: it asks for a mark sync first and is skipped, the store read-only, if the mark cannot be synced past it). Scenario: a consumer confirms a trigger to true and the process stops a moment later, the mark synced only on request; the cursor file already holds the latch, so the resume reports the condition true as no transition; every cursor file synced names no position above the mark synced at that moment (`kvik/tests/consumers.rs` `a_cursor_file_never_names_a_position_above_the_synced_mark`, `routes_cursors_and_latches_survive_a_restart`; `kvik/tests/consumer_properties.rs` `deliveries_after_crashes_follow_the_rules`: after crashes part-way through a confirmation, a condition reported as no transition is always in a state handed out before a confirmation) |
| Q-160 | Objects in the deep tier: keys, claim marker and envelope | `format.md` A.17, `spec.md` §8 Deep and Encryption, `api.md` Backup and restore (settled by roadmap 8.1a; owner: backups always encrypted, no unencrypted mode; XChaCha20-Poly1305 from an audited crate; 64 KiB authenticated chunks under a header with format version, algorithm id, key check value and nonce base). Keys start at the store id with no engine prefix (Q-198), keep a log segment's local name with other kinds under `files/<kind>/`, and list a generation's records in epoch order; a restore finds the store id by listing. Every object, `STORE`, records, claim markers and probes included, is an envelope (amended by Q-340: claim markers and checksum probes are not). An uploaded record is its A.2 block alone (amended by Q-610: preceded by the epoch of the record before it); the store id is bound by key derivation and associated data. Subkeys and the key check value come from HMAC-SHA-256 of the key and store id; chunk nonces are the nonce base, the chunk index and a final flag, with the header and object key as associated data, so ranges decrypt alone and truncation, reordering and moved objects fail. **The nonce base is derived** (SIV-style, from the key, the object key and the plaintext's SHA-256) rather than random, as the owner's ruling worded it. Scenario: the sink accepts a repeated `put` only with identical bytes, so with a random base a driver that restarts after its upload landed would send other bytes and lose the key for good, unless it kept a local record of each attempt, a new mechanism. Reason: the derived base makes a file's envelope the same at every attempt; it reveals only that two objects under one object key and key are identical, and keys never hold other content. To be confirmed by the owner as a change of wording. Golden files and the code are roadmap 8.1b |
| Q-162 | Buckets without conditional writes or checksum checks | `spec.md` §8 Bucket and §13, `format.md` A.17 Probe, `api.md` Backup and restore (owner's ruling at roadmap 8.1a: such a bucket is refused, `BackupRefused`; no weaker mode). The engine probes at `attach_backup` and before a restore claims: a second create under a taken key must be refused (scenario (a)), a put with a wrong SHA-256 must be rejected and leave nothing (scenario (b), a bucket that trusts the sent hash), and `stat` must report a hash. Scenario (a) is added to §13, whose metadata fallback is dropped. The probe object stays (no deletion), one per store on a bucket that passes. A checksum check that fails only sometimes is still caught by the sampled download alone. Telling a missing checksum from other sink failures: Q-210 |
| Q-180 | Stored nodes keep sums, integrals and increases exact | `spec.md` §5.6, §6, invariants 6, 7, 21, 22; `format-tree.md` T.3 (settled at the review of roadmap 7.1a: exact, stored as dyadic numbers, folded with a dyadic accumulator; measurements in the question) |
| Q-181 | Which zero is the minimum of +0.0 and −0.0 | `spec.md` §5.2, §5.6; `format-tree.md` T.5 (settled at the review of roadmap 7.1a: IEEE total order, −0.0 below +0.0) |
| Q-182 | A state name declared twice | `spec.md` §2.1 `value type`; `format.md` A.6 field 13 (settled at the review of roadmap 7.1a: names are distinct, a definition repeating one is refused) |
| Q-190 | The Parquet export schema as a user contract | `docs/export.md` Columns, Metadata and Compatibility (settled by the owner, 2026-11-16: the schema as drafted, `kvik.export_version` = `1`; adding a column or metadata key is not breaking, renaming, retyping or removing one raises the version) |
| Q-191 | The default view of an export, and Parquet for orphaned and set-aside facts | `docs/export.md` Scope and Options (settled by the owner, 2026-11-16: (a) `Current` by default, `History` named for re-ingest; (b) the same schema, one file per call, built at 8.3) |
| Q-192 | Decimal values in the export, and the scale in force on `SeriesInfo` | `docs/export.md` Columns (settled by the owner, 2026-11-16: (a) `DECIMAL(19, scale in force)` with `stored_scale`; (b) the scale in force with decimal series on the durable backend, 6.4b; `ScaleAboveExport` until then) |
| Q-193 | Fact flags as boolean columns or a bitfield | `docs/export.md` Columns (settled by the owner, 2026-11-16: boolean columns) |
| Q-194 | Quality and named states in the export | `docs/export.md` Columns and File layout (settled by the owner, 2026-11-16: dictionary-encoded quality strings; a named state as index plus `state_name`) |
| Q-195 | A `changed_at` of kind `time` beyond the timestamp range; forecast labels | `spec.md` §2.2, `api.md` Errors and Writing, `format.md` A.5, `docs/export.md` (settled by the owner, 2026-11-16: (a) refused at ingest with `Refusal::ChangedAtOutOfRange` on both backends, `i64::MAX` accepted; (b) forecast exports keep their column names, labelled by `kvik.timing`; the stored case is Q-760) |
| Q-196 | Export file layout, compression and store-level metadata | `docs/export.md` File layout, Partitioned layout, Options and Metadata; `api.md` Reader processes (settled by the owner, 2026-11-16: one file per series per period, row groups of at most 131,072 rows, zstd level 1; site and store identity in the metadata, the store id from `Reader::store_id`; `site=<site>/series=<escaped name>/month=<UTC YYYY-MM>/<view>.parquet`, amended 2026-11-17 to `site=<site>/area=<s1>/device=<s2>/metric=<s3>/month=<UTC YYYY-MM>/<view>.parquet`, each segment escaped on its own and a name without three non-empty segments refused (Q-670); a clipped month as `<view>.partial-<from_ns>_<to_ns>.parquet`; the rest of the store-level metadata at 8.3, Q-196 (c); names: Q-761) |
| Q-197 | The interval end in an export, and calendar-length interval series | `docs/export.md` Columns (settled by the owner, 2026-11-16: keep `interval_end`; calendar-length ends from the interval zone under the tzdata version in force at export, recorded in the metadata, built at 7.3) |
| Q-108 | A restore from a rebuilt manifest only partly uploaded | §8 Generations (settled by roadmap 8.3a, as the question suggested: a rebuilt manifest counts for a restore only once it is whole in the bucket, its closing empty record, the one without field 100, uploaded; the restore reads from the last such rebuild record up to the next rebuild record, and with none whole it reads the records before the first rebuild record that opens a rebuilt manifest, which the old manifest uploaded before it; records upload in epoch order, so a whole rebuilt manifest is self-contained). Scenario: a rebuild record and some carried records are in the bucket, the rest not; read from the rebuild record, a restore misses files named only by records not yet carried, and their positions become a gap or a set-aside range although the bucket holds the files and the records naming them. A rule keyed on the rebuild record alone reads that truncated list. What a whole rebuilt manifest restores before its files are verified again is Q-295. The selection is `kvik_backup::plan::records_to_read`, which the restore driver applies before the fold (`backup/tests/restore_plan.rs` `q108_a_rebuilt_manifest_only_partly_uploaded_is_not_read`, `q108_records_after_a_whole_rebuilt_manifest_are_read_with_it_and_a_later_partial_one_is_not`) |
| Q-290 | The restored end and recovery entries inside segments | §8 Restore step 1 (settled by roadmap 8.3a, the general case of the rule). The plan scans the copied log segments' ranges in the source generation; a gap that recorded lost ranges cover does not end the restored log, nor one followed directly by a recovery entry whose `after` the caller names (`PlanInput::recovery_after`); a gap that holds a position of a listed segment with no copy is never bridged, since the entry is then not the next restored position. The restore driver names none, since the record is written before any file is downloaded, and needs none: a recovery entry's gap is recorded as a lost range (cause 2) by the start that wrote the entry (step 6) or by the next start that can write (step 4), and the segment holding the entry is sealed, and listed by a sealed-segment record, only by a run that could write after that, so the listing has the higher epoch; records upload in epoch order, so a restore that lists the segment has read the loss. Gaps inside a listed segment's range are recorded losses for the same reason (rule 2 regions at step 4), so a segment's range is taken as held. The loss side is guaranteed by the log's start, which goes read-only rather than seal without the record. The listing side rests on the sealed-segment record being appended after the start that recorded the gaps, an invariant roadmap 6.3b states (`format.md` A.14, Q-220 (e): no sealed-segment record has an epoch below a lost-range record of the start that preceded its seal; `log/tests/seal_describe.rs` `no_description_leaves_the_writer_before_the_gap_below_a_recovery_entry_is_recorded`, `a_start_that_cannot_record_the_gap_below_a_recovery_entry_describes_nothing`), and roadmap 8.4a needs a kill-loop case with a truncated tail, a seal and a restore. Should it fail, the plan ends the log at the gap and sets later data aside: conservative. The hook stays for a caller that has read the segments. Scenario: a power loss truncates a tail; the start writes a recovery entry at F above K + 1 and the lost range K + 1 to F − 1; a restore from the records sees a gap between the segment ending at K and the one starting at F. Tests: `backup/tests/restore_plan.rs` `a_gap_a_recorded_loss_covers_does_not_end_the_restored_log_a_partly_covered_one_does`, `a_gap_followed_by_a_recovery_entry_naming_the_position_before_it_does_not_end_the_restored_log`, `a_recovery_entry_in_a_segment_with_no_copy_bridges_nothing`, `the_restored_end_is_the_one_the_spec_walks_to` |
| Q-293 | What a restore plan lists, places and fetches | §8 Restore steps 1 and 3, Generations; `format.md` A.14 `0x25` (decided by roadmap 8.3a). Every live file with a recorded copy in deep is listed, set-aside files included, and no other file; the copy counts whether or not a fresh check has marked it verified since a rebuild (Q-295). A listed file's tier is the one the source records give it, which is where the restore places it (step 3). The files fetched are every listed log segment (with no tree checkpoint, step 1 downloads the whole sealed log) and every other listed file placed on hot or cold, in ascending position. The restored log is scanned over log segments only: a compacted file holds only some positions of its range, and log segments are never retired by compaction (§7). When the source generation's sequence 1 is not held, the restored end is that of the restore record read, or `0:0` in generation 0. Positions of an earlier generation at or below a carried end that no copied segment or recorded loss holds fail the plan: `NotCopied` when a listed segment without a copy holds them, else `MissingBelowEnd`. Scenario: a store compacted some segments into a current file in deep and lost no segment; the plan fetches the segments, lists the compacted file and leaves it in deep. Tests: `backup/tests/restore_plan.rs` `a_listed_segment_with_no_copy_ends_the_restored_log_and_is_not_named`, `a_compacted_file_on_hot_is_listed_and_fetched_but_never_fills_a_gap`, `a_compacted_file_left_in_deep_is_listed_but_not_fetched`, `a_file_missing_at_or_below_a_carried_end_fails_the_plan`, `the_plan_names_every_copied_file_and_never_one_without_a_copy`, `a_recorded_copy_is_restored_from_after_a_rebuild_clears_its_verified_mark` |
| Q-295 | A restore from a rebuilt manifest whose files are not yet verified again | §8 Generations, §7 Manifest (the owner's ruling at the review of roadmap 8.3a): a restore plan uses a file's recorded copy in deep (`FileEntry::deep`), not the verified mark. A copy recorded but gone from the bucket since is found when it is fetched, by the hash checks and the lost-object rule (Q-82), and reported as `data_lost`: loud. Leaving it out of a claimed generation would be silent and permanent, since later restores read that generation's records only. "Verified in deep" (Q-280) keeps its meaning for the upload queue only: what roadmap 8.1c must check again or upload again after a rebuild. A carried restore record, like a carried copy-verified record, gives its files' copies and no verified mark. Scenario: segments 1 to 3 (`0:1` to `0:30`) are uploaded and verified; the manifest is rebuilt and its records upload whole; the rebuild clears every mark and the carried records set none; a disaster then strikes before the files are checked again. Read as "verified only", a full restore claims generation 1 with restored end `0:0` and lists no file, and the segments are in no record a later restore reads; with the copy, it restores to `0:30`, as from the old records a moment earlier, so the plan is monotone in the records uploaded across a rebuild. Tests: `backup/tests/restore_plan.rs` `a_recorded_copy_is_restored_from_after_a_rebuild_clears_its_verified_mark`, `q108_a_rebuilt_manifest_only_partly_uploaded_is_not_read`, `the_plan_is_monotone_in_the_records_uploaded`; `kvik/tests/restore_plan_model.rs` (rebuilt manifests); `files/src/catalogue.rs` `a_carried_restore_record_gives_copies_but_no_verified_mark` |
| Q-39 (f) | A crash while the restore record is written | §8 Restore, Failures and end (settled by roadmap 8.3b: the restore record reaches `MANIFEST` whole: the manifest's header and the record as its first record are written as `MANIFEST.new`, synced and renamed, and the directory synced; a directory holding only `MANIFEST.new` is a restore that did not reach its record, which is removed, and the restore starts again with the next generation, its claim kept. A torn record therefore never reaches `MANIFEST`. `backup/tests/restore.rs`, `a_restore_killed_at_every_file_operation_completes_after_a_restart`, `a_leftover_manifest_before_the_record_is_removed_and_the_next_generation_claimed`) |
| Q-82 | How a restore tells that the bucket has lost or corrupted a listed file | §8 Restore, Failures and end (settled by roadmap 8.3b with the owner's thresholds: a wrong key is told by the key check value in `STORE`'s envelope header before any file is judged, `RestoreError::WrongKey`, and records nothing; a listed file is lost when `stat` reports it absent on 3 tries spanning at least 10 minutes while `STORE` is reported present in the same step; it is corrupt at once when the reported size or ciphertext SHA-256 differs from its copy, and after 3 downloads with the right key whose bytes are not the copy, whose chunks fail authentication or whose plaintext hash is not its description's (a sink's `HashMismatch` on a download counts as one); any other fault, a `Transient` download included, is retried with backoff and never counts. The thresholds are `RestoreConfig::lost_tries` and `lost_over`; tries are counted per run, so a restart starts them again. A lost or corrupt file becomes a lost range, cause 3 (Q-302), carrying the restore record's reset stamp, the one in force when it is recorded (§7 Readers, `format.md` A.14 field 13). `backup/src/restore/judge.rs`, `backup/tests/restore.rs`, `backup/tests/restore_properties.rs`) |
| Q-213 | The checksum probe's key, which the key contract cannot build | `backup/src/keys.rs` (settled by roadmap 8.3b at its review, by Q-213's suggestion: the checksum probe keeps its own key `<sid>/probe/<nonce:32 hex>`, built by `Keys::checksum_probe` and parsed as `ObjectName::ChecksumProbe`, so a refused bucket's wrong-hash probe never lands on `<sid>/probe` and a later probe cannot mistake it for an earlier one) |
| Q-214 | A restore that meets a bucket it must refuse | `api.md` Backup and restore, Restore (settled by roadmap 8.3b at its review, by Q-214's suggestion: `RestoreDriver::prepare` probes the bucket in a full restore after the key check and `STORE` and before reading the claims, with the probe `format.md` A.17 defines for `attach_backup` (`<sid>/probe` for conditional writes, then a wrong-hash `put` to a fresh `<sid>/probe/<nonce>` that must fail and leave nothing); a failure is the additive `RestoreError::Refused(Refused)` (`NoConditionalWrites`, `NoChecksums`), which `Store::restore` maps to `BackupError::Refused`; a sink fault during the probe stays `RestoreError::Sink`; a drill and a resumed restore do not probe; the probe's bytes come from the restorer's nonce source. `RestoreError::SeveralStores` maps to the additive `BackupError::SeveralStores { store_ids }`. A `stat` that reports no SHA-256 reaches the probe as the sink's `Failed` (the S3 sink refuses such a reply), so it fails the restore as `Sink`, not as `Refused(NoChecksums)`. `backup/src/restore/probe.rs`, `backup/tests/restore_guards.rs`) |
| Q-304 | A manifest record object the bucket has lost or a listing does not show | `format.md` A.17 Uploaded forms, §8 Generations, `api.md` Backup and restore, Restore (settled by roadmap 8.3g: each uploaded record names its predecessor in its manifest (Q-610), a restore checks that chain and the epochs after the last listed and the last read (Q-611, Q-612); the cases these checks do not see are closed by the last-uploaded marker (Q-614). Scenario: a record missing from a listing that shows others left no trace, as epochs may skip, and the plan was made from fewer records with nothing reported. Decided at the review of roadmap 8.3b, and kept: a generation that lists no record while `stat` reports its record at epoch 1 is a lagging listing, `RestoreError::ListingIncomplete`, which `Store::restore` reports as `BackupError::Sink(Transient)`; with none reported the generation has no records (a claim whose restore stopped before its record). `backup/src/restore/driver.rs` `source_records`; `backup/tests/restore_guards.rs`, `backup/tests/restore_chain.rs`) |
| Q-305 | A claim refused with `HashMismatch` while nothing is stored | §8 Generations; `api.md` Backup and restore, No overwrite (settled by roadmap 8.3b at its review. Scenario: a restore's claim `put` for generation g fails with `HashMismatch` and stores nothing; taking g + 1 leaves g unclaimed, and a later drill, which runs one above the highest claimed generation read by `stat` in ascending order (Q-163), stops at the gap, plans from an older generation and passes while ignoring the live one. Ruling: after a `HashMismatch` the restore reads the marker by `stat`; present, it takes the next generation (Q-161: the marker may be another restore's); absent, it tries g again, at most 3 tries of g, then fails with the sink's `HashMismatch` without taking another generation, since a sink that rejects every upload must not walk the generations. `backup/src/restore/driver.rs` `claim`, `backup/tests/restore_guards.rs`) |
| Q-300 | The restore record's bytes ahead of its layout | `format.md` A.14 `0x25` (settled when roadmap 8.3b was stacked on 8.3a, as the question asked: the executor writes and reads the restore record with `RestoreRecord::to_body` and `from_body`, and the provisional codec is gone; `backup/src/restore/codec.rs` keeps only the uploaded form of a record. Scenario: the provisional codec wrote a carried loss as 100 fixed bytes, which 8.3a's strict check of every `0x25` in `Record::encode` refuses, and could not carry a lost range's reset stamp (field 13). Nothing was written with it. `backup/src/uploaded.rs` `a_restore_record_round_trips_through_its_uploaded_form`; the executor tests in `backup/tests/restore.rs`) |
| Q-340 | Objects in the bucket that are not envelopes | `format.md` A.17 (owner's ruling when roadmap 8.3b was stacked on 8.1a's A.17: claim markers and checksum probes are unsealed objects of fixed form whose meaning lies in their key, and the only exceptions to "every object is an envelope"; this amends the Q-160 wording. The probe's content and the change of `restore/probe.rs` from a sealed random probe to it were decided at the stacking, under the ruling. A claim marker is the 12 bytes of A.17, read only by whether its key exists (Q-163); a checksum probe is the 19 ASCII bytes `kvik checksum probe` under a fresh `<sid>/probe/<nonce>`, stored only by a bucket that is refused. Scenario: A.17 said every object is an envelope while the restore executor put claim markers unsealed and sealed a random checksum probe; a sealed probe binds nothing a refused bucket's stray object needs, and a marker's store id and generation are already in its key. Neither holds any of the store's data. `backup/src/restore/probe.rs`; `backup/tests/restore_guards.rs` `a_bucket_without_checksum_checks_is_refused_before_the_claim_and_a_drill_does_not_probe`) |
| Q-271 | Where `format.md` A.17 and the backup interfaces disagree | `format.md` A.17 Keys, Claim marker, Probe (settled as superseded by the owner's ruling: the A.17 merged at roadmap 8.1a already takes the interfaces' forms, (a) other kinds under `files/<kind:2 hex>/` and (b) the 12-byte claim marker (Q-198 (e) and (f)), so `Keys` and `ClaimMarker` stay as they are; part (c), the checksum probe's key, is Q-213, settled by roadmap 8.3b. Scenario: 8.1b was built against an earlier A.17 draft with `data/` and `tree/` prefixes, a 40-byte marker and a checksum probe key `Keys` could not build; the merge of 8.1a and 8.1b ruled the interfaces authoritative before anything was uploaded. `backup/tests/keys.rs`) |
| Q-320 | Which bytes beyond a same-boot mark make a reset point | `spec.md` §3 Recovery rules 1 and 3, Adoption, first start; `format.md` A.4 (settled by the owner's ruling Q-125, with or without a readable mark: the bytes after the header of an unused segment beyond the mark's segment are reported in a recovery entry's tail before the file is deleted; valid seal blocks are not) |
| Q-321 | The lowest unsealed segment damaged, zeroed or missing, with no sealed segment below it | `spec.md` §3 Recovery rule 2; `format.md` A.4 (settled after the review of Q-320: with no sealed segment the chain of generation 0 starts at position 1, so the gap below the first segment kept is a recorded `data_lost`) |
| Q-85 | A loss in the bucket found outside a restore | §8 Self-repair (Lost in the bucket), Corrupt block, No deletion, invariant 10, §11 Health (settled at roadmap 8.5a by the owner's ruling). Scenario: fill pressure moved a sealed segment off the local tiers once verified in deep; the scheduled check then finds the object gone, and no kept input can compact it again. Ruling: the object is judged by the owner's lookup rule, lost after 3 failed lookups spanning at least 10 minutes while the bucket answers (transient faults never count, Q-82); with a verified local copy it is uploaded again and is no loss; with none, the positions no other file holds are a lost range of cause 3, lost in the bucket, a `data_lost` that degrades health until closed (Q-124 (a)) and goes to the re-send stage; every case is a corruption, named by a quarantine record (`0x26` reason 4), and alerts until seen. An object that answers again while its loss is open restores it in full; once closed it stays closed (Q-246). `Model::lose_object`, `Model::check_bucket`; `tests/self_repair.rs` `s8_self_repair_lost_object_needs_three_lookups_over_ten_minutes_while_the_bucket_answers`, `s8_self_repair_a_lost_object_with_a_local_copy_is_uploaded_again`, `s8_self_repair_an_object_that_answers_again_restores_its_open_loss_in_full` |
| Q-88 | The grammar of name patterns | `spec.md` §4 Funnel, Routing; `api.md` Errors, Consuming; `format.md` A.6 fields 55 and 62 (settled at roadmap 5.6a as a pre-release narrowing. Scenario: §4 selects series by name patterns and refuses a routing change that closes a cycle through "two patterns that one name could match", which must be decidable from the two patterns alone, yet no document defined a pattern; the provisional backends let `*` match any run of characters within a segment (`plant/line-*/power`), which needs an intersection test per segment. Ruling, as given: a pattern has exactly the three segments of a name, each a literal or `*`, which matches one whole segment; a `*` inside a segment is refused when routes are compiled (`RouteError::PartialWildcard`); two patterns overlap when, segment by segment, they are equal or either is `*`; an id overlaps a pattern when any name of its series matches; correct the Q-78 example. Reason: overlap is decidable per segment, and the grammar is a subset of every wider one, so widening it later breaks nobody. Q-78's example corrected: `plant/*/power` matches `plant/line-1/power`, not `plant/line-1/power-smoothed`, so a consumer that selects the first and emits `plant/*/power-smoothed` closes no cycle. Decided at 5.6a, not part of the ruling, for the gate to confirm (Q-670): (1) a pattern is also refused, in a funnel and in `emits`, with `RouteError::Segments`, `EmptySegment` or `TooLong` (over the 255 bytes of `format.md` A.6 field 55), checked before `PartialWildcard`: "exactly three segments" and the field leave no other reading, and both backends then refuse the same patterns; (2) `*` matches an empty name segment, since a name a log holds from before the three-segment rule may have one (Q-670 (a)) and it is a whole segment; (3) there is no escape, so a name segment holding `*` is matched only by `*`: an escape would widen the grammar now; (4) a pattern a log already holds that the grammar refuses is read as built, each segment a literal, and refused when routed again (Q-670 (b), confirmed by the owner's ruling): no entry is rejected or rewritten, and the application learns of it. `kvik/src/pattern.rs` and its unit tests; `kvik/src/durable/consumers.rs` tests; `kvik/tests/patterns.rs`, the overlap property against a brute-force oracle and the differential of the two backends) |
| Q-110 | The residual cost of resynchronising over nested batch images | `format.md` common rules (Scan bound), A.2, A.14; `design.md` Budgets; `SECURITY.md` (ruled by the owner: kvik defends against crafted files by bounding scan work per file, rather than placing a file crafted by a writer of the store outside the restart budget. Scenario: resynchronisation candidates whose framing and `batch_crc32c` hold can nest one per 80 bytes, and each was checksummed in full, about 40 n² bytes for n images in one 8 MiB window; boundary reads along a chain of blocks whose bodies are batch images walked the rest of the chain at every kept batch; the manifest's forward search checksummed each record image. Implemented by roadmap 4.9d: one prefix-CRC pass, a body checksum derived from two prefix values by a CRC shift, each block judged once by the boundary reads, each chain of block headers walked once per scan and never more than a batch beyond its candidate (so the block starts remembered span one batch), the tail's readable commit blocks found alongside the candidates and only below the last non-zero byte (so the table is asked in rising order), the same acceptance rule; the reference scan kept as test support checksums directly, walks every chain to its end afresh in each region and reads every boundary in full, as before. `log/src/scan/tests.rs` `nested_batch_images_checksum_at_most_twice_the_window`, `boundary_reads_along_one_chain_judge_each_block_once`, `resynchronising_in_many_regions_walks_a_shared_chain_once`, `walked_chains_longer_than_a_batch_remember_at_most_a_batch_of_block_starts`, `a_torn_tail_with_readable_commits_checksums_each_byte_once`; `log/src/manifest.rs` `the_forward_search_over_nested_records_checksums_linear_bytes`; `log/tests/scan_bound.rs`; `files/src/image.rs` `nested_records_checksum_at_most_twice_the_file`; the segment and manifest fuzz targets assert the bound. The body checks are Q-230, the cost on undamaged segments Q-231. Fix round after the review, by the owner's ruling applying this directive: checksummed bytes were bounded but time was not, as each failing candidate had its bodies decoded in full; a crafted 8 MiB window of 42,000 nested facts images sharing columns of 65,536 rows took 304 s, a 10 MB manifest of 20,000 records sharing one field list 148 s. The fix round allowed at most K = 8 failed reads (a candidate read in full, at least one body checked, rejected: a resynchronisation candidate, a boundary read keeping no batch, a manifest record whose fields are rejected) per window of file offsets, 8 MiB in a segment and 16 MiB in the manifest; past K no other candidate in the window is read in full, and the rest of the window is passed as damaged and reported as lost with the invalid region, corrupt record or torn tail it falls in, so self-repair can restore it from a copy. The reason given for K, that damage gives a handful of failed reads at most, did not hold (re-check below). The reference scan keeps no cap. `log/tests/crafted_files.rs` (the review's probes, with a smoke ceiling on time and the body bytes failed reads check per window); `files/src/image.rs` `a_crafted_search_passes_the_rest_of_a_window_once_failed_reads_use_its_budget`, `a_full_crafted_window_stays_within_the_budget_and_a_smoke_ceiling`; `ScanStats` counts body bytes checked and the most charged to failed reads in a window, and `within_bound`, which the fuzz targets assert, checks both. A search that passes a window and finds no later record is Q-650. Re-check, by the lead's decision applying the owner's directive: a count of failed reads is reached by ordinary damage, as each damaged batch of two or more blocks is one failed read; nine bit flips or zeroed sectors in one 8 MiB window made the bounded scan drop 1.1 to 2.9 MB of healthy batches the reference scan keeps, which breaks the condition that damage is never affected. The count is replaced by a byte budget: once the failed reads starting in a window have checked 16 MiB of body (`FAILED_READ_BUDGET`), the rest of the window is passed as damaged and reported as before. 16 MiB because failed reads caused by damage check disjoint bodies, each inside its own batch, so those starting in one 8 MiB segment window check less than the window and one batch beyond it; crafted nesting reaches it, as its bodies share bytes. Older complete batches left at their offsets (Q-87, Q-117) fail only on their sequence and are disjoint too: `log/tests/scan_bound.rs` `stale_complete_batches_ahead_of_kept_data_stay_under_the_budget` (about 8 MB charged over a 15 MiB run). Through the writer they can only lie beyond the write point, as a truncation removes them and new batches overwrite them in order, so none precedes kept data. A manifest record needs a valid checksum to be a failed read, so damage charges nothing there. `log/tests/scan_bound.rs` `the_bounded_scan_agrees_with_the_reference_scan_under_disjoint_damage_in_one_window` (1 to 64 disjoint bit flips, zeroed 512-byte sectors or 4 KiB pages in one window of engine-shaped batches; it fails under the count of eight); `ScanStats::failed_bytes_peak` replaces the count. The worst case and the 1 s budget are Q-230 and Q-651) |
| Q-200 | Corruption found while the store is open | §3 Recovery rule 2 (a region below the mark found by a read of the writer's process or of a reader process is reported by that process from then on, not yet recorded, and recorded under the same identity by the next start; once the writer's process has found it, its facts are in no key's history, so a replay is recorded again), §11 Health, `api.md` Health (`recorded` false until then). Scenario: three acknowledged batches; the second is damaged on disk while the store is open; readers returned two facts, `losses()` was empty, health was `Ok`, and a replay of the lost reading was acknowledged `Duplicate` against the writer's in-memory history, so the adapter could not repair it until a restart (`kvik/tests/durable_losses.rs` `a_batch_damaged_while_the_store_is_open_is_reported_at_once_and_its_replay_recorded`). Ruling (review of the durable backend): a region found that overlaps a loss already found or recorded is that loss grown, under its first identity (§3 Order of a start step 4); the grown loss is `recorded` only while a record covers its whole range, and an acknowledgement covers only the range the loss had when it was given (§8 Acknowledging a loss), so growth degrades health again. A process also reads again bytes it has read when the newest batch it has read no longer reads the same (the reader voids back to the last batch that does); a loss it found stays found across that void, since lost positions are never reused. Scenarios: (i) ONE, TWO, THREE; THREE damaged and found, then acknowledged; FOUR written and damaged: FOUR was swallowed by the found loss of the same identity, its replay was `Duplicate` and health stayed `Ok` (`damage_next_to_a_found_loss_extends_it_and_needs_a_new_acknowledgement`; with a restart between, `damage_next_to_a_recorded_loss_grows_it_unrecorded_until_the_next_start`); (ii) TWO damaged and found by both processes, then THREE, which both had read, damaged: the reader process voided THREE with the loss and did not report the region again, so it showed one fact, no loss and health `Ok`, and the replay of THREE was `Duplicate` (`damage_to_a_batch_read_after_a_found_loss_extends_it_in_every_process`). Accepted limits: damage to a batch the process has already read is not found by `losses()` or `health()` unless it is the newest; it is found when a query of the process reads it; until then a replay of a fact still in the recent window is dropped, and one outside it is recorded unchecked (Q-750); other readers and the next start find it (Q-928; `damage_to_the_kept_log_is_found_by_the_stores_first_read_of_it`, `a_replay_of_a_fact_of_the_damaged_kept_log_is_dropped_while_its_key_is_in_the_window`); a lost range holding a definition leaves the writer's registry ahead of its readers until the next start, as Q-33 describes for a start; the identity a start records for a found loss that grew downward is open (Q-203) |
| Q-201 | Loss reports that cannot be read | §11 Health (they degrade health: `losses_unknown`), `api.md` Health (`Reason::LossesUnknown`; `losses()` keeps the signature accepted at gate 3.3 and returns the reports the process already holds). Scenario: the mark file of an open store becomes unreadable; `losses()` turned the reader's error into an empty list and health said `Ok`, although a loss could not be ruled out (`kvik/tests/durable_losses.rs` `unreadable_loss_reports_degrade_health`, `losses_keep_the_grown_range_the_process_holds_when_the_reports_cannot_be_read`) |
| Q-230 | Body checks of checksum-valid bodies that nest | `format.md` common rules (Scan bound), A.2, A.5, A.6, A.14; `design.md` Budgets; `SECURITY.md` (settled by the owner's ruling after roadmap 4.9d: the per-window cost is bounded by a constant fixed by the format, so the total is accepted for v1 and the exact bound stated instead of "linear whatever its bytes". Scenario: a block whose checksums hold still has its body checked by type, and a writer of the file can nest such bodies, n facts blocks a few dozen bytes apart each running to its own commit block with a valid `batch_crc32c`, so the decoding grows with the square of n within one window. Bound: each body is checked a bounded number of times per candidate; a resynchronisation candidate is read in full only at the offset its commit block names, so at most one per commit block, its bodies within w = 8 MiB; a manifest forward-search candidate is one per 8-byte step, its body within w = 16 MiB; so at most about L/8 × w for a file of L bytes. Reaching it needs the checksums right, so only a writer of the file can. Superseded by the budget of Q-110's fix round, as the measured figure was about 40 minutes per 64 MiB segment: body checks now total at most 2L, the direct read that found damage, and per window 16 MiB of failed reads plus the read that reached it (two batches), that is about 390 MiB of bodies for a 64 MiB segment and 100 MiB for a 16 MiB one; at least 20 bytes per facts row and about 100 ns per row at most (a 255-byte text value, release build) give about 2 s and 0.6 s. A manifest window holds its valid records once and 16 MiB of failed reads plus one body: at most about 0.25 s per 16 MiB for the start's reader and 0.45 s for the whole-file reader, measured at 0.16 s and 0.28 s on a crafted 16.5 MB window (load about 4 on a shared 4-core machine). The review's crafted segment construction measured 0.12 s per 16 MiB. Above the 1 s budget at 64 MiB: Q-651) |
| Q-231 | The bounded scan's cost on undamaged segments | `format.md` common rules (Scan bound); `design.md` Budgets (settled by the owner's ruling after roadmap 4.9d: undamaged segments take a cheap path, the bounded path runs only once damage is found, and a crafted file must not be able to force the slow path repeatedly. Scenario: on a full, undamaged 64 MiB segment in a release build the bounded scan took 170 ms with 280,000 small batches and 76 ms with 15,000 large ones, against 56 ms and 29 ms for the reference scan, with restarts measured at 0.39–0.74 s against the 1 s budget. Implemented: a scan reads each batch at a boundary once with direct checksums until the first boundary read that finds no complete batch; from there to the end of that scan it uses the prefix table and the remembered verdicts. The switch is one way, so a crafted file costs at most one failed direct read per scan (at most a batch) before the bounded path, and the bytes checksummed stay within 2L plus that read. After: 52 ms and 32 ms on the same segments, equal to the reference. `log/src/scan/tests.rs` `an_undamaged_segment_is_read_in_one_pass_of_direct_reads`; the crafted-file tests of Q-110 and the differential property are unchanged) |
| Q-240 | An alert on every repaired corruption, and when it ends | §8 Self-repair, Attention; §11 Health; `api.md` `attention`, `acknowledge_corruptions` (settled by roadmap 8.5a under the owner's ruling that an alert is raised on every repaired corruption; how long it lasts was decided there). Scenario: a hot segment is found damaged and swapped for its copy from deep within seconds; health never leaves `Ok`, and a person who looks once a day would never learn that the disk corrupts data, if the alert ended with the repair or at a restart. Ruling: each corruption alerts from its finding until a manifest record names it as seen (`acknowledge_corruptions`, `acknowledge_losses` naming the loss it caused, `acknowledge_manifest_rebuild` for a manifest record); a restart, time, a read of health and the end of the repair never clear it, and a corruption's alert never degrades health, so that a repaired store is `Ok` and still asks for a person (repeated corruption does, Q-247). A truncated tail and a `possible_loss` are not corruptions. Applied again at the review of roadmap 8.5a, by the owner's ruling that every repaired corruption alerts: a hot segment swapped from deep and seen was damaged again a week later and swapped again, with no alert and no count, because a corruption was named by its file and a second quarantine record for the file was not written. Each finding is now a corruption of its own, named by its quarantine record's epoch (`format.md` A.14 corruption seen, 24 bytes), so a second finding alerts and counts. `tests/self_repair.rs` `s8_self_repair_alerts_on_a_repaired_corruption_until_it_is_seen`, `s8_self_repair_a_second_corruption_of_the_same_file_alerts_and_counts`, `s8_self_repair_a_start_records_a_second_finding_made_while_stopped` |
| Q-241 | What a restore carries of closing outcomes and corruptions | §8 Self-repair, Generations; `format.md` A.14 `0x25` fields 11 to 13 (settled at roadmap 8.5a by the owner's ruling). Scenario: a loss closed as "re-send ended" is carried in field 7 with identity, first and last only, and would read back as closed by an operator; quarantine records and "seen" marks are not carried, so a seen corruption would alert again and an unseen file-named one would stop. Ruling: a restore carries the repair outcomes and the corruptions not yet seen, so no alert is lost: field 11 the outcome of each carried range closed otherwise than by an operator, field 12 each corruption seen, field 13 each corruption not yet seen. `files/` decodes `0x25` strictly and refuses them until roadmap 8.5b decodes them, with `0x24` fields 3 to 5 and `0x26` (one item owns all three, from the review of roadmap 8.5a); roadmap 8.5c writes them; the model's restore carries no losses (Q-294) |
| Q-242 | How the engine knows that nothing listens for re-send requests | §8 Self-repair stage 2; `api.md` `repair.resend_wait` (settled by roadmap 8.5a). Scenario: a start records a lost range before the application has called `repairs()`; a stream opened later still receives every open request, so the engine cannot tell an application that never listens from one that has not subscribed yet. Ruling: the application says it by `repair.resend_wait` 0, which ends stage 2 at once; otherwise the stage ends at `repair_done` or when the wait passes. `tests/self_repair.rs` `s8_self_repair_closes_at_once_when_nothing_listens` |
| Q-244 | Corruptions after a manifest rebuild: alerts and the repeated-corruption count | §8 Self-repair, Attention, §7 Manifest; `format.md` A.14 (settled at the review of roadmap 8.5a by the owner's ruling, taking the reviewer's rule over the recommendation of a new `0x28` field). Scenario: a rebuild never reconstructs `0x26`; a corrupt span can hold quarantine records and seen marks, so after a rebuild an unseen corruption could lose its alert and the count of `repair.suspect_after` could fall. A new rebuild field listing the corruptions it "could not reconstruct" cannot be filled: valid `0x26` records are carried unchanged with field 100, and the only ones missing lie in the unreadable span. Ruling, with no new field: after a rebuild the corruptions are (a) those the carried quarantine and lost-range records name, as before; (b) the corrupt manifest record itself, named and dated by the rebuild record, so one lost `0x26` is offset by one gained; (c) each file under `quarantine/` that no carried record names, dated at the rebuild record, with no fresh `0x26`; (d) each lost range found again with cause 5 (unknown), dated at the rebuild record. Each alerts until seen. The count errs high for what can be seen, and is low only by corruptions already repaired and seen whose records lay in the same corrupt span, less one; that bound is accepted. The model has no manifest rebuild; roadmap 8.5c builds it |
| Q-245 | Delivery and queries while a quarantined file waits for its copy | §8 Self-repair stage 1, Corrupt block; §4 Resync (settled by roadmap 8.5a). Scenario: a consumer catching up reaches a sealed segment that was just quarantined and whose copy is being fetched from deep. Sending a `resync` there would make a reset point where nothing is lost; skipping the segment would break invariant 2. Ruling: the catch-up waits for the swap, its time behind measured; a query over the file returns the Corrupt block error until the swap, and while deep cannot be reached the store waits with a backup reason in health. Nothing is recorded, so no reset point arises. Two more at the review of roadmap 8.5a. (a) With the hot copy damaged and the object reported absent once, a scrub recorded the lost range at once, a reset point and a `resync` for every consumer on what may be a lagging listing: while deep answers but reports the object absent, stage 1 waits for the lost-object rule (Q-82) before recording. (b) A copy swapped in while the engine was stopped wrote no record, so the next start recorded the range found and hid intact facts: a swap is a manifest record (Corrupt block), so it is refused while the engine cannot write. `tests/self_repair.rs` `s8_self_repair_stage_1_waits_for_the_lost_object_rule_before_recording`, `s8_self_repair_no_copy_is_swapped_in_while_the_store_cannot_write` |
| Q-246 | A copy found after its loss was closed | §8 Self-repair stage 3 (settled by roadmap 8.5a). Scenario: a segment lost in the bucket is closed as "re-send ended" after 24 h; the sources re-sent what they still had, at new positions. On day 3 a lagging listing shows the object again. Swapping it in would put facts back at positions every consumer was told to forget, beside their re-sent copies at newer positions, and change history the application has already reconciled. Ruling: a closed loss is not opened again by a copy found later; the closing record stands and the list of losses keeps it. A loss whose range grows is open again, at stage 2. Added at the review of roadmap 8.5a: an object lost with no local copy was restored in full when it answered again, then lost again a week later; the old lost-range record still stood, so the second loss was filtered out and the store reported `Ok` with the facts gone. Rule: a loss closed as restored in full whose positions are lost again is open again under the same identity, at stage 2, by a new lost-range record; a closing record "restored in full" closes only the lost-range records written before it (`format.md` A.14 `0x24`), and the new finding is a new corruption. Also added there: a hot segment damaged while its object was lost, its loss closed after 24 h, then the object answered again; the swap was refused but the quarantined file still counted as open, so health stayed fatal for ever. A quarantined file holds an error, fatal in health, only while a swap can still happen: not once its positions hold a closed loss, nor once its object is judged lost. `tests/self_repair.rs` `s8_self_repair_a_closed_loss_stays_closed_when_its_copy_appears`, `s8_self_repair_a_loss_restored_in_full_and_lost_again_is_open_again`, `s8_self_repair_a_closed_loss_is_not_restored_from_a_copy_found_later`, `s8_self_repair_a_loss_restored_from_deep_and_damaged_again_with_deep_gone_is_open_again`, `s8_self_repair_a_copy_swapped_in_after_its_object_was_judged_lost_is_watched_again`, `s8_self_repair_adjacent_segments_found_while_stopped_keep_a_loss_each` |
| Q-247 | The default of `repair.suspect_after`, and what repeated corruption does | §8 Self-repair, Attention; §11 Health; `api.md` Configuration, `Reason::StorageSuspect` (settled at roadmap 8.5a by the owner's ruling). Scenario: every corruption alerts until seen, so a person who sees each one may not notice that the storage keeps corrupting data. Ruling: 2 corruptions in 30 days by default, configurable (`repair.suspect_after`); while the count of corruptions in the window, seen or not, is at or above it, health is degraded with the reason `storage_suspect` (`Reason::StorageSuspect`), as well as the alert, so failing storage stays visible; it ends when the window passes the older corruptions. The count is of findings, so one failing disk damaging the same file twice in 30 days makes the storage suspect (confirmed by the owner at the review of roadmap 8.5a). It is read from the manifest's records (quarantine records and the losses `losses()` reports, each once under its identity), never from a running counter, so a restart or a record that extends a loss counts nothing again (`s8_self_repair_corruptions_are_read_from_records_so_a_restart_or_growth_counts_once`). `tests/self_repair.rs` `s8_self_repair_repeated_corruption_makes_the_storage_suspect`, `s8_self_repair_a_second_corruption_of_the_same_file_alerts_and_counts` |
| Q-248 | `acknowledge_losses` on a loss whose copy is being brought back | §8 Self-repair, Acknowledging a loss (settled at roadmap 8.5a by the owner's ruling, as the model had it). Scenario: an operator acknowledges a recorded loss in stage 1; closed by an operator, Q-246 would keep its copy from being swapped in and the quarantined file would hold fatal health for ever. Ruling: `acknowledge_losses` cannot close a loss while its repair copy is in progress; it writes no closing record for it (it still names its corruption seen), and the loss closes as restored in full when the copy is swapped in. `tests/self_repair.rs` `s8_self_repair_an_operator_does_not_close_a_loss_whose_copy_is_coming`; `tests/recovery_histories.rs` (the acknowledgement step) |
| Q-614 | A restore that reads an earlier state with nothing found | §8 Generations, Restore drill; `format.md` A.17 Last-uploaded marker; `api.md` Backup and restore, Restore (settled by the owner's ruling of 2026-11-17: "a store-side 'last uploaded epoch' marker object, written after each record; a restore that cannot reach or match it fails loudly"; its review's fix round as below). Scenario: (a) the bucket loses a generation's last record; (b) a lagging listing hides the last record behind more than `TAIL_PROBES` skipped epochs; (c) the bucket loses, or such a listing hides, the last old record before an unfinished rebuilt manifest, whose rebuild record names nothing (Q-610). In each a drill passed and a full restore claimed a generation over an earlier state. Decision: (a) Form. Objects are never overwritten, so the marker is a series, `<sid>/uploaded/<generation>/<sequence>`, sequences from 1 with no gap, each an envelope of a 60-byte plaintext (`format.md` A.17): magic, version 1, generation, sequence, `epoch` (the last record uploaded), `open` (the first record of the latest manifest uploaded), the first and last record a restore of the records uploaded reads, CRC32C. Magic and version come first and keep their offsets, so a later version of any length is a version refusal. The read range is what covers (c): with a rebuilt manifest not yet whole a restore reads the old records, and the marker names where they end. (b) Writing. After each record is put and before any later object, never before: the upload driver's next step, and a full restore after its restore record (sequence 1), again when resumed. A put that meets another marker's content takes the next sequence. At its first step the upload driver finds the latest marker (listing, then `stat` past it, doubling then halving: no gap, so a lagging listing hides none), fetches the records listed above it and those `stat` reports within `TAIL_PROBES` epochs after the highest it knows (so a record put just before a restart and hidden by the listing is counted, as a restore would find it), checks that their uploaded forms chain from the marker's epoch (a break whose predecessor `stat` reports is a lagging listing, and the step is retried rather than a marker built from it), and puts a marker at once when they are ahead: a restart between a record and its marker, an unreadable latest marker where the one before names less, a bucket from before markers. The marker it starts from is the latest readable one, found as a restore finds it: those that do not open, decode or match their key, or that the bucket no longer holds, are passed over for the one before (Q-1030); with none readable it starts from none and makes the marker from every record found. A marker of a version it does not read, met on the way down, stops its uploads (`UploadError::MarkerVersion`): an earlier version is never put after a later one that opens. (c) Checking. A restore and a drill read the generation's latest marker before listing its records, so what it names was uploaded before the listing began. A marker that does not open, decode or match its key is passed over for the one before it, and the restore reports it (`Prepared::marker_fallback`); no readable marker is `MarkerUnreadable`, and so is a latest readable one of a later version, with no fallback past it. The listing must show `epoch`, and the records read must start and end no earlier than the marked range (records uploaded after the marker only move it later). An epoch it names that is not listed is asked by `stat`: reported, a lagging listing (`ListingIncomplete`); not, `MarkerUnreached`, unless the operator accepts an earlier state (Q-962). Both ends listed and the records read still earlier: `MarkerMismatch`. Records and no marker: `MarkerMissing`, unless `RestoreConfig::accept_unmarked` (off by default) restores the generation without the check; a marker present is always checked. A generation that lists no records and has a marker is not skipped. Chain breaks are judged before the read range, so a lost first record stays `BrokenChain`; the `stat` of the marked range's first epoch cannot be reached once the chain passed, and is kept as a defence. (d) Lagging listing. One driver counts consecutive `prepare` calls that find the marker ahead of the listing; the tenth (`MARKER_LISTING_TRIES`) fails as `MarkerNotListed`, so a drill of a live store on a bucket whose listings lag longer than its uploads fails loudly instead of retrying for ever (run it from a paused copy, Q-612 (d)); any other outcome starts the count again. The bound counts calls, so the caller paces them (Q-961). (e) Older buckets. Failing by default strands few: a store still running puts a marker at its upload driver's next start; buckets from before 8.3g's uploaded form are unreadable anyway (Q-960); what needs `accept_unmarked` is a bucket of a store that is gone, written between 8.3g and markers, a store gone between its generation's first record and that record's marker, and a full restore that stopped between its record and its marker and is not resumed (resuming puts the marker). A drill that runs between a generation's first record and its marker fails as `MarkerMissing` and passes when tried again, as the message says. (f) Residual, silent, each needing two faults at once: a record put and not yet marked when the store dies, then lost, or hidden behind more than `TAIL_PROBES` skipped epochs (inherent in "after each record"; the upload driver's tail probe closes the same window at a restart only within that bound); a lost latest marker together with a lost last record (the marker before it names less); a marker put acknowledged and not kept (markers are put with a plain `put`, not verified as records are, Q-284) under a lagging marker listing, where the probing stops at the gap; a second writer of one generation with an older cursor putting a marker that names less (one writer per generation is the rule); every marker of the generation unreadable together with a lost record (a restore fails as `MarkerUnreadable`, then the upload driver, finding no readable marker, puts one from the records left, which names less); a later-version marker damaged so that its envelope does not open, passed over as a damaged version 1 marker would be (Q-1030). A damaged latest marker with a record lost that the marker before it names stays loud: the upload driver starts from that marker, as the restore does, and keeps naming the lost record. (g) Owner: what "fails loudly" costs. For a store that is gone, one lost last record refuses every restore and drill as `MarkerUnreached` for good, including what the records before it hold, and `accept_unmarked` does not apply; a damaged latest marker no longer refuses (the fallback), but a store still running heals a lost record only by uploading after it. What an operator may accept then is Q-962. Cost: one more object per record; a driver start asks `stat` for 8 epochs more; a restore lists the markers and asks `stat` for one or two more (`stat` alone would do in about 2 log2 N calls, worth it if generations grow long); the catch-up on a bucket with no marker fetches every record of the generation in one step, from the first again after a transient fault, once. Applying the ruling recorded Q-960, Q-961 and Q-962. `backup/src/marker.rs` (format vector, refusals, summary); `backup/tests/marker.rs` (properties: the summary against `records_to_read`, round trip, decoding, the latest found whatever the listing hides and however it pages, 2,500 markers across pages of 1,000); `backup/tests/restore_marker.rs` (each scenario, a marker behind the records, ahead with retries, at the bound and after a success, missing and accepted, the fallback and its limits, a later version, mismatched, an earlier state accepted, the restore's own marker, also when resumed); `backup/tests/upload_marker.rs` (after each record, a restart between record and marker, with a rebuild and the record hidden, a catch-up whose listing hides a record, a bucket with no marker, an unreadable latest marker, a catch-up from the readable marker below it, also past one the bucket no longer holds, that keeps naming a lost record, a transient failure reading the latest marker retried, a later version, also below an unreadable latest one, a taken sequence); the marker invariants in `upload_support::check_done` and the upload histories; `restore_chain_properties.rs` now also loses and hides the last record and the one before an opening rebuild record) |
| Q-960 | No kept bucket holds the record form from before roadmap 8.3g | `format.md` A.17 Uploaded forms, Last-uploaded marker (settled by the owner's ruling of 2026-11-18: confirmed, no bucket to be kept holds records of the form from before roadmap 8.3g; was Q-614 (f)). Scenario: roadmap 8.3g gave the uploaded record form 8 leading bytes (`previous`, Q-610) with no version marker, so an object of the earlier form fails to decode, at a restore and at an upgraded store's first upload step, with a message that does not say why. Decided (engineering): the last-uploaded marker (Q-614) is versioned and its version 1 stands for the uploaded forms in force; a later version is refused, never overwritten by an earlier build (`UploadError::MarkerVersion`, `MarkerUnreadable`); a generation with records and no marker fails as `MarkerMissing`, whose text names a bucket written before markers. The record form keeps no version field. Not modelled: the model uploads no manifest records |
| Q-962 | What an operator may accept when a marker names records the bucket does not hold | §8 Generations; `api.md` Backup and restore, Restore (settled by the owner's ruling of 2026-11-18: accepted as built, off by default). Scenario: the store is gone and the bucket has lost the last record of the generation read (or the last old record before an unfinished rebuilt manifest). The marker names it, so every restore and drill fails as `MarkerUnreached` for good, including everything the records before it hold; `accept_unmarked` does not apply, since a marker present is always checked. Before markers the same bucket restored the earlier state with nothing said (Q-614 (a), (c)). Ruling: `RestoreConfig::accept_earlier_state`, off by default: the restore then reads what the bucket holds, as far as the chain reaches, and names in `Prepared::earlier_state` the marker, each record it names that the bucket does not hold, the last record read and the last the marker names (the epochs between are not restored). It never covers a lagging listing (still retried, also one hiding the record behind more than `TAIL_PROBES` skipped epochs), a chain break (Q-611 (g), still refused) or a mismatch. `Store::restore` exposes it with roadmap 8.3d (Q-961). `backup/tests/restore_marker.rs` (`q962_*`) |
| Q-1030 | How far a reader scans down past unreadable last-uploaded markers, and a damaged later-version marker | `format.md` A.17 Last-uploaded marker (decided by the lead at the Q-614 follow-ups). Scenario: a restore, and now the upload driver's first step, pass over markers that do not open, decode or match their key for the one before. A marker a later build wrote and the bucket then damaged so that its envelope does not open cannot be told from a damaged version 1 marker, since the version is inside the envelope, so it is passed over: a restore may then check an earlier version 1 marker, and the upload driver may put a version 1 marker after it. Decision: the scan down has no bound, and the limit is stated, not closed. N unreadable markers in a row cost N `get` of a 60-byte plaintext in its envelope before a readable one, a later version or the first sequence; each needs its own fault, so N stays small (a reader holding another key fails on the store descriptor before it reaches the scan, at an upload start and at a restore's `check_key`, so a wrong key does not cost N `get`), and a bound would turn a bucket whose earlier markers are good into a refusal for no gain in safety. Closing the later-version case would need the version outside the envelope, a format change for a case needing a later build, a rollback and damage together. `backup/src/restore/source.rs` `latest_marker`, `backup/src/upload/mark.rs` `latest_readable` |
| Q-260 | A restored generation's first mark and the first start in it | §8 Generations, Restore step 1; §3 Order of a start (settled by the owner's acceptance of roadmap 8.3c's decision, matching the model: the first mark `init_generation` writes is read as no mark by the generation's first start, which writes no recovery entry and takes ingest at the first position; refined by the owner's ruling that Q-125 prevails: only that boot-less mark is read so, and the mark the first start writes is read as any mark; see the question) |
| Q-262 | The writer's scans in a restored generation: set-aside batches and the id floor | §8 Restore step 1, §3 Recovery rule 3, §2.1 (settled by the owner's acceptance of roadmap 8.3c's decision: set-aside batches reserve ids and give no consumer or interval; the three changes in generation 0 are listed in the question) |
| Q-264 | Position 1 as the chain's start in a restored generation | §3 Recovery rule 2, §8 Generations (settled by the owner's ruling: with no sealed segment of its own, a generation's first position anchors the log start, so a lowest segment damaged, zeroed or missing is `data_lost` from it; see the question) |
| Q-266 | A full restore retried after a failure past its claim | §8 Generations, Restore; `api.md` Backup and restore (settled by the owner's ruling for v1: in-memory claim reuse on the same driver; a crash burns a generation, which is safe; a durable note is a follow-up; see the question) |
| Q-600 | A lost object that answers again after its loss was closed | §8 Self-repair stage 3; `format.md` A.14 `0x26`, `0x27` (settled after the review of roadmap 8.5a by the owner's ruling, as proposed). Scenario: the scheduled check judges an evicted segment's object lost; with no local copy its positions are a lost range, offered for re-sending and closed as "re-send ended" after 24 h; the sources re-sent what they had, at new positions. On day 3 a lagging listing shows the object again, with its recorded size and hash. A fresh `0x27` would put it back in a restore's plan, so a later restore would bring its facts back at the closed loss's positions, beside their re-sent copies at newer positions. Ruling: no new `0x27`; a closed loss stays closed in every generation. The object stays judged lost and is looked up no more, its copy is never swapped in and holds no error, and its `0x26` reason 4 with no later `0x27` keeps it out of every restore's plan. `tests/self_repair.rs` `s8_self_repair_a_closed_loss_is_not_restored_from_a_copy_found_later`, `s8_self_repair_a_closed_loss_stays_closed_when_its_copy_appears`; the restore side is not yet pinned (see Where the model differs, "A closed loss's object in a restore") |
| Q-612 | The tail check: records after the last listed, found, not found or unanswered | §8 Generations, Restore drill; `api.md` Backup and restore, Restore (decided by roadmap 8.3g; (d) settled by the owner's ruling, accepted as built). Scenario: a listing that hides the newest records, or records uploaded after it, leave nothing for the chain (Q-611) to name: the restore reads an earlier state and, when it is full, claims a generation over it. Decision: (a) right after the listing, before any record is fetched, the restore asks `stat` for the `TAIL_PROBES` (8) epochs after the last listed (`plan::after_last`); after the chain check, where the records read end before the listing (a rebuilt manifest not yet whole follows), for the epochs after the last read and below the next listed, at most 8 (`plan::read_tail`); and where nothing is read, for epoch 1. The bound of 8 lets a hidden record behind skipped epochs (format A.14) still be found. (b) Reported: `ListingIncomplete`, retried. Not reported: the restore goes on. Unanswered (transient or not): the restore fails with the sink's error and never goes on, since going on could read an earlier state. (c) What these checks do not find, the last-uploaded marker does (Q-614). (d) A drill of a live store, or a full restore while the source store still uploads, can see the next record uploaded between its listing and the `stat` (at most eight, right after the listing) and fail as `ListingIncomplete` where it passed before. Ruling: accepted; such a drill is retried, and on a bucket whose listings lag longer than the time between two record uploads it never passes, so there a drill runs from a paused copy, a copy of the store's objects made while the store uploads nothing (spec §8 Restore drill, `api.md` Restore). Each retry of a full restore takes a new generation (Q-615). `backup/tests/restore_chain.rs` (`q612_*`: found, not found and unanswered, after the last listed and after the last read; the `stat` before the first fetch) |
| Q-630 | Acknowledged positions lost with the whole unsealed log when nothing names them | §3 Commit, Durable mark, Recovery rule 2; `format.md` A.4, A.10 (settled by the owner's ruling, option (c): the first batch in each segment is acknowledged only after the mark naming it is synced, so a durable mark names a position in every segment holding acknowledged data and losing it whole is `data_lost` from the anchor; see the question. Merged with roadmap 5.5a, by the lead's decision at the merge: a flagged batch is acknowledged before any sync, so the rule is for a segment's first batch when it is unflagged, and a segment that begins with flagged batches has no anchor until the next mark sync, another thing the `written` level gives up, `spec.md` §3 Commit) |
| Q-640 | A description that never stops failing to open or read its segment | `spec.md` §8 Corrupt block, §11; `api.md` Health (settled by the lead's ruling at the re-check of roadmap 6.3b: `SegmentUnreadable` once the retry wait reaches its 10-minute cap, cleared when a read succeeds; an unlisted segment is never backed up, so a silent indefinite retry is a hidden loss of backup) |
| Q-670 (a) and (b) | Names outside the pattern grammar, and stored patterns it refuses | `spec.md` §2.1 `name`, §4 Funnel; `format.md` A.6 fields 55 and 62; `api.md` Series, Consuming (settled by the owner's ruling after roadmap 5.6a). Scenario: (a) Q-88 holds a pattern to the three segments of a name, but a name was any 1 to 255 bytes, so `plant/power` and `a/b/c/d` could be defined and no pattern selected them, and a name with an empty segment (`a//c`) was reached only through `*`. (b) A log written before 5.6a may hold a funnel or `emits` with a pattern the grammar now refuses, such as `plant/line-*/power`. Ruling: (a) a name has exactly three `/`-separated segments, none empty, at `define_series` and at `alias`, on both backends; another is refused with `DefineError::Invalid { field: "name", .. }`, an existing variant (the reference model's `RegistryError::BadName`, a new variant of that crate's enum, maps to it). A segment may hold `*`, which only `*` matches (Q-88). A log written before the rule that holds a name of another shape opens unchanged: no entry is rejected or rewritten at a start, the series stays readable and writable by id and by that name, defining it again under that name is refused, and an alias of the right shape gives it a name patterns can select. (b) The provisional reading is confirmed: a stored funnel or `emits` with a refused pattern is read as built, each segment a literal and `*` alone a wildcard, stays in force, and is refused when routed or declared again. `model/tests/names.rs`; `kvik/tests/patterns.rs` `a_name_without_three_non_empty_segments_is_refused_on_both_backends` and the differential of the two backends over names of three segments; `kvik/src/durable/registry.rs` `a_logged_name_of_another_shape_opens_and_stays_readable_by_id`; `kvik/src/durable/consumers.rs` `a_stored_funnel_with_a_refused_pattern_is_read_literally_and_refused_when_routed_again` |
| Q-705 | The model backend's `outranked` when a later reading of the same call outranks an earlier one | `kvik/src/backend/mod.rs`, the model's `CommitReport` (settled by the lead, as proposed: the model backend takes `outranked` from the engine's commit-time judgement, as the durable backend and the reference model's engine already decide) |
| Q-706 | An emit's read-back of its batch when no consumer is registered | `api.md` Health and statistics; `kvik/src/durable/consumers.rs`, `deliver/hub.rs` (settled by the lead: with no consumer registered an emit leaves timers and read-back to the `kvik-deliver` thread, and `refresh_errors` and `corruptions` may lag its return; a flag raised before `take_consumer` pumps keeps what a consumer receives unchanged) |
| Q-713 | A `min_sync_interval` beyond `u64` nanoseconds | `log/src/writer/pacing.rs` (settled with the review of the adaptive interval: the interval is held in `u64` nanoseconds, so a ceiling above about 584 years counts as 584 years and the schedule cannot overflow) |
| Q-750 | The duplicate rule's slow path on damage found during a run | `kvik/src/durable/ingest.rs`, `deliver/recent.rs`, `health.rs` (settled by the owner's ruling at the review of roadmap 11.0: recorded and delivered flagged unchecked, counted on their own, health degraded until the next start; ingest never stops; an unrecorded damaged region after it is Q-755) |
| Q-830 | Whether every poll runs every consumer's timers | `kvik/src/durable/deliver/funnels.rs` (decided by the lead: only timers that are due if exactly equivalent; it is not, since a held-for or stale-for evaluation reads the live state at `now` and counts unchecked evaluations and reads of the log, so all are kept; the per-poll work that changed nothing is removed) |
| Q-831 | A segment holding a recovery entry read whole by every lookup | `log/src/reader/lookup.rs` (settled by the lead: the batch map is kept with each entry's place and `after`, applied in file order as a scan applies them; an entry mapped before its batch was damaged keeps voiding for that reader until a start) |
| Q-833 | Recovery taking the last sealed segment's last position from its trailer alone | `log/src/writer/recovery.rs` (declined by the lead: recovery keeps reading the last sealed segment, so damage there is still noticed at a start) |
| Q-921 | Forgetting a range found lost while open, with bounded decision history | `History::forget_shown` empties every window, which then holds no key at or below its series' latest `t` (an explicit floor that can hold nothing, so a series at `i64::MAX` is right, as `keep_budget` and `evict_series` now are), removes every revised-key entry, and keeps of the facts removed only those whose calls the log may not show yet, in the overlay; the bytes held return to zero and the latest `t` stays. Afterwards a key at or below it is decided from the log, which skips the lost regions; in-order ingest decides in memory. No fact in memory carries a position. Built beside the durable fixes' decider, which stays the oracle (`kvik/src/durable/ingest/bounded/differential.rs`). Tests: `kvik/src/durable/history.rs` `forgetting_keeps_only_facts_in_flight_in_the_overlay_and_holds_no_bytes`, `a_series_whose_latest_t_is_the_greatest_time_is_emptied_by_a_forgetting_and_by_the_budget`. Decided by the design that reconciles the durable fixes with bounded decision history |
| Q-922 | The regions a process has reported lost | Kept as a union of byte spans per segment, overlapping and touching spans merged, in the latch beside the reports (`kvik/src/durable/loss/regions.rs`). Scenario: batches A B C D; B and C damaged, D intact: one region from A's end to D's start, one loss under B's identity in every process, and each replay recorded (`kvik/tests/durable_losses.rs` `two_damaged_neighbours_are_one_loss_as_a_reader_process_reports_it_and_each_replay_is_recorded`). Decided by the design that reconciles the durable fixes with bounded decision history |
| Q-923 | Calls acknowledged into a lost range, without positions kept | `Decider::settle(acks, next, fresh)` takes the calls in flight in call order and stops at the first still pending, so the calls seen acknowledged are a prefix and are the ones the writer stops tracking; notes whether any overlaps a range already lost; makes every call below the first pending readable; merges the fresh ranges (overlapping or touching ranges become one); and forgets once if a call or a fresh range called for it. Scenario: calls 5 and 6, 6's batch lost; the poll sees 5 pending and 6 ready: 6 stays tracked, so a replay of its fact waits for it rather than being acknowledged at once (`kvik/src/durable/ingest/bounded/differential.rs` `oracle_acknowledgements_polled_out_of_order_stop_at_the_first_pending`; `kvik/src/durable/ingest/bounded/tests.rs` `a_call_acknowledged_inside_a_range_already_lost_leaves_the_history`) |
| Q-924 | The lateness reference after a forgetting | `Exact(Option<t>)` or `AtMost { upper, least }`: every reference fact the log shows is at or below `upper`, and `least` is the highest reference fact not shown, wherever held (window, revised-key entry or overlay). A reading the two bounds decide reads nothing; otherwise the slow path reads the series' reference facts at or below `upper`, highest part first, parts of the window span doubling, and stops at the first found (the reading is late) or where no lower reference could make it late (`upper` narrows to `t + 2S`). The state becomes exact only when every part read was complete; a fact an incomplete read finds proves lateness and raises `least`. In-order ingest never reads. Tests in `kvik/src/durable/ingest/bounded/tests.rs`: `a_lost_latest_fact_no_longer_holds_the_lateness_reference`, `a_reference_fact_in_the_overlay_at_a_loss_still_holds_the_lateness_reference`, `a_read_that_finds_no_reference_narrows_it_and_a_reading_it_then_decides_reads_nothing`, `a_reference_fact_an_incomplete_read_finds_proves_lateness_and_none_found_is_not_late_and_counted` |
| Q-926 | Where the writer's process keeps its loss reports | The files' reports (the manifest's lost ranges and the kept log's recovery entries) and this process's finds come from the loss watch (Q-927), whose `loss::Finder` holds the recovery entries and the last commits, and no longer from the index of facts; the start's unrecorded losses from the log writer's start report; what every scanning reader of the process found from the latch, which keeps the reports, the ranges for the decisions, the closed ranges (Q-938) and the regions (Q-922); acknowledgements as before. `losses()` merges recorded reports, then the start's, then the watch's and the latch's finds (`kvik/src/durable/status.rs`). Test: `kvik/src/durable/loss/tests.rs` `the_watchs_reports_taken_in_parts_equal_a_full_reads_and_it_keeps_no_segment_bytes` |
| Q-927 | What `losses()` and `health()` read in the writer's process | A log reader of their own, the loss watch, caught up through the kept log at open and moved only by these two calls: each reads everything written since the previous one, one part at a time, and the newest batch it had read again, then releases its segment bytes; a refresh that fails is `LossesUnknown`. Q-200's accepted limit keeps its wording; queries' finds reach the latch from the store's own reader. Scenario: TWO damaged after the store read it; `s.losses()` with no query finds it, as the durable fixes' reader did (`damage_to_a_batch_read_after_a_found_loss_extends_it_in_every_process`) |
| Q-938 | A fact lost after the store's own reader read it | Before the store's own reader answers, it removes from its index the facts at the positions of every closed range the latch took since its last answer (lost positions are never reused). The closed range ends an open-ended region at the next batch the same read returned, after the read's events are all taken, or at the mark read, so Q-937's wide bound removes no intact fact. Scenario: ONE, TWO, THREE; a query; TWO damaged; `s.losses()` finds it; the replay is recorded and the query shows three facts, the replay in TWO's place (`kvik/tests/durable_losses.rs` `a_fact_lost_after_the_stores_reader_read_it_is_shown_once_with_its_replay`; `kvik/src/durable/loss/tests.rs` `an_open_ended_region_is_closed_at_the_next_batch_of_the_part_and_its_report_kept_wide`). With published views (roadmap 9.0b2, Q-902) the round that publishes the next view notes its own finds and forgets every range the latch took since from the index the view is cloned from; a view a call still holds keeps what it was published with, as a cut does (`a_latched_range_leaves_the_next_view_and_a_view_held_since_keeps_it`, `kvik/src/durable/loss/tests.rs`; per read entry point, `kvik/tests/durable_losses_reads.rs`). The writer's series directory follows the same closed ranges (Q-952) |
| Q-940 | A found loss that bridges two held by the latch | The latch keeps its reports ordered by first position, no two overlapping, and finds the ones a new report overlaps by search; a report that overlaps two makes them one loss under the identity of the lower. The merge of `losses()` already gave one report there; the latch now agrees with it. Scenario: losses at 20–22 and 40–41 held; damage between them is found as 23–40: one loss, identity 20 (`kvik/src/durable/loss/tests.rs` `the_latch_holds_one_report_per_loss_in_order_and_a_bridged_pair_becomes_one`) |
| Q-941 | The finder's commit after a void, with a bounded memory | `loss::Finder` keeps the last 65 commits, not one per batch: a rewrite voids at most the 64 batches kvik-log's reader remembers, and a recovery entry's void is followed by its own batch. After a void past what is kept, the region that follows starts after the void's position with no commit time before it, rather than at the log's first position. Test: `kvik/src/durable/loss/tests.rs` `a_region_after_a_void_past_the_commits_kept_starts_after_the_void` |
| Q-942 | The facts a slow-path span read meets before damage | `LocalFacts::read_between` returns the facts it could read and whether the read was complete; `between` keeps its former answer on top of it. The reference read (Q-924) uses an incomplete read's facts. Test: `kvik/src/durable/keys.rs` `a_lookup_that_fails_reports_every_key_unreadable` |
| Q-943 | What makes the decisions forget a range found lost while open | Amends the reconciling design's §2.4 step 3: `Decider::settle` forgets for every fresh range whose find grew the union of regions the latch holds (Q-922), not for a range outside the lost ranges, since a range reported too wide (Q-937) would otherwise hide a later, real loss inside it. A find of damage already found grows nothing, whatever its range, and forgets nothing. Scenario: position 1 damaged and reported lost to the head, 1..3; positions 2 and 3, two versions of a key, intact; a third version enters the revised-key index; position 3 is then damaged and found: the replay of its version is recorded and the version below is outranked by the third, not by the lost one (`kvik/src/durable/ingest/bounded/tests/losses.rs` `a_loss_found_inside_a_range_already_reported_too_wide_forgets_its_facts`; the oracle differential reports ranges wider than the fake log loses, `oracle_a_loss_found_inside_a_range_already_reported_too_wide`) |
| Q-944 | A region met inside a loss a finder already holds | `loss::Finder` keeps the regions it has met and hands on a region met for the first time even when it lies inside a held report, with that report unchanged, so its span and closed range reach the latch; the latch then grows its regions and the store's own reader forgets the closed range (Q-938). Scenario: a sealed segment's last batch is damaged and reported lost to the mark (Q-937); the newest batch is then damaged: its replay is recorded and a query shows it once (`kvik/tests/durable_losses.rs` `damage_inside_a_loss_reported_too_wide_reaches_the_stores_reader_and_its_replay_is_recorded`; `kvik/src/durable/loss/tests.rs` `a_region_inside_a_report_held_too_wide_still_reaches_the_latch_with_its_closed_range`) |
| Q-945 | A key revised in every call and the decision state's byte budget | Bounded decision history kept a revised-key entry whole while any of its facts was in flight, and tried to evict only right after noting a new fact, which is in flight: a key given a new version in every call grew without bound (56 bytes a call, one entry of 10,001 facts after 10,000 calls under a 1,000-byte budget). `History::keep_budget` now evicts, last, revised-key entries with facts in flight: the facts the log shows leave, those in flight stay in the overlay, and the key returns to the slow path, which reads it whole at its next revision. The bytes held are within the budget after every fact noted; the cost is a read of the key from the log per revision while the budget is short. Tests: `kvik/src/durable/history.rs` `a_key_revised_in_every_call_is_evicted_once_its_earlier_facts_are_in_the_log` (in `history/tests.rs`), `kvik/src/durable/ingest/bounded/tests.rs` `a_key_revised_in_every_call_stays_within_a_tiny_budget_and_its_versions_are_still_decided`; the oracle differential asserts the budget after every call Then, before any window key, an entry holding more than a quarter of the budget leaves the same way, so a key revised in every call pays for itself and the other series keep their windows; each of its revisions then reads the key's whole history from the log, held for the call outside the budget (`a_key_revised_in_every_call_beyond_the_budget_leaves_the_other_series_windows_and_reads_only_itself`). Not covered, performance only: five or more keys revised in every call, each with facts in flight and each between a fifth and a quarter of the budget, pass that rule, and the window keys leave first as before (about a million versions a key at the default budget, a few dozen at a small `window_memory`). If it matters: evict the largest entry first while the entries with facts in flight together hold more than the budget |
| Q-929 | `LocalDataUnreadable { regions }` beside the losses a process reports | Derived at `health()`: a region the start passed over counts unless a manifest record holds it or a loss the start found and could not record is in its segment (the start's report gives no byte span, so the segment stands for it); a region a consumer's read met counts unless the latch's union covers it, whichever came first. `DeliveryStats::regions_unread` stays the count of distinct spans met. Scenario: the read-only start's reasons are exactly `[DataLost]` (`kvik/tests/durable_losses.rs` `a_start_left_read_only_reports_the_losses_it_could_not_record`; `kvik/src/durable/loss/regions.rs` `a_region_is_recorded_only_inside_a_lost_range_of_its_segment`) |
| Q-930 | The deliverer's summaries after a loss | The deliverer is a scanning reader: its refresh goes through its own `loss::Finder` and notes finds in the latch; each closed range the latch takes that starts at or below what its reader has read leaves its summaries stale, and `held()` rebuilds them once by `read_summary` |
| Q-946 | A start whose log cannot be read | An open error, as the durable fixes pin it (`kvik/tests/durable_losses.rs` `a_start_that_could_not_write_a_readable_mark_is_an_open_error`); bounded decision history opened such a store read-only with nothing read, and that branch is gone. The start's reader then always becomes the loss watch, so `losses()` never falls back to the store's own reader |
| Q-947 | The ranges a start finds and the decisions | The start's events go through the watch's finder and each region is latched as a report (Q-926), but the start's ranges are cleared from the decisions' fresh ranges once it is done (`Latch::started`): the start's state never noted their facts, so they forget nothing (Q-921) |
| Q-948 | A query of the writer's process read in parts, under a cut | The store's own reader refreshes in parts of `start::PART` items, so a first read of a long log holds one part beside the index; a part the log reader discarded because the reset stamp changed while it read is read once more, and a second discarded part in the same call is `CutExpired` (Q-850, counted per call, not per part). Parts taken before a discard stay applied: the next part continues from what the reader holds and voids what a reset removed. Every part's finds are noted to the latch (Q-928) and the latch's closed ranges are forgotten (Q-938) before the cut is taken, so a call answers under its cut with no latched fact. The note per entry point is pinned by `every_read_entry_point_notes_a_loss_its_refresh_finds_and_its_replay_is_shown_once` (`kvik/tests/durable_losses_reads.rs`, nine entry points, on the caught-up path; its reader never held the lost fact, so the forgetting is Q-938's tests, through the one refresh all nine share), the read once more by `a_reset_during_a_call_is_read_again_under_the_new_cut_and_a_second_is_cut_expired` (`kvik/src/durable/read/cut.rs`) |
| Q-949 | The derived files beside the seal's description and the mark sync at a segment's first commit | A seal hands out its description (Q-220: length and SHA-256, taken in the pass that computes `file_crc32c`), and the log directory's record is built from it and the job (`directory_record`), so a segment is read once per seal for both. A sealed segment no record lists that fails its checks or cannot be read (`SegmentDamaged`, `SegmentUnreadable`) is reason `0xffff` (other) in the status file, A.18 having no code for it. A seal by size follows the first commit into the next segment, whose mark sync names a position past it (Q-630), so it never lies above the synced mark. A seal by time takes the next segment into use with no commit, so the sealed segment can lie above the synced mark until the next mark sync, and the derived files hold it back until then (a waiting directory forces that sync); `the_first_read_lists_no_segment_above_the_synced_mark` (`log/tests/directories.rs`) reaches that state with a timed seal. A first start after a lost `derived/` reads each unlisted sealed segment twice, once to describe it for the manifest (Q-221) and once for the log directory |
| Q-950 | A loss watch not polled across the removal of the segment it stands in | For a removal a manifest record names, as retention and compaction will write (none removes a sealed segment yet; roadmap 6.3b): not `LossesUnknown`; the reader voids what it took from the removed segment and reads on from the segments present, and the records report no loss for the removed positions. That check by record comes with the records. Until then no removal is named, so a reader that stood in the removed segment reports `SegmentMissing` and every other reader passes over it as it does a segment recovery deleted (Q-852): Q-951, where both are pinned |
| Q-777 | The live candidate's write time and what bounded state cannot tell | §2.3, §7 Derived files; `format.md` A.18 (settled by roadmap 9.0c, as built at 9.0a). The write time is the later of the file's `created_at` and every `received_at` it covers; the state keeps the candidate, the earliest and the latest good fact ahead, and in a versioned series up to 16 bad facts above the candidate; where that cannot tell, the entry sets "candidate unsure" or "more pending", and the counts, latest `t` and lateness reference are always exact. A reader continues the same state (`kvik/src/durable/derived/state.rs`) and judges it at its `now`; what it cannot tell it finds in the log at its cut (Q-1004). The latest fact ahead stays out of the entry: with more pending facts a reader searches once the first falls due, one lookup. `the_fold_equals_a_full_index`, `a_fold_continued_from_a_directory_equals_the_whole_fold` (`kvik/src/durable/derived/tests.rs`); `kvik/src/durable/read/latest/histories.rs` `latest_values_equal_the_models_at_every_check`; `kvik/src/durable/read/latest/cases.rs` `a_pending_fact_falling_due_after_the_directory_is_live_from_its_entry_with_no_search` |
| Q-952 | A series directory and a loss found at or below its `through` | A.18: a derived file is a function of the kept log only while nothing at or below its `through` is lost. The writer's fold (`SeriesFold`) watches the latch: on a closed range the latch takes that starts at or below what the fold has taken (folded or waiting) it stops, the run writes no further series directory, and the writer removes the series directories on disk (`SealHook::series_void`), so no file through the loss stays the newest. A store opening whose start found a loss that no lost range of the manifest holds continues no kept series directory and writes none in that run: it removes the family (Q-1002). The manifest's ranges are exempt for the rebuild's cost only: a file `kept_series` keeps was folded with every recorded range already lost, so continuing it gives the same entries. The latch hands on a region's closed range once, when the region is first met (`Latch::note`): a region met again holds no fact any reader still took, and a re-find after a restart's rebuild no longer stops the fold. Tests: `kvik/tests/derived.rs` `a_fact_found_lost_while_open_is_never_counted_by_a_series_directory_of_the_run`, `a_series_directory_counting_a_fact_found_lost_is_removed_while_the_store_idles`, `a_run_over_a_region_the_start_meets_and_no_lost_range_records_writes_no_series_directory`, `a_query_meeting_a_region_the_start_met_adds_no_loss_and_the_run_writes_no_series_directory`; `kvik/src/durable/derived/tests.rs` `a_loss_found_ahead_of_the_fold_leaves_it_whole_and_one_behind_it_stops_it`; `kvik/src/durable/loss/tests.rs` `closed_ranges_are_handed_out_once_per_reader_and_an_empty_one_or_a_region_met_again_never` |
| Q-925 | `late` when the lateness reference cannot be read | §2.2 `late`, §11 Health; `api.md` Health (settled by the owner's ruling of 2026-11-18, option (a), as built). Scenario: after a loss found while open, a series has no newer reference fact; the read of older facts (Q-924) meets damage no scanning reader has reported, or cannot read the log, and finds no reference fact, so the first clause of `late` cannot be decided: the facts that would decide it are neither in memory nor readable. Ruling: the reading is not late by that clause, so the fact may lack a `late` flag the durable fixes would have set, and it is counted in `LocalDataUnreadable { readings }` (`Plan::reference_unread`), once whichever checks went unmade; that field's meaning widens from "duplicate check unmade" to "a check unmade for want of local data" (spec §2.2, `api.md` Health). `kvik/src/durable/ingest/tests/losses.rs` `a_reading_whose_reference_cannot_be_read_is_not_late_and_is_counted`, `a_reference_fact_an_incomplete_read_finds_proves_lateness_and_none_found_is_not_late_and_counted`; `kvik/tests/durable_losses_open.rs` `a_reading_with_its_key_and_reference_unreadable_is_counted_once` |
| Q-928 | Damage to a batch of the kept log that the store's own reader finds at a query | §3 Recovery rule 2, §11 Health (Q-200) (settled by the owner's ruling of 2026-11-18, as built). Scenario: a store is opened again and a batch its start read (the kept log) is damaged; the durable fixes kept every fact the start read, so the store went on showing the fact, reported no loss and dropped a replay until the next start; bounded memory cannot show a fact whose bytes are gone. Ruling: a query reads the batch with the store's own reader, which finds the damage as any scanning reader does, and the latch takes it like any other find: `DataLost`, not yet recorded, the fact no longer shown and a replay recorded (Q-200's main rule, sooner). With no query the store does not read the batch again: `losses()` does not find it, and a replay of a fact whose key is in the recent window is dropped until the next start records the loss; outside the window its lookup meets the damage and it is recorded unchecked (Q-750). Q-200's limit says so. `kvik/tests/durable_losses.rs` `damage_to_the_kept_log_is_found_by_the_stores_first_read_of_it`, `a_replay_of_a_fact_of_the_damaged_kept_log_is_dropped_while_its_key_is_in_the_window`; the `Kept` place of `kvik/tests/durable_losses_open.rs` `a_loss_found_while_open_decides_as_a_restart_over_it_does`; `kvik/tests/slow_path.rs` `a_region_a_consumer_met_first_is_no_longer_counted_once_the_process_reports_it`, whose process reports the region by a query of the kept log |
| Q-951 | A sealed segment missing with no record naming its removal | `api.md` Health; `log/src/reader/pass.rs` `check_recent` (settled by the owner's ruling of 2026-11-18: the limit stands until roadmap 6.3b). Scenario: sealed segments A, B and C; B holds acknowledged facts; B disappears and no manifest record names its removal, which today is a fault, since nothing in the tree removes a sealed segment yet. Ruling: option (a), as built. A caught-up reader whose newest returned batch lies in B checks that batch at its next call, and B gone under either extension is `SegmentMissing`; it lasts for that reader until the store, or the reader process, is restarted, as the reader keeps that batch as its newest, so every later call fails: `LossesUnknown` for the loss watch at every poll, `NotAvailable` for every query. Sound because recovery deletes only segments holding no batch in use, and a seal only renames. A fresh reader, a reader that never stood in B and the next start stay silent, passing over B as over a segment recovery deleted (Q-852), until the removal records of roadmap 6.3b exist, since only they separate a removal from a fault; a lost range over the gap in positions (option (b)) would ask for acknowledgement of what a later version calls a retirement. The same fault is therefore reported or not by where each reader stood. Edge: after a removed segment a reader passed over, a damaged region at the head of the next segment takes its first position from the last batch read before it (`loss::Finder`), so its report and closed range take in B's positions, and with no batch after it in its segment the report runs to the mark read (Q-937): too wide, never too narrow; the closed range ends at the next batch, which a query still shows. `log/tests/reader.rs` `a_caught_up_reader_whose_newest_batch_segment_is_gone_reports_it_missing`, `a_reader_that_found_its_newest_batch_segment_gone_fails_every_later_call_until_reopened`; `kvik/tests/durable_losses.rs` `a_watch_whose_segment_is_removed_reports_losses_unknown`, `a_removed_segment_no_reader_stands_in_is_passed_over_and_later_damage_still_found`, `a_region_after_a_removed_segment_is_reported_from_the_removed_positions` |
| Q-810 | What the manifest's layout of a compacted file may repeat from the file | `docs/format-compacted.md` B.1 Description (settled by the lead at the review of roadmap 6.2a1: description field 10, positions per generation, verified by the writer and checked by the reader at open; field 13, `complete`; both in `Layout`) |
| Q-797 | The layout of a compacted segment | `docs/format-compacted.md` B.1, B.2 (decided by the lead for the compaction track, applied by roadmap 6.2a1: a chunk's directory follows its blocks; at most 64 blocks a chunk; the chunk index at the file's tail; the chunk index object an extraction carrying the segment's length and hash, with the ranged read of the segment's tail as fall-back. Reason: the writer streams, and one local file per segment halves small files, syncs and crash states on a card) |
| Q-802 | A chunk's value bounds | `docs/format-compacted.md` B.1 Bounds (decided by the lead for the compaction track, applied by roadmap 6.2a1: IEEE 754 total order without NaN and a `has_nan` flag; decimals at the chunk's highest stored scale; no bounds rather than saturated ones. `chunk/src/file/stats.rs`) |
| Q-803 | The chunk summary | `docs/format-compacted.md` B.1 Chunk directory (decided by the lead for the compaction track, applied by roadmap 6.2a1: defined and not written in v1; its safe use depends on conditions only the summary tree's item can state and test) |
| Q-805 | Commit times in compacted data | `docs/format-compacted.md` B.1 Commit times (decided by the lead for the compaction track, applied by roadmap 6.2a1: a per-file table of whole batches with `received_at` and the commit time, about a byte a reading, so as-of time keeps the log's precision after a release; a position no row holds makes the file invalid) |
| Q-650 | A manifest search that passes a window and finds no later record | `spec.md` §7 Manifest; `format.md` A.14 (settled by the owner's ruling: the passed bytes are a corrupt span with no later record, kept and repaired by a copy or a rebuild; the rebuild record's field 7 writes its `before` as 0; see the question) |
| Q-651 | The crafted-file worst case against the 1 s restart budget | `format.md` common rules (Scan bound); `design.md` Budgets; `SECURITY.md` (settled by the owner's ruling: each kept body checked once and cheaper body checks with the same verdicts, measured inside the budget; see the question) |
| Q-870 | Definitions and short state names against the 1 s restart budget | `format.md` common rules (Scan bound); `design.md` Budgets; `SECURITY.md` (settled as a technical decision with no format change: fields split once and found by an index, state names keyed and told distinct by a randomised table that sorts after a long probe run; every entry type measured at most about 2.9 ns a byte, the body checks of a crafted 64 MiB segment about 0.8 s; see the question) |
| Q-873 | The time of the header walk on a crafted file | `format.md` common rules (Scan bound); `design.md` Budgets; `SECURITY.md` (settled by roadmap 4.9e as a technical decision with no format change: once damage is found a header checksum at most twice per 8-byte step (the chain ends, and a torn tail's commit search) and a block's header read at most eight times, the chain ends and a boundary read's verdicts kept in rings by offset; time measured, the dearest crafted 64 MiB segment built 0.61 s; what a start is promised is Q-970; see the question) |
| Q-970 | What a start is promised on a crafted or damaged file, against the 1 s restart budget | `format.md` common rules (Scan bound); `design.md` Budgets; `SECURITY.md`; `api.md` Configuration (settled by the owner's ruling of 2026-11-18: the measured per-file figure at the default `log.segment_size`, plus a warning for a larger segment size; no bound in seconds, no format change). Scenario: the work of a scan is bounded per byte (Q-110, Q-651, Q-873), but not its time: a header read costs about 20 to 31 ns and a body byte checked at most about 2.9 ns on one machine, and the bounds allow more of both than any file built uses. Bodies: at most 264 MiB checked for a 64 MiB segment, priced at the dearest entry 0.8 s, while the dearest file built to the body bound measured 0.22 s. Header reads: for 16-byte blocks at the measured cost about 1 s at 64 MiB before any body is checked, while the dearest file built measured 0.61 s. No argument excludes a file that stacks overlapping bodies on the header reads. Ruling: what a start is promised for one crafted or damaged file is the figure measured at the default segment size: a 64 MiB log segment 0.25 to 0.61 s on one shared 4-core cloud machine (release build, best of five), a 16 MiB manifest about 0.3 to 0.4 s with its body checks. The work is linear in a file's length, so a segment of `segment_size` costs about `segment_size / 64 MiB` times as much (the 1 GiB maximum some 10 s), and a start that reads several crafted files pays for each. A store whose `log.segment_size` is above the default warns when it reads files from a bucket others can write; it cannot tell who can write a bucket, so it warns for every bucket it is given: `Store::attach_backup` on the durable backend first logs a `tracing` warning naming the size and its ratio to the default (`LogConfig::bucket_warning`). `Store::restore` takes no configuration, so a restored store is to give the same warning at its first open, when roadmap 8.3d wires the restore. `kvik/src/config.rs` `the_bucket_warning_is_given_above_the_default_segment_size_only_and_names_the_ratio`; `kvik/tests/backup_interfaces.rs` `attaching_a_bucket_warns_only_above_the_default_segment_size_on_the_durable_backend` |
| Q-882 | Entry type 10 added inside format version 1 as a critical type, its item carrying an `op` byte (series labels design) | `format.md` A.6 type 10 |
| Q-890 | A label change is per-key items, not a whole-set replacement (series labels design) | §2.1 Labels |
| Q-891 | Effective times in the future are allowed (series labels design) | §2.1 Labels, §2.3 Label views |
| Q-892 | Label entries held in immutable label parts, not in the registry file, with the `has_labels` flag (series labels design) | §2.1, `format.md` A.8, A.16, Label parts |
| Q-893 | Labels kept out of the series directory, the chunk index and chunk headers (series labels design) | §2.1 Labels |
| Q-1102 | A start's lost range across two recorded losses | §3 Order of a start step 4, `format.md` A.14 lost range, `api.md` Health (ruled by the owner, 2026-11-21: one loss under the lower identity, option (a)) |
| Q-1109 | Delivery before durability: an automation may act on a reading later withdrawn | §4 Provisional streams, `api.md` Provisional streams (ruled by the owner, 2026-11-22: acceptable; opt-in per stream, each delivery labelled provisional or durable, a loss reported as withdrawn) |
| Q-1200 | A future fact outranked before its `t` | Q-1148 (ruled by the owner, 2026-11-26: not an arrival at `t`) |
| Q-1201 | Records held for a consumer not taken in this run | `api.md` Memory (ruled by the owner, 2026-11-26: none held) |
| Q-1202 | `CursorBehindLog` on the durable backend | `api.md` Consumers (ruled by the owner, 2026-11-26: model only until the log trims) |
| Q-1203 | A fan-out round that fails after taking events | `api.md` Live reads (ruled by the owner, 2026-11-26: not whole, `Lagged` to every session) |
| Q-1145 | A gate's state when it starts | §6 Gates, Transitions (ruled by the owner, 2026-11-26: (b), seeded from history) |

## Where the model differs from the specification
The model implements the summary semantics (§5, §6), calendar results (§5.7, §6 Windows: a day or month result is a
window evaluation over its span, edge stretches included, in the site zone or a named IANA zone; stored day and month
nodes, kept only for spans on 15-minute boundaries of a series with a stored 15-minute level, are inputs only where
every day they cover was taken in the zone of the query, compared after resolving links, their span is the day's and
their tzdata version is the one in force, and never past the credit limit; days with no quarter-hours, days whose midnight
does not exist and historical offsets off 15-minute boundaries; the site zone as an effective-dated log entry, whose
change rebuilds calendar nodes (for days around a change to another offset, see the table), and tzdata changes,
recorded as a log entry, which rebuild only the affected days (for when a release is adopted, see the table); a
midnight in a gap that straddles it and a midnight that occurs twice (the earlier instant); calendar windows since local midnight, this month and relative days and
months; rows 20, 20a, 21 and 21a, invariants 5, 7, 8, 17 and 22 over calendar days; `tests/rules.rs` `s5_7_…` and
`s6_calendar_…`), the duplicate rule with `revise` (§2.4: identity by value and
adapter-supplied quality; a fact whose key's current fact is on no local tier, hot or cold, is recorded and counted
`duplicates_unchecked`, outside a restore whatever the revised-key index holds (see the table); invariant 15, rows 2,
2a and 2b), versioned series (§2.1–§2.4: `changed_at` required on
every reading of a versioned series and refused on any other, `revise` refused there, rank `(changed_at, position)`
in every view, identity including `changed_at`, the version index and the revised-key index, carried by each tree
checkpoint, so that during a restore ranks are exact and a replay of an earlier value of a key revised by the
checkpoint is dropped before the current chunks are back; §4: an outranked fact delivered flagged `outranked`, live and in catch-up (where catch-up judges it from the local
history and, while a restore brings detail back, for a fact after the checkpoint from the checkpoint's version index;
a replayed fact at or before the checkpoint whose outranking fact is not yet local can miss the flag), counted, and
never fed to a condition or refreshing stale-for; §5.6: it changes no node; invariants 4, 15 and 19, rows 2c, 2d and
37), compaction and files (§7: per-series current and revision chunks, ranked within the compaction's inputs, and a
chunk of the registry and routing entries; one manifest record that adds, retires and moves files atomically;
compaction of compacted files publishing its outputs and retiring its inputs in one record; retired files removed
locally only after the grace period on the writer's monotonic clock since the later of the record and the writer's
start, once their replacements are verified in another tier and no live lease names an older epoch, and, for
invariant 12, once the retired file itself has a verified copy in another tier; reader leases with boot id and
refresh counter, expired on the writer's monotonic clock, first observed at the writer's start, void on another boot;
a restore leaves out files retired by a record whose added files are in the bucket; invariant 14 among compacted files, a sealed log segment compacted once, rows 10
and 26; `tests/rules.rs` `s7_…`), tiers and eviction (§3 Disk space: tiering and eviction continue at every mark;
§8: sealed log segments and revision chunks leave hot at 90 days, revision chunks by an age per series that may be
never; current detail stays except under the retention class, a registry entry in force from its position that
moves a diagnostic series' current and revision chunks whose span ends more than its period before `now`, and moves
nothing back; fill pressure moves verified sealed log segments first, oldest first, then the current chunks of the
oldest data, never the registry's chunk; every move needs a verified copy in cold or deep and is recorded in one
manifest record; invariant 12; `tests/rules.rs` `s3_low_mark_…` and `s8_eviction_…`, `s8_retention_…`,
`s8_fill_…`), the late and future
flags by timing and per-series lateness bound (§2.2), `t_source` and the clock (§2.2, §2.5: `host_time`; before
synchronisation, `unsynced_clock`, no `future` or `late`, and a host-stamped emit waits as back-pressure, counted;
row 22a), invalid values (§2.5: NaN, ±∞ and a negative counter value recorded as bad quality without a value,
flagged `no_value` and counted), interval series (§2.1, §2.2, §5.2, §5.4, §6: length and phase fixed by the definition, a reading off its phase
refused and counted, `t + d` standing for `t` in `future`, in the second clause of `late`, in live state and in
windows of an observed series, the node form only for levels whose edges no interval crosses, windows from stored
nodes in the middle and apportioned edges flagged `apportioned`, sum and rate over covered seconds, avg, integral,
increase and time-in-state refused; rows 20a, 40–42, invariants 5, 18 and 22), series timing (§2.1, §2.2, §2.3, §4, §5.4, §6: no `future`, and no `late` by its first clause, on a scheduled or
forecast series; no live state for a forecast series; the credit limit `now` for an observed series and the window
end for a scheduled or forecast one, a window wholly after `now` on an observed series being empty; scheduled live
staleness, on `now`, and stale-for conditions over a scheduled series that follow it on the timer; a stale-for condition over a forecast series, or any condition over one without a windowed operator,
refused when routed; rows 36, 37b, 38 and 39, invariants 18, 20 and 21), the density rule (§5.1, §5.7: the 1-minute level stored only for an expected interval of 5 s or shorter, the
15-minute level and calendar nodes only for 60 s or shorter, other levels answered from facts, also in a tree
checkpoint and its verification; row 20a for a series without a stored 15-minute level), counters and rate (§5.5, §6: resets, wraps and regressions, each regression
contributing 0 and counted; rate as the increase over the elapsed time between the readings that bound its steps,
empty with no step, flagged when a step spans more than S; row 18a, `tests/rules.rs` `s6_rate_…`), retirement at `t_r` (§2.1, also for a reading that waited in the queue while the
retirement committed), the durable mark and recovery by boot id (§3),
delivery (§4, §6: routing changes as log entries taking effect at their position, in live delivery and in catch-up
(for a route before a restore's checkpoint, see the table); condition funnels evaluated on routed facts and on the
timer, never on replayed facts, their deliveries carrying the last committed position and a per-funnel sequence (for
their identity, see the table); condition state now from the latch, a threshold between its levels keeping the latched
state, and a funnel added at run time with no latch; one latch per condition funnel, moved when the consumer confirms;
on every resume, after a restart, a recovery on a new boot or a consumer restart, a replay up to the last committed
position followed by `resync` with the live state and the condition state now, a transition where it differs from
the latch or there is no latch, or `resync` alone when the cursor is too far behind the local log segments; and
catch-up after overflow from the last position enqueued in the session, condition funnels silent meanwhile and
the present state sent on re-attaching where it differs (for reads from deep, see the table); rows 14–16 and 34, invariants 2, 3, 11, 13
and 16; `tests/rules.rs` `s4_…` and `s6_…`), loss of a corrupt log segment with no other copy (§8; the whole range, recorded as a lost range, see the table), lost ranges in the open
segment found by recovery (§3 Recovery rule 2, row 11a: later batches kept, the range recorded in the manifest and
reported as `data_lost`, its positions not reused; see the table), and generations, tree checkpoints
and restore (§2.2, §8 Continuous backup, Generations, Restore drill, Restore steps 1–4 and the paragraph after them,
rows 28, 28a and 28b; for 28a, a crash before the record, during step 1 or between later downloads, a download
being atomic): a tree checkpoint carries the stored nodes, the registry and each series' latest recorded `t`
as of its position, and is uploaded only once log segments in the bucket hold every position up to it; a restore
claims the generation one above the highest claimed or present in the bucket by a create-if-absent marker, taking the
next if the marker exists, and starts it at sequence 1; a bucket with nothing backed up is refused; the restored end
is the last position before the first gap, and later positions of older generations are set aside, outside views,
trees, the registry and delivery in the writer; the restore writes a local record (see the table) and downloads
the latest checkpoint at or below the restored end and the log segments after it (with no checkpoint, the whole log)
before it accepts ingest; window queries are then flagged `restoring`; until the current chunks (and with them the
revised-key index) are back, a fact at or before the latest known `t` of its series that matches no local fact is
recorded and counted `duplicates_unchecked`; a consumer whose kept cursor is at or below the restored end catches up
to it, flagged `replayed`, if log segments after it are local, and otherwise only receives `resync`; every consumer
then receives `generation_changed` and `resync`; current chunks, then revision chunks, then log segments come back
one per step, a position in two files must have identical content, and as each span's detail is back its nodes are
recomputed from the facts as of the checkpoint's position and compared with the checkpoint's, a difference is
counted and the recomputed nodes win, and the `derived-unverified` label goes when every span the checkpoint covers
is verified; a restart after the record resumes the restore in its generation (but see the table for a failed
retry), finishing an interrupted step 1 before it accepts ingest, and one before it starts again with the next
generation; a drill is read-only, claims nothing and raises no health condition of its own; a writer that
finds a higher generation claimed stops uploading and stays read-only across restarts, with fatal health
(`tests/rules.rs` `s8_…`, `tests/invariants.rs`
`inv01_only_restore_starts_generation` and `inv09_stepwise_restore_from_checkpoint`). It does not yet implement:

| Rule | Specification | Model now | Test (`tests/model.rs` unless stated) |
|---|---|---|---|
| Commit time and as-of time | §2.2, §2.3 | As-of by `received_at` | — |
| Durability levels | §2.1 `durability`, §3 Batches, Commit, Recovery rules 1–3 | The level is fixed for the life of a series (no parameter change, Q-60, Q-256), so the registry at K is read from the definitions; the queue commits as one batch, flagged only when every reading is at the `written` level (the engine cuts the queue by call into runs of one class, Q-255); `max_unsynced` is the caller's `sync_log`; batches reach the disk in order, so an invalid region before a complete flagged batch (rule 2, Q-257) and writing kept batches again are not modelled; adoption is not modelled; `Outcome`'s level and `LogStats.acknowledged` are not modelled (Q-680); a failed sync keeps its flagged batches on the same boot unless `evict_failed_write_back` drops them (Q-254); `remount_keeping` says how many unsynced flagged batches reached the disk (Q-250) | `tests/durability.rs` |
| Effective-dated parameters | §2.1, §5.1 | Every parameter (staleness limit, expected interval, counter rules, lateness bound) is fixed for the life of a series, so neither a later change of the expected interval, which leaves the stored levels as they are (§5.1), nor a past-effective change of a parameter a node depends on (Q-83) arises | `calendar_depends_on_zone_utc_levels_do_not` |
| Zones compared by canonical name | §5.7: "after resolving IANA links" | The bundled database has no link table, so a zone's identity is a fingerprint of its bundled rules (initial offset and abbreviation, every transition to 2200), which a link shares; two zones with identical rules would count as one. Europe/Amsterdam is a link to Europe/Brussels there | `tests/rules.rs` `s5_7_named_zone_compared_after_resolving_links` |
| tzdata version | §5.7: recorded per calendar file | The bundled database is labelled `bundled`; a later release is the bundled data with one zone's rules replaced (`Tzdb::with_rules`) | `tests/edge_cases.rs` `row_21_tzdata_change_rebuilds_the_affected_days` |
| Calendar files | §5.1: per-series files on hot, loaded at start | Built from the trees at every start with the tzdata version the log names, taken from the releases this process has seen (the host's database when it has not seen that version), and refreshed per changed minute at commit | `tests/invariants.rs` `inv08_calendar_rebuild_equals_live` |
| Adopting a tzdata release | §5.7: the writer reads the host's release at open, about once a minute and at each local midnight, and whenever it is not the version in force writes a tzdata entry naming it and rebuilds the affected days, without a restart; readers and drills never refuse, and compute calendar results with their own release, using a stored node only where its recorded release is theirs (Q-93 (a)) | The model holds several releases: installing one (`Model::install_tzdb`) writes nothing, results then follow the new rules before any entry names them, a restart keeps the version the log names, and `Model::rebuild_calendars` writes the tzdata entry and rebuilds when called; nothing is refused | `tests/open_questions.rs` `q76_tzdata_change_is_a_log_entry_written_by_the_rebuild` (its restart step), `tests/edge_cases.rs` `row_21_tzdata_change_rebuilds_the_affected_days` (results between the install and the rebuild) |
| Decimal series | §2.1 `decimal scale` (default 4, raised by an entry, never lowered; range check against the largest magnitude, a function of the log; the scale in force after a lost raise, Q-100), `excess precision` (`round` flagged `rounded`, `refuse`, `auto-raise`; `f64` input converted to its shortest decimal first; the policy changed at a position, Q-100), §2.4 (exact numeric comparison and rounded facts, Q-101), §2.5 (out of range, Q-102), §5.6 (node scale, Q-100), row 24 | Values are `f64` only (`ValueType::Number`): there is no scale, raise, policy, `rounded` flag or out-of-range value. The model property tests of Q-100 to Q-102 wait for decimal values in the model (before roadmap 5.1; Q-100 (c) before 7.1) | `tests/edge_cases.rs` `row_24_unit_or_kind_change_is_new_series` |
| Local days around a site-zone change | §5.7: the day of date D starts at the first instant whose local date, in the zone in force at that instant, is D or later, so days and months tile; "today" is the latest date whose day has started | `calendar::Zoning::Site` takes a day in the latest entry's zone under which it starts at or after that entry's effective time, else in the store's zone, so a change to another offset leaves hours in no day or two days overlapping; a month node is stored only where its days are contiguous | `tests/open_questions.rs` `q75_site_zone_change_to_another_offset_leaves_a_gap_or_an_overlap` |
| New-consumer start; `emits` and cycles; name patterns | §4 New consumer, Routing (a consumer declares `emits` and emits through its own handle; a change that closes a cycle is refused), row 33; name patterns, §4 Funnel (Q-88) | Consumers are declared when the model is created and start before the first position; a consumer declares no `emits` and emits through the store like any adapter, and no cycle is rejected; a selection is a list of ids or every series | `tests/edge_cases.rs` `row_33_emit_from_a_delivery_does_not_block_and_a_cycle_is_not_rejected` |
| Routes before a restore's checkpoint | §2.1, §4 Routing: routing changes are kept with their positions in the registry file, which every tree checkpoint carries, so a route is in force from step 1 of a restore, and after its log segment has left hot | Routes are taken from the entries that are local; a route added before the checkpoint is not in force until the file holding its entry is downloaded | `tests/open_questions.rs` `q64_route_before_checkpoint_not_in_force_until_its_entry_is_back` |
| Identity of a condition delivery | §4 Order, At-least-once: `(funnel, sequence)`, the sequence in memory, from 1 at every engine start, valid until the consumer's next `resync`; nothing persisted | `(generation, funnel, sequence)`, the sequence kept on disk when the delivery is issued, never issued twice for a funnel within a generation; the `kvik` in-memory backend numbers each funnel's deliveries from 1 at every start, as the specification does and the durable backend compares against | `tests/open_questions.rs` `q61_condition_delivery_sequence_survives_a_crash`, `q61_condition_delivery_identity_includes_the_generation`; `tests/rules.rs` `s4_condition_deliveries_unique_by_funnel_and_sequence` (unique across restarts), `s4_timer_deliveries_carry_last_position_and_funnel_sequence` (the generation in the id) |
| Catch-up after overflow from deep | §4 Overflow, §8 Behind consumers: catch-up reads hot and cold only; where its next entry is on no local tier it ends with `resync`; transitions not delivered meanwhile are counted (`transitions_not_delivered`) | Catch-up after overflow reads an entry wherever it is held; the transitions in between are not counted | — |
| Funnels with an operator and no condition; conditions over a selection of several series; held-for over a windowed operator | §4 Funnel, §6 | Refused when routed (§6 Multi-series leaves composition to the consumer); held-for is modelled on the live-state value only, so that its resume can be rebuilt exactly from the facts | — |
| Timer cadence | §6 Cadence: every 1 s, configurable per funnel | Condition funnels are evaluated on the timer when `Model::tick` is called | — |
| Cursor file synced at most every 1 s | §4 Consumer state (and at once when a confirmation changes a latch, Q-132) | The cursor and the latches are durable at once on confirmation; the mark file is synced first when the cursor would name a position above the last synced mark (§3 Durable mark) | — |
| Age of a replayed delivery | §4 Replay: from `received_at` | Not modelled; replayed deliveries keep their original times | `consumer_down_for_a_day_resumes_from_cursor_replayed` |
| Back-pressure wait | §3, §11: the wait is measured | Waits are counted, not timed (the model is synchronous) | `tests/rules.rs` `s3_full_ingest_queue_waits`, `tests/edge_cases.rs` `row_13_…` |
| Group commit and its sync budget | §3 Group commit: a batch with nothing in flight is synced at once while the sync budget holds a sync, else at its next refill, taking everything queued (Q-980 to Q-985) | Not modelled: each commit is one batch and one sync, with no timing (the model is synchronous) | — |
| Live staleness for stale-for conditions | §2.5, §5.4: bad-quality facts do not refresh it (see Q-16) | Any routed reading refreshes it; a resume takes the time since the last reading's `received_at` | `tests/open_questions.rs` `q16_stale_for_true_now_reported_true_on_resync` |
| Clock jumps in health | §6 Windows, §11 | Not modelled | — |
| Unknown condition and live state during a restore | §6 Conditions, §2.3 Live state, §4 Resync, §8 Restore step 2, invariant 16 (Q-49 (b)): an evaluation with no local detail is skipped and counted (`evaluations_unknown`), the condition keeps its latched state, `resync` reports "unknown", and the first evaluation with detail is a transition if it differs from the latch or there is none; live state of a series whose latest recorded `t` no local fact or stored node holds is unknown, not none | Not modelled: a series that stores no level answers windows and live state during a restore from its local facts, without a flag; nothing is reported unknown or counted | — |
| Condition state on resume | §6 Restart and resume, invariant 16 | The engine resumes conditions with `Runner::resume`, from the value now, the history rebuilt from the tree (held-for, stale-for) and the latch; a standalone `Runner::rebuild` scans a history of values and counts a true condition as fired | `restart_rebuilds_threshold_without_firing_again`, `tests/open_questions.rs` `q63_…` |
| Cursor of a consumer after a restore | §4 Consumer state, invariant 13: a cursor is never ahead of the consumer's confirmation | A consumer without a kept cursor, or whose catch-up segments are not local, gets the restored end written as its cursor, which it never confirmed, standing in for "treated as beyond the restored end" (§8 Restore step 2) | `tests/rules.rs` `s8_cursor_catches_up_only_if_its_segments_are_local` |
| Tree files and their checkpoint | §5.1 | Tree rebuilt from facts at restart (tree checkpoints are taken only for backup, by an explicit call) | — |
| Calendar results while restoring | §8 Restore step 2, §6 Old spans | As for windows (see "Window edges over detail that is not local"): a calendar span with an edge inside a minute, such as a day in Africa/Monrovia before 1972, is refused with `Restoring` | — |
| Compaction of compacted files | §7 Policy: merge when a series' chunks overlapping any 15-minute span exceed 4 | Counts and merges the live current chunks of a series on the fast disk only; revision chunks are never merged | `tests/rules.rs` `s7_merge_publishes_outputs_and_retires_inputs_in_one_record` |
| Revised-key index outside a restore | §2.4, rows 2b and 2c: a match with the key's entry in the revised-key index is a duplicate wherever the facts are; only a key with a single fact off the machine, or an incomplete entry, is recorded unchecked | Outside a restore, a fact whose key's current fact is on no local tier is recorded unchecked whatever the index holds; during a restore it is dropped | `tests/open_questions.rs` `q72_replay_off_the_machine_recorded_unchecked_although_the_index_holds_it`; `tests/edge_cases.rs` `row_02c_replay_never_reverts_a_versioned_correction` (its off-tier case) |
| Chunk indexes after a restore | §8 Restore step 1: the chunk index of every listed compacted file is downloaded before ingest is accepted | A restore brings every file back, so no chunk index is missing | — |
| Revised-key index, recent window | §2.4, §5.1, §8 Restore step 2: "A key that gains a second fact meanwhile, in the replay or by ingest, has its index entry completed when its chunk is back" | A full in-memory history of every key stands in for the index and the recent window; while restoring, a key revised only after the checkpoint has its entry completed when every current chunk is back, not when its own chunk is | `tests/rules.rs` `s8_index_entry_completed_when_the_current_chunks_are_back` |
| Age of a compacted file | §8 Eviction at 90 days: no rule for the age of a merged or compacted file | A compacted file takes the seal time of its oldest input, so a revision made today, merged with a 60-day-old chunk, leaves hot after 30 days | `tests/rules.rs` `s8_merge_keeps_the_age_of_its_oldest_input` |
| Clock of the eviction age | §8 Eviction at 90 days: no clock named | The engine clock `now`, as for the retention class, from the seal of a log segment, or of a compacted file's oldest input, so a merge does not reset it | `tests/rules.rs` `s8_merge_keeps_the_age_of_its_oldest_input` |
| Verification of compaction outputs | §7 Procedure: fact count, per-series checksum of facts, each chunk's summary equals the summary recomputed from its input facts | The outputs hold exactly the input entries; chunks carry no summary | — |
| A restore without the log segments after its checkpoint | §8 Restore step 1 | Where no backed-up log segment holds a position after the checkpoint, step 1 takes the first file in download order that holds it | — |
| Host-stamped readings before clock synchronisation | §2.5: they wait in the ingest queue (back-pressure) | The emit returns `Wait::UnsyncedClock` and the reading takes no queue capacity; the adapter emits it again later, and the wait is counted per attempt, not measured | `tests/edge_cases.rs` `row_22a_…` |
| Interval series | §2.1 (an `i64` value with a decimal scale), §6 (an apportioned amount computed in f64) | Values are `f64`, and sums, including apportioned amounts, are exact rationals, so invariant 22 holds exactly, also over the days of a month in a +05:45 zone | `tests/invariants.rs` `inv22_…` |
| Calendar-length interval series | §2.1 `interval zone` (also unknown or become a link, Q-80) and `day start`, §5.4 displaced readings, §5.7, §6 (count, min, max and last, Q-81), rows 43–46, invariant 23 (Q-45) | Interval series have a fixed length and phase only; calendar lengths, interval zones, day starts and displaced readings are not modelled yet | — |
| Lateness reference in a tree checkpoint | §8 tree checkpoints, Restore step 1, row 4a (Q-74) | A tree checkpoint carries only the latest recorded `t`, which stands in for the lateness reference while a restore brings detail back; outside a restore the first clause of `late` compares with the lateness reference | `tests/invariants.rs` `inv24_late_against_the_lateness_reference`, `tests/open_questions.rs` `q74_a_far_future_reading_leaves_later_on_time_readings_on_time` |
| Queries over a quarantined range return an error | §8 Corrupt block | The range reads from another tier | — |
| Compacting a corrupt file again from its kept inputs | §8 Corrupt block, row 11 | Quarantine needs a verified copy in another tier | — |
| Corruption inside the open segment; lost ranges, `data_lost` | §3 Recovery rule 2 and Order of a start, §4 Resync, §8 Corrupt block (reported when found; for the open segment, recorded by the next start that can write), §11, row 11a, invariants 1, 2, 9 and 10 | Damage is injected while the engine is down, only to a batch of facts or a recovery entry alone in its batch below the durable mark (Q-33), found and reported by the next recovery, and recorded by the next recovery that can write (Q-79); a block found corrupt at run time is not modelled. The range recorded is the positions the batch held, which is the range the rule names when its neighbours are valid batches. A damaged batch that a remount or a lost mark leaves beyond the mark, at the end of the open segment, is a torn tail: beyond a same-boot mark it is part of a rule 1 reset (Q-125). A lost range recorded at run time (a scrub of a sealed segment) sends no plain `resync` to consumers past it | `tests/edge_cases.rs` `row_11a_…`, `tests/invariants.rs` `inv01_gap_free_except_lost_ranges` |
| A corrupt sealed log segment with no other copy loses only its damaged region | §8 Corrupt block, row 11: valid batches copied to a new segment by one manifest record, which ends the error | The whole segment's range is lost, recorded as a lost range and reported as `data_lost`, so its positions are not issued again; the file stays quarantined, and its fatal health ends with the lost-range record (§8 Self-repair) | `tests/rules.rs` `s8_corrupt_log_segment_with_no_other_copy_loses_range` |
| Lost ranges and copies in the model | §8 Self-repair stage 1, Q-246; §8 Hashes | A recorded lost range holds nothing in `Disk::all_entries` until it is closed as restored in full, whatever file is readable at its positions; the upload queue takes only files with a verified local copy (hot and not damaged, or cold), so a damaged file is never uploaded and an object the bucket lost is not put again by the queue | `tests/self_repair.rs` |
| Self-repair | §8 Self-repair | Closing records are written by an explicit step (`Model::repair`, `repair_done`, `restore_file`), not as time passes; a re-send request carries positions and the wait, not series or commit times; the bucket loses objects but never corrupts one; a corrupt manifest record (a corruption of its own) is not modelled; re-sent readings are ordinary ingest. Restored positions are excluded from catch-up, since the reset point covers them | `tests/self_repair.rs`; `tests/recovery_histories.rs` |
| Recovery: the default mark sync, torn bytes, Adoption and the last batch written again | §3 Durable mark, Recovery rules 1 and 3, Order of a start, `api.md` `log.mark_sync` (default `Interval(1 s)`) | The model's default is the mark synced at every commit, so existing histories lose no acknowledged batch; the timed sync is `MarkSync::Timed`, fired by `Model::sync_mark_file`. A tail is a count of batches and whether one was torn, not bytes. A start is never cut short, so no recovery entry is adopted, and no batch or header is written again. A reset stamp's nonce comes from a counter. A start whose own sync fails delivers nothing, timer-driven condition deliveries included, and compaction and tree checkpoints are refused while read-only | `tests/edge_cases.rs` `row_07_…`, `row_07a_…`, `row_12a_…` |
| Orphaned facts and id floors | §2.1, §3 Commit, invariants 2 and 4 (Q-33) | A batch holding anything but facts cannot be damaged or lost from a tail (the mark file is synced at once after it, and the fault injections refuse it), so no fact is orphaned; recovery entries carry the id floor, which no definition can exceed | `tests/edge_cases.rs` `row_11a_fault_injection_limits` |
| Torn and corrupt manifest records | §7 Manifest, §8 Corrupt block, invariant 10 (Q-52) | Manifest records are written atomically and never corrupted; there is no empty record and no rewrite of the last record at start | — |
| Positions set aside beyond the restored end | §8 Restore step 1: excluded by position also in reader processes; a set-aside definition keeps its id reserved; the set-aside range reported in health from the restore record at every start (see Q-90); exported for re-ingest (`api.md` `export_set_aside`) | Downloaded in step 1, kept and excluded by position from views, trees, the registry and delivery in the writer; not reported; their series ids are not reserved; reader epochs (§7) still read them; no export | `tests/rules.rs` `s8_restored_end_before_first_gap`, `tests/open_questions.rs` `q38_…` |
| Restore record and the files a restore reads | §8 Generations, §8 Restore step 1: the restore record lists every file it restores from, set-aside segments included, each with its position range, and carries the earlier restored ends; it is uploaded before any other file of its generation, and the claims are then read again; a restore reads only the records of the highest generation below its own that has any, and a position set aside once stays so | The record is local and holds generation, checkpoint and restored end; a restore reads every file in the bucket whatever its generation, except files retired by a record whose added files are all in the bucket, and does not read the claims again. A second restore after a restore with a gap therefore finds the old gap again and sets aside the generation that restore started; under the specification its restored end is `1:1` and generation 1 is kept | `tests/open_questions.rs` `q37_second_restore_finds_the_old_gap_again` (expects the model's result) |
| A failed retry of a restore | §8 Restore, the paragraph after step 4: a restart after the record resumes the restore in its generation | In `restart_restore`, a retry after a crash before the record that writes its record and then fails in step 1 is discarded and stays pending, so the next restart claims another generation | `tests/open_questions.rs` `q39_failed_restart_of_a_restore_is_reported` |
| Failures after step 1 | §8 Restore, Failures and end: a file that cannot be fetched, or whose plaintext hash fails, is retried with degraded health naming it; a content conflict is named in health with both files | Downloads do not fail; a conflicting file is not installed and the step returns `Conflict`, with no health reason | `tests/open_questions.rs` `q39_conflicting_file_leaves_restore_in_progress` |
| A listed object lost or corrupted in the bucket | §8 Restore, Failures and end, §8 Corrupt block and No deletion, invariants 9 and 10 (Q-39 (e)): treated as Corrupt block with deep as the failed tier, in step 1 or later; compacted again from the other listed files, the positions no other listed file holds recorded as a lost range and reported as `data_lost`; the restore completes | Not modelled: the bucket never loses or corrupts a file, so no restore records a lost range or ends that way | — |
| End of `restoring` | §8 Restore step 3 and Failures and end: only files the restore record places on hot (and cold where configured) are brought back, and `restoring` ends when they are installed, replaced or recorded lost (Q-39 (e)); the label goes when the spans whose detail comes back are verified | Every file the restore restores from comes back, and `restoring` ends when all are local; the label goes when every span the checkpoint covers is verified | `tests/open_questions.rs` `q39_restoring_ends_when_every_file_is_local` |
| Query flags and fact reads | §11 Query; §8 Restore step 2 and Failures and end: every query result carries `restoring` while restoring, and fact reads fetch detail not yet local on demand | Only window results carry flags (`restoring`, `summary_stale`); while restoring, `current`, `facts` and `latest` return the local facts, unflagged, without fetching | — |
| Window edges over detail that is not local | §6 Old spans, §8 Restore step 2: an edge moves inward to a node boundary of the finest stored level, or with none to where its detail is local, flagged `approximate_edges`; with no span left, the error `detail_not_local`; no bound is refused for its resolution | No window is flagged `approximate_edges` and no edge moves; while restoring, a window bound inside a minute is refused with `Restoring`. A series that stores no level, or only the 15-minute level, answers the unstored levels from its local facts: a minute the replay touches inside a quarter-hour the checkpoint holds is flagged `summary_stale`, but a window over part of a quarter-hour whose detail is not yet local is computed from the local facts without a flag (see Q-49 (b)) | `tests/open_questions.rs` `q39_no_sub_minute_query_or_checkpoint_while_restoring` |
| Replaying the log after a checkpoint | §8 Restore step 1: "replay them into the tree" | A minute for which the checkpoint holds a node and the replay (or new ingest) brings a fact, while the minute's earlier detail is not local, is recomputed from the local facts and flagged `summary_stale` until the detail is back (row 29); an engine that folds facts in `t` order into the checkpoint's node would hold it exact | `tests/rules.rs` `s8_replayed_minute_without_its_detail_is_stale` |
| Lost ranges and the restored end | §8 Restore step 1: a recorded lost range is not a gap | Every missing position is a gap: lost-range records stay in the local manifest and the bucket does not carry them, so a restore sets aside the positions after a lost range | — |
| A closed loss's object in a restore | §8 Self-repair stage 3, Q-600; `format.md` A.14 `0x26`: an object judged lost with no later `0x27` is out of every restore's plan, so a closed loss stays closed in every generation | The bucket carries no quarantine records, so the model's restore plans every object in it: after a lagging listing shows a closed loss's object again, a restore brings its facts back. The durable plan reads quarantine records once roadmap 8.5b decodes them; a test pinning the rule on both comes with that reading (in the model, a set on `Bucket` that stands in for the records, as `retired` does) | — |
