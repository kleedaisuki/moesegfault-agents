# Conversation output-capacity escrow integration

This is the state-domain integration contract for the independent logical-byte
budget described in [Conversation capacity](conversation-capacity.md). It does
not reserve money or physical R2 bytes. The approved policy is to reject before
paid generation when the account cannot reserve the guaranteed append envelope.
Near-full early rejection is intentional; a small actual answer refunds unused
reserved capacity after its canonical publication.

## State and order

| Boundary | Required invariant |
| --- | --- |
| Resolve effective new input | Prompt rendering, accepted steering and canonical input IDs are final; raw request identity remains unchanged. |
| `/responses/reserve` | Exact input claims and bounded append-output capacity are reserved atomically under one immutable generation. No user-journal/run acceptance or paid call has happened. |
| Canonical queued Session save | The accepted Conversation revision and returned capacity generation are durable. |
| `/responses/activate` | The authority verifies the same response, Session, revision and capacity generation before model funding. A delayed replaced candidate cannot activate. |
| Generation / accepted additional input | Existing escrow cannot be consumed by another Conversation. Additional input extends the same generation before advancing the input clock or journal. |
| Terminal append | Actual immutable body/provenance/header allocations debit the held escrow atomically; there is no release-then-reserve gap. |
| Header publication | Canonical visibility proves whether actual allocations are retained. Unknown acknowledgment is recovered from authority storage, never a timeout guess. |
| Conversion | Retain actual bytes and refund unused logical capacity idempotently. |
| Session terminal / cleanup | Release the authority writer and its separate funded cleanup scope without regenerating completed model work. |

Known failed/cancelled no-append outcomes close the capacity generation before
refunding it. A later append or steering extension under that generation must
fail. Unknown model/resource monetary outcomes keep their existing financial
holds; releasing unused logical-byte capacity does not invent zero usage.
Only provably never-activated pending admissions may be retired through the
pending-admission recovery protocol. Activated long tasks have no inactivity TTL.

## Executable envelope, not a token estimate

`response_limits::OutputLimits::for_response(response_id)` is the common source
of truth for canonical append-output bytes, implied item count and owned ID size.
Its proof is in [Canonical output budget](canonical-output-budget.md). The quota
quote includes exact canonical new-input bytes and the actual metadata DTOs,
bounded per-output item/provenance/header representations and final image-reference
metadata. It excludes private provider payloads, duplicated Response envelopes,
preview events and referred image binary bytes.

Canonical legacy/native/hosted writes use the same checked output boundary.
In-progress replacement counts as one item. Repeated SSE events and final replay
do not reserve the same output twice. A whole turn is validated before completed
caller-executable actions are published; overflow is an explicit canonical error,
not successful truncation or permission to repeat a paid service call.

## Owner interfaces

- `conversation_quota::escrow::Request`: canonical `input_items`, private input
  provenance, agent/pin/profile metadata and the trusted warmup flag.
- `conversation_routes::reserve_capacity(...)`: definite capacity rejection with
  required/available bytes, or a durable `capacity_generation`.
- `activate_generation(...)`: generation-fenced activation after the queued save.
- `reserve_additional_input(...)`: idempotent submission identity and exact new
  claims, before legacy management steering is accepted.
- `ConversationBinding`: persist the generation with the accepted revision.
- `ConversationExecutionContext`: carry that generation to terminal append.

The quota owner owns Actor/SQL/publication and RPC helpers. The Session coupling
owner owns the pre-accept reserve hook, activation, terminal generation and legacy
steering hook. The canonical budget owner owns all output writers. This split
must not create a second harness or separate public input normalization policy.

## Required incremental acceptance

1. Two Conversations race for one owner's remaining capacity: at most the funded
   envelope succeeds, and the rejected request makes no provider call.
2. Another mutation runs while generation is held: terminal actual conversion
   still succeeds without exceeding the owner cap.
3. Lost reservation, publication and conversion acknowledgments retain exact
   identities; retries neither charge twice nor expose half an append.
4. Additional steering is rejected before journal/clock changes if its exact
   extension cannot be reserved.
5. Known cancel/failure releases capacity and writer authority, then a delayed old
   generation cannot append. Unknown monetary usage remains held.
