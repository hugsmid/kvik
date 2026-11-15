# kvik-sink-api

The deep tier's interface for the kvik time-series store (`docs/spec.md` §8, `docs/api.md` Backup and restore):

- `BackupSink`, an object store with no overwrite and no delete: `put` (create if absent, with the SHA-256),
  `stat`, ranged `get` and paged `list`, each returning a boxed `Send` future that needs no runtime;
- `Stored`, `ListPage`, `SinkError` and `SinkFuture`, what the trait reports and how it fails;
- `Verified`, the proof that the bucket reported the expected size and SHA-256, and `VerifyError`.

A leaf crate with no other kvik dependency, so that the store, the sinks (`kvik-sink`) and the backup driver use one
trait. `kvik` re-exports every item unchanged. The object layout in the bucket is `docs/format.md` A.17. Not
published.
