# kvik-export

Parquet export of a kvik store's facts (roadmap 6.4). The schema, export version 1, is in
[`docs/export.md`](../docs/export.md), with the decisions behind it (Q-190 to Q-197 in
[`model/OPEN-QUESTIONS.md`](../model/OPEN-QUESTIONS.md)).

- `export_series(&reader, id, &options, writer)` streams one series' facts from `Reader::facts` into one Parquet
  file; the exporter holds at most one row group and one batch of rows (end to end once `Reader::facts` streams).
- `export_partitioned(&reader, id, &options, open)` writes one file per UTC month with a fact, at
  `site=<site>/series=<series>/month=<YYYY-MM>/<view>.parquet` (a month cut by the range gets a `.partial-` name);
  `options.site` is required. Both calls write the reader's store id (`Reader::store_id`) to every file's metadata,
  and the partitioned one the site label.
- `export_rows` does the same as `export_series` for rows from any source; `SeriesSchema::from_info` gives a series'
  schema.
- `read_rows` reads a file back into `ExportRow`s from its Parquet types alone.

Built on the `parquet` crate (Apache-2.0) with only its `arrow` and `zstd` features.

```sh
cargo test -p kvik-export
```
