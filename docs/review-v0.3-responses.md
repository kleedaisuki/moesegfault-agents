# Independent Responses protocol and recovery review

This is an implementation-stage review, not a release approval. The v0.3
Responses modules are being integrated concurrently; source-path findings below
must be rechecked after the owner completes integration and against compiled
Worker tests. No live credentials, model calls or production changes were used.

## Contracts inspected

The review uses the project [Responses design](responses-spaces-v0.3.md), the
existing [session runtime](session-runtime.md), and the actual edge/index/wire/DO
code rather than assuming that parsing an SDK fixture establishes execution
correctness. Owner-scoped `response_routes` lookup and reserving an immutable
response/session identity before dispatch are sound initial design choices.

Official background streaming resumes using the last disclosed event's
`sequence_number`; those cursors must retain their meaning across reconnect and
isolate replacement. See [OpenAI background mode](https://developers.openai.com/api/docs/guides/background).
For stored responses the client can chain by `previous_response_id`; with
`store:false` the client instead replays output items. See
[OpenAI programmatic tools](https://developers.openai.com/api/docs/guides/tools-programmatic-tool-calling).

## Findings sent to the implementation owners

### P1 — Never reuse a disclosed stream sequence after recovery

Initial `session_responses.rs::response_event` increments and broadcasts events
from in-memory `ResponseRun.events` without persisting those increments. Provider
delta callbacks are synchronous, so a crash after receiving text but before the
provider-result commit restores an earlier sequence counter. Recovery can then
assign an already-disclosed number to a different terminal event. A client
resuming with `starting_after` can miss that event entirely.

Required correction: fence durable sequence allocation or otherwise persist
event identity before disclosure. Batching is acceptable but must not reassign
identities. Required discriminating test: observe delta sequence N, replace the
DO while provider outcome is uncertain, resume after N, and see one later terminal
without repeating the inference or tool effects. Source correction now reserves
a high-water sequence range before provider I/O and skips it on cold recovery;
the per-delta hot path no longer clones the entire session. A subsequent review
found that a fresh reservation is also needed before post-recovery non-provider
events: cold load skips to the previous high-water mark, but a second reset after
an uncommitted `response.in_progress` could reuse that mark. The existing
single-reset test did not exercise this. The owner subsequently made cold load
renew and persist the new allocation fence before releasing the constructor's
concurrency block, preserved the existing `journal_count`, rejected events outside
that fence and durably renewed terminal capacity before finalization. The repeated
reset/exhaustion pure test now covers the identified sequence. Status: **source
correction independently inspected; actual-reset/compiled verification pending**.

### P2 — Enforce unretained/deleted parent policy at continuation admission

Initial `SessionInner::create_response` checks the requested parent only against
`data.response_head`; the edge resolves that parent from a persistent routing row.
`response_route` hides a terminal `store:false` response from retrieval, but its
ID remains the head and can still start a child. Deleted parents have the same
admission path. This contradicts the design's unretained-response chaining policy.

Required correction: read and validate the parent's retained/tombstoned policy
before accepting the child, without weakening idempotent retrieval of a previously
accepted request. Test both ordinary and SDK retry paths after parent completion,
deletion and chain advancement. The DO now reads the parent and rejects both
deleted and unretained representations before admission; idempotent existing-ID
handling remains earlier. Status: **source corrected; compiled verification pending**.

### P2 — Distinguish fresh imported function pairs from outstanding calls

The initial DO admission calls `validate_tool_outputs` with only the session's
pending calls. A fresh request with a valid imported `function_call` followed by
its matching `function_call_output` has an empty pending set and nonempty outputs,
so it is rejected even though the wire converter accepts it and the design
explicitly supports fresh text/function history imports.

Required correction: validate the complete ordered fresh call/output history,
separately from the exact outstanding continuation set. Reject orphaned, duplicate
and partially matched outputs before appending any input. Fresh imports now select
their declared calls rather than the session pending set; ordered-history and
duplicate-call validation now use an ordered call-group validator that also
permits reused provider call IDs across completed historical groups. Status:
**source corrected; compiled verification pending**.

### P2 — Preserve refusal and abnormal provider termination

The initial `response_completion` manually builds an `output_text` item and ignores
`completion.message.extra.refusal`, despite `responses_wire::output_items` already
recognizing refusal parts. It only marks `finish_reason == length` as incomplete;
`content_filter` can be exposed as normal completion without its reason.

Required correction: normalize typed refusal content/events and map known abnormal
finish reasons to truthful terminal states. Verify SDK output items, streaming
events and immutable retrieval agree. `content_filter` is now mapped to incomplete;
typed refusal parts/events and upstream refusal accumulation have now been added.
Status: **source corrected; compiled verification pending**.

### P1 — Settle known provider results before honouring cancellation

The first integrated `session.rs::step` checked `cancel_requested` before handling
`pending_completion` and `pending_compaction`. Cancellation arriving during provider
I/O could therefore discard an already durable successful result with reported
usage, leave its reservation unresolved, and expose a cancelled response with zero
usage. This is not an uncertain external outcome: the platform already has it.

The guard now excludes known pending results, so accounting can consume them first.
Required test: cancel a delayed successful background provider request, then observe
cancelled state with all reported usage, no replacement inference/tool dispatch and
no new unresolved reservation. Status: **source corrected; compiled verification pending**.

### P1 — Replay limits must never prevent terminal commitment

The initial event-count/byte cap rejected all subsequent events, including the
terminal event emitted by `commit_response`, stranding a run indefinitely once it
reached capacity. The code now evicts an amortized replay prefix and rejects an
expired cursor explicitly, preserving capacity for ordinary terminal events.

One remaining boundary was sent to the owner: output budget is checked after a
new output item is emitted and pushed. An individual near-16-MiB provider result
can still make a single event or the final response larger than the event budget.
Check the public-output budget before publishing that item, retain canonical
source data separately, and guarantee a small terminal error representation.
Status: **partially corrected; oversized-item test pending**.

The current implementation now computes projected public-output size before
publishing the next completion item, retaining canonical source messages separately.
Status: **source corrected; limit/compiled verification pending**.

### P1 — Repair the wakeup before acknowledging an active retry receipt

An acceptance failure after live memory is updated but before `persist` or
`arm_alarm` succeeds leaves the warm DO holding a queued response. A same-key
continuation retry bypasses the edge's initial-session `/init` path and the first
DO existing-ID branch immediately observes that in-memory response without
flushing/rearming it. This can leave execution delayed until the old idle alarm,
or lose the uncommitted acceptance on isolate eviction.

Required correction: before acknowledging an existing active same-ID receipt,
flush the canonical run and ensure its durable wakeup using the normal serialized
path. Terminal immutable receipt retries should remain read-only. Test transient
acceptance-write and alarm failures on a continuation, then retry exactly the same
key: one input append, one response identity, prompt recovery, no provider replay.
The current existing-active-ID path now flushes and rearms before observing it;
terminal receipt retries remain read-only. Status: **source corrected; fault and
compiled verification pending**.

### P1 — Recheck archival after resolving attachments

The parent reviewer identified a yield between attachment resolution and final
admission that initially failed to recheck Closed/archive-preparing/archive-pending
state. An idle archival could complete during the yield, after which the rejected
request could mutate its prefix/journal and clear pending local calls before
`append()` failed on Closed state.

The current source rechecks all archival/closing states after the attachment await
and preflights `append_user_input` plus `start_run` on a cloned domain record before
any acceptance mutations. There is no await between that preflight and mutation.
Status: **source correction independently inspected; fault/compiled verification pending**.

## Additional admission checks requested

- Reserve client function names against all registered built-ins, not only those
  enabled in one agent. A local name must not shadow a server capability. The
  current DO uses the global `tool_kind` registry for this check.
- Define the mixed Chat/Responses boundary: a Chat append after a response must not
  silently make that old response ID denote a different history. Check the fixed
  input boundary or explicitly invalidate its continuation head. `start_run` now
  invalidates the response head for a Chat run.
- Idempotent repeats must be recognized before head/stale-parent checks, so a
  successful accepted request can be recovered after a later response advances
  the chain.
- The per-delta hot path currently clones `snapshot()` including growing journal
  and event arrays. The performance lead was asked to replace this quadratic
  pattern after establishing the baseline, without weakening event durability.
- Reported reasoning-token usage must be accumulated across inference and compact
  rounds when available; a nullable detail is not a license to discard supplied
  provider measurements. The current integration adds an aggregation helper.
- Distinguish top-level request-only `instructions` from historical system/developer
  message items in `input`. The official non-inheritance rule applies to the former;
  flattening both into one replaced prefix can erase supplied conversation context
  on continuation. The current runtime stores historical instructions separately
  with defaulted session state and rebuilds them into every new response prefix;
  top-level request instructions remain response-only. Source correction inspected.
- Client JSON schemas now use the mature `jsonschema` validator with default
  retrieval features disabled, an explicit denying retriever, local-only references,
  bounded JSON complexity and linear-time regex policy. This is a useful capability
  boundary; it still needs the actual Wasm build gate.

## JSON Schema resource-admission review

### P1 — Bound expanded evaluation and compiled patterns, not only source JSON

The initial `responses_schema::compile` bounds source bytes, depth and node count,
but those are not bounds on validation work or compiled pattern memory. Two
discriminating inputs fit comfortably within every initial source limit:

| Case | JSON bytes | Nodes | Depth | Amplification |
| --- | ---: | ---: | ---: | --- |
| `$defs` DAG with `S0 = integer`, `Si = allOf[ref S(i-1), ref S(i-1)]`, object property `x` referring to `S31` | 2,094 | 197 | 5 | A valid scalar `x=0` requires `2^31` leaf visits without evaluation memoization |
| 128 distinct string patterns `[a-z]{40000}` through `[a-z]{40127}` | 6,322 | 387 | 3 | Each tiny pattern expands into a large compiled automaton |

The exact published `jsonschema 0.49.9` source archive was downloaded into the
repository-local review cache and its SHA-256 matched Cargo.lock:
`59ec8a241beed129f06114aa68007e905ca350e7baeb6e17a7631bb7978d91b2`.
`keywords/all_of.rs::is_valid` evaluates both branches;
`keywords/ref_.rs::DirectRefValidator::is_valid` delegates without evaluation
memoization; the memoization in `node.rs::PendingSchemaNode` is limited to its
recursive/container path, not these repeated scalar evaluations. Compiled-alias
reuse alone does not remove repeated validation work.
[Exact upstream source](https://docs.rs/crate/jsonschema/0.49.9/source/)

A deliberately small native probe linked an already-built Rust `regex` dependency
(no Cargo dependency build) and compiled one pattern at a time. `[a-z]{40000}`
was rejected with a 1,000,000-byte compiled-regex limit and accepted with a
4,000,000-byte limit, taking 35 ms in this local debug-linked probe. This is not a
Worker CPU benchmark; it establishes that a 12-byte pattern can exceed 1 MiB of
compiled representation. Allowing many distinct such patterns can exceed a
128-MiB Worker even though the source schema is only a few KiB. Linear-time
matching does not solve compilation amplification. `PatternOptions::regex()`
leaves explicit size/DFA overrides unset in the inspected upstream source.

Root approved a conservative new-v0.3 admission policy: reject cyclic schemas and
excessive expanded-reference complexity with explicit 400 errors, support common
bounded acyclic `$defs`, configure compiled-regex/DFA limits, and enforce an
aggregate budget across all function schemas plus structured output. This is a
resource policy around the mature validator, not a replacement validator.
Status: **owner implementing; adversarial compiled verification pending**.

The owner subsequently added request-wide application/pattern budgets, an
acyclic-reference admission graph, explicit 64-KiB regex and DFA limits, and
schema/instance work-product limits. Those address the reference and pattern
cases at source level; compiled gate results remain required.

### P1 — Arbitrary-precision exponents also need a lexical allocation budget

The exact `jsonschema-value 0.49.9` dependency archive was inspected separately;
its SHA-256 is
`7ec7637f83e510868ae6ed625f7ebfbbde4554ee8ce49854caa5126a8b9b9ecb`.
`src/numeric.rs::bignum::try_parse_bigint` accepts decimal shifts up to one million,
appends that many zero digits, then parses a `BigInt`.
`src/numeric_check.rs::compile_bound` retains that BigInt in each numeric bound.
This library-level cap is far too large to multiply across many user-controlled
schema positions inside a 128-MiB Worker.

A schema with 1,000 distinct `minimum` literals from `1e999000` to `1e999999`
is only **21,048 bytes** and roughly 1,003 expanded applications. It passes the
initial raw and reference-work limits but can retain hundreds of MiB of exact
numeric bounds. The instance `{"x":1e1000000}` is only **15 bytes**, so a work budget
based only on serialized instance bytes also understates its conversion cost.
Negative exponents and exact decimal fractions require the same consideration.

Correction sent to the schema owner: inspect number lexemes before invoking the
validator, cap per-number expanded digit/exponent size and aggregate expanded
numeric work across schemas/request/instance, and include effective numeric work
in the application budget. Preserve ordinary exact integers beyond u64; rejecting
astronomical allocations does not require reverting to rounded floating point.
The owner added coefficient-digits plus absolute-exponent admission (4,096 digits
per number), aggregate schema/request/instance budgets and effective numeric work
in the instance product before compilation/validation. The final source also
rejects nested modern/legacy resource IDs and accounts for referenced pointer
targets outside ordinary schema positions, so admission and validator resolution
cannot silently use different resource scopes. Status: **source correction
independently inspected; compiled adversarial tests pending**. No expensive
numeric conversion was executed locally.

The raw JSON examples are retained as `.temp/v0.3-review/numeric-schema.json` and
`numeric-instance.json`. They were constructed as text without invoking a
big-number parser. A bounded rejection test should prove the policy runs before
the mature validator constructs those numbers.

The fully typed fixture generator and bounded native probe are retained under
`.temp/v0.3-review/`; they do not invoke a model or execute the exponential case.
The latter should only be run in a time-bounded isolated CI process when verifying
that admission rejects it before validation.

## Final review gate

Recheck the final implementation and the exact-source compiled Worker failure,
concurrency and SDK journeys before resolving these entries. This document does
not claim that a missing field in an unfinished module is an accepted release bug.

At the end of this source-review pass, all concrete findings above have source
corrections inspected and no additional blocking source defect remains. This does
not resolve the compiled SDK, recovery-fault and real staging acceptance gates.
