# Source history, snapshots and incremental reads

## Current contract, checked at 71d1e3b

The session has two distinct sequences:

- `PersistedSession.messages`: the bounded inference view, including the pinned
  system prefix and derived compaction summary.
- `PersistedSession.journal`: append-only source messages, including user appends,
  assistant tool calls, tool results and final responses. Compaction never replaces it.

**`GET /v1/sessions/{id}` already exposes the source journal as `messages`.** It
does not expose a second `source_journal` field or send both source and compacted
prompt. The dashboard renders that source `messages` array; compaction therefore
does not make prior turns disappear. The real-provider compaction acceptance test
checks that the old source prefix and original payload remain unchanged.

The remaining costs are concrete:

1. `snapshot()` clones both the active prompt and complete journal, including paths
   that need only a record/run field. `save_state()` clones again to change the
   stored journal count even though `journal` is skipped by Serde serialization.
2. `/status` clones the snapshot, builds an owned JSON value of the journal, then
   serializes it. Each successful full poll transfers the complete journal again.
3. The measured baseline cold `load_state()` read every journal entry's index,
   then each chunk, in serial. The bounded bulk-read change described below now
   addresses that cost without changing journal keys or the public API.

Do not fix these by making `messages` mean a compacted prompt again, truncating
default history, or silently omitting tool/reasoning continuation fields. Those
would break existing SDK/CLI consumers and the source-journal acceptance contract.

## Bounded, reproducible measurement

The opt-in probe imports the **actual compiled library** `Message`, `FunctionCall`
and `ToolCall` types through native-only, documentation-hidden exports. It does not
path-import provider internals or reimplement their types, so provider module
dependencies resolve through the same library graph as normal native builds.
Compilation can be checked without executing the probe:

```sh
cargo test --locked -p agents-worker --test history_perf --no-run
```

It creates 128 complete bash call/result rounds, one initial
user message and one final answer: 258 source messages. Each assistant includes a
short continuation-reasoning field and a real-shaped JSON tool call. Tool results
wrap deterministic varied test-log lines in the daemon's stdout/stderr/exit shape.
The active view models a compaction after round 112 followed by 16 recent rounds.

The 4 KiB and 32 KiB stdout cases represent short check output and moderately
verbose test output. They are synthetic workload controls, **not observed customer
traffic distributions**. Neither hits the daemon's 1 MiB default output ceiling.

```sh
cargo test --locked -p agents-worker --test history_perf -- --ignored --nocapture
node scripts/perf/history-browser.mjs
```

Artifacts stay in `.cache/perf/history/`: `native.json`, `browser.json` and the two
trace payloads. The test is ignored by default and requires no credentials,
container or provider call. It is intentionally outside regular `--lib` CI tests.
For production CPU decisions rerun with `--release` in Actions; the local numbers
below use native **debug** Rust and must not be represented as Worker CPU timings.

Observed 2026-10-23 on Windows x86-64, i9-12900H, Rust test/debug and Node 26.10.0;
three warmups and 25 measured samples per operation:

| Quantity | 128 × 4 KiB stdout | 128 × 32 KiB stdout |
| --- | ---: | ---: |
| Source JSON response, history portion | 664,678 B | 4,455,014 B |
| gzip payload | 89,353 B | 799,163 B |
| Two-message tail delta | 4,488 B | 34,100 B |
| Native snapshot clone, p50 | 0.457 ms | 2.534 ms |
| Native current history response pipeline, p50 | 16.600 ms | 175.676 ms |
| Native borrowed full serialization prototype, p50 | 15.795 ms | 171.988 ms |
| Native two-message delta prototype, p50 | 0.096 ms | 1.244 ms |
| V8 JSON parse, p50 | 0.381 ms | 2.200 ms |
| V8 parse + unchanged TanStack structural sharing, p50 | 0.619 ms | 2.637 ms |
| Current sequential journal storage `get` calls on cold load | 516 | 516 |

The response benchmark isolates the history portion; small account/run/pin fields,
Workers/wasm, network, storage latency and browser layout are excluded. The borrowed
serialization prototype asserts JSON-value equality with the current pipeline.
It is not deployed, and its small improvement here is **not** a reason for a broad
serialization rewrite. Reducing repeated payload is much more promising.

