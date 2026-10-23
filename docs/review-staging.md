# Staging integration and trust-boundary review

Re-reviewed 2026-10-23 against the integrated source. This records independent
code-path assessment, not deployment acceptance or security certification. No real
Identity login, provider transaction, Cloudflare crash injection, or staging E2E
journey was performed by this reviewer. Production source was not modified.

## Historical terminal-phase finding — resolved

### P1: Final answer persistence lacks a terminal run recovery marker

Location: `crates/agents-worker/src/session.rs:621-630,746-779` at re-review.

The pending-completion branch appends the final assistant message, clears
`pending_completion`, increments the round, and persists the session. It then
returns the response to `finish`, which awaits resource accounting before clearing
`run`. If the DO is replaced after that persistence but before `run` is cleared,
recovery has an active run, no pending completion, no tools, and
`provider_in_flight=false`. `step` therefore dispatches another inference instead
of finishing the already completed answer. This can produce an unsolicited second
answer/provider charge. In-flight fences do not cover the completed-final phase.

Remedy: durably mark a terminal run result/phase before publishing the final
assistant update; recovery must replay settlement/finish without new inference.
Acceptance: terminate after the final-assistant write and during settlement;
recovery settles once and the provider invocation count does not increase.

Status: resolved by the durable terminal result and finalization timestamp described below.

## Previously identified blockers: resolved by source re-review

| Finding | Current corrective boundary |
|---|---|
| Subscription snapshots were arrays but Store decoded single objects | `store.rs::account_plan` decodes `Vec<Subscription>` and flattens; account/admission use the shared tier resolver |
| Closed state skipped failed container release | `archive_pending` survives Closed, alarm prioritizes its recovery, successful release clears and persists it; resume refuses pending archival |
| Repeat Closed publication failed D1 guard | `update_session_inner` permits Closed-to-Closed while retaining ownership, prefix, and input revision checks; it does not allow stale reopen |
| Ordinary inference replay after uncertain provider outcome | Durable `provider_in_flight` before dispatch; uncertain recovered outcome fails rather than replays |
| Compaction inference replay | Durable `compaction_in_flight` and `pending_compaction`; result stored before settlement/context replacement |
| Bash replay after daemon replacement | DO persists dispatch ID plus boot ID before POST; prior dispatch only polls; new boot or missing result returns uncertain without POST replay |
| Steering stranded during settlement | Finish rechecks queued input and starts a fresh run before its next await |
| Concurrent multi-chunk state publication | Persistence mutex serializes writers |
| Retired JWKS keys inherited forever | Current remote JWKS is authoritative; removed keys are not carried forward |

These are control-flow resolutions, not deployed fault-injection results. Bash's
safe uncertain outcome does not promise completion or recovery of a lost result.

## Deployment and billing gates

- Config now includes `BILLING_API_ORIGIN`, model prices, resource prices, and run
  resource reservation values in staging and production.
- Resource observations are integrated into runtime, Store, workspace, archive,
  storage accounting, and reconciliation, rather than existing only as helpers.
- Real subscription provisioning, allowances, model usage, idle container lease,
  R2 intervals, and D1 rows must still be checked through staging user journeys.
  Source integration does not prove deployed tariffs, entitlements, or settlement.
- Root reports Rust tests and WASM checks passing. This reviewer did not rerun
  that shared suite or claim those results as independently executed validation.

## Trust boundaries inspected without an additional concrete finding

Owner-scoped routes and D1 lookups precede DO forwarding; platform API keys cannot
manage provider keys; provider credentials are owner-scoped and decrypted in
Worker code; provider origins are allowlisted and redirects rejected; workspace
bash clears its environment and drops to UID 1000 while supervisor credentials
remain root-owned; browser mutations require allowed Origin and CSRF token.
AuthSession serializes rotating refreshes and persists an inflight marker so an
uncertain refresh forces re-login rather than predecessor replay. Logout remains
possible without refreshing an expired access token.

This does not replace a real authenticated staging acceptance run.

## Relevant external contracts

Cloudflare alarms have at-least-once execution; a deduplicated downstream ledger
does not itself make upstream inference/shell effects exactly once:
<https://developers.cloudflare.com/durable-objects/api/alarms/>.