6. Input IDs colliding with any retained item, including tombstones, are rejected
   before paid inference rather than stranding terminal finalization.
7. Large late output cannot orphan an already-completed client action; known
   provider usage settles once and no completed request is replayed.

These are new behavior/failure journeys. Passing basic CRUD, Chat and time-tool
baselines are not a reason to rerun those suites for every escrow edit.


## Session integration checkpoint

The create path now reserves exact canonical `original_items` after effective
prompt/steering/media preparation and admission preflights, before input-journal
or run acceptance. A definite capacity rejection keeps its original HTTP status
and required/available values. The queued run stores the returned bounded
generation; normal activation refuses a missing generation rather than falling
back to the private non-generating management API.

Legacy management steering and terminal Conversation conversion share the
Session persistence mutex. An additional input claim is reserved before the
user clock/journal changes, and those same canonical items extend the active
Response input body. A reservation begun before finalization is ordered before
conversion; input already finalizing, cancelled or converted is rejected.
Cancellation re-reads current state under that same mutex. Once a Conversation
has a known terminal result/error, an execution end, or a prepared publication,
a late cancel callback cannot change the logical outcome after append conversion.
Warmup sets its known terminal result before finalization as well.

Known failure/cancellation uses the generation-fenced no-append receipt before
marking preparation complete. A known successful result with an independent
accounting failure still follows the append path and retains its monetary hold.
Deterministic pre-publication application rejection can close the generation and
finish the Response with `conversation_publication_rejected`; canonical output
and actual usage remain inspectable and no model/service is replayed. Transport,
5xx, retryable 408/425/429, or a pending-capacity publication marker do not authorize
that release. A durable publication-error decision survives finalization retries.

Output provenance is a shared origin on the terminal wire, not one identical
Session/Response/provider-binding object repeated for every output item. The
authority materializes the same per-item private provenance after validation.
This keeps the transport envelope within the canonical budget without a smaller
arbitrary item count or successful truncation.

Generation parsing, activation, late cancellation and definite-rejection
classification are executable runtime fences. The focused compiled-Worker
lifecycle uses one summary and one generation to observe the pre-existing hold,
actual conversion, metadata deletion and explicit purge, then cold read-only
Response survival. It does not replay accepted auto-window generations.
Nine local actual-SQL lifecycle/concurrency/property checks passed; this is not
a substitute for the new Worker lifecycle or subsequent real-provider staging.

## Definite rejection and pending recovery

`reclaim_pending` is shared by admission and explicit purge. Activated runs and
legacy/unknown leases without a pending deadline cannot be reclaimed by elapsed
time. A definite capacity rejection uses the exact Response/generation fence
immediately instead of waiting for that deadline. It reloads canonical header
state after a failed header allocation, removes only never-accepted admission
membership and releases the writer. The response cleanup funding closes only
after the authority confirms release and actual resource measurements settle.
A settled rejected identity with no admission membership cannot reopen a writer;
accepted completed/cancelled memberships retain their existing retry path.
Unknown publication/accounting outcomes keep their holds.

Actions `40348610976`, source `c1bac6f`, Worker artifact `12662202226`, passed
only the new `capacity-rejection` lifecycle. Declared synthetic pre-existing
logical history drove the real public conditional admission boundary without
writing 256 MiB. The actual Worker returned capacity 409, immediately released
the writer, settled its known resource cleanup scope, fenced the rejected-key
retry, and accepted metadata DELETE plus irreversible purge without a TTL wait.
Provider calls, counter calls and model ledger rows were all zero. Earlier
successful generations and nine SQL properties were not replayed.

A small subsequent call-site integration preserves idempotent writer-release
acknowledgments after irreversible purge. No-append acknowledgments require the
exact already-released escrow generation; admission/activation/extension/append
remain denied. This lets a retired queued Session finish cleanup without a
permanent purged-404 loop, without inventing zero unknown model usage. That
terminal acknowledgment wiring remains for the next coherent artifact build;
it is not relabeled as executed in the earlier source-bound receipt.

## Actual Cloudflare pre-admission interruption (2026-10-27)

