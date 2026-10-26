# Owned Code Interpreter contexts across fresh harnesses

Bounded implementation contract for the full Responses CID integration. This
note changes no production code or deployment; its concrete database scope is
one catalog table, using the project's globally assigned migration sequence.

## Decision

**Keep a fresh model harness fresh; delegate only the owned compute resource.**
An explicit `container: "cntr_…"` selects an existing Python namespace, not its
creator's conversation or Agent configuration. Create/admit the destination
Session normally and route its code operations to the physical source Session DO.
The source is a resource actor for these private RPCs; it does not run another
model, select tools, append messages or alter either Agent pin.

Use the existing `Session` / `Session4G` implementations and namespaces. No new
Container application, DO class, global lock service, copied workspace or second
harness is required. The same rule applies to same-Agent and different-Agent
requests; a same-Agent execution-view reset is not a safe shortcut because a fresh
request may need a newly revised pin while the source still owns retained history.

This is a workload decision, not a rule to minimize class count at any cost:

| Option | Simplifies | Costs / obligations |
| --- | --- | --- |
| Source Session's resource-only API, recommended now | Reuses the actual warm VM, supervisor, boot/effect/release journals and Bash/code gate; no second allocation for the common shared workspace | Must separate harness-closed from resource-retained and pass guest funding explicitly |
| Dedicated resource DO with shared Rust executor | Removes any creator-harness lifecycle from new resources; natural future explicit Containers management | Requires new 1g/4g bindings/apps, account-scoped physical leases, and moving both Bash/code ownership or paying for two VMs in common mixed-tool Sessions |
| Route fresh request to source harness or reset its mutable view | Superficially few edits | Wrong history/pin semantics; reject this option |

Keep the resource API independent of source harness fields so a dedicated executor
can replace its implementation later. If future resource workloads justify that
move, migrate **new** physical resources first and retain the existing backend for
live contexts; do not relocate a live Python heap or relabel an old physical pin.

```text
authenticated owner + target Agent + explicit CID
                       |
             D1 owner/context lookup        routing hint only
                       |
        new or already selected guest Session
        [guest pin, messages, run, model, budget]
                       |
           funded owned-context admission
                       v
    source Session/Profile DO -- one resource-operation gate
    [context identity, borrow ledger, effects, physical lease]
                       |
            same authenticated supervisor
                       v
      actual boot + CID + pinned network policy + Python
```

This separates two identities that the current bridge conflates:

| Identity | Owner and authority |
| --- | --- |
| Harness Session | Its own Agent pin, conversation, user-input clock and model execution |
| Code context | Source physical Session/profile, daemon boot, logical CID and fixed policy |
| Borrow | Source actor's durable admission of one guest run and its effective code capability |
| Code effect | Source actor's immutable guest-run/call/context identity and result fence |
| Physical compute | Existing source `ComputeLease`, allocation clock and release journal |

## What current code establishes

- `SessionRecord.runtime_profile` is immutable; `runtime_profiles::namespace_for`
  selects `SESSIONS` or `SESSION_4G` from the owned source record.
- `Workspace::execute_python` currently checks its **local** physical profile,
  fences a request against the authenticated daemon boot and never re-POSTs an
  unresolved old dispatch. Keep that implementation as the source-side executor.
- The daemon currently has one live interpreter context per physical Session and
  serializes Bash, Python, imports, captures and checkpoints through its operation
  gate. Python state is volatile; its artifacts are separately published Files.
- `AuthContext` and `api_keys` currently carry account identity, not an Agent-scoped
  API-key ACL (`auth.rs::authenticate`). Do not invent an ACL that the system does
  not enforce. Today the authenticated account can explicitly select its owned
  resource; the guest's effective code declaration remains mandatory.
- `execution_leases` in migration 0019 is currently for account operations with
  `session_id IS NULL`. It is **not** an existing general Session run lease and
  must not be treated as proof for every borrowed Session execution.

## Catalog and capacity