The configured SQLite-backed namespaces use the SQLite storage limits, not the
legacy KV backend's 128 KiB value limit:
<https://developers.cloudflare.com/durable-objects/platform/limits/>.

## Subsequent re-review: terminal recovery fixed; concurrency hold defect confirmed

The final-answer finding above is **resolved by inspection**. `RunState.terminal`
is persisted with the final assistant update; `observed_step` returns that value
before creating new observations or calling the provider. `ended_at_ms` pins the
settlement time boundary across retries. Known successful answers are preserved
with a reconciliation-pending billing marker rather than discarded because an
accounting observation is uncertain.

### Historical P1: Financial uncertainty occupied an execution concurrency slot — resolved

Current locations: `metering.rs:118` concurrency admission; `resources.rs:277-300`
settlement; `session.rs::finish` around lines 829-850.

`settle_run` deliberately leaves the financial reservation `reserved` when an
observation is pending. `finish` nevertheless clears the running task and moves
the session to Idle. Admission counts every reserved `run:%` reservation as an
active execution. Therefore a stopped task with one uncertain resource quantity
still consumes concurrency indefinitely; on Free (one run) no subsequent task
can run, even though its predecessor has finished. A concrete trigger is a crash
after `require_observation` and before recording its quantity, which leaves that
attempt's observation pending while the recovered task can finish.

Remedy: represent execution completion separately from financial reservation
settlement, and release only the execution slot at termination. Keep the unknown
financial hold intact for reconciliation. Test an ended run with a pending
observation: the hold remains reserved, but the next run is admitted when budget
is sufficient; an actually executing run still enforces the concurrency cap.

Root and runtime notified; resolved by migration 0007 and the execution-finished boundary below.

Reproduction executed locally against all six current migrations in an in-memory
SQLite database: seed an account with 1,000,000 microUSD and a stopped run's
100-microUSD reservation still reserved. The exact admission count predicate
returned 1; with Free's limit 1, `next_run_allowed` was false despite sufficient
credit. This probes the SQL coupling, not a deployed crash workflow.


## Final source re-review status

No unresolved substantive finding remains in this scoped review. This is not a
release certification: authenticated staging and deployed recovery E2E remain
outstanding.

The execution/financial-lifetime defect is now resolved. Migration
`0007_execution_lifecycle.sql` adds immutable `execution_finished_at` independently
of reservation state. `finish` durably records its end boundary and calls
`metering::end_execution` before financial settlement. Admission counts only
reservations with no execution-finished timestamp. Unknown spend remains held.
Terminal errors are also retained for recovery without restarting execution.

Independent in-memory SQLite verification against all seven current migrations:

1. An active reserved run consumed one execution slot.
2. Setting its execution-finished timestamp reduced the active count to zero.
3. Its reservation remained `reserved` for 100 microUSD and the account retained
   that exact 100-microUSD hold.
4. Attempting to clear the finished timestamp raised `execution_already_finished`.

These checks passed. The initial six-migration counterexample above remains as
historical rationale for the state separation, not a current open defect.

## Additional public-API review: subscription freshness is not entitlement expiry

### Historical P1: SDK-only paid users downgraded to Free after five minutes — resolved

Current code at this additional check:

- `auth.rs:219-246`: platform API-key authentication returns without Billing sync.
- `auth.rs:603-604`: successful Billing sync stores `valid_until = now + 300`.
- `store.rs:243`, `auth.rs:635`, `allowances.rs:35`: snapshots past that timestamp
  are discarded before selecting active subscriptions.

A paid user who logs in, creates a platform key, then uses only OpenAI SDK calls
therefore falls to Free at login+301 seconds even when the verified subscription
has weeks left. For Pro this changes concurrent runs from 20 to 1 and config quota
from 200 to 2. Existing allowance high-water marks avoid an immediate clawback,
but do not repair the admission limits or the next allowance period.

Remedy: separate last-fetch freshness from entitlement validity using Billing's
actual grant/revocation contract, or provide a server-side refresh path that does
not require an active browser or native human access token. Merely extending an
arbitrary TTL or requiring periodic dashboard visits is not an API-platform fix.

Identity/Billing owner is researching the upstream contract and implementing the
appropriate boundary. Root notified. This is a new concrete open finding, not a
reopening of the resolved snapshot JSON-shape issue.


