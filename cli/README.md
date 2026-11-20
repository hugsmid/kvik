# kvik-cli

`kvik`, the read-only command-line tool for a kvik store directory. It opens a reader, never the store: in the
store directory it writes only its lease under `leases/`, released when it ends. Without write access to `leases/`
it refuses with the reason (spec §7 Readers).

```text
kvik series  --store <dir> [pattern...]
kvik latest  --store <dir> [pattern...] [--live] [--for 10s]
kvik tail    --store <dir> [pattern...] [--interval 200ms] [--for 10s]
kvik health  --store <dir>
kvik info    --store <dir>
```

Every command takes `--format table|csv|jsonl` and `--zone <zone>` (times are UTC otherwise); `KVIK_STORE` stands
in for `--store`. A pattern has three `/`-separated segments, each a literal or `*`. A tail opens at the newest
durable position; a tail from a time waits for the as-of time of roadmap 9.0f.
