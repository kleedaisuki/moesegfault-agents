# Full Responses: immutable lineage, workspace branches, and Conversations

Architecture decision for the expanded full-Responses objective, based on source
`6552a93`. This is an implementation plan, not an acceptance claim. The previous
linear-only exclusion in `responses-spaces-v0.3.md` is superseded by the new scope.

## Decision

Keep one execution harness and one mutable workspace per Session DO. A retained
Response becomes an immutable branch point, not an alias for the Session's latest
state. The first eligible continuation can reuse the warm Session; historical,
concurrent, or different-agent continuations create independent Sessions from the
same branch point. A separate Conversation DO owns the model-independent item
log; it does not execute models or replace Session admission/accounting.

### Authoritative state

| State | Authority | Invariant |
| --- | --- | --- |
| Response request/output/events | Existing Session DO per-ID record | Unchanged immutable retrieval/replay boundary |
| Branch point | Per-response Session DO key | Fixed source end, pending call set and workspace seed; prepared before terminal publication |
| Original source prefix | Existing append-only Session journal | Compaction never rewrites it |
| Pinned config | Original immutable Session record | Same-agent descendants retain it; different target agents receive a new target pin |
| Mutable execution/workspace | One destination Session DO | A fork never rewinds or mutates its source Session |
| Owned archive blob and strong references | Private R2 + D1 owner/ref registry | Shared immutable bytes, one physical retained-byte clock |
| Conversation metadata/items/revision | New owner-scoped Conversation DO | No model pin; current visible items may change without changing prior Responses |
| Response routing/idempotency | Existing D1 response index | Reserve child ID before dispatch; canonical DO state still decides whether work exists |

Current `ResponseRun` does **not** retain a journal boundary or an old workspace
anchor. Removing `response_fork_not_supported` alone would execute historical
children against current state and is incorrect.

## Branch point: references, not prompt copies

Suggested private record:

```rust
struct BranchPoint {
    response_id: String,
    source_session_id: String,
    source_end: usize,
    pending_calls: Vec<ToolCall>,
    workspace: WorkspaceSeed,
}

enum WorkspaceSeed {
    Empty,
    Archive(OwnedArchiveRef),
}
```

The source Session supplies the exact pinned config, source prefix and protected
historical system/developer messages. Reconstruct a historical child's inference
context from its immutable source prefix, not the source Session's latest active
prompt. Rebuild the server/pinned prefix and apply the **new** request's top-level
instructions. A previous response's top-level instructions are not inherited.
Normal model-budget compaction can summarize that reconstructed context when
needed. Historical forks may therefore incur a fresh compaction, but the ordinary
warm-head path retains its compacted prompt and cache.

Do not serialize the entire active prompt at every response boundary: with a
growing conversation that produces quadratic retained text. `source_end` plus
the existing immutable journal is enough for the current normalized message
model. Full-protocol opaque items must retain their provider/protocol/model/route
binding; do not forward encrypted reasoning or signatures to an incompatible
target merely because both inputs are JSON.

The first functional slice may materialize the source prefix **once per actual
fork** into the destination journal using existing chunk helpers. That is bounded
by the user-created branch count, not by every continuation. Retain a `SourceRef`
descriptor in the private seed interface so a measured shared-segment import can
replace copying later without changing public IDs or semantics. Do not add a
remote rope/GC framework before measuring branch workloads.

`journal.len()` is the authoritative warm logical count today: `load_state`
hydrates all entries, while the live `journal_count` can lag until serialization.
Coordinate any change to that invariant with runtime storage optimization.

### Same agent versus a different agent

- Omitted `model` with `previous_response_id` selects the parent's agent ID.
- Same-agent descendants use the parent's exact pin, even if its configuration
  has since been edited. Current-head reuse remains an existing Session.
- An explicitly different owned agent is a new Session pinned from that target
  configuration. Inherit conversation data and the owned workspace seed, not the
  parent's tool permissions or pinned agent instructions.
- Disabling workspace tools in the target does not erase an inherited archive.
  Keep the seed as dormant owned data; do not start a Container. A later eligible
  descendant can restore it.

### Existing responses

New optional serialized fields must default cleanly. An old **current-head**
response can acquire a branch point lazily while its Session is idle, has no
uncommitted input, and still represents that exact head. An old non-head response
has no provable historical workspace/source boundary. Return an explicit
historical-state-unavailable error rather than guessing from timestamps, matching
text, today's checkpoint, or the latest assistant. Existing retrieval, downloads,
Chat and warm continuation remain valid; missing pre-feature information is not
permission to manufacture user history.

## Fork dispatch and failure identity

Keep the existing reserve-before-dispatch idempotency order:

1. Reserve the child response ID and owner request hash in D1, initially routed
   to the source Session as today.