`migrations/0023_code_contexts.sql` and `code_contexts.rs` implement one catalog.
A private immutable allocation ID reserves capacity before outer inference; its
public CID remains null until the real kernel is prepared. The source publishes
that CID, original boot and opaque policy revision before native output. A CID
never moves to another source or revives after expiration.

The owner's independent Code cap initially equals the plan's session limit.
Capacity counts distinct retained source VMs, not every namespace generation or
guest borrow. A terminal namespace whose publication/cleanup still owns compute
retains capacity until source-confirmed release. This logical capacity is not a
financial hold and does not manufacture an exit timestamp or refund unknown cost.
Pending claims require the original funded, execution-active run; its finished
fence prevents late metadata requests from recreating a released reservation.

The catalog remains a routing projection. Every source RPC rechecks canonical
owner, CID, profile, boot, policy and use identity. Code, model history, provider
keys, network secrets and Python state never enter the catalog. The source's
immutable published artifact spool remains protected until durable guest upload
and ACK, even after its code namespace expires.

## Private resource protocol

Suggested types, not public request fields:

```rust
struct OwnedCodeContext {
    owner: String,
    id: String,
    source_session: String,
    profile: RuntimeProfile,
    daemon_boot: String,
    policy_revision: String,
}

struct CallerCodeAuthority {
    guest_session: String,
    guest_run: String,
    guest_agent: String,
    effective_binding_digest: String,
    input_revision: u64,
    idle_deadline_ms: u64,
    funding: CapturedResourceScope,
}
```

`CapturedResourceScope` pins the existing reservation ID, exact account/optional
Session scope and resource-pricing revision/snapshot. It is created by the funded
guest actor, not deserialized as authority from public JSON. The source verifies
the reservation belongs to that owner/scope and is available before doing work.

Source-side methods:

| Private method | Contract |
| --- | --- |
| `inspect_context` | Read-only canonical identity/status/policy summary; no start, lease renewal or clock refresh |
| `admit_context_use` | After guest run funding and before outer inference, durably attach the guest and return an opaque source-validated use ticket |
| `execute_context` | Validate ticket and exact operation digest, serialize through the resource gate, then use existing Workspace Python fence |
| `poll_context_execution` | Return the same known outcome or bounded pending/unknown status; never re-execute |
| `cancel_context_execution` | Cancel only the ticket's exact execution; preserve cancel-before-admission and unknown-outcome fences |
| `read_context_artifact` / `ack_context_artifact` | Scope to known outcome/artifacts; ACK only after the guest's durable publication receipt |
| `finish_context_use` | Remove the active-run interest only after canonical termination and known artifact acknowledgements |

Use fresh internally constructed requests through the owned source namespace,
following the existing Conversation/private-source trust boundary. Never forward
external trust headers, supervisor bearer tokens, provider keys or a caller-chosen
source Session directly. The source persists the ticket's identity/scope and
validates it; the public CID alone authorizes nothing.

Do not hold `blockConcurrencyWhile` across code/network/R2 I/O. Introduce one
source-local resource-operation mutex shared by **all** current local and borrowed
Workspace operations. It protects startup/restore/lease/release state as well as
daemon access; the daemon gate alone does not serialize Worker-side manifests.
Model inference holds no such resource mutex. Cancellation and read-only polling
must remain able to reach an active operation rather than waiting behind its
whole execution lock. Durable source receipts protect recovery after the mutex's
in-memory lifetime ends.

## Fresh admission, permissions and effect identity

1. Authenticate, select the target Agent and resolve the owned CID through D1.
   Unknown/unowned IDs fail without model or Container work. Do not change guest
   routing to the old harness merely because the source shares an Agent ID.
2. Bind an `Existing(OwnedCodeContext)` target separately from `LocalAuto`.
   Borrowed code's memory comes from the source reference, not the guest's own
   optional Bash workspace profile. No live resize or guest pin mutation occurs.
3. Perform normal guest run/plan funding, then source `admit_context_use` before
   paying for outer inference. A context that expires before admission fails
   without model spending. Counting uses inspection only and creates no borrow.
