# Conversation authority implementation

`conversation_state.rs` implements a separate Rust Durable Object. It owns no
model, agent pin, container or billing execution. The outer authenticated router
must select the object through an owner-scoped `conversation_routes` row and
set `X-Agents-Owner` itself; never forward this header from an external request.
Every object operation independently checks the stored owner.

## Private interface

| Method/path | Body/result |
| --- | --- |
| POST `/init` | `{id, metadata?, items?}`; idempotent conversation creation |
| GET/POST/DELETE `/metadata` | SDK conversation object; update `{metadata}`; delete tombstones container only |
| GET `/items?order=desc&limit=20&after=...` | SDK list/cursor envelope, defaults descending |
| POST `/items` | `{items}` with at most 20 items; returns SDK list |
| GET/DELETE `/items/{id}` | Retrieve item / tombstone visibility; delete returns conversation |
| GET `/snapshot` | `{id, revision, items}` in canonical chronological order for immutable Response admission |
| POST `/responses/commit` | `{response_id, items}`; appends new input/output once, returns committed revision |

The Response admission owner must persist its accepted snapshot/revision before
paid execution. A later conversation item deletion changes future snapshots;
it must not edit an already accepted Response or its branch point. Completion
append recovery is keyed by Response identity, never a reason to regenerate.
The snapshot interface deliberately does not bind a model or Session ID.

## Storage and failures

Item bodies live in independent immutable, chunked keys; metadata/header updates
do not reserialize all prior item bodies. The header owns item order, tombstones,
revision and Response append receipts. Body candidates are written first; the
chunked header pointer is published last. An interrupted append therefore cannot
expose half a batch. Response receipt and visibility publish together, and later
Response retries return the original committed revision. Neither conversation
nor item deletion physically removes historical bodies. These user-data keys
must not be placed under intermediate-state TTL cleanup.

The header's ordered ID index and receipt map currently scale linearly with item
count (not total content bytes). If very long-lived conversations become a
measured dominant workload, page the index without changing this private API.

## Integration ownership

Migration `0015_conversations.sql` creates only the owner-scoped D1 routing
identity. The DO is authoritative for actual data. State integration now includes both modules, the `ConversationState` export,
authenticated public routing and `CONVERSATION_STATE` binding with SQLite class
`ConversationState` under migration `v2-full-responses`. Cross-Session Response
admission/terminal coupling and operation/storage accounting are separate next
steps; public CRUD integration does not imply those features are complete.

## Focused verification

Rustfmt and `git diff --check` pass. Pure Rust tests cover metadata character
limits, item normalization preserving stable IDs, and the 20-item mutation cap.
Heavy compilation and real-worker persistence journeys are delegated to root's
GitHub Actions checkpoint, not executed on this resource-constrained host.

Authoritative schema inspected: installed official OpenAI SDK
`node_modules/openai/src/resources/conversations/{conversations,items}.ts`.
In particular, Conversation create has no model and deleting a conversation
explicitly does not delete its items.

## Public boundary

`conversation_routes.rs::handle(req, &RouteContext, owner)` implements the public
`/v1/conversations` and `/v1/conversations/{id}/items` CRUD boundary, including
owner-scoped lookup, method/query/body validation, SDK wire envelopes, and
creation idempotency-key conflict detection. It creates fresh private requests
and never forwards caller-owned trust headers. Optional output projection honors
`include`/`include[]`; internal snapshots retain canonical item values regardless
of public projection. Item listing and admission snapshots use the existing
batched chunk reader instead of performing sequential storage reads per item.

Integration helpers:

- `snapshot(&Env, owner, id) -> Result<Option<ConversationSnapshot>>`
- `commit_response(&Env, owner, id, response_id, items) -> Result<Response>`

`ConversationSnapshot` is serializable `{id, revision, items}`. A missing or
deleted conversation returns `None`; internal transport/storage failures are not
silently treated as empty history. Completion append returns a status-bearing
Response so the execution owner can distinguish deletion from recoverable
transport failure without repeating paid generation.

Add `mod conversation_routes;` to the crate, then dispatch the already
authenticated `/v1/conversations` prefix through this handler. The shared entry
point files remain with the integration owner.

SQLite validation applied migrations 0001 through 0015 in a fresh in-memory
database successfully; `conversation_routes` has `id`, `account_id`,
`request_hash` and `created_at`. Six pure Rust tests are ready for the root CI
checkpoint; no local Rust compilation was performed.


## Immutable Response admission and execution hint

Private POST `/responses/admit` accepts `{response_id}` and returns the accepted
`{id, revision, items, predecessor_response_id, predecessor_session_id,
predecessor_agent_id, predecessor_pin_hash}`. Predecessor fields are nullable.
A separately chunked receipt stores the visible chronological item IDs, not
copied item bodies. Repeated admission reads those exact immutable bodies even
if later item tombstones hide them from new snapshots. Header receipt membership
is the publication marker; incomplete receipt candidates are never authority.

The header owns a durable active Response writer lease. Another uncommitted
writer receives 409 `conversation_busy` with `error.active_response_id` for
recovery diagnostics. There is deliberately no inactivity TTL: a legitimate
long-running agent must retain its lease. Canonical pre-admission rejection or
cancellation calls POST `/responses/release` with `{response_id}`; it only clears
a matching lease. Admission receipts remain durable for retries/audit. This
serialization policy can later change without changing item ownership.