2. Dispatch normally. An existing identical response returns its original result.
   A still-eligible current head accepts through the existing canonical mutex.
3. A stale/busy historical parent instead returns a **private** branch-required
   signal only after validating that the requested retained parent has a branch
   point. No model/tool execution or source mutation occurs on this path.
4. Export the frozen seed; derive the destination Session ID deterministically
   from owner and child response ID. Acquire a transfer reference to its workspace
   blob before releasing the source export fence.
5. Admit/initialize that destination Session with the normal plan/concurrency
   limits and the trusted seed. Initialization is idempotent and rejects a
   different seed fingerprint under the same destination identity.
6. Compare-and-set `response_routes.session_id` from the expected source to the
   initialized destination, then dispatch the original child request there.

The child starts with a trusted external-parent binding, its fixed source prefix,
and the parent's outstanding local calls. It need not duplicate the parent's
Response/output/event record; reads of that parent continue using its own routing
identity. Results supplied by a child consume only **that branch's** pending call
set. Two children can submit different outputs without consuming their sibling's
calls. Server tools already executed in the parent are history, never replayed.

Persist a per-child fork allocation before exporting a seed when needed to make
retries select the same destination. A parent must never later accept an allocated
child ID locally. D1 routing is a projection of that decision, not authority to
create a second execution. A failed quota admission leaves no paid run; transfer
refs/empty destination rows use proof-based existing recovery, not an assumed
timeout refund. A stale index read may reload/redirect observation, never replay
an accepted POST because transport failed.

An explicit `X-Agents-Session` plus a parent ID validates the parent's owned
Session binding. It must not authorize overwriting an arbitrary destination
workspace. Return the actual new destination Session ID after a fork.

## Workspace seal and shared blobs

A historical workspace cannot be reconstructed after later mutation unless its
bytes were frozen at a boundary. Seal a dirty workspace at terminal response
preparation, before the run's resource wall endpoint/funding is finalized. Reuse
the current immutable archive when `dirty_bytes == 0`; an unused workspace has
the `Empty` seed and does not start a Container. On a retry, use the same response
operation identity and known snapshot; never execute its commands again.

Do not create an automatically visible `workspace_versions` row for every
response. That would pollute the user version list and consume its immutable
version quota. Use one typed owner-blob/reference primitive for live manifests,
user versions, response branch points and fork transfers:

```text
workspace_blobs: owner + format + digest + archive_bytes -> immutable object
workspace_blob_refs: owner + kind + ref_id -> blob
kind: session-current | session-candidate | response | fork-transfer | version
```

The shared storage owner must settle this schema once with the performance
workstream. New owner blobs must live outside legacy session-checkpoint GC
namespaces. An already protected immutable user-version object can register
lazily. A legacy session-prefix archive may need one checksum-verified promotion
copy into that protected namespace: a new D1 reference cannot stop an old service
that deletes R2 bytes without understanding that reference. Do not perform a
blanket copy/backfill or alter the public `manifest_key`/snapshot/download ABI.
An explicit persist can write one owner blob and bind both its live manifest
and user version, eliminating the current archive copy and duplicate retained
bytes. First mutating child work writes a new blob: this is object-reference
copy-on-write, not an unsupported claim of shared Container filesystems.

Publish protection before pointers: create a candidate ref, publish the R2
manifest, then atomically switch the current ref/release its predecessor. A
response ref precedes terminal publication; a child current ref precedes release
of its fork-transfer ref. Both eager cleanup and orphan GC must understand all
strong refs before they can remove an object. Published user versions never
expire. New shared bytes are charged once through the existing nullable-session
account resource ledger. Match hidden branch-history capacity to the root's
Space quota policy; do not silently make retained history unmetered.

## Conversations and mutable item visibility

Conversations are model-independent; `POST /conversations` has no model. Do not
alias a Conversation to the current fixed-agent Session and reject a later
different-agent response. Add a small Rust Conversation DO plus owner routing
metadata, not a second execution harness.

Suggested state: metadata, append-only item records, item visibility tombstones,
revision, append/idempotency receipts, latest committed workspace response ref,
and a reusable execution-Session hint. Item update/delete changes visibility for
future snapshots; it never mutates accepted Response input/output/events.
Conversation deletion tombstones the container, **not its stored items**, matching
the official SDK contract. Do not resurrect it during a late response commit.

Response admission takes an immutable visible-item snapshot/revision. The
conversation's items are prepended to request input items. `conversation` and
`previous_response_id` are mutually exclusive. After canonical completion, append
that response's input/output items exactly once using its response ID as the
commit identity; keep recovery of this append separate from paid execution.

