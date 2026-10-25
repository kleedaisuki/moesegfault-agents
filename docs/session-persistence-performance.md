# Session persistence: remove copies, not user history

## Workload and initial evidence

This work targets already tested session persistence at baseline `6552a93`, not a
new conversation representation. The representative workload is a long agent
session whose source journal survives compaction: 128 complete tool rounds with
4 KiB or 32 KiB tool output, followed by a small summary or sixteen recent rounds
in the active prompt. These are synthetic controls, not customer distributions.

[The existing history probe](session-history.md) measured a 4,455,014-byte source
JSON representation and a native/debug snapshot-clone median of 2.534 ms for the
32 KiB case. Those measurements are historical native CPU results, not Worker
latency or a prediction of this change's benefit. They established a concrete
copying cost worth removing before considering a broad copy-on-write redesign.

Inspection found two unnecessary deep copies of the same source:

1. `persist_with_observation` already owns a snapshot, but `save_state` cloned it
   again only to refresh `journal_count`. The journal is `#[serde(skip)]`, yet
   ordinary `Clone` still copies every source message and tool-output string.
2. `arm_alarm` cloned the complete session to inspect only status, flags, pending
   counts and the input clock. Its pure deadline function never reads source text.

For the 32 KiB control, each removed copy contains at least 4 MiB of stdout alone;
message metadata, active context, run state, allocation overhead and deallocation
are additional. A persist-plus-alarm sequence previously made three source
snapshot copies and now retains only its original coherent persistence snapshot.
This is a source-level copy analysis, not a measured heap/RSS reduction.

## Data ownership and implementation

`save_state` now accepts a mutable **owned snapshot**, refreshes its derived count
in place and serializes that same struct through Serde. No live `RefCell` borrow
crosses an await. No duplicate persistence DTO or handwritten list of fields can
silently omit fields added by subsequent Responses work.

The live journal remains a complete append-only vector. `load_state` rehydrates
all committed entries; live appends can advance `journal.len()` before the cached
`journal_count` changes. Therefore the count comes from the actual owned journal,
not from that potentially stale cached field. Updating the owned snapshot does
not claim to synchronize the separate live cache's count.

The existing persistence mutex, entry ordering, chunk-generation publication and
state publication order remain unchanged. The separately stored `journal:count`
pointer is written last **only when it changes**. An unchanged count needs no put;
an absent pointer still means zero as before. New source entries and retries after
an interrupted publication retain their existing write/recovery behavior.

Alarm scheduling computes the existing pure `alarm_deadline` under a short
immutable borrow, drops that borrow, then performs the native storage operation.
It does not alter deadlines, user-input activity, run ownership or billing fences.

No source entry, pinned configuration, immutable response, financial record or
workspace version is removed. The encoded state format is byte-identical to the
previous clone-and-serialize path. The change reduces copying and redundant API
work; it does not reduce the required durable user-data footprint.

## Verification and quantitative comparison

`session_persistence_perf.rs` uses the actual `PersistedSession`, provider
`Message`/`ToolCall`, production `encode_state` and production `alarm_deadline`.

- A 32-case Unicode property verifies byte-identical encoded state against the
  previous implementation, despite arbitrary stale counts. Pending steering,
  compacted prompt and projection metadata remain intact.
- Three deterministic cases compare old/new state encoding and alarm calculation:
  4 KiB outputs with a compact summary, 32 KiB outputs with that summary, and
  32 KiB outputs with sixteen recent inference rounds.
- Each CPU comparison uses three warmups and 25 observations, recording p50/p95.
  Timing is descriptive, not a flaky acceptance threshold.
- Both paths must encode identical bytes and produce identical alarm deadlines.

The regular Worker library test writes
`.cache/perf/v3/runtime-persistence.json`, collected by the existing
`performance-v3` artifact. It records only synthetic sizes and timings, not user
text, identity, paths or credentials. Run it in the existing cached Actions job:

```sh
cargo test --locked -p agents-worker --lib persistence_perf -- --nocapture
```

