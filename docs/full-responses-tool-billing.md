# Hosted service fees: independent, exact upstream holds

This is the implementation contract for `hosted_service_metering.rs`, not proof
of a deployed paid tool journey. The root must review the route's pricing profile
and enable actual dispatch separately. The adapter creates no new wallet, table,
resource enum or payment integration.

## One operation, one independent hold

The Session harness first persists its tool intent, including the operation ID,
request digest, admitted timestamp and frozen `ServiceBudget`. It then calls:

```text
prepare_hold(env, owner, session?, operation, request_digest, admitted_at_ms, budget)
    -> ServiceHold
validate_dispatch_time(started_at_ms)
    -> permission to dispatch within this frozen admission
restore_existing_hold(env, owner, session?, operation, digest, admitted_at_ms, budget)
    -> optional matching hold, without creating or admitting anything
```

The supplied timestamp must be the same after cold recovery. Generating a fresh
timestamp while repeating a successful reservation would change its immutable
request hash. A small serializable hold records only owner/session/operation,
request digest and the price profile, never an API key or tool payload.

The ledger key is `tool:<service>:<operation>`, not `run:*`. Its ID additionally
hashes the owner. It neither consumes a second inference concurrency slot nor
mutates a settled language-model hold. D1's existing atomic balance/allowance
triggers remain the budget authority. The actual run was already admitted and
must still pay for Worker/Container/R2/D1 use through its normal resource meter.

Settlement and release perform an owner/session-scoped **read-only** verification
of the existing hold. They cannot create a missing reservation after an upstream
request was already sent. Request hash, idempotency key and ceiling must match.

Cold recovery first reconstructs this same identity and looks for its existing
row. `prepare_hold` returns a matching reserved hold even after its five-minute
dispatch window expired, without reserving again or changing any timestamp. The
caller must still validate the window immediately before a new HTTP request.
This distinction closes the crash gap between successful D1 reservation and
persisting the hold in DO state: a cancelled, never-dispatched operation can
recover and release the original hold rather than strand funds indefinitely.

`restore_existing_hold` also permits matching settled/released rows for explicit
known-result replay or idempotent cleanup. That financial recovery is not a new
dispatch capability. `prepare_hold` refuses finalized rows, and an expired
missing hold cannot create a new reservation. Unknown dispatches keep their
holds; expiry alone is never release evidence.

## Known units, not fabricated tokens

| Budget | Ledger unit | Authority |
| --- | --- | --- |
| Seedream 5 output | `generated_image` | Exact reported USD cost, otherwise the pinned reviewed per-image catalog tariff |
| DeepSeek Search/Fetch | `successful_search_request` | Adapter receipt: one HTTP 200 nonempty request, or zero for a known nonbillable response |
| Other reviewed service | `generated_image` or `provider_request` | Required exact provider-reported USD cost for a platform-funded route |

`Resource::Model` is the existing upstream API-credit bucket. The non-token unit,
quantity, price revision and provider request ID preserve what was actually paid.
No input/output token usage is synthesized. USD decimals reuse the existing
arbitrary-precision `store::usd_to_microusd`, rounding upward once per metered
event; there is no binary floating-point money arithmetic.

BYOK holds reserve and settle zero **platform upstream** credit. Their original
provider receipts/costs remain private provenance, and platform infrastructure
still incurs its own real charges. A BYOK receipt without USD cost is not a
reason to fabricate a cost or charge the platform wallet.

## Direct DeepSeek request tariff

The standalone Search/Fetch API costs 750 microUSD per successful nonempty
request. The official November 1 free-period note is scoped to separate
server-side Harness/Claude Code/Codex tool calls. It is not an entitlement for
this application's direct route. Its pinned revision is
`deepseek-direct-search-fetch-2026-10-26`.

A platform request reserves **750 microUSD** before dispatch. A known successful
nonempty receipt settles that amount; proved empty/failed requests settle zero.
BYOK upstream settlement remains zero on the platform wallet. Unknown outcomes
retain their original hold and never authorize another provider request.

The admitted start/completion window is at most five minutes. Actual dispatch
must not precede the persisted admission or exceed `not_after_ms`. The exact
known response stores both start and completion times. An operation whose window
expired must be re-admitted before any new dispatch; a timed-out dispatched
operation is **not** retry permission.

Search transport has a four-minute bound. A local timeout does not prove that
the remote operation stopped or was unbilled, so its receipt remains unknown.

## Commit and failure ordering

1. Persist intent and frozen admission identity.
2. Reserve its independent upstream fee hold and physical image capacity.
3. Persist dispatch start, then invoke the prepared route once.
4. Persist the exact known receipt/output source before calling `settle_hold`.
5. Append the immutable `upstream` event and settle the same hold idempotently.
6. Complete canonical artifact publication without another paid invocation.

`release_known_not_dispatched` is deliberately explicit. It must never be used
as a generic error handler. A durable, provider-confirmed nonbillable response
may instead settle `KnownUsage::ConfirmedNoCharge`; HTTP timeouts, missing costs
and unparsed error bodies are not such proof. A reported cost above the prepaid
ceiling fails settlement and keeps the existing hold for reconciliation. The
caller must not discard the durable known result or regenerate the paid image.

## Verification and primary sources

The module defines nine focused unit/property tests for decimal precision,
ownership/idempotency identity, BYOK, typed receipts and direct-route pricing.
Their Rust compilation/execution belongs to the integrated CI. The six actual
SQLite journeys in `tests/test_hosted_service_metering_sql.py` pass against the
production migrations and exact shared reservation/append/finalize queries. They
cover a paid-image replay alongside an untouched outer hold, an unknown search
hold and proved-empty settlement, owner/session recovery checks, BYOK/ceiling
enforcement, expired never-dispatched cleanup, and already-settled receipt replay.
No paid upstream request was made for this implementation.

- [DeepSeek Search/Fetch billing](https://api-docs.deepseek.com/guides/search/)
  distinguishes direct API pricing from the separate server-side promotion.
- [OpenRouter Seedream 5 Flash endpoint catalog](https://openrouter.ai/api/v1/images/models/bytedance-seed/seedream-5-0-flash/endpoints)
  was reread on 2026-10-25: Seed provider, one output image, USD 0.018/output
  image and zero input-image fee. A catalog observation is not a perpetual price
  guarantee; deployments must pin/review the profile and reserve enough credit.
- [Existing billing contract](billing.md) remains authoritative for allowance
  vintages, owner identity, subscription admission and immutable financial data.
