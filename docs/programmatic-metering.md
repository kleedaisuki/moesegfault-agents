# Programmatic runtime funding

`programmatic_metering.rs` reuses the existing immutable D1 wallet. It does not
create another resource ledger, payment API, schema or JavaScript execution
loop. The Rust actor remains responsible for durable dispatch fencing; a funded
reservation is never permission to repeat an unknown program execution.

## Explicit public allocation profile

The operator must configure these existing `RESOURCE_PRICING_JSON` tariffs with
`resource = "worker"`, a common explicit deployment version, and the exact rates:

| Unit | Numerator / denominator, microUSD | Evidence and meaning |
| --- | --- | --- |
| `dynamic_worker_creation_day` | `2000 / 1` | Root-approved public-rate allocation per owned opaque Worker ID/code/UTC day |
| `dynamic_worker_request` | `3 / 10` | One actual Loader fetch request; existing per-event integer rounding charges 1 microUSD |
| `dynamic_worker_cpu_slot_100ms` | `2 / 1` | One reserved-capacity slot per configured/enforced 100 ms of the accepted program's single native request |

These are allocations at public marginal rates, **not** a claim about the final
Cloudflare invoice after its included monthly usage. A missing or incompatible
tariff disables programmatic admission explicitly; ordinary resource consumers
and health checks do not acquire a new configuration requirement.

The CPU allocation's approved revision is
`2026-10-25-dynamic-allocation-v1`, recorded as the allocation profile in its
receipt. It is not the global tariff version: the existing common-version
RESOURCE_PRICING_JSON contract remains unchanged. Admission pins the exact
operator tariff and deployment version;
missing/wrong configuration fails only programmatic admission, not ordinary
health/resource consumers. It is **reserved capacity, not measured CPU time**.
Startup parse/initialization is disclosed as part of the daily creation
allocation, not claimed to be another measured or bound CPU duration.

The runtime now has a single duplex native fetch whose NDJSON request/response
stream carries nested tool signals. Its configured `limits.cpuMs = 100` covers
that original execution context, including JavaScript continuations. After an
actual native source and valid frame, only the runtime can mint the private,
non-deserializable `EnforcedProgramBudget`. `record_capacity_allocation` verifies
its Worker/code/configuration against the pinned hold, computes
`ceil(configured_ms / 100)` and settles the published named allocation **once per
accepted program identity**. The initial enforced profile is 100 ms: one slot,
2 microUSD. Awaiting tools and sending/receiving stream frames adds no slot or
request. Early known success need not refund that provided published allocation.

A live allocation receipt closes the earlier provisional `MeteringGap`. Unknown
execution, no trusted single-origin scope witness, or a legacy hold without a
pinned allocation tariff remains an explicit gap; it is never implicitly charged
using a current tariff or fabricated actual CPU quantity. No `cpu_ms` usage or
await wall time is recorded. Multiple independent resume fetches are not a proof
of one cumulative 100 ms scope and cannot mint the allocation witness.

## Identity and atomic deduplication

`TurnAdmission` contains trusted owner/session, opaque Worker ID, full module
SHA-256, durable actual-request attempt ID, original admitted time and the
configured single-origin CPU limit. The initial profile admits only
`cpu_limit_ms = 100`; that field is a real runtime configuration, not a CPU
observation.

- Creation identity hashes owner + opaque Worker ID + code revision + UTC day.
  Stable `.get()` resumes in that day reuse its known allocation. Changed code,
  a fresh `.load()` identity, another owner or a new UTC day does not reuse it.
- Request identity additionally binds the durable actual-fetch attempt, not a
  semantic await or a transport retry.
- The unresolved CPU hold binds only the accepted program identity, not a resume
  attempt or a new UTC day.

