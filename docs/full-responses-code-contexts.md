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

The focused lifecycle acceptance is `Code container outlives archived harness
and explicit release preserves Files` in `tests/worker/hosted-code.test.mjs`, plus
new native context metadata/release and durable-write tests. Existing known-cell
publication must never claim a new resource or replay Python during recovery.
The separately configured executor/BYOK route is not an implicit capability of
these public GET/DELETE routes.


## Borrowed execution data path

`code_owned_bindings.rs` resolves owner/catalog/live identity before outer model
admission. The frozen `CodeContainer::Existing.source` contains only the real
CID, source actor/profile, boot and policy revision. Its allocated memory affects
Code execution, not the fresh guest Session's physical profile or instructions.
A borrowed 4g resource therefore does not allocate or price a second 4g VM.

The same-actor case uses the local runtime rather than making a reentrant RPC
while holding that actor's operation lock. Cross-Session calls use
`code_borrowed_client.rs` and `session_code_borrowed.rs`; neither endpoint starts
or resumes the source model harness. Each fresh internal request carries the
existing funded actor grant for the **guest** Session/run. Source wall time is
observed against that grant; the single physical lease remains source-owned.
A ticket is an idempotency identity, not authorization. The receiver validates
owner, fixed generation, live allocation, requested memory, no network-policy
override, and guest funding. Only the code string is a model-controlled argument.

```text
Guest model/context -> CodeRuntime::Borrowed -> source resource RPC -> same kernel
       |                       |                         |
       +-- guest outcome ------+                 source compute lease
       +-- Files publication -> artifact ACK -> publication-complete barrier
```

Source receipts separate small mutable metadata from exact outcome bytes.
Result caching and publication metadata share a short metadata gate; execution,
checkpoint and namespace mutation share the operation gate. Poll/cancel never
wait behind user execution. Receipt state is `prepared -> result_known ->
published`; a late poll cannot overwrite published ACK state. A published
receipt is a permanent non-execution fence. After the guest has durably saved
the exact result and all Files publication/ACK decisions, the source clears its
large outcome payload while retaining the small ticket/artifact fence. It is not
a second archive of the guest's logs or conversation.

The new Worker journey `owned CID reuses the archived source kernel with fresh
guest context and no second VM` checks source archive, another Agent/Session and
model key, unchanged source user clock and model-call count, isolated guest
instructions, the same CID without another namespace preparation or guest VM,
one catalog allocation, compacted source result bytes, and a foreign-owner
rejection before any model or code execution. This controlled-supervisor journey
establishes routing/state ownership, not an additional claim of real Python
execution; the native kernel acceptance remains separate.


## Resource use must have a funded idle horizon

A thirty-minute compute lease with a fifteen-minute renewal margin is not enough
by itself to promise twenty minutes after each Code use: at minute twelve, the
old threshold permits reuse even though the new resource deadline is minute
thirty-two and physical funding ends at minute thirty. Before a fresh managed
prepare or borrowed cell, `ensure_code_lease` requires coverage of the independent
twenty-minute resource clock plus the bounded two-minute native prepare window.
This is an explicit-use admission requirement, not background renewal. Idle
heartbeats still neither advance resource last-use nor reserve new money.
The previous interval is measured/settled before activating the next lease, using
the existing reserve-before-transfer protocol and immutable tariffs. Ordinary
Bash callers pass no minimum horizon and retain their previous renewal policy.
Known/ambiguous result drains do not renew or start a replacement kernel.

The focused Worker counterexample retains the same live source VM but gives its
already reserved compute lease eighteen minutes of remaining horizon. The
pre-fix compiled Worker executes the new cell but fails the assertion that its
promised resource expiry is covered by physical funding. The new journey checks
a distinct next lease, coverage through resource expiry, and settlement of the
old interval without a second VM. It is a controlled state-boundary test, not a
claim of waiting twelve wall-clock minutes or a new real-Python acceptance.


## Independent compatible executor BYOK

An Agent may separately configure
`tool_services.code_interpreter={"provider":"workspace_daemon","credential_id":"..."}`.
This is an owned bearer credential for a compatible **executor service root**,
not the outer language-model credential and not an inferred vendor execute API.
The platform calls the real workspace-daemon `/v1/python` context, cell, status,
cancel, file and artifact APIs; it never adds another inference to emulate Code.
Origins are immutable and redirects are rejected. Decrypted keys are neither
serialized nor logged.

The authenticated capability contract reports actual accepted memory limits and
an atomic prepare boot header. External reference executors validate exact
1g/4g cgroups; the separately attested managed Cloudflare VM boundary is described
in `full-responses-interpreter-memory.md`. The executor memory domain also expresses16g/64g without
pretending those are available Cloudflare SKUs. Provider selection precedes CF
physical-profile selection. `source_profile` routes the original actor only;
`executor_kind` and `memory_limit` describe its actual Code resource. An external
4g executor can therefore use an ordinary1g actor without starting any local VM.

