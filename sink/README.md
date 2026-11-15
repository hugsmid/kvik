# kvik-sink

Backup sinks for the kvik time-series store (roadmap 8.2): an implementation of `kvik::BackupSink` over
S3-compatible object storage, and an in-memory fake bucket for tests.

- **`S3Sink`**: Signature Version 4 in the `Authorization` header; path-style or virtual-host addressing. Uploads are
  single-part and conditional (`If-None-Match: *`), carry the SHA-256 as `x-amz-checksum-sha256` and the payload hash,
  and are followed by a `HEAD` whose reported size and SHA-256 are what `put` returns; a retry asks first and does not
  send the body again if the object is there. An existing object with the same size and hash makes a repeated `put`
  succeed. Other content is never overwritten: it is `SinkError::Exists` while no attempt of the call can have reached
  the bucket, and `HashMismatch` after one that may have landed. Whole downloads must carry the reported hash and match
  it, else `HashMismatch` (not retried) when the body has its declared length, `Transient` when it has none; ranges are
  half-open. Listings are paged (`start_after`) and must be listings. An absent key is `None` from `stat` only when the
  bucket answers that it exists, asked on each 404 to a `HEAD`, and `NotFound` from `get` only on a 404 with the code
  `NoSuchKey`. Calls run on the sink's worker threads (`kvik-sink-N`) behind a bounded queue (`Busy` when full), with
  connect and exchange timeouts and retries with exponential backoff and jitter; the returned futures never block and
  need no runtime. Objects are held whole in memory, up to `max_object_size` (default 256 MiB; a larger download is
  `TooLarge`); other response bodies are capped at 16 MiB. `http://` is accepted only for a loopback host unless
  `allow_plain_http` is set.
- **`fake::FakeBucket`**: the same protocol subset in memory, at the transport seam. It checks signatures, payload
  hashes and checksums, honours conditional writes, serves ranges and pages listings, and injects faults: server errors
  (also with a given error code), timeouts before and after a request is applied, requests held unanswered, cut uploads,
  truncated downloads, a bucket that stores other bytes, one that trusts the sent hash, one that reports none, damage in
  the bucket and lagging listings.
- **`verify`**: `upload` and `check` give a `kvik::Verified` only after the bucket reports the expected hash;
  `download` checks the bytes.

The bucket must honour conditional writes, check and report whole-object SHA-256 checksums, and have versioning or
object lock (spec §8 Bucket); spec §13 lists confirming this as a risk to measure. There is no multipart upload: a
multipart object's checksum is not the SHA-256 of its bytes, and the specification asks for a single-part upload,
so objects above 5 GiB are refused.

```sh
cargo run -p kvik-sink --example fake_bucket
```

Tests never leave the machine: most run against the fake in memory, and two run the HTTP transport against the fake
served on the loopback interface.