The existing account/key uniqueness and budget triggers atomically reserve and
deduplicate each dimension. Creation's immutable metadata records the first
claiming attempt. Another attempt cannot treat a pending unknown creation hold
as evidence of a usable isolate. A known settled daily creation is read-only
reuse, including after an operator price-revision change; its original reviewed
tariff remains the immutable authority. Financial metadata uses the established
`usage_reservations.request_hash` snapshot convention and contains no raw program
source, provider credentials or tool arguments.

## Admission, dispatch and known receipts

```text
persist TurnAdmission in the actor
prepare_turn(env, admission) -> FundedTurn
persist actor dispatch intent
FundedTurn.verify_before_dispatch(env, worker_id, code_revision, cpu_limit, now)
invoke exactly one native runtime fetch
persist trusted native receipt and actor outcome
record_known_execution(env, funding, start_ms, completion_ms) -> MeteringGap
record_capacity_allocation(env, funding, runtime_enforced_budget) -> CapacityAllocationReceipt
```

`FundedTurn` deliberately has no `Deserialize` implementation and no public
fields. Only scoped, validated existing ledger rows mint it. Verification checks
the actual runtime identity/configuration and current reserved state, the
30-second start-admission window, and unchanged UTC day immediately before a
new request. It rechecks the trusted clock **after** asynchronous ledger reads;
the runtime also calls synchronous `verify_dispatch_time` immediately before
native Loader effects. A D1 wait across midnight or the 30-second admission
window cannot admit a new-day request under a stale old-day hold. These guards
do not supersede the actor's stronger no-replay fence.

Any known native fetch response is a request observation even if its status/body
does not prove successful isolate execution. `record_known_request` can settle
that actual quantity one while leaving creation/CPU uncertain. A validated
known execution separately settles the first daily creation, provided the
start/completion span has a definite UTC day. A response spanning midnight keeps
the creation hold pending rather than guessing the supplier's attribution day.
No synchronous CPU counter is inferred from a successful reply. The separate
trusted capacity witness is not Caller JSON or a self-reported JavaScript field.

## Recovery and cancellation

`restore_turn` is read-only and works after start-window/UTC expiry and after
settlement. It validates owner/session, immutable metadata, identifiers and
amounts without refreshing the admitted timestamp, looking up a new price, or
creating a reservation after a possibly executed request. Partial funding returns
`None`; it is not permission to manufacture missing holds after an unknown call.

`release_partial_known_not_dispatched` requires the actor's durable proof that
this attempt never dispatched. It discovers existing partial rows read-only,
releases only the current attempt's unconsumed records, and preserves another
attempt's CPU gap and a shared known daily creation. It refuses a contradictory
already-known request. An unknown result remains held and never creates another
JS invocation. A cancelled first creation claim ends that identity's admission;
it is not silently resurrected under a new attempt.

## Verification

Nine focused Rust unit/property tests cover stable/day/code/owner identities,
unknown creation fences, reviewed rates and rounding, expired recovery, profile
revision reuse, delayed verification across midnight, named capacity and unknown
gaps. All nine passed after real Wasm compilation in
[GHA 40339977394](https://github.com/kleedaisuki/moesegfault-agents/actions/runs/40339977394).
Six earlier actual SQLite
journeys use all production migrations and exact shared reserve/append/finalize
statements: distinct native requests with one daily creation, changed identity/day,
unknown scoped recovery, partial-budget cleanup, and one known named capacity
allocation with no extra wait charges. These pass locally;
those historical SQL checks used no Loader or external paid API. The integrated
compiled-Worker journey now checks the actual Loader, Session and shared wallet,
including replayed Responses and two awaits under one configured CPU allocation.
Its initial GHA execution is pending; staging and production outcomes are not
inferred from unit or SQL results.

Primary source: [Cloudflare Dynamic Workers pricing](https://developers.cloudflare.com/dynamic-workers/pricing/),
read 2026-10-25. It defines unique ID/code daily counts, `.load()` identity
behavior, per-fetch requests, and startup plus active CPU rather than I/O wait.