The reference protocol has a dedicated single-instance kernel. Its normalized
service origin/path is leased exclusively across source allocations after
successful authentication; another source must use the owned CID or a separate
instance. A new Auto configuration must not silently share or reset another
source's live heap. Existing CIDs keep using the original actor, credential route,
boot and policy. Model arguments still cannot select another resource.

Migration0027 adds independent backend/memory, exclusive executor lease and
cleanup-funding descriptors. Remote resources reserve a small account-scoped
cleanup hold before execution, so twenty-minute expiry does not depend on a
local CF compute lease or a fresh wallet top-up after harness archive. Upstream
executor fees are BYOK; only actually consumed platform infrastructure is charged
here. Known/ambiguous dispatches retain their original no-replay fence. Exact
outcomes and ordinary Files publication decisions precede artifact ACK; scratch
is not a fake durable Python heap.

## Accepted executor increment and actual CF staging boundary

The separate executor implementation is integrated from topic source `1759066`
with the transport, immutable-identity, cleanup-hold and executor-first admission
repairs. CI `40330823824` succeeded with the compiled Worker at
`3828b61b23c43a1a39352ff9ce3ae98f399cc6d5` (Worker artifact `12653224948`,
receipt artifact `12655312277`). Its actual Docker executor is the image at
`7c2692092e40d3a5594d57b935552c24e5080c87`, built by the successful
`native-executor` job in run `40324162172` (image artifact `12650502303`, native
receipt `12650522161`). That earlier run's Worker job failed; only its successful
native image/capability evidence is reused.

The new real owned-CID guest journey subsequently passed in run `40335887876`
at test source `77fc998fc1503bd889582d83bdeb70ff462eeb63`, reusing those exact
binaries. Receipt artifact `12656223867` records two real cells, one namespace
prepare, two published/ACKed artifact captures, one explicit release and the
unchanged nine-byte CSV. The original source harness prints 42, is archived,
and the isolated guest prints 43 from the same Python heap without a local VM.
The daemon content-addresses equal artifact bytes, so the two legitimate cell
captures can have the same artifact ID; ACK fencing is per cell/publication, not
global content-ID uniqueness. The earlier failed fixture jobs remain failed and
must not cause another replay of the accepted cells.

These are real Rust Worker plus real Docker executor receipts, not a claim of
actual Cloudflare managed Code acceptance. Staging previously retained the old
`b1edf977` daemon with Code and `Session4G` disabled. The explicit deployment
selector `web_search,file_search,code_interpreter` now retains both `Session/basic`
and `Session4G/standard-1`, keeps accepted Search services, and requires normal
verified daemon provenance rather than the old disabled-runtime exception.

`tests/e2e/hosted-code-sdk.mjs --memory 1g|4g` is the tracked narrow live driver.
Each selected profile permits one new direct-DeepSeek Response and one Python
cell, with an 8192-token context and 512-token output bound. The cell prints 42,
publishes the exact nine-byte CSV and reads actual kernel `MemTotal`. The original
live failures established that Cloudflare's process cgroup is unbounded, not a
finite 1g/4g cgroup. Owned container metadata must therefore supply matching
`cloudflare_vm` allocation/class/image evidence, with separate nominal allocation
and observed guest bytes; the actual cell observation must equal that guest value.
It then archives the harness, observes the independent CID still running, deletes
the CID, requires 404, and reads the unchanged ordinary File. A fresh receipt
fences intent before submission; ambiguous accepted operations must be reconciled,
not retried. Actual Cloudflare profile runs, application/SKU identity and scoped
financial closure are still required before claiming managed Code delivery.

## Actual managed CF startup observation (2026-10-26)

Normal CI `40338717364` succeeded at `1b7f2cd09fc0be114fbbab673c85b5e0a894811f`
(Worker artifact `12657717444`, daemon image `12658872196`, dashboard
`12657194328`). Deployment `40339986401` enabled accepted Search/FileSearch plus
Code without invoking legacy paid acceptance. The basic application
`a03bce76-eb9d-4db6-a9a1-e486fab9fe60` reached version 19 with the new image;
the new standard-1 application `a032fd91-f451-4d4b-87b9-60e12e8af304` exposed
4096 MiB, 0.5 vCPU and 8 GB disk. The basic application's GET continued reporting
its old configuration during its two-step rollout. The rollout completed at
14:36:23 UTC; a later GET showed version 19, the exact new image and five healthy
instances. A green deploy alone did not establish that intermediate convergence.

Each profile received one genuinely new direct-DeepSeek Response, not a replay:

| Profile | Response | Stateless count | Original tool result |
| --- | --- | --- | --- |
| 4g | `resp_64db734809b346039c6fc8a38f469b3b` | 474 | `code_memory_profile_mismatch` |
| 1g | `resp_c9b4a22ebf894af79e44e9d9e2bf49fd` | 471 | `code_memory_profile_mismatch` |