### Paid API-only re-review resolution

Resolved by current source inspection and independently executed SQL tests.
`Store::subscriptions` reads account/product-scoped verified grants without using
refresh TTL as expiry. Store admission, account response, and allowance sync all
share that reader; the existing Billing policy still enforces actual product,
entitlement, active status, and exact grant period boundaries.

The reviewer inspected upstream Billing's `crates/billing/src/repository.rs:111`
subscription reader: it calculates active/expired solely from period end. The
Identity/Billing owner additionally inspected routes, OpenAPI and schema at
revision `b7f6deb857dbedc87d23a00d118a3305c1b09fb1` and recorded the absence of
subscription early-revocation/cancellation in `docs/billing.md`. This policy does
not promise discovery of a later renewal without a new authorized snapshot.

Executed locally: `python -m unittest discover -s tests -p
 test_subscription_snapshots_sql.py -v` — **4 passed**. Coverage includes actual
shared subscription SQL after browser logout and refresh expiry, tenant/product
isolation, idempotent paid funding, and the unified-reader wiring. Exact grant
expiry remains enforced by the existing Rust billing policy tests (reported by
the owning agent, not independently rerun here).

After this additional paid-API fix, no unresolved substantive issue remains in
the review scope. Real authenticated staging acceptance remains required.

## Bounded platform-boundary review (2026-10-24, HEAD 9e48e3e)

Scope: remaining Worker/JS/DO/Container boundary and cleanup recovery only. No
production source change or full build. Read the pinned worker-rs 0.8.7 binding
contracts and actual-WASM harness coverage; the latter exercises D1, auth, alarms,
SSE and idle archival, but does not instantiate a Cloudflare Container.

### P1: Successful container destroy can strand archival on retry

Location at this HEAD: `workspace.rs:716-741`, specifically destroy at 720,
clock finalization at 721, and `workspace_container_exit_unreconciled` at 734.

Release destroys the container before finalizing/clearing the persisted allocation
clock. If destroy succeeds but the response is lost, or the DO is replaced before
clock finalization, a retry sees a stopped container and nonzero clock quantity.
It returns `workspace_container_exit_unreconciled`. No branch clears or transfers
that clock on a subsequent release attempt, so retrying has the same result.
Session archival remains pending and explicit resume is refused indefinitely.

Preserving an unknown financial interval is appropriate; making an already
stopped container's cleanup wait forever for that interval is not. Separate
physical release completion from uncertain accounting, as already done for
execution concurrency. Persist a release phase/intent and allow a confirmed
stopped container to complete archival while retaining the unknown interval and
hold for reconciliation. Do not invent a precise stop time from retry time.

Regression: inject failure immediately after successful destroy and before clock
finalization; retry must complete archival without restarting the container or
forgiving the unresolved spend. The current actual-WASM and daemon-only Docker
journeys do not cover this DO-to-Cloudflare-Container fault boundary.

Root notified. Other inspected startup option names (`env`, `enableInternet`),
getTcpPort dispatch and DO batch-storage map types match the pinned SDK bindings;
no additional concrete ABI finding was established in this bounded pass.

### Release P1 resolution — independently re-reviewed

**Resolved in the current working-tree patch by source inspection and seven
focused native tests.** The original finding above is retained as rationale.
No further blocking residual was found within this release-recovery scope.

- `release` checkpoints before durably writing `ReleaseIntent`, which captures
  the original clock and lease identity before destroy can occur.
- A successful destroy acknowledgement saves its timestamp. If recovery instead
  sees an already-stopped container without a saved acknowledgement, accounting
  remains pending; retry time is not invented as its actual stop time.
- `ReleaseOutcome` is durably saved before detaching active pointers. Interrupted
  detachment retries against the same original identities. Unknown accounting
  retains the intent under `workspace:release-accounting:<id>` and its hold.
- Session archival consumes physical completion independently of settlement:
  `complete_archive` requires Closed and clears `archive_pending`, preserving
  history and exposing `billing_pending`/`resource_reconciliation_pending`.
- Before a later container incarnation starts, `restore_if_needed` completes old
  detachment and deletes the old active release journal. A new container cannot
  inherit or be stopped by its predecessor's completed release intent.
