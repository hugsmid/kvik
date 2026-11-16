# kvik Parquet export, version 1

`spec.md` §11 leaves the Parquet export schema to a separate document; this is it. The schema is a promise to
users: the owner settled it on 2026-11-16 (Q-190 to Q-197, `model/OPEN-QUESTIONS.md`). The crate
[`kvik-export`](../export/) implements it (roadmap 6.4a); roadmap 6.4b adds the differential check of compacted data
against it. Parquet is the export format only (`design.md`); stored files keep the own format.

## Compatibility (Q-190)

Every file carries `kvik.export_version` = `1`. Adding a column or a metadata key is not a breaking change and keeps
the version, so a reader ignores columns and keys it does not know. Renaming, retyping or removing a column or a
metadata key, or changing what one holds, raises the version.

## Scope

One call exports the facts of one series with `t` in a half-open range, in one view (`spec.md` §2.3), read through
`Reader::facts`, into one Parquet file (`export_series`), or into one file per UTC month in the partitioned layout
below (`export_partitioned`): one row per fact. Orphaned facts (`api.md` `export_orphaned`) and facts set
aside by a restore (`export_set_aside`) stay separate calls that return facts for re-ingest; their Parquet form,
the same schema in one file per call with rows of several series, comes with roadmap 8.3 (Q-191 (b)).

## Options

| Option | Default | Meaning |
|---|---|---|
| `view` | `Current` (Q-191) | `Current`, `AsOf(P)`, `AsOfTime(T)` or `History`; export `History` for re-ingest (`spec.md` §8 Re-ingest), as only it carries every revision and so the duplicate rule's state |
| `range` | every `t` below `i64::MAX` | half-open range of event time |
| `row_group_rows` | 131,072 | most rows in a row group |
| `batch_rows` | 8,192 | rows gathered before they are handed to the writer; 1 to `MAX_BATCH_ROWS` (1,048,576) |
| `site` | none | site label, written as `kvik.site` and naming the partition folder: 1 to 64 bytes of `a`–`z`, `0`–`9`, `-` and `_`, starting with a letter or digit (`check_site`) |
| `store_id` | none | the store id, written as `kvik.store_id` |

The writer holds the open row group in memory, so the exporter's memory is bounded by `row_group_rows` plus one
batch, whatever the number of facts. The bound holds end to end once `Reader::facts` streams block by block (`api.md`
Reading); today both backends gather the range before the first fact is returned. Rows are written in the order
`Reader::facts` returns them.

The store id is an option because the public `Reader` cannot read it, nor the tzdata version, durable position or
reset stamp, without a side effect (`refresh`). Until the export call joins the store's interface (roadmap 8.3), the
caller passes it; the store-level metadata the store gives itself, and the saved point of a continued export (row 7b,
after 5.3), come then (Q-196 (c)).

## File layout (Q-196)

- One file per series per call, or per series per UTC month in the partitioned layout. Row groups hold at most
  `row_group_rows` rows; an empty export is a valid file with the schema and no row group.
- Columns are compressed with zstd at level 1 (`ZSTD_LEVEL`). `quality` and `supplied_quality` are always
  dictionary-encoded (Q-194); other columns are where the writer finds it pays, so repeated names cost little.
- The contract is the Parquet physical and logical types below. The Arrow schema is embedded as well
  (`ARROW:schema`) for Arrow readers; the crate's own read-back ignores it and reads the Parquet types alone.

Note: zstd is the crate's only C dependency (`zstd-sys`). Snappy or LZ4, both available in pure Rust,
would remove it at some cost in size; every common Parquet reader reads all three.

## Partitioned layout (Q-196)

For consolidating many sites' exports in one bucket, `export_partitioned` writes, under a site label it requires,

```text
site=<site>/series=<series>/month=<YYYY-MM>/<view>.parquet
```

- `<site>` is the site label as given; `check_site` refuses any other byte, so the folder is the same on every file
  system and bucket, whether or not it folds case.
- `<series>` is the series' display name, escaped: bytes `a`–`z`, `0`–`9`, `-` and `_` are kept and every other byte,
  upper-case letters, `/`, `.` and `%` included, is written as `%XX` in upper-case hex (`escape_segment`). The
  escape is reversible (`unescape_segment`), one name has exactly one segment, and two names never share a folder
  even where case is folded. Hive-style readers that percent-decode partition values read the name back; they add
  `site`, `series` and `month` columns, which no file column is named. A name's segment is up to three times its
  length, so a long name can exceed a local file system's 255-byte limit on a file name; and a renamed series is
  filed under its new name from then on, earlier months exported again included, with `kvik.series_id` tying the
  two (Q-761).
- `<YYYY-MM>` is the UTC month of `t`, for every series: `t` is UTC, so the cut needs no tzdata and is the same in
  every release and every site's zone, and months line up across sites. An interval is filed under the month of its
  start. A file holds the facts with `t` in its month and in `range`, and its `kvik.range_from_ns` and
  `kvik.range_to_ns` say which.
- `<view>` is `current`, `history`, `as_of-<generation>-<sequence>` or `as_of_time-<ns>`, so exports in different
  views do not replace one another; a query over the bucket picks one name.
- One file per series per month. Only months with a fact in `range` get a file. `open` is called with each file's
  relative path, in month order; one pass over `range` finds the months, then each month is read and written on its
  own, so each file is as consistent as one `export_series` call. A failure stops the export: the files before it
  are complete, the one it was writing is not.

## Columns (Q-190)

