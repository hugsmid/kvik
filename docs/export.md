# kvik Parquet export, v0 (provisional; owner decision pending)

`spec.md` §11 leaves the Parquet export schema to a separate document; this is it. The schema is a promise to
users, so every choice below is provisional until the owner decides the questions Q-190 to Q-197
(`model/OPEN-QUESTIONS.md`), each recorded with a recommendation. The crate [`kvik-export`](../export/) implements
this draft (roadmap 6.4a); roadmap 6.4b adds the differential check of compacted data against it. Parquet is the
export format only (`design.md`); stored files keep the own format.

## Scope

One call exports the facts of one series with `t` in a half-open range, in one view (`spec.md` §2.3), read through
`Reader::facts`, into one Parquet file: one row per fact. Orphaned facts (`api.md` `export_orphaned`) and facts set
aside by a restore (`export_set_aside`) stay separate calls that return facts for re-ingest (Q-191).

## Options

| Option | Default | Meaning |
|---|---|---|
| `view` | `Current` (Q-191) | `Current`, `AsOf(P)`, `AsOfTime(T)` or `History` |
| `range` | every `t` below `i64::MAX` | half-open range of event time |
| `row_group_rows` | 131,072 | most rows in a row group |
| `batch_rows` | 8,192 | rows gathered before they are handed to the writer |

The writer holds the open row group in memory, so the exporter's memory is bounded by `row_group_rows` plus one
batch, whatever the number of facts. The bound holds end to end once `Reader::facts` streams block by block (`api.md`
Reading); today both backends gather the range before the first fact is returned. Rows are written in the order `Reader::facts` returns them.

## File layout (Q-196)

- One file per series per call. Row groups hold at most `row_group_rows` rows; an empty export is a valid file with
  the schema and no row group.
- Columns are compressed with zstd at level 1 (`ZSTD_LEVEL`) and dictionary-encoded where the writer finds it
  pays, so repeated names and quality strings cost little.
- The contract is the Parquet physical and logical types below. The Arrow schema is embedded as well
  (`ARROW:schema`) for Arrow readers; the crate's own read-back ignores it and reads the Parquet types alone.

Caveat (Q-196): zstd is the crate's only C dependency (`zstd-sys`). Snappy or LZ4, both available in pure Rust,
would remove it at some cost in size; every common Parquet reader reads all three.

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
| decimal | `FIXED_LEN_BYTE_ARRAY(9)`, `DECIMAL(19, s)`, `s` the series' scale; each value rescaled exactly from its stored scale, which `stored_scale` keeps, so the fact's units and scale come back unchanged (Q-192) |
| boolean state | `BOOLEAN` |
| named state | `INT32`, `INT(16, false)`: the index into the declared names, with `state_name` beside it |
| text | `BYTE_ARRAY`, `STRING` |

A decimal series' scale is at most 18 (`spec.md` §2.1); a series with a higher scale is refused before any row is
written. Calendar-length interval series are not exported yet; neither backend stores them (Q-197).

Caveats:
- (Q-190) `TIMESTAMP(NANOS)` and unsigned `INT(64, false)` are not read natively by every engine: some read
  nanosecond timestamps only as microseconds or as plain `INT64`, and some read `UINT_64` as signed, so a `sequence`
  or a `changed_at` of kind `version` above `i64::MAX` shows negative there.
- (Q-192) The decimal column's type follows the series' scale, so a raise changes it: files exported before and
  after a raise have `DECIMAL(19, 2)` and `DECIMAL(19, 4)`, say, and may not concatenate without a cast.

## Metadata

File key-value metadata, every value a string:

| Key | Value |
|---|---|
| `kvik.export_version` | `0` while this schema is provisional (Q-190) |
| `kvik.series_id`, `kvik.series_name`, `kvik.unit` | the series |
| `kvik.kind` | `gauge`, `counter`, `state`, `text` or `interval` |
| `kvik.value_type` | `f64`, `decimal(s)`, `bool`, `state` or `text` |
| `kvik.timing` | `observed`, `scheduled` or `forecast`; in a forecast series `t` is the valid time and `changed_at` the issue time |
| `kvik.state_names` | named-state series only: the declared names, as a JSON array of strings |
| `kvik.changed_at_kind` | versioned series only: `time` or `version` |
| `kvik.interval_length_ns` | interval series only: `d` in nanoseconds |
| `kvik.view` | `current`, `history`, `as_of <generation>:<sequence>` or `as_of_time <ns>` |
| `kvik.range_from_ns`, `kvik.range_to_ns` | the range of `t` |

## Errors

An export never writes a null or a rounded value in place of a fact it cannot represent; it fails, and its output
is incomplete. It fails on: a value that does not match the series' value type, or a state index beyond the
declared names; a decimal stored above the column's scale, or beyond 19 digits after rescaling; `changed_at` missing
on a versioned series or present on another; a `changed_at` of kind `time` above `i64::MAX` (Q-195); an interval
end beyond the timestamp range; a fact of another series; a series kind the schema does not cover, or a decimal
scale above 18 (`UnsupportedSeries`).