- Without any post-checkpoint release intent, an unexpectedly stopped active
  incarnation remains a data-recovery error, not a falsely successful archive.

Executed independently from cached native artifacts (no full build):

```text
cargo test -p agents-worker workspace::tests --lib --locked
6 passed
cargo test -p agents-worker committed_archive_finishes_despite_financial_hold_without_losing_history --lib --locked
1 passed
```

These cover saved pending outcomes, generation-conflict rejection, original
clock/lease retention across serialization, checkpoint-source guards, and the
actual Session archive completion transition. They do not simulate Cloudflare's
real destroy-response loss or establish deployed Container fault-injection E2E.

## Independent allowance transition review (2026-10-24)

Scope: `allowances.rs`, allowance/vintage triggers, migration
`0009_authoritative_paid_allowance.sql`, [wallet](wallet.md) and [pricing](pricing.md)
contracts. No live D1 access, fund issuance, provider calls or production edits.
The staging-observed $48.744159 calendar-prorated Pro balance was accepted as
supplied context and reproduced by the owner's production-SQL October fixture.

### Findings resolved before the reviewed patch stabilized

A lower currently verified tier could not repair a legacy Free-anchored higher
nominal tier: its replacement delta was negative and failed a constraint. An
intermediate proposed floor (`target = max(nominal, spent + holds)`) avoided debt
but converted a held upper bound into spendable credit: a 9-million hold under
an 8-million Go entitlement yielded 9 million available after release. This was
independently reproduced in SQLite; that approach was discarded, not shipped as
the reviewed solution.

The final model eliminates that refund exception:

- Ordinary Free-to-first-paid and same/higher-tier legacy repair retain the
  existing funding identity and immutable reservation vintages. The replacement
  journal records the old fields, verified paid anchor/end/source, target and
  offset. Prior actual spending/holds remain inside that paid cap.
- A legacy window whose **nominal high-water tier** exceeds the currently verified
  tier retires as an old source. The new paid window receives its normal current
  entitlement; old holds keep old vintages, and their unused refunds burn rather
  than enter the new wallet. No signed correction or synthetic hold-floor grant
  is needed.
- Paid-to-paid same-cycle upgrades retain prorated high-water semantics, using
  the authoritative paid anchor rather than the former Free calendar anchor.
- Paid expiry burns its unused allowance. Free subsidy is capped at $1 per UTC
  calendar month total: returning to Free within a month already funded before
  the upgrade need not grant another dollar. A new UTC month grants normally.

### Independent verification

`tests/test_allowance_conservation_sql.py` is a reviewer-owned verification
artifact using the actual extracted production GRANT/REPLACE SQL and all current
migrations, not an alternate wallet implementation. Its 108 state sequences vary
first-paid/same-tier legacy/lower-tier retirement, actual spend, pending holds,
zero/partial settlement, replay, and settlement before/after rollover. It also
checks an old 9-million hold under a new 8-million Go window and concurrent
credentials using two SQLite connections: exactly one replacement journal event
and one target allowance result.

Executed locally:

```text
python -m unittest discover -s tests -p test_allowance_conservation_sql.py -v
1 property-sequence test passed (108 sequences plus explicit hold/concurrency checks)
python -m unittest discover -s tests -p test_allowance_sql.py -v
15 passed
python -m unittest discover -s tests -p test_subscription_snapshots_sql.py -v
4 passed
```

Migration 0009 is additive and does not rewrite existing financial provenance
merely on application. Replacement journal insertion, wallet delta and window
handoff share SQLite statement atomicity; retries cannot partially commit the
handoff. Do not remove the journal or undo financial rows as an application
rollback mechanism. An older binary does not understand the corrected paid-anchor
proration, so rollback acceptance must not promise identical funding semantics;
prefer a forward fix once a replacement is committed.

**Conclusion:** no current blocking conservation, replay, concurrency, old-hold
refund or paid-expiry issue was found in this bounded final model. These are
local SQL/state-transition results, not proof that staging has been migrated or
that its existing balance has already been repaired.

Final cheap compatibility sweep: `python -m unittest discover -s tests -p
 'test_*sql.py' -q` — **60 passed in 1.549 seconds**, including the new independent
conservation sequence test. No full Rust/WASM build was run by this financial
review pass.