`/responses/commit` optionally accepts `session_id`, `agent_id`, `pin_hash` and
`accepted_revision`. A mismatched accepted revision returns 409. A complete,
valid execution context can publish a warm hint only if no metadata/item change
intervened since acceptance. The hint carries the adopted post-append revision;
new admissions expose it only when that revision still exactly matches. No
model is bound to the Conversation; the execution integrator additionally
matches agent/pin before reusing the hinted Session.

Deletion prevents new admissions, but existing accepted receipts remain
readable. An accepted Response can commit its new input/output after deletion;
the metadata tombstone remains set, and no new Conversation or admission is
resurrected. A legacy private commit without acceptance cannot bypass deletion.
Commit retries preserve the original append revision and never regenerate.

Route helpers `admit` and `release` return status-bearing Responses so callers
can propagate 409/404 without treating failures as empty context. Deserialize a
successful admission as `ConversationAdmission`. `commit_response_context`
accepts an optional `ConversationExecutionContext`; existing `snapshot` and
`commit_response` helpers remain available for compatibility. Response execution
must append only its **new** input/output, not reappend accepted predecessor
items.

The accepted-ID list is O(visible item count) per actual admission. This avoids
quadratic duplication of content bodies but not quadratic ID-reference volume
for extremely long conversations. A scalable later representation can retain
immutable revision pages plus tombstone revision boundaries without changing
the admission API. Do not delete receipt-referenced bodies under ordinary TTL
cleanup.

## Pending admission versus activated execution

Admission first reserves a **pending** writer for 120 seconds. Same-Response
admission renews only that pending deadline and reuses its immutable receipt.
Another writer can replace an expired pending candidate. POST
`/responses/activate {response_id, session_id, accepted_revision}` validates the
still-current, nonexpired candidate and its receipt, then durably binds execution
to that Session. Activation retries from that same Session are idempotent;
replaced candidates, expired candidates and another Session receive 409.
The Session integration calls activation after persisting the queued RunState
and before execution admission/paid work. A stale activation does not authorize
paid execution. Activated execution has no deadline, even for long jobs. Legacy
active identities lacking activation/deadline fields retain the old indefinite
fence rather than inventing an expiry during rolling upgrade.

A terminal commit may pass `defer_release: true`. This requires its matching
activated Session, publishes content/provenance/hint once, and keeps the active
fence until the Session canonically clears its run and calls `/responses/release`.
Commit replay never clears a held fence. This separates content durability from
financial cutoff and canonical completion without admitting a competing writer
in the middle of finalization.

`revision` now describes prompt content, not cosmetic metadata. Updating
metadata does not increment it or invalidate an otherwise exact warm hint.
Item additions/deletions and container deletion still advance it.

## Private per-item provenance

`ConversationExecutionContext` now includes `defer_release` and an
`item_provenance` map keyed by newly appended item ID. Its values are
`ConversationItemProvenance {source_session_id?, response_id?, provider_binding?}`.
The context derives `Default` to keep call-site extensions simple. Only the
trusted automatic commit path can publish this map. Public create/items APIs
cannot submit it and canonical public item JSON does not contain it.

Admission returns a serde-default `item_provenance` map alongside canonical
items. This lets execution distinguish platform-retained native/provider-bound
items from caller-provided raw opaque payloads; the absence of provenance is
not proof of a compatible provider binding. Provenance bodies publish before
the same item/header commit marker, remain immutable, and are not duplicated
inside every acceptance receipt.

Additional verification prepared: three pure lease tests cover deadline edges,
activated long runs, renewal/release and rolling legacy state; the compiled
Worker journey now exercises activation fencing, deferred commit replay,
metadata-neutral warm hints, private-versus-raw provenance and accepted late
completion after deletion. Only syntax/format checks ran locally; actual Worker
execution awaits the captain's freshly compiled artifact.


## Canonical Session coupling

`session_conversations.rs` connects the authority to the existing Session harness.
The transport's accepted admission is ephemeral: `ResponseRun` persists only the
Conversation ID, accepted revision and terminal preparation receipt, not another
copy of all historical item bodies. A compatible warm hint selects the private
predecessor without changing the public `previous_response_id` field. A fresh
execution lane prepares its historical source locally and applies it only inside
the existing Session acceptance fence. Historical media is resolved and pinned
once for that fresh lane; ordinary warm continuations append only new input.

The queued Session run is durable before authority activation. Activation is
checked before model/resource run admission, so replacing an expired, never
activated candidate cannot authorize a delayed old paid execution. Warmup uses
the same authority activation but does not fabricate model usage.

Terminal preparation appends only the Response's newly accepted input and output,
with private per-item provider provenance. It occurs before the run's financial
cutoff. The authority keeps its activated writer fence until the Session's
terminal state is durably cleared. A release outbox is persisted by that same
pre-cutoff Session publication; healthy completion drains it before publishing
terminal followers. Failed release keeps the outbox and a bounded retry deadline,
including for otherwise closed Sessions, without generating a second response.
The retained per-Response account cleanup reservation funds release after the
model/resource reservation has settled.

This coupling does not by itself establish workspace inheritance for a new
Conversation lane. The edge integration must retain the accepted latest workspace
archive independently of warm-cache eligibility; a history edit or agent switch
must never silently substitute an empty workspace.
