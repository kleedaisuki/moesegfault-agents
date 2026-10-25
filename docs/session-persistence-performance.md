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

New-source CPU measurements and compiled-Worker compatibility remain pending until
the root's next coherent CI checkpoint. No cold local Rust build or paid-model
probe was started for this work. Native timing excludes Worker Wasm, KV latency,
network, model time and real allocator/RSS measurements. Do not report the removed
copy lower bound as a measured end-to-end speedup.

Cloudflare's [SQLite storage API](https://developers.cloudflare.com/durable-objects/api/sqlite-storage-api/)
is strongly consistent, and its [pricing contract](https://developers.cloudflare.com/durable-objects/platform/pricing/)
counts KV methods through the hidden SQLite table. Batched/cache hits are not a
license to assume zero storage cost. The counter optimization's immediate claim
is one fewer `put` invocation on unchanged publications; native invocation counts
and actual provider billing must not be conflated. Serde's
[field attributes](https://serde.rs/field-attrs.html) define `skip` serialization;
they do not change the derived Rust clone operation.