| Column | Parquet type | Null | Content |
|---|---|---|---|
| `series_id` | `INT32`, `INT(32, false)` | never | series id (`spec.md` §2.1) |
| `series_name` | `BYTE_ARRAY`, `STRING` | never | the series' display name at export time |
| `t` | `INT64`, `TIMESTAMP(NANOS, UTC)` | never | event time |
| `generation` | `INT32`, `INT(32, false)` | never | position (`spec.md` §2.2) |
| `sequence` | `INT64`, `INT(64, false)` | never | position |
| `received_at` | `INT64`, `TIMESTAMP(NANOS, UTC)` | never | host clock at commit |
| `value` | by value type, below | exactly when the fact has no value (`spec.md` §2.5) | the value |
| `stored_scale` | `INT32`, `INT(8, false)` | with `value` | decimal series only: the scale the fact was stored at (Q-192) |
| `state_name` | `BYTE_ARRAY`, `STRING` | with `value` | named-state series only: the declared name of the index (Q-194) |
| `quality` | `BYTE_ARRAY`, `STRING` | never | `good`, `estimated`, `suspect` or `bad`, after stamping (Q-194) |
| `supplied_quality` | `BYTE_ARRAY`, `STRING` | never | quality as the adapter supplied it (`spec.md` §2.4) |
| `future`, `late`, `revise`, `host_time`, `unsynced_clock`, `no_value`, `rounded` | `BOOLEAN` | never | the fact flags (`spec.md` §2.2, §2.5; Q-193) |
| `changed_at` | kind `time`: `INT64`, `TIMESTAMP(NANOS, UTC)`; kind `version`: `INT64`, `INT(64, false)` | never | versioned series only (`spec.md` §2.1 `changed_at kind`; Q-195) |
| `interval_end` | `INT64`, `TIMESTAMP(NANOS, UTC)` | never | interval series only: `t + d` (`spec.md` §2.1; Q-197) |

| Value type (`spec.md` §2.1) | `value` column |
|---|---|
| `f64` | `DOUBLE`, bit for bit (`-0.0` is kept) |
| decimal | `FIXED_LEN_BYTE_ARRAY(9)`, `DECIMAL(19, s)`, `s` the series' scale; each value rescaled exactly from its stored scale, which `stored_scale` keeps, so the fact's units and scale come back unchanged (Q-192). Until 6.4b `s` is the definition's scale, so a series raised after definition is refused (`ScaleAboveExport`) once a fact is stored above it |
| boolean state | `BOOLEAN` |
| named state | `INT32`, `INT(16, false)`: the index into the declared names, with `state_name` beside it |
| text | `BYTE_ARRAY`, `STRING` |

A decimal series' scale is at most 18 (`spec.md` §2.1); a series with a higher scale is refused before any row is
written. Calendar-length interval series are not exported yet; neither backend stores them. When they are (roadmap
7.3), each `interval_end` is taken from the interval zone under the tzdata version in force at export, which the
metadata records (Q-197).

Caveats:
- (Q-190) `TIMESTAMP(NANOS)` and unsigned `INT(64, false)` are not read natively by every engine: some read
  nanosecond timestamps only as microseconds or as plain `INT64`, and some read `UINT_64` as signed, so a `sequence`
  or a `changed_at` of kind `version` above `i64::MAX` shows negative there.
- (Q-192) The decimal column's type follows the series' scale, so a raise changes it: files exported before and
  after a raise have `DECIMAL(19, 2)` and `DECIMAL(19, 4)`, say, and may not concatenate without a cast. Until 6.4b
  the exporter knows only the definition's scale (`SeriesInfo`), so it refuses a series with a fact stored above it
  rather than taking the raised scale. Neither backend stores decimal series yet, so this cannot happen today.

## Metadata

File key-value metadata, every value a string:

| Key | Value |
|---|---|
| `kvik.export_version` | `1` (Compatibility) |
| `kvik.series_id`, `kvik.series_name`, `kvik.unit` | the series |
| `kvik.kind` | `gauge`, `counter`, `state`, `text` or `interval` |
| `kvik.value_type` | `f64`, `decimal(s)`, `bool`, `state` or `text` |
| `kvik.timing` | `observed`, `scheduled` or `forecast`; in a forecast series `t` is the valid time and `changed_at` the issue time |
| `kvik.state_names` | named-state series only: the declared names, as a JSON array of strings |
| `kvik.changed_at_kind` | versioned series only: `time` or `version` |
| `kvik.interval_length_ns` | interval series only: `d` in nanoseconds |
| `kvik.view` | `current`, `history`, `as_of <generation>:<sequence>` or `as_of_time <ns>` |
| `kvik.range_from_ns`, `kvik.range_to_ns` | the range of `t` |
| `kvik.site` | the site label, when given |
| `kvik.store_id` | the store id as 32 lower-case hex digits (`format.md` A.17 Keys), when given |

## Errors

An export never writes a null or a rounded value in place of a fact it cannot represent; it fails, and its output
is incomplete: it has no Parquet footer, and `read_rows` refuses it. Options are checked before anything is read. It
fails on: a value that does not match the series' value type, or a state index beyond the declared names; a value
present with the `no_value` flag, or absent without it (`NoValueMismatch`; facts from either backend never do this,
so only rows given to `export_rows` can); a decimal stored above the column's scale, or beyond 19 digits after
rescaling; `changed_at` missing on a versioned series or present on another; a `changed_at` of kind `time` above
`i64::MAX` (ingest refuses one, Q-195, so only rows given to `export_rows` or a log that holds one can, Q-760); an
interval end beyond the timestamp range; a fact of another series; a series kind the schema does not cover, or a
decimal scale above 18 (`UnsupportedSeries`). A site label outside its alphabet, or a partitioned export without one,
is `InvalidOptions`; a file the partitioned export cannot open is `Open`, with its path.