The larger trace's unchanged 30-second idle polling would transfer 8.91 MB/min
raw (1.60 MB/min at the fixture's gzip ratio); a separately observed SDK-run session
polled every two seconds would reach 133.65 MB/min raw. Those are calculated byte
rates, not measured traffic. Attached dashboard streaming already pauses polling.
TanStack's structural sharing can retain the previous data reference on an
unchanged response, so do not assume every unchanged poll rerenders 258 articles.
The Node measurement includes real `replaceEqualDeep`, but not mobile layout/paint.

## Recommended evolution: explicit opt-in, unchanged defaults

Prioritize incremental source reads over eager virtualization or a new cache tier.
The journal is already the right data model; use its stable source sequence as the
cursor instead of inventing a second transcript representation.

### A. Conditional metadata polling

Add a persisted monotonic `view_revision` incremented at every published session
state transition: append/pending, assistant/tool result, status/run, compaction,
checkpoint/error and archive/resume. `input_revision` is insufficient: assistant
and tool completions do not append user input.

An authenticated, owner-checked `GET` with matching `If-None-Match` can return 304
**before** cloning/serializing the transcript. Preserve the existing full response
for a client that does not send a validator. Continue private/no-store behavior;
the client explicitly keeps its own authorized query value. A monotonic persisted
counter is cheaper than hashing the entire transcript for every poll. Compare the
validator only after authentication/ownership checks, never in a public cache.

Dashboard support must handle 304 deliberately: retain the old query value, not
pass an empty body to the generic JSON parser or treat it as an error. Abort and
hidden-tab behavior remain intact. Independent provider token deltas stay on SSE;
do not persist every streamed token merely to advance this revision.

### B. Source-message pages and append deltas

Add `GET /v1/sessions/{id}/messages` without changing the existing POST endpoint:

```text
?before=258&limit=64  → source sequence [194,258), ascending display order
?after=258&limit=64   → newly published source messages, ascending order
no cursor            → newest page, with a stable high-water mark
```

Return explicit `start`, `end`, `high_water`, `has_older` / `has_newer` and `data`.
Cursors are exclusive monotonic journal offsets, not active-prompt offsets. Cap
`limit` and add a response-byte soft limit; always allow one complete message so
large valid tool output cannot produce an empty-page loop. Reject negative,
out-of-range and conflicting cursors instead of truncating them silently.

Use `high_water` to anchor pages while concurrent steering or tools append: loading
older messages and then appending from that anchor must neither lose nor duplicate
a source entry. Compaction and archive/resume do not reset the source sequence.
The new view may be initially implemented against the in-memory journal; it still
saves bandwidth/JSON/browser work without a storage migration.

The dashboard initially loads the newest page and offers older history on demand;
subsequent refreshes request only entries after the published cursor. Keep message
identity `(session_id, source_sequence)` stable across pagination, SSE replacement
and compaction. Do not reconcile optimistic streamed text by matching content.
Existing `GET /sessions/{id}` remains full source history for compatibility.

### C. Implemented: bounded batch cold reads

`session_storage.rs` uses worker-rs 0.8.7's `Storage::get_multiple`. Index reads use
the platform's 128-key ceiling. Chunk reads use **both** 128 keys and a 4 MiB raw-byte
budget: stored `Vec<u8>` values expand into JS arrays, so 128 full 256 KiB chunks
would be an unsafe memory burst, not a free optimization.

New indices retain `generation` and `chunks` and add optional `bytes`. Exact lengths
allow efficient bounded packing and validation. Old indices without `bytes` remain
readable and conservatively budget each chunk at 256 KiB. Publication remains index
last; old generation deletion order is unchanged. A missing index still becomes
`journal_entry_missing`, a missing chunk remains `session_chunk_missing`, and shape,
length and reconstruction-offset inconsistencies fail rather than return partial
history. Lookup uses explicit keys, never the returned map's iteration order.

For the measured one-chunk-per-message trace, 258 indices plus 258 chunks use three
index and three chunk batches instead of 516 serial awaits when byte metadata is
available. Legacy indices take three index and 17 conservative chunk batches, still
bounded and compatible. This is a tested API-call-count reduction, not a measured
live cold-start speedup; local caching, gates and scheduling affect the real benefit.
Proptest reconstructs varied multi-chunk entries from unordered maps, exercises both
index formats, rejects arbitrary missing chunks, and checks key/byte batch limits.

```sh
cargo test --locked -p agents-worker --lib session_storage::tests
cargo check --locked -p agents-worker --target wasm32-unknown-unknown
```

Do not use storage-map lexicographic order as source order (`journal:10` sorts
before `journal:2`). Keep generation/index publication and incomplete-write
validation intact. A SQLite append table with direct range reads could eventually
simplify both persistence and history paging, but requires a deliberate migration;
do not add it merely to ship the initial dashboard.

## Acceptance before adopting any of these changes

- Legacy full GET JSON remains equivalent across compaction, close and resume.
- Complete tool-call/result groups survive source paging; each entry appears once
  when pages/deltas are joined, including steering during an in-flight operation.
- No-change conditional reads skip transcript cloning/encoding; relevant non-user
  state changes invalidate the validator even when `input_revision` is unchanged.
- Invalid cursors, oversized single messages, concurrent appends, dropped SSE and
  restart/recovery do not cause missing, duplicated or silently truncated history.
- Measure real first history render, background bytes and cold rehydration on a
  128-round trace, plus release/Worker CPU and memory; retain tail/failure results.

Primary references: [Cloudflare SQLite storage/get](https://developers.cloudflare.com/durable-objects/api/sqlite-storage-api/#get)
for bulk-key limits and storage gates; [TanStack structural sharing](https://tanstack.com/query/latest/docs/framework/react/guides/important-defaults)
for unchanged query behavior. These mechanisms fit the existing ownership model;
neither requires a cross-tenant cache or changing user-visible history semantics.
