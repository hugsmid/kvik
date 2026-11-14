# kvik

The public Rust interface of the kvik store, as outlined in [`docs/api.md`](../docs/api.md) and governed by the
[specification](../docs/spec.md). Applications can write adapters and consumers against it today; the durable log
engine will replace the backend behind the same interface (roadmap section 4 onwards).

## Backend: in-memory, not durable, for development and tests

> [!WARNING]
> **Nothing is written to disk yet.** The only backend, behind the default feature `model-backend`, is the
> executable [reference model](../model/): exact, slow and entirely in memory. Every store lives in the memory of the
> current process and is lost when the process ends.

`Store::open` takes a directory but only uses it as a name. Within one process, opening the same directory again after
`Store::shutdown` (or after dropping the store, which acts as a crash) restarts the store as it was left, so restarts,
catch-up, replay and `resync` can be exercised in tests. `Reader::open` finds a store opened in the same process.

The interface is asynchronous but needs no particular runtime and starts no threads. Timers (window expiry, held-for
and stale-for conditions, scheduled values taking effect) run when the store is called: before every emit and
delivery poll, and on `Store::tick`. A waiting `Consumer::next` is woken by the next emit, routing change or tick.

## Example

```sh
cargo run -p kvik --example quickstart
```

[`examples/quickstart.rs`](examples/quickstart.rs) opens a store, defines a gauge and a counter, emits readings,
receives and confirms a threshold condition's transition, and queries a rolling window and a calendar day.

## Coverage

What the model backend supports:

| Area | Supported |
|---|---|
| Opening | `Store::open` and `shutdown`, one writer per directory, `Reader::open`, site-zone check, restart |
| Environment | `Clock`, `Fs` and `BootId` seams with production and test implementations; the backend uses the clock only |
| Series | gauge, counter, boolean and named state, text, fixed-length interval series with a phase; expected interval, staleness, record on change, counter rules, lateness bound, versioned, timing, retention class; `alias`, `retire`, `series` by id or name |
| Writing | `emit`, `emit_with` (`Intent::Revise`), `try_emit`; refusals per index; back-pressure (queue full, unsynchronised clock for host-stamped readings, critical disk) |
| Consuming | consumers by name, funnels by ids, name pattern or all; threshold, on-change, held-for and stale-for conditions, with or without a rolling operator; `next`, `try_next`, `confirm`; `replayed`, `future` and `outranked` flags; `resync` with live and condition state; timer deliveries |
| Reading | live state with staleness; rolling, relative, absolute and calendar windows in the site zone or a named zone; calendar day and month results; facts in the current, as-of (position or time) and history views; chart buckets; the interval containing a time |
| Health | health with reasons; statistics per store, series and consumer |

What returns `Error::NotSupportedYet`, or is never produced:

- decimal value types (`ValueType::Decimal`) and calendar-length interval series (a local day or month, with a day
  start);
- parameter changes other than the retention class;
- a new consumer starting at a position or a time (`Start::Position`, `Start::Time`);
- a funnel with an operator but no condition, and a held-for condition over an operator;
- backup, restore, export of set-aside facts and rebuild;
- the `Unknown` live and condition states, `approximate_edges`, `displaced`, a trigger's value and coverage, commit
  latency, backup lag and restore progress, which need the durable engine, tiers or restore.

Name patterns in a funnel's selection work, but their grammar is provisional (`*` within one `/`-separated segment),
as the specification does not define one yet ([Q-88](../model/OPEN-QUESTIONS.md#q-88--the-grammar-of-name-patterns)).
`Store::emit` waits under back-pressure only for clock synchronisation; a batch larger than the ingest queue, or free
space below the critical mark, returns `Error::Busy`, as nothing in this backend would end the wait.