[CI 40186260916](https://github.com/kleedaisuki/moesegfault-agents/actions/runs/40186260916)
completed successfully at exact source
`1c2620161419b6ad7f6c1aeb55db689315202c59`, including all six normal jobs,
260 Worker library tests and 47 actual-Worker checks. Both new persistence tests
passed, including the 32-case Unicode equivalence property. Its `performance-v3`
artifact contains the following **same-run Linux/x86-64 native debug** comparison:

| Tool output / recent active rounds | Encoded state | Previous encoding p50 | New encoding p50 | Change |
| --- | ---: | ---: | ---: | ---: |
| 4 KiB / summary only | 2,285 B | 0.377265 ms | 0.150757 ms | -60.04% |
| 32 KiB / summary only | 2,285 B | 0.653191 ms | 0.146651 ms | -77.55% |
| 32 KiB / sixteen rounds | 531,303 B | 33.538239 ms | 32.895154 ms | -1.92% |

The latter case explains the limit: encoding a large active prompt still dominates;
removing a redundant historical clone does not eliminate serialization of the
required prompt. There is no claim that every persistence call gets a 77% speedup.
For the 32 KiB cases, the two removed clone paths together contain at least
8,426,592 bytes of UTF-8 source values per save-plus-alarm sequence, excluding
allocation/container overhead and other state. That is a source-level copy
quantity, not observed peak RSS. Alarm calculation changed from a 0.496371 ms
snapshot median in the summary-only case to a borrowed calculation below 0.001 ms;
the tiny latter measurement is near timer overhead and is not a useful speedup SLO.

No cold local Rust build or paid-model probe was started for this work. Native
timing excludes Worker Wasm, KV latency, network, model time and real allocator/RSS
measurements. Do not report the removed copy lower bound as a measured end-to-end
speedup. The source encoding format, byte equality and cold/steering/terminal
recovery contracts passed the complete compiled suite, not only this microprobe.

Cloudflare's [SQLite storage API](https://developers.cloudflare.com/durable-objects/api/sqlite-storage-api/)
is strongly consistent, and its [pricing contract](https://developers.cloudflare.com/durable-objects/platform/pricing/)
counts KV methods through the hidden SQLite table. Batched/cache hits are not a
license to assume zero storage cost. The counter optimization's immediate claim
is one fewer `put` invocation on unchanged publications; native invocation counts
and actual provider billing must not be conflated. Serde's
[field attributes](https://serde.rs/field-attrs.html) define `skip` serialization;
they do not change the derived Rust clone operation.

## Native storage-call baseline

A test-only Session subclass wrapped the real native SQLite storage binding and
delegated all other behavior to the retained `40175322441` Wasm artifact, whose
state persistence mechanism matches the pre-optimization baseline. Two ordinary
non-background Responses turns made exactly two controlled provider dispatches
and retained four source messages at input revision two. The source, revision and
last-user-input clock were unchanged by reading the probe counters.

| Native API operation | Before | Exact-source candidate |
| --- | ---: | ---: |
| State generation publications | 21 | 21 |
| Journal counter puts | 21 | 4 |
| Journal counter puts repeating its stored value | 17 | 0 |
| Final source counter | 4 | 4 |

The baseline report is `.cache/perf/runtime/storage-counter-baseline.json`; the
temporary native seam and reproduction instructions are under
`.temp/runtime-perf/`. It uses no real provider, staging principal or external
network. It is not a persistence emulator or production fault endpoint.

The candidate probe passed against the downloaded **exact** `1c262016` compiled
Worker from CI, not the concurrently modified full-API working files. It required
zero unchanged counter puts, at most four source counter publications, exactly two
provider calls, four unchanged source messages and unchanged input revision/clock.
Counter puts fell 21 to 4 (80.95% fewer native API invocations), while all 21
canonical state publications remained. This is lower write amplification, not
elision of acknowledged state or a claim of a measured 80.95% bill reduction.

Receipts are `.cache/perf/runtime/storage-counter-baseline.json` and
`storage-counter-after-1c2620.json`; comparison logs and downloaded CPU measurements
are in that directory. Every label is exclusively created, preserving evidence.
One subsequent old-artifact control timed out under local contention before its
counter assertion; it is not counted as a successful negative control. The probe
now gives its single accepted operations a 30-second observation budget without
changing data/execution assertions or retrying an accepted creation. The successful
candidate disposed Miniflare and cleaned up its synthetic principal normally.
