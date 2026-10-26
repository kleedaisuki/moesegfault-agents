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

Focused pure regressions cover strict generation parsing/activation, late cancel
latches and safe rejection classification. Compilation and actual Worker escrow
journeys remain a core CI responsibility; local checks here are formatting and
diff validation only.