4. Derive an effect from guest run/call plus immutable context identity. Persist
   the source operation fingerprint before POST and retain its exact outcome
   before replying. Retries of another guest or changed arguments cannot borrow
   the old effect receipt. Boot loss or accepted-but-missing outcome means unknown,
   never a POST under the same old logical CID.
5. The guest publishes its own native item, model tool result, citations and Files.
   Neither source messages nor source Agent tools are imported. Unpublished known
   artifacts keep their existing source spool fence until the guest confirms them.

Cross-Agent reuse within the same account is real resource reuse, not a model
history fork. It requires explicit CID selection and a guest effective code grant.
The source's immutable network policy is part of that selected resource; return
its nonsecret summary during admission and reject attempts to replace it. The
source Agent's MCP credentials, model-provider keys, instructions, Bash registry
and programmatic tool grants are not resource capabilities and are never inherited.
Any future nested callback must use the guest's admitted capability proof, not the
source Agent's registry. If narrower API-key/resource scopes are introduced later,
both edge and source must enforce that real principal restriction; account match
alone must not silently override it.

## Lifetimes, archive and billing

The conversation and code-resource clocks are independent. Both explicit Session
close and ten-minute inactivity archive release only the harness/workspace interest.
An independently live Code resource retains the same VM/kernel until its twenty-
minute resource inactivity deadline or an explicit standard Containers DELETE.
Closing the creator does not refresh either clock or restart its model harness.
Old Bash-only close behavior is unchanged.

Code inspection/counting does not extend `last_active_at`. A real admitted Code
operation refreshes the resource clock once; transport/publication retries of the
same operation do not create another grace interval. A known execution or artifact
publication fence cannot be discarded merely because a timer elapsed. Periodic
funded VM keepalive may maintain the allocated resource until its absolute deadline
without claiming a new user input or extending that deadline.

The source archives its conversation while retaining the independent Code interest.
It checkpoints actual durable filesystem changes before physical release and keeps
the warm compute budget open while the Code resource owns the VM. Standard
`GET /v1/containers/{cid}` inspects ownership/lifetime without model execution;
`DELETE` revokes that Code resource and explicitly drains/cancels its operations.
A pending cleanup returns an observable conflict, not a fake successful release.
Published Files survive code-resource deletion. If no other workspace/resource
interest remains, cleanup destroys the VM, commits released capacity using its
already-reserved cleanup funds, then finishes financial reconciliation. Session
close returns retained CIDs so ongoing allocation is discoverable rather than hidden.

The physical source `ComputeLease` is renewed and charged **once**, regardless of
how many guest runs reference it. The source physical profile determines its
allocation tariff. Guest RPC overhead, input/object I/O and publication observations
use the captured guest funding scope: do not instantiate
`ResourceMeter(source_session, guest_run)` when that reservation belongs to the
guest Session. Preserve cause IDs in bounded telemetry, but do not open a second
Container clock or invent a second model/execution reservation for the resource.
Both source and guest DO work may consume resources; identical nested intervals
must not be recorded twice by wrapper and executor. Record causal linkage, not
code, logs, policy secrets or user source text, in operational telemetry.

The upstream OpenAI service documents explicit CID reuse, fixed memory for a
container's lifetime and a twenty-minute resource inactivity window. This
platform keeps the user-requested ten-minute harness policy separate from the
Code resource's explicit twenty-minute contract, with visible retained allocation
and standard explicit release. Contexts are ephemeral and expired CIDs
cannot be revived; durable artifacts remain under the established Files contract.

## Integration and focused acceptance

1. Allocate the single catalog migration and add immutable projection/status
   repair, without changing existing local code behavior.
2. Introduce `LocalAuto` versus `Existing(OwnedCodeContext)` binding targets and
   source-only resource handlers/gate. Keep guest fresh context and correct funding.
3. Add source attachment/release coordination, then register catalog publication
   in known context outcomes. No new global application/class is required.