The one authorized context submission on Worker `1d1c3a2` / assembly `3539c1c9`
returned HTTP 500 after 63,951 ms. The original route was
`resp_78a70f2f64e24b659f72c9cbf73e67c0`, Session
`f743705e-343d-41be-9988-c8ce1523f39c`. Read-only reconciliation found no model
reservation, no retained Response and no running Session. Its separate account
scope `conv-response:resp_78a70f2f64e24b659f72c9cbf73e67c0` retained its original
20,000-microUSD hold with all resource observations known: actor 28,460 ms,
parent 35,445 ms. Logical output escrow remained reserved at 93,071,671 bytes.
No model request was repeated and no balance or escrow row was manually edited.

The whole funded Conversation request was wrapped in `blockConcurrencyWhile`,
including grant checks, asynchronous D1 publication/accounting and their final
observations. Cloudflare documents a [30-second callback timeout followed by
object reset](https://developers.cloudflare.com/durable-objects/api/state/), and
[recommends keeping this guard out of ordinary external-I/O request
handling](https://developers.cloudflare.com/durable-objects/best-practices/rules-of-durable-objects/).
The actor duration excludes grant setup and observation closure; it is not the
complete guard duration. The measured RPC interval exceeds that platform bound.
This is a concrete primitive mismatch, not evidence that inference failed.

Conversation requests now retain the same per-object serialization using a Rust
async mutex, without placing remote I/O inside the platform initialization guard.
Published header/intents, numeric quota coordination and unknown-money fences
are unchanged. A new zero-model actual-Worker test inserts one real 33-second
asynchronous D1 delay and overlaps two metadata writers; it checks final order,
no regular `blockConcurrencyWhile` use and real measured long actor time.
Actions `40363780747`, source `00b6db05`, Worker artifact `12669457980`,
passed that one new actual-Worker case in 35,788 ms. It observed real actor time
above 33 seconds, preserved the second writer's final metadata, used zero
regular platform guards, and left no pending resource observations or model
ledger entries. No external service or model was called.

Structured operational events preserve Conversation/request/operation/grant IDs,
trace/span correlation, lock waiting, RPC duration, grant checking, individual
D1 funding/RPC/observation durations, and pending/activated/generation boundaries.
Known Conversation transport failure has a diagnostic 503 code rather than
being silently flattened to `internal_error`; original response route identity
is retained on admission failure. Private source remains in its controlled
canonical stores and diagnostic receipts; keys and private bodies are not logged.
Original financial/logical holds still require fenced recovery on the original
IDs after the corrected artifact is deployed, not a new paid submission.

### Original observed resource scope closure

After a canonical never-activated pending generation is retired and its writer
is durably cleared, reclamation now validates the original owner-bound
`conv-response:` maintenance scope and delegates to `ResourceMeter::settle_run`.
It neither records a new old interval nor reprices historical resource usage:
the original ledger sum is the settlement authority. The current cleanup scope
pays for its own lookup. Any original pending observation still prevents
settlement; model-money reservations are a different scope and are untouched.
A closed admission may still be observed through its retained accepted receipt,
but cannot acquire a fresh pending writer or funding grant for new work.

Actions `40367465993`, source `9201f205`, Worker artifact `12669804907`, passed
one new zero-model lifecycle in 3,654 ms. The actual public create lost its
successful reserve acknowledgment before Session acceptance, leaving a real
known resource hold and output escrow. A fixture-only deadline advance selected
normal never-activated recovery; standard metadata DELETE/purge released escrow
and settled the original scope to exactly its prior ledger sum. Original ledger
row count/amount were unchanged; no provider, model or external service ran.
This mirrors the preserved 3539 pre-admission failure without repeating its
Response POST. The later small closed-admission observation guard remains for
the next coherent artifact build, not relabeled as part of that earlier receipt.

For the original staging recovery, deploy the corrected artifact first and use
the existing owned Conversation metadata DELETE/purge path. Check the same
Response/Session IDs still show no running/model work; verify the original
logical generation becomes released with zero remaining bytes and the exact
`conv-response:` scope settles to its original ledger sum. If any canonical
activation or unknown observation exists, stop at its fence and retain the hold.
Do not modify balance rows, invent durations or submit another Response.