Reuse a warm execution Session only when target agent/pin and adopted conversation
revision match the snapshot. Rebuild the **active inference view** from the new
conversation snapshot when explicit item mutations change context; do not rewrite
the Session's source execution journal or retain deleted system instructions in
the active prefix. Different agents or incompatible lanes use independent
Sessions/workspace seeds. A reusable Session hint is a cache, never authority.

Serialize conflicting response writers with a durable conversation admission
lease; ordinary item reads remain available. The full-contract owner must verify
official concurrent-writer behavior before fixing reject/queue semantics. Do not
let two continuations consume one outstanding local call set or publish an
invalid tool transcript merely to avoid a conflict. This is the remaining narrow
policy question, not justification to omit Conversations or bind them to a model.

Items create/list/retrieve/delete, bounded pagination and includes use official
wire items and stable IDs. Item deletion must retain data referenced by accepted
snapshots; use visibility revisions or immutable snapshot copies. Reconcile an
ambiguous append through the same response ID, not by running the model again.

## Implementable order and acceptance

1. Add defaulted per-response source boundary/pending-call branch point and
   trusted export/import DTOs. Begin with text-only historical/sibling children;
   prove parent retrieval/events/source unchanged and distinct child execution IDs.
2. Add private branch-required dispatch, deterministic destination initialization
   and routing CAS. Race two valid children, idempotent duplicate child creation,
   quota failure and lost initialization/routing acknowledgements.
3. Integrate the single shared blob/ref layer and terminal workspace sealing.
   Prove same parent files in both children, independent mutation/restore, one
   physical unchanged base archive/clock, no Container startup on fork/read, and
   no deletion by eager/orphan GC.
4. Add Conversation DO metadata/items/revisions, then `conversation` response
   binding. Prove model switching, item tombstones, old Response immutability,
   exactly one append per response and conflict/recovery behavior.
5. Exercise real Python/Node SDK workflows and owner-isolation/financial/read-clock
   invariants in Actions. The early text slice is not the completed full objective.

Root assigns migration numbers, class migration tags and release order. Shared
blob/ref GC awareness is a service rollback floor; old clients remain valid.
Runtime perf retains ownership of `save_state`/`load_state`/`snapshot` helper edits.

## Primary grounding

