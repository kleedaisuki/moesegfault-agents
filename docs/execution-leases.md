# Local execution leases for account model operations

## Two facts, two lifetimes

A provider request can outlive the local actor's network I/O. Cold recovery can
therefore establish **no local execution** without establishing zero provider
usage, a failed upstream request, or a refundable monetary hold. These facts
must not share an expiration timer or a finalization operation.

`0019_execution_leases.sql` adds an optional immutable generation to a run
reservation and a small execution metadata row. One reservation INSERT admits
both funding and capacity: the existing budget trigger and the new lease
creation trigger belong to the same SQLite/D1 transaction. Budget failure rolls
back both. Admission counts all legacy active reserved runs and new active leases,
excluding only new exact-identity leases explicitly marked `no_local_execution`.
No age, alarm deadline, stale heartbeat or elapsed task duration releases capacity.

## Identity and actor fences

New account operation receipts receive a generation when accepted and durably
persist it before admission. Retries reuse that generation. The admission engine
must validate owner, operation identity, request hash, amount, reservation state,
generation, and active execution status before dispatch. A released generation
cannot acquire a new lease or dispatch again under the same operation identity.

After a cold actor observes an in-flight durable phase but no recoverable result,
it first persists a terminal failure fence, then releases only its exact
`(owner, reservation_id, generation)` execution lease. A delayed old callback or
another tenant cannot release a different generation. Metadata transitions only
once from `active` to `no_local_execution`; generation and terminal state are
immutable. Repeating the same release is idempotent.

A known RESULT is recovered before classifying an in-flight operation as unknown;
that path resumes accounting/projection, not another provider call. Healthy
in-flight work stays active regardless of duration. Terminal success/failure and
retention tombstones must retry local-lease release even when no duration
measurement survived. A cold actor never calls `finish_run`, invents a zero
measurement, or marks unknown money settled/released merely to free capacity.

## Legacy and rollout

No existing reservation is rewritten. Missing generation/lease metadata means
legacy conservative counting under `execution_finished_at`, not permission to
release or guess a lease. Existing immutable ledger, wallet, provider completion
receipt and resource observation state are unchanged. Session reservation APIs
retain their original behavior. The new account-operation API uses the same
reservation engine with optional lease metadata; this is not a parallel wallet.

An old binary does not recognize explicit released leases and may conservatively
count their still-funded unknown reservations against capacity. Preserve the
migration and use lease-aware binaries for availability after rollout; never fix
rollback availability by releasing unknown funds.

## Pre-dispatch rejection

All fallible admission, completion-reservation, provider-resolution and request
preparation work happens before the durable `in_flight` dispatch fence. A failure
is durably terminal, not an unbounded five-second Accepted retry queue. Known
budget rejection is HTTP 402, run capacity rejection 429, and identity conflict
409. An unclassified admission failure is fenced as `compaction_admission_unknown`
503, never a reason to issue another provider request. Error projection uses a
fixed code allowlist, not raw provider/configuration text.

The actor persists `rejected_before_dispatch` with Failed before attempting
cleanup. That authoritative no-dispatch proof permits release only of exact
operation holds without any ledger entry or resource observation. A pending
observation keeps its run funding held. An unrelated hold, old generation,
legacy reservation or unknown provider outcome is never refunded. Terminal
observer notification precedes cleanup, so a transient accounting failure cannot
turn a known rejection into an invisible queue. The next alarm retries cleanup,
not inference; same-key callers read the durable rejection.

A durable provider RESULT with permanently missing or invalid usage is also
terminal, not an eternal Known accounting retry. Permanent usage/cost/bound
validation and explicit immutable-ledger/funding constraints become a fenced
failure, release only local capacity, and keep the model hold and result evidence.
`rejected_before_dispatch` remains false. A bounded provider request identifier
is retained in the minimal receipt for reconciliation after private result
retention ends. Transient database I/O still retries accounting from the same
known result, never another provider call.

## Verification and runtime integration

`tests/test_execution_leases_sql.py` runs the real admission/release/unused-hold
SQL with all migrations in SQLite. Ten focused tests cover funded unknown
outcomes, strict tenant/generation identity, no healthy-task TTL, conservative
legacy rows, atomic budget rollback, immutable terminal leases, proven-unused
refunds, pending-observation protection, stale-generation refund rejection, and
six competing connections admitting exactly one plan slot/hold.

The shared reservation engine accepts an optional account-operation generation;
legacy Session wrappers still use the same original interface. The actual Worker
journey `ModelOperation execution lease cold loss and terminal admission rejection`
uses a test-only subclass to seed a lost in-flight actor and real immutable
financial holds. It checks actual Rust failure/recovery, unchanged unknown money,
no provider replay, a new independent operation using the freed Free slot, and
public terminal budget rejection both before run admission and while reserving
the model completion. The same planned successful provider call is paused at the
controlled transport: a distinct operation must receive 429 without another
provider call or monetary hold, while the healthy original keeps its slot. The
fixture waits for the original terminal cleanup acknowledgment before beginning
the next independent scenario; it does not reinterpret observer publication as
proof that asynchronous cleanup has already finished. An actual synthetic native provider response without usage
also proves terminal failure, unchanged reserved model state, local-capacity
release and exact same-key no-replay behavior. Synthetic seeding is not evidence of a previously executed
external model call. The fixture module is absent from production bundles.
These new native/Wasm tests require the integration captain's GHA execution;
source/SQL approval alone is not runtime acceptance.
