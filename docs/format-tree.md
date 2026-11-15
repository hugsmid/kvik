# kvik file format, v1: summary nodes

The bytes of one stored summary node (`spec.md` §5.2, §5.6; `format.md` Part B, tree; FC-34). Written by roadmap
7.1a and implemented by the `kvik-tree` crate. The tree file around the nodes (levels, the checkpoint position, each
node's version and `summary_stale` flag) is laid out by roadmap 7.1b. The common rules of `format.md` Part A apply;
where this document and `spec.md` differ, `spec.md` wins.

## T.1 Scope
A stored node is the fold of its readings in `t` order (a 1-minute node) or the left-to-right merge of its children
(`spec.md` §5.6), and its bytes are a function of those readings and the series parameters alone, so that a stored
node, a rebuilt one and a chunk summary over the same readings (roadmap 6.2) compare byte for byte (invariants 5
and 8). The node version and `summary_stale` are not functions of the readings, so they belong to the record that
holds a node, not to its bytes. A window evaluation (`spec.md` §6: edge stretches, a held value with no reading
inside, an apportioned amount) has no stored form; an encoder refuses every one the checks of T.6 can tell from a
fold (a stretched window whose stretches keep within its first and last readings cannot be told apart).

Decimal series (`spec.md` §5.6: `i128` sums at the node's `u8` scale) take form ids of their own, laid out with
decimal series in the durable backend; they are additions under the critical rule, not a new major version.

Notation: `i64` and `u16` are little-endian; `uv` is a minimal unsigned LEB128 (`format-chunk.md` C.2) of at most
64 bits (`uv64`) or 127 bits (`uv127`); `f64` is the 8-byte bit pattern, little-endian, finite.

## T.2 Node
| Field | Encoding | Notes |
|---|---|---|
| `form` | `u8` | 1 gauge, 2 counter, 3 state, 4 text, 5 interval; every other id is reserved and critical (a version refusal) |
| `start` | `i64` | span start |
| `width` | `uv64` | `end − start`; `end` fits `i64` |
| `count` | `uv64` | readings in the span |
| `coverage` | `uv127` | nanoseconds with a valid value |
| `first_dt` | `uv64` | `count ≥ 1`: first `t` − `start` |
| `first` | value (T.4) | `count ≥ 1` |
| `last_dt` | `uv64` | `count ≥ 2`: last `t` − first `t`, at least 1 |
| `last` | value (T.4) | `count ≥ 2`; with one reading, last is first |
| body | T.5 | per form |

The node ends where its body ends; its length comes from the record that holds it.

## T.3 Exact numbers
Sums, integrals and increases are exact (Q-180). Every one a fold or merge gives is dyadic: a finite `f64` is
m × 2^e with e ≥ −1074, and the fields are sums and differences of `f64` values and their products with whole
nanoseconds. So each is stored as ± mantissa × 2^`exp` with an odd mantissa:

| Field | Encoding | Notes |
|---|---|---|
| `sign` | `u8` | 0 zero (nothing follows), 1 positive, 2 negative |
| `exp` | `i16` | at least −1074 |
| `len` | `uv64` | mantissa bytes, 1 to 272 |
| `mantissa` | `len` bytes, little-endian | odd; last byte not zero |

The magnitude is below 2^1100: a node holds fewer than 2^64 readings and covers less than 2^64 ns, so a sum is below
2^1088, an integral below 2^1088 and an increase below 2^1090. Mantissa and exponent therefore take at most 272
bytes; values of similar magnitude, the usual case, take a few. A 15-minute node of a gauge read every 5 s takes about
80 bytes in all (`gauge-quarter.node`); a node holding both 5e−324 and 1e300 takes about 600.

## T.4 Values and state keys
By form: gauge, counter and interval values are `f64`. A state value and a state key are a tag, `u8` 0 false, 1 true,
2 a named state followed by its `u16` index among the series' declared names (`format.md` A.5). A text value is a
`u8` length and that many bytes of UTF-8.

## T.5 Bodies
| Form | Body |
|---|---|
| gauge | `sum` (T.3), `integral` in value × ns (T.3); with `count ≥ 1`, `min` and `max` (`f64`) |
| counter | `increase` (T.3), `resets`, `regressions`, `long_steps` (each `uv64`); the steps are `count − 1` and the elapsed time last `t` − first `t`, so neither is stored |
| state | `transitions` (`uv64`), `entries` (`uv64`), then per state in ascending key order its key (T.4) and its time in ns (`uv127`, at least 1) |
| text | nothing |
| interval | `sum` (T.3); with `count ≥ 1`, `min` and `max` (`f64`) |

Key order: false, true, then named states by index. Min and max are in IEEE total order, so −0.0 is below +0.0
whatever the order of folding (Q-181).

**Canonical writing.** Every field has one encoding, so a node has one byte sequence; decoding any accepted input and
encoding it again gives the same bytes. Golden files rely on it.

## T.6 Checks
A reader refuses, naming the offset: bytes ending inside a field, or bytes after the node; a `uv` not in its shortest
form or out of range; a reserved form (`UnsupportedForm`, a version refusal); an `end` beyond `i64`; coverage with no
readings; coverage beyond last `t` − first `t` on any form but interval (holds lie between consecutive readings); a
first `t` outside the span; a last `t` not after the first or not before `end`; a value or bound that is not finite;
text that is not UTF-8; an unknown state tag; an exact number with an unknown sign, an even mantissa, a mantissa with
a zero last byte, a length outside 1 to 272 or a magnitude outside T.3; a gauge or interval sum with no readings; a
gauge integral with no coverage; first or last outside min and max, or min and max that differ with one reading; a
counter increase with no step, or more resets and regressions, or long steps, than steps; more state transitions than
steps; states not in ascending order, a state time of zero, or state times that do not add up to the coverage. An
encoder refuses every node a reader would refuse.

## T.7 What an implementation must test
Merge associative and the empty node its identity, bitwise; folding any split of readings equals folding them all;
fold, merge and the window credit equal the reference model's; every folded or merged node round-trips; golden files
of every form and an unknown form; a fuzz target whose accepted inputs encode to themselves.