- [Official conversation-state guide](https://developers.openai.com/api/docs/guides/conversation-state)
  and [migration guide](https://developers.openai.com/api/docs/guides/migrate-to-responses)
  explicitly describe previous-response chains/forks and persistent Conversations.
- [Official create contract](https://developers.openai.com/api/reference/python/resources/responses/methods/create)
  specifies mutual exclusion of `previous_response_id`/`conversation` and
  response-scoped top-level instructions.
- Installed official Node SDK `src/resources/conversations/{conversations,items}.ts`
  confirms model-independent creation, metadata, 20-item mutation batches,
  pagination/includes and non-cascading conversation deletion.
- Existing [workspace version](workspace-versions.md) and [data lifecycle](data-lifecycle.md)
  contracts preserve private immutable archives, strong publication fences and
  financial provenance. This design extends those mechanisms instead of adding
  an unrelated filesystem or transaction system.


## Implemented foundation checkpoint

The first text/history branch slice is now implemented in `session_branch.rs`,
`response_routes.rs`, `response_index.rs` and narrow Session hooks. Terminal
responses retain a defaulted reference-only `BranchPoint`; an actual fork exports
only its immutable source prefix. Destination IDs are owner/response-derived,
normal session admission enforces concurrency/allowances, imports are seed-hash
idempotent, and a routing compare-and-set publishes the initialized destination
before dispatch. A durable source dispatch fence prevents stale edge requests
from executing the same child in the old Session. Same-agent forks retain the
original pin; explicit other-agent forks resolve the target pin and do not forward
provider-private assistant continuation fields across that boundary.

The historical upgrade path only seals an idle, unchanged old current head. Old
non-head responses without boundaries explicitly fail; no timestamps or message
matching fabricate unavailable state. Warm current-head continuation remains the
existing path. Workspace sealing is now integrated before the run financial cutoff: immutable
owner-blob references are retained with response boundaries, and child imports
bind their own current reference without copying R2 data or starting a Container.
Only genuinely unavailable pre-feature/unfunded historical state remains explicitly
`unsealed`. Native rich-item history is now bound to an immutable private generation log.
Branch points retain its generation/end/provider binding; actual forks export the
exact prefix once and atomically import a child-owned generation. Incompatible
provider routes may use an explicitly portable visible-history projection, never
blindly forward encrypted reasoning/signatures or unsupported rich tool state.

Conversation metadata/items and public routing now exist in
`conversation_state.rs` and `conversation_routes.rs` with migration
`0015_conversations.sql`. The model-independent authority keeps item bodies
immutable, tombstones future visibility, preserves items after container deletion,
and commits response appends idempotently. Its module exports, authenticated route hook and staging/production binding
(`v2-full-responses`) are integrated. Response admission/terminal integration and
resource accounting remain explicit next steps. Actual compiled-Worker SDK CRUD
and historical-fork journeys are in `tests/worker/conversations.test.mjs` and
`tests/worker/responses-branch.test.mjs`; only syntax/format checks have run locally.


Response deletion persists a pending anchor-release intent before publishing the
canonical response tombstone. Reference cleanup uses the same Session persistence
mutex; failures retain the intent and use bounded retry backoff without delaying
unrelated admitted work. The alarm deadline is the minimum of the ordinary work
schedule and pending cleanup, including otherwise closed Sessions. Descendant,
current-workspace and user-version references remain independent strong owners.


Archive export acquires a renewable 24-hour `fork-transfer` reference before the
source dispatch fence/reply. The initialized child releases it only after its own
current-workspace reference and imported Session pointer are durable. Abandoned
admission attempts are intermediate state: only their transfer leases expire;
response, current-workspace and saved-version references never use that TTL.


## Next integration: Conversation execution admission

The authority admission receipt is the immutable acceptance boundary, not a copy
of the current Conversation on every retry. The public request hash must remain
based on the caller's original request; injecting newly changed Conversation
history before hashing would turn a legitimate retry into an idempotency conflict.
Only the newly supplied input and new output are appended on terminal commit,
never the prepended historical items.

A matching predecessor Session/agent/pin/adopted-revision hint can use the existing
warm head and append only new input. A changed visible item set or different
agent selects an independent execution lane seeded from the accepted receipt.
The public `previous_response_id` stays null for Conversation-backed requests;
an internal accepted-context binding expresses the predecessor without mixing
the two public history authorities. Source execution journals remain immutable.

A pending edge admission must not strand a Conversation after the edge dies, but
an executing long task must not lose its writer ownership to a timeout. Use a
pending admission phase with a generation/fence and an explicit canonical Session
activation compare-and-set before paid execution. Only never-activated pending
candidates may expire. A delayed old Session cannot activate a replaced admission.
Activated work clears ownership only through canonical terminal/rejection recovery,
not user inactivity. The current foundation's explicit writer gate has no timeout;
this activation/recovery coupling is not yet implemented.

Automatic terminal appends should retain per-item origin (source Response/Session
provider binding) for opaque native continuation items. A latest-agent hint alone
is insufficient in a Conversation that has changed models: encrypted reasoning
from an earlier route must not be blindly forwarded through an unrelated provider.
Caller-supplied opaque input and server-retained provider-origin output are distinct
provenance cases. Portable visible items remain ordinary Conversation data.


## Coherent integration checkpoint: implemented versus remaining

The next source checkpoint now includes actual Conversation-to-Response coupling,
not only CRUD. Accepted Conversation snapshots seed new execution lanes once;
matching pin/session/revision hints reuse the warm head. Canonical queued runs
activate the authority before model funding, terminal appends occur before the
financial cutoff, and a durable cleanup outbox releases the retained response
funding/authority fence before healthy terminal publication. Public Conversation
reads and mutations have funded exact D1/DO operation observations.

The same checkpoint includes private connection-parent authority for no-store
WebSocket continuations, transient archive anchors, source read-only context DTOs,
accepted steering source-range exclusions, prompt-version inheritance under a
queue-bound deterministic successor proof, and the finite runtime profile model.
`Session4G` delegates the same Session harness and daemon; trusted persisted profile
selects its namespace. Basic legacy pins/prices remain unchanged. Standard4g uses
the approved allocation tariff, not an invented exact active-CPU measurement.

The following are real remaining work, not exclusions from the full objective:

- Fresh Conversation lanes must inherit an immutable latest-workspace anchor
  independently of the warm-context hint. Item edits and agent changes invalidate
  context reuse but must not erase files. The current text-only integration does
  not yet provide this separate ancestry reference; do not claim workspace-
  Conversation acceptance or promote paid user flows until it is implemented.
- Generate-false warmup must retain an unchanged existing trusted workspace
  archive without starting compute. A legacy/unfunded mutable manifest remains
  explicitly unsealed; it must never be labeled an empty workspace.
- Owner-sealed canonical item resolution must cover unowned opaque/reference
  inputs; the conservative native provenance adapter does not pretend that raw
  ciphertext is trusted platform state.
- Conversation retained logical-byte capacity is an independent budget from
  physical Space/R2 bytes and monetary resource fees. The quota helper and SQL
  proposal are intentionally unregistered until the authority publication and
  ambiguous-result recovery hooks are integrated.

Once Standard4g data exists, profile-aware routing is a service rollback floor.
Older profile-unaware binaries cannot serve those Sessions; their immutable D1
profile guards prevent silent relabeling, not a transparent service downgrade.