4. Run new actual Worker + real supervisor journeys, not the old generic suites:
   - Fresh same-Agent request uses variables from the selected CID but receives no
     source messages/instructions and keeps a newly chosen guest pin.
   - Fresh different-Agent request uses the same owned kernel/profile while its
     own tools/network-policy-change attempt cannot inherit/escalate source tools.
   - Another account and forged ticket/source Session/funding scope fail before
     model, code or lease renewal; current account-level API keys remain compatible.
   - Concurrent source-local and guest cells serialize; snapshot/release cannot
     overlap either; cancellation remains prompt and does not target another guest.
   - Source user-idle archive does not stop an active guest operation; guest input
     does not change the source user clock; final last-interest removal releases.
   - Lost replies, actor eviction, boot change, late cancel and publication failure
     do not rerun code; known artifacts remain until durable guest publish + ACK.
   - A 1g guest's own workspace can coexist with a borrowed 4g context without
     resize or tariff confusion; source allocation and guest I/O are each billed
     once under the right reservation scopes.

## Evidence and engineering rationale

- Existing ownership/recovery contracts: [interpreter](full-responses-interpreter.md),
  [physical profiles](full-responses-interpreter-memory.md),
  [workspace runtime](runtime-workspaces.md), and
  [immutable response lineage](full-responses-state.md).
- [Official OpenAI Code Interpreter guide](https://developers.openai.com/api/docs/guides/tools-code-interpreter)
  establishes independent explicit-CID use and ephemeral resource semantics.
- [Cloudflare Durable Object rules](https://developers.cloudflare.com/durable-objects/best-practices/rules-of-durable-objects/)
  distinguish storage/input gates from external-I/O interleaving; serialization
  must cover resource decisions, not rely on a single-threaded marketing shorthand.
- [Chubby (OSDI 2006), primary publication](https://research.google/pubs/the-chubby-lock-service-for-loosely-coupled-distributed-systems/)
  motivates validating generation-bound ownership at the receiving resource.
  Here the source actor validates CID/boot/policy/ticket on every operation; the
  catalog and elapsed time are not fences and no general distributed lock is added.

## Local lifecycle implementation boundary (2026-10-26)

The first catalog slice is implemented by `code_contexts.rs`,
`session_code_resources.rs`, `workspace_code_contexts.rs`, and the standard
`code_context_routes.rs` GET/DELETE handler. Migration 0023 is additive; deployed
0015/0016 are unchanged. The independent registry is chunked under
`code-resources.v1`; normal alarm selection uses a cached deadline rather than
rereading it for every model step. A five-minute physical inactivity heartbeat
never advances the Code last-use clock. Source close receipts expose retained
CIDs, while legacy Bash-only close receipts retain their previous shape.

A source-local execution, workspace import, Bash/persistence operation, archive,
and logical namespace retirement share one operation mutex. Cancellation and
metadata inspection bypass that long-running gate. The native daemon's GET for
a specified CID uses an identity cache while a cell owns the kernel mutex, so
inspection does not wait behind user Python. A wrong or late old CID cannot
retire a replacement namespace. Retirement retains immutable artifact spools;
pending publication prevents physical release.

The final cleanup catalog writes precede settlement/detachment of the existing
physical lease. `release_preserving_funding` retains that already-reserved budget
after confirmed stop, avoiding a fresh-wallet requirement during zero-credit
cleanup. Session archival instead spends its existing cleanup backstop when a
Code allocation continues to own the compute lease. Python writes to an
explicitly world-shared durable workspace are detected with the existing tree
fingerprint; ordinary interpreter scratch remains ephemeral, and published
artifacts remain ordinary Files with explicit-delete semantics.

The focused new acceptance is `Code container outlives archived harness and
explicit release preserves Files` in `tests/worker/hosted-code.test.mjs`, plus
new native context metadata/release and durable-write tests. This slice does
**not** yet implement a guest Session's borrowed execution RPC or the separate
configured-executor BYOK transport; those are subsequent required slices, not
implicit capabilities of the public GET/DELETE routes. Existing known-cell
publication must never claim a new resource or replay Python during recovery.
