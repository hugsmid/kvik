# Ingest latency through emit, 765142c against b339ee0

Host: 4 cores; `scripts/bench-emit-ab.sh 765142c <out> 3`; runs alternate base, head.

## Summary

| Median of three runs | Base | Head | Change |
|---|---|---|---|
| Home rate, median per call | 1.42 ms | 1.57 ms | +0.15 ms (+11 %) |
| Home rate, p99 per call | 3.90 ms | 4.36 ms | +0.46 ms |
| Saturation, readings/s | 10,916 | 10,308 | −5.6 % |
| Saturation, median per call | 9.38 ms | 9.84 ms | +0.46 ms |

The head is slower in every one of the three pairs, by less than the spread between runs of one side (base
saturation 10,735 to 12,267 readings/s). Read as a small cost of the dirty set on the commit path (a sorted key list
and a hash insert per pair under the decision lock, a second lock at the acknowledgement), at the edge of this
host's noise; not "unchanged". The host is a shared 4-core machine and both sides were built and run at reduced
priority, so absolute figures are not comparable with the other measurements here.

## base, run 1

```
home: 900 calls, best 535.231µs, median 1.257846ms, p99 3.897404ms, worst 46.183297ms
saturation: 1227 calls, best 584.807µs, median 8.01089ms, p99 12.30101ms, worst 57.14789ms
saturation: 123 calls/s, 12267 readings/s
```

## head, run 1

```
home: 900 calls, best 791.912µs, median 1.493914ms, p99 5.894039ms, worst 16.088393ms
saturation: 1159 calls, best 751.475µs, median 8.766889ms, p99 10.996508ms, worst 19.836957ms
saturation: 116 calls/s, 11589 readings/s
```

## base, run 2

```
home: 900 calls, best 902.866µs, median 1.416698ms, p99 3.751652ms, worst 18.817644ms
saturation: 1074 calls, best 1.218718ms, median 9.667596ms, p99 11.228223ms, worst 31.261002ms
saturation: 107 calls/s, 10735 readings/s
```

## head, run 2

```
home: 900 calls, best 890.67µs, median 1.641955ms, p99 4.363975ms, worst 23.671076ms
saturation: 1016 calls, best 535.063µs, median 9.930917ms, p99 12.133469ms, worst 33.879281ms
saturation: 102 calls/s, 10155 readings/s
```

## base, run 3

```
home: 900 calls, best 882.149µs, median 1.415571ms, p99 4.112618ms, worst 22.457507ms
saturation: 1092 calls, best 1.102259ms, median 9.381595ms, p99 11.341676ms, worst 14.083239ms
saturation: 109 calls/s, 10916 readings/s
```

## head, run 3

```
home: 900 calls, best 854.304µs, median 1.565593ms, p99 3.978659ms, worst 50.840622ms
saturation: 1031 calls, best 879.641µs, median 9.844042ms, p99 12.472118ms, worst 67.979983ms
saturation: 103 calls/s, 10308 readings/s
```

