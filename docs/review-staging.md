# Staging integration and trust-boundary review

Re-reviewed 2026-10-23 against the integrated source. This records independent
code-path assessment, not deployment acceptance or security certification. No real
Identity login, provider transaction, Cloudflare crash injection, or staging E2E
journey was performed by this reviewer. Production source was not modified.

## Current finding

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

Owner: runtime and root notified. Status: corrective implementation requested.

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

### P1: Financial uncertainty permanently occupies an execution concurrency slot

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

Root and runtime notified. This is the current unresolved code-path finding.

Reproduction executed locally against all six current migrations in an in-memory
SQLite database: seed an account with 1,000,000 microUSD and a stopped run's
100-microUSD reservation still reserved. The exact admission count predicate
returned 1; with Free's limit 1, `next_run_allowed` was false despite sufficient
credit. This probes the SQL coupling, not a deployed crash workflow.