Both models called the correct `__agents_code_interpreter` alias. Both failed
before a public Code item, CID, Python cell or File appeared. The 4g Session's
actual D1 profile and saved pin were both `standard4g`, not basic; its reserved
Code allocation had no CID. The source decoder collapsed supervisor 422 errors
into the same mismatch code, so the error is not evidence of incorrect Session
routing. The outer responses nevertheless completed after a second model round.
This is an observed failure requiring repair, **not managed Code acceptance**.
The original Sessions were archived and their temporary Agents deleted. Each
retained a separate compute hold/pending observation requiring normal resource
settlement, never manual SQL refunds. The 4g allocation later expired and marked
physical release through its existing alarm, independently of that money hold.

A new model-free `workspace/commit` diagnostic successfully started the same
basic image with zero LLM calls and zero Python cells. SSH required a configured
matching authorized key, so no key was added and no old VM was restarted. That
new Session was normally archived. A separately authorized, single new bounded
Bash Response then read only safe OS metadata and was normally archived:
`resp_652bf9e5b9e24124ab6c334242a7cd76`. Its actual guest observation was:

- cgroup membership: `0::/`.
- cgroup2 mount root: `/..`, mounted at `/sys/fs/cgroup`.
- Standard v2 `memory.max` and v1 `memory.limit_in_bytes` were not readable in
  that guest view; they must not be invented from an application label.
- `MemTotal=1242872 KiB`, `MemAvailable=1133584 KiB`; physical guest memory is not
  equal to the advertised 1 GiB workload allocation and cannot simply replace a
  missing cgroup limit.

These observations distinguish a Docker cgroup assumption from the actual CF
runtime. The repair must explicitly model the constraint source and preserve
strict ordinary-container checks, while retiring failed reserved allocations
and settling their real start/stop costs. The new profile-specific live behavior
and financial closure remain required after repair. Safe metadata and original
operation identities are retained under `.temp/code-staging/` in the integration
checkout; the original source/run conclusions remain unchanged.

not retried. At driver preparation, actual Cloudflare runs and scoped financial
closure were still pending; the following receipts close that managed boundary.

## Actual managed Cloudflare delivery completed, 2026-10-27

Staging deployment `40360107662` enabled the explicit Code selector without a
rebuild. Its assembly/daemon source is
`3539c1c9fb3a247c465249eb59a4dd2a438b116e`; actual Worker source remains
`1d1c3a21d6bf91cfd90029bf7230834ae414c83b`, not the newer assembly label.
Deployment provenance binds Worker artifact `12663258034` from `40349988842`
and daemon artifact `12663918464` from successful normal CI `40353675534`.

Each repaired profile used one new bounded direct-DeepSeek Response and one real
Python cell. Neither original failed Code request was replayed. Both passed all
five native Code phases and original-payload Python SDK 3.37 validation (117
events for 1g, 121 for 4g), printed 42, and published the exact nine-byte CSV with
SHA-256 `e642f71729782d3a01113233973eacdad72f4ddf49d5e9b9535c28ae82fceef5`.

| Profile | Native response | Real CID, subsequently deleted | Allocation / observed guest bytes | Total / compute microUSD |
| --- | --- | --- | --- | --- |
| basic 1g | `resp_27e56a74268b440da021c61678a03f2e` | `cntr_28aad6df300a4c069ba582d5a120a847` | 1073741824 / 1272700928 | 1815 / 1183 |
| standard-1 4g | `resp_f1e0c4f679424601af1c3013b9fa153d` | `cntr_d48f7fbfe72f4966a6c3c67bec2e1772` | 4294967296 / 4366254080 | 1298 / 849 |

Both live metadata proofs reported `capacity_source=cloudflare_vm`, the matching
`Session`/`Session4G` class and exact verified daemon source. Each actual cell's
independent `MemTotal` observation matched the separately published guest bytes;
neither result pretends the guest's unbounded cgroup has a finite 1g/4g ceiling.

For each same accepted operation, normal harness archival left the CID running;
explicit Containers DELETE then produced CID 404 while the ordinary published
File remained readable with the original nine bytes. Both catalog allocations
are deleted/released and both compute leases settled against their actual wall
intervals. The scoped postflights include the new account operations as well as
Session rows: zero new holds and zero pending observations. Managed catalog
`memory_limit=NULL` correctly retains its existing `source_profile` fallback;
no representation rewrite or manual SQL settlement was used. All collectors
stopped after the final receipts.

The operator receipts are in the integration worktree's
`.temp/code-staging/repaired-{1g,4g}-{receipt,postflight-confirmed}.json`.
The earlier failed preparations and their genuinely unknown exit-time financial
holds remain separate historical reconciliation work; they are not relabeled
successful or silently refunded by these new clean receipts. Do not rerun either
repaired profile, the accepted BYOK guest journey, or the original paid failures.
