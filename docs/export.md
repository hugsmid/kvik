# kvik Parquet export, version 1

`spec.md` §11 leaves the Parquet export schema to a separate document; this is it. The schema is a promise to
users: the owner settled it on 2026-11-16 (Q-190 to Q-197, `model/OPEN-QUESTIONS.md`). The crate
[`kvik-export`](../export/) implements it (roadmap 6.4a); roadmap 6.4b adds the differential check of compacted data
against it. Parquet is the export format only (`design.md`); stored files keep the own format.

## Compatibility (Q-190)

Every file carries `kvik.export_version` = `1`. Adding a column or a metadata key is not a breaking change and keeps
the version, so a reader should ignore columns and keys it does not know. Renaming, retyping or removing a column or
a metadata key raises the version. No file column may be named `site`, `series` or `month`: Hive-style readers of the
partitioned layout add columns of those names.

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
| `site` | none | site label, written as `kvik.site` and naming the partition folder: 1 to 64 bytes of `a`–`z`, `0`–`9`, `-` and `_`, starting with a letter (`check_site`) |
| `store_id` | the reader's | written as `kvik.store_id`; `export_series` and `export_partitioned` take it from `Reader::store_id` and refuse a different one; `export_rows`, which has no reader, writes it only when given |

The writer holds the open row group in memory, so the exporter's memory is bounded by `row_group_rows` plus one
batch, whatever the number of facts. The bound holds end to end once `Reader::facts` streams block by block (`api.md`
Reading); today both backends gather the range before the first fact is returned. Rows are written in the order
`Reader::facts` returns them.

Every file `export_series` or `export_partitioned` writes carries the store id, which `Reader::store_id` gives
without reading a file (`api.md` Reader processes), and every partitioned file the site label. The rest of the
store-level metadata (tzdata version, durable position, reset stamp) is not reachable through the public `Reader`
without a side effect (`refresh`); it comes with the export call in the store's interface (roadmap 8.3), and the
saved point of a continued export (row 7b) after 5.3 (Q-196 (c)).

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
site=<site>/series=<series>/month=<YYYY-MM>/<view>.parquet                          a whole month
site=<site>/series=<series>/month=<YYYY-MM>/<view>.partial-<from_ns>_<to_ns>.parquet   a month clipped by range
```

- `<site>` is the site label as given; `check_site` refuses any other byte, so the folder is the same on every file
  system and bucket, whether or not it folds case, and its first letter keeps a reader from taking it for a number.
- `<series>` is the series' display name, escaped: bytes `a`–`z`, `0`–`9`, `-` and `_` are kept and every other byte,
  upper-case letters, `/`, `.` and `%` included, is written as `%XX` in upper-case hex (`escape_segment`). The
  escape is reversible (`unescape_segment`), one name has exactly one segment, and two names never share a folder
  even where case is folded. DuckDB, pyarrow and polars decode it back to the name, non-ASCII names included; Hive
  and Spark were not checked, and may decode a multi-byte character one byte at a time (`kvik.series_name` in the
  footer is exact either way). A name's segment is up to three times its length, so a long name can exceed a local
  file system's 255-byte limit on a file name; and a renamed series is filed under its new name from then on,
  earlier months exported again included, with `kvik.series_id` tying the two (Q-761).
- `<YYYY-MM>` is the UTC month of `t`, for every series: `t` is UTC, so the cut needs no tzdata and is the same in
  every release and every site's zone, and months line up across sites. An interval is filed under the month of its
  start.
- `<view>` is `current`, `history`, `as_of-<generation>-<sequence>` or `as_of_time-<ns>`, so exports in different
  views do not replace one another; a query over the bucket picks one name.
- A month wholly inside `range` is written as `<view>.parquet` and holds the whole month. A month that `range` cuts
  is written as `<view>.partial-<from_ns>_<to_ns>.parquet`, its bounds those of the clipped range (signed decimal
  nanoseconds, as `kvik.range_from_ns` and `kvik.range_to_ns` hold them), so a partial export never replaces a whole
  month's file. A reader takes whole months by the exact name (`<root>/**/current.parquet`) and tells a partial file
  by `.partial-` in its name or by its range keys. An export that should replace whole months passes a range of whole
  months.
- One file per series per month. Only months with a fact in `range` get a file. `open` is called with each file's
  relative path, in month order.

Reading the bucket: a query reads one file name, here the whole months of the current view, since a folder can also
hold other views and partial months whose rows would be counted again. Hive-style readers add `site`, `series` and
`month` columns and guess their types, so an all-digit escaped series name such as `0042` is read as the number 42.
Declare the three as strings:

```text
DuckDB:  read_parquet('<root>/**/current.parquet', hive_partitioning = true, hive_types = {'site': VARCHAR, 'series': VARCHAR, 'month': VARCHAR})
pyarrow: pyarrow.dataset.dataset(glob.glob(root + "/**/current.parquet", recursive=True), format="parquet", partitioning=pyarrow.dataset.partitioning(pyarrow.schema([("site", pyarrow.string()), ("series", pyarrow.string()), ("month", pyarrow.string())]), flavor="hive"), partition_base_dir=root)
polars:  polars.scan_parquet(root + "/**/current.parquet", hive_partitioning=True, hive_schema={"site": polars.String, "series": polars.String, "month": polars.String})
```

Consistency and cost: one pass over `range` finds the months, then each month is read and written on its own. Each
file is as consistent as one `export_series` call, but the files of one call are not one cut of the store: a reader
sees commits made while the call runs, so a later month's file can hold a fact committed after an earlier month's
file was written, and a month whose first fact is committed between the two passes gets no file until the next
call. The range is read twice, and the first pass holds the whole range in memory until `Reader::facts` streams
(Options), so a call over a series' whole history reads it twice; a periodic export should pass the months it means
to refresh.

Publishing: on success and on failure alike the writer `open` returned is dropped, with no signal between. A failure
stops the export: the files before it are complete, the one it was writing has no footer. So `open` should hand out a
temporary name or an in-memory buffer, and the caller publishes the paths in the returned list once the call returns
`Ok`, and discards everything on `Err`; a writer that truncates the final path, or uploads when dropped, can replace
a good file with a broken one. A per-file completion signal is left to the export call in the store's interface
(roadmap 8.3).

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
| `kvik.site` | the site label: in every partitioned file, and in others when given |
| `kvik.store_id` | the store id as 32 lower-case hex digits (`format.md` A.17 Keys): in every file of `export_series` and `export_partitioned`, and from `export_rows` when given |

## Errors

An export never writes a null or a rounded value in place of a fact it cannot represent; it fails, and its output
is incomplete: it has no Parquet footer, and `read_rows` refuses it. Options are checked before anything is read. It
fails on: a value that does not match the series' value type, or a state index beyond the declared names; a value
present with the `no_value` flag, or absent without it (`NoValueMismatch`; facts from either backend never do this,
so only rows given to `export_rows` can); a decimal stored above the column's scale, or beyond 19 digits after
rescaling; `changed_at` missing on a versioned series or present on another; a `changed_at` of kind `time` above
`i64::MAX` (ingest refuses one, Q-195, so only rows given to `export_rows` or a log that holds one can, Q-760); an
interval end beyond the timestamp range; a fact of another series; a series kind the schema does not cover, or a
decimal scale above 18 (`UnsupportedSeries`). A site label outside its alphabet, a partitioned export without one, or
a store id other than the reader's is `InvalidOptions`; a file the partitioned export cannot open is `Open`, with its
path.
