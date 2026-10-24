# Billing, admission, and usage accounting

## Trust boundary and existing Billing contract

Identity authenticates a principal `(issuer, sub)`. Agents stores that pair, never
email or display name. A confidential Agents OIDC client must be deliberately
registered in the same environment-specific billing subject sector as Subscribe;
adding an accepted audience alone does not join pairwise subjects. Browser code
only calls its own BFF. Access tokens, activation codes, BYOK values, and encrypted
token envelopes must never enter telemetry or client diagnostics.

The inspected upstream contract is
[Billing OpenAPI](https://github.com/kleedaisuki/moesegfault-subscriptions/blob/main/openapi/billing.yaml)
and its actual
[route implementation](https://github.com/kleedaisuki/moesegfault-subscriptions/blob/main/crates/billing/src/lib.rs).
`GET /v1/me` returns `{account, subscriptions}`. Each grant has `product_id`,
`plan_id`, `status`, Unix-second `current_period_start` / `current_period_end`,
and `entitlements`. Active means `start <= now < end`; plan names never authorize
anything. Missing or expired grants retain Free limits. A temporarily unavailable refresh does not invalidate a previously verified, still-active grant.

The initial production catalog inspection on 2026-10-23 found `platform-monthly`
(`platform.access`) and amail products, not an Agents plan. Agents onboarding now
targets product `agents` with plan IDs `agents-go`, `agents-plus`, `agents-pro` and
entitlements `agents.access` plus the corresponding `agents.tier.go`,
`agents.tier.plus`, or `agents.tier.pro`. Registration/promotion and the exact
identity client, billing audience, shared sector, and subscription checkout remain
environment-specific deployment work. A requested or staging-registered plan is
not proof of production activation. Subscribe owns activation; Agents does not
introduce a payment provider.

## Refresh freshness is not grant validity

All authorization consumers use `Store::subscriptions`: the dashboard account response,
SDK/API-key quota admission, and monthly allowance reconciliation. The persisted legacy
`subscription_snapshots.valid_until` name is only a refresh-freshness hint; it is not an
entitlement expiry. The grant itself authorizes only while its product, tier, status and
`current_period_start <= server_now < current_period_end` match local policy. Browser
logout removes the browser session, not an independently issued platform API key or its
subscription grant. A paid SDK-only user therefore remains paid after the five-minute
snapshot refresh deadline and does not need an open dashboard to retain a 30-day grant.

This policy is based on the actual upstream contract inspected on 2026-10-23 at revision
[`b7f6deb857dbedc87d23a00d118a3305c1b09fb1`](https://github.com/kleedaisuki/moesegfault-subscriptions/tree/b7f6deb857dbedc87d23a00d118a3305c1b09fb1): the Billing
OpenAPI and route table expose account/profile reads, activation-code issuance/redemption,
and amail-specific spending authorizations, but **no subscription cancellation, revocation,
refund, or subscription-change webhook**. Subscription rows have no revoked state;
`repository::subscriptions` computes `active` versus `expired` from their period end.
The activation trigger can extend the granted period; it does not provide early revocation.
The `/v1/authorizations/*` revocation operations concern amail authorization capabilities,
not the subscription grant. See the linked upstream route, repository and migration sources.

Human OIDC calls refresh Billing snapshots when their access token is available. An API key
is not forwarded to Billing and does not impersonate an OAuth bearer credential. Without
an authenticated push/reconciliation contract, Agents cannot discover a renewal beyond its
last verified grant end or promise instant reaction to an out-of-band administrator database
edit. A future cancellation or revocation capability requires an explicit trusted event feed,
service credential or delegated refresh design; it must not be approximated by revoking paid
SDK access every five minutes or relaxing Identity audience checks.

Regression evidence is in `tests/test_subscription_snapshots_sql.py` (actual shared SQL,
API-key use after browser logout, refresh deadlines, tenant isolation and idempotent funding)
and the Rust `sdk_only_paid_grant_survives_refresh_ttl_but_not_subscription_expiry` test.

### Subscribe management return

The dashboard account response supplies an environment-paired `subscription_url`
with `app=agents` and an exact HTTPS `return_to` ending in `/billing/return` on
the corresponding `adash` origin. This is a product continuation, not an OAuth
redirect URI or evidence of payment. The dashboard follows it in the same tab;
Subscribe alone owns activation and must accept the exact destination in its
own environment's return allowlist.

`GET /billing/return` is explicitly Worker-first, not static SPA fallback. It
authenticates its own host-only Agents browser session; an absent or expired
session starts ordinary OIDC with the same safe local return path. Inside the
session's serialized AuthSession, a server-owned flag fetches Billing using that
session's current access token, bypassing the normal 60-second snapshot-fetch
coalescing window. It never trusts a query-supplied plan, payer, tier or return
destination, and cannot import a different Subscribe principal's grant.

After a typed successful Billing response and allowance reconciliation it
redirects to `/#usage`. The new document selects Usage & plan from that existing
hash and cold-loads account and usage data, so no stale React Query state from
the departing dashboard can mask a new activation or renewal. Normal API reads
and API-key grant validity remain unchanged.

A failed request, non-200 response, or malformed subscription array does not
replace the last verified snapshot or call a temporary outage a successful
refresh. The return lands at `/?billing_refresh=failed#usage` with an explicit
retry link back to the fixed return endpoint. Existing grants still expire at
their real period end. No bearer token or refresh capability enters browser URLs.

The initial rollout is **staging only**. Before promoting the Agents code to
production, the independently deployed Subscribe production allowlist must first
accept exactly `https://adash.moesegfault.dev/billing/return`; adding only the
staging destination is not proof that the production round trip is enabled.
## Local policy

`agents_auth::billing::PlanPolicy` is local entitlement-to-limit policy, not an
upstream catalog. Product must match `agents`; the tier-specific entitlement
determines the tier. `agents.access` alone never selects a paid tier, nor does a
plan display name. `resolve_tier` selects the highest active grant, not the sum of
overlapping grants. `select_subscription` returns its authoritative subscription
period; equal-tier ties select later start/end then stable subscription ID.

| Limit | Free | Go | Plus | Pro |
| --- | ---: | ---: | ---: | ---: |
| Agent configurations | 2 | 10 | 30 | 200 |
| Live sessions | 2 | 5 | 15 | 100 |
| Concurrent runs | 1 | 2 | 5 | 20 |
| Included monthly API-equivalent USD | $1 | $8 | $18 | $180 |

The compatibility fields `PlanPolicy.paid` and `paid_entitlement` mean Pro limits
and `agents.tier.pro`; callers must use `resolve_tier` rather than reducing
`limits == paid` to a binary Free/Pro plan. Go and Plus remain distinct tiers.

### Included allowance funding

Included allowance is a versioned product allocation, not a cash deposit or a
payment processor. The low-level `usage_accounts` row starts with zero budget;
the trusted funding integration must atomically issue the applicable included
allowance before normal admission. It is no longer correct to leave every new
Free account permanently unfunded.

`BillingTier::monthly_allowance` returns exact microUSD values. Free funding is
keyed by the **server UTC calendar month**, never browser time or repeated login.
`free_period` uses the `time` library and returns a half-open `[start, end)` period,
including leap years and December rollover. Paid funding uses a verified Billing
subscription period and a durable unique allocation key, not a fresh key on each
profile fetch. Highest-tier selection means duplicate or overlapping grants never
stack allowance. Upgrading from Free must replace/offset its allocation rather
than adding a full paid allowance on top of the existing Free allowance.

`prorated_upgrade(from, to, period, now)` computes only the **positive difference**
in included allowance multiplied by the remaining fraction of the current
period. It rounds credit down to whole microUSD. For example, halfway through a
Go period, upgrading to Plus adds `(18 - 8) / 2 = $5`, not a new $18 grant. Equal
tier or downgrade produces zero current-period credit; downgrades are scheduled
for the next cycle and never claw back spent or reserved current-period credit.
The funding implementation must persist upgrade provenance/idempotency in D1;
this pure function does not itself issue durable credit.

Upstream's same-plan activation extension preserves `current_period_start` and
extends `current_period_end`, while subscription ID remains stable. Consequently,
using a changing `(subscription_id, start, end)` as an unconditional new full
funding grant can duplicate credit. Funding reconciliation must distinguish an
extension from a new cycle and preserve its original grant/upgrade records.
See the upstream
[activation trigger](https://github.com/kleedaisuki/moesegfault-subscriptions/blob/main/migrations/billing/0001_billing.sql).

BYOK removes platform model-provider charges, not platform resource charges.
All model and resource usage consumes the same API-equivalent allowance. Missing
price or insufficient budget must fail closed before additional paid work starts.

## Exact accounting and immutable provenance

Money is integer microUSD (one millionth of USD) within signed SQLite INTEGER
range. `MicroUsd` rejects negative values and arithmetic overflow. Resource
tariffs represent rational prices and round upward once per uniquely identified
metered event. Pin a `price_version`; do not reprice old events. Tariffs and
provider model pricing are deployment configuration, never user input.

D1 JavaScript bindings cannot exactly represent arbitrary 64-bit integers as
numbers. `metering.rs` sends decimal strings and uses `CAST(? AS INTEGER)`;
reads cast money back to TEXT before Rust deserialization. Do not replace these
bindings with `JsValue::from_f64`. Public dashboards should likewise serialize
large money integers as decimal strings or enforce a documented safe range.

`usage_accounts` owns aggregate budget, spent, and reserved amounts.
`usage_reservations` owns admission and immutable request identity.
`usage_ledger` owns resource events, their quantities, tariff versions, and optional
non-secret upstream request IDs. This audit data is financial provenance, not
ephemeral telemetry; don't TTL-delete it as diagnostics.

```text
                       append resource/model events
                                     |
reserve budget -> reserved ----------+--> settled (sum of immutable events)
                       |
                       +----------------> released (only with no recorded usage)
```

An unknown provider outcome must retain its reservation for reconciliation.
Timeout is not evidence that inference was free. Charge observed partial usage
and settle it; release only known unused holds. Do not settle missing usage as
zero. Reservations bound every run/provider call; extend or create a new hold
before additional work, never spend first and discover insufficient balance later.

Execution capacity has a separate lifetime from that financial hold. Migration
`0007_execution_lifecycle.sql` adds immutable `execution_finished_at` (Unix
milliseconds). `end_execution` records the first definitive execution end without
settling or releasing any money. Concurrent-run admission counts only reserved
`run:` leases whose execution has not ended. Thus delayed financial reconciliation
cannot occupy a Free user's sole execution slot indefinitely. Replaying an ended
run cannot launch inference again. Unknown legacy execution states remain held;
only explicit idle/closed session snapshots or finalized financial rows are
backfilled as finished.

Replay identity is `(account_id, idempotency_key)` plus an immutable request hash
and original amount. Resource event identity is `(reservation_id, event_key)` plus
its exact charge payload. An identical retry replays; a changed payload conflicts.
Use a request hash covering all cost-relevant pinned input, provider, model,
instructions and limits. Retain original keys across unknown network outcomes.

## Transaction boundaries

The initial migration uses SQLite constraints and triggers so every individual
admission/finalization statement also updates budget atomically. It rejects
overspend, release of recorded usage, non-matching settlement, non-integer money,
mutation of reservation identity, and audit-event update/deletion. Financial
records intentionally block cascading account deletion; deletion needs a reviewed
financial retention/anonymization workflow.

Use the supplied `metering::{reserve, reserve_run, append, record_resource, settle,
release}` helpers. `reserve_run` counts held `run:` keys inside the same INSERT;
run cap and budget cannot race across Workers. Calls read the immutable replay row
after writing; authorization is enforced by the owning Store before passing IDs.
Do not expose these low-level IDs as unscoped user mutation endpoints.

Configuration/session caps use one conditional INSERT, not a list/count followed
by a separate insert:

```sql
INSERT INTO agents (id, account_id, name, config_json, created_at, updated_at)
SELECT ?, ?, ?, ?, ?, ?
WHERE (SELECT COUNT(*) FROM agents
       WHERE account_id = ? AND deleted_at IS NULL) < ?
RETURNING id;
```

For dependent multi-statement changes use D1 `batch()`; its statements execute
transactionally. Never send interactive `BEGIN/COMMIT` across awaits.
[D1 batch documentation](https://developers.cloudflare.com/d1/worker-api/d1-database/).
Migration tooling must parse trigger `BEGIN ... END` blocks correctly; naive
semicolon splitting corrupts this schema.

## Resource metering and retention

`ResourceTariff` and `record_resource` accept an observed quantity with an explicit
unit and reviewed rational tariff. The catalog must distinguish CPU time,
memory-time, disk-time, egress, R2 byte-time / operations, D1 rows/storage and DO
requests/duration as appropriate; one generic runtime duration is not an upstream
bill. Aggregate cheap high-frequency events into deterministic batches, avoiding a
D1 write for every streamed token or filesystem write.

Cloudflare documents
[Containers billing metrics](https://developers.cloudflare.com/analytics/graphql-api/tutorials/querying-container-metrics/)
and [pricing dimensions](https://developers.cloudflare.com/containers/platform/pricing/).
Platform billing estimates come from `containersMetricsAdaptiveGroups`; CPU and
wall-clock duration are not interchangeable. A separately published fixed
workspace-time tariff is possible, but must not be described as exact upstream
pass-through. Runtime measurement and reconciliation must be wired before
claiming resource billing is operational.

User data: agent configs, session manifests, workspace snapshots and encrypted
secrets. Intermediate state: OIDC transactions, expiring browser sessions and
recoverable staging manifests. Observability: redacted traces and metrics.
Financial provenance: ledger/reservations and tariff versions. Retention jobs
must target these classes separately. `diagnostics_consents.enabled` records
explicit consent; diagnostic reports are redacted, bounded to 64 KiB and have an
independent expiry. Never infer consent from sign-in or subscription.

## Verification

`python -m unittest discover -s tests -p test_billing_sql.py -v` exercises full
reserve / usage / retry / settle journeys, concurrent budget admission, immutable
audit enforcement, and exact money above JavaScript's safe integer range.
`cargo test -p agents-auth` covers all four tier limits and allowances, strongest
active-grant resolution, UTC month boundaries, prorated upgrade monotonicity,
entitlement timing, idempotency conflicts, release semantics, exact rational
prices and conservation across interleaved runs.
These local tests do not establish deployed Billing audience registration,
funding, provider reconciliation, or Cloudflare metric ingestion acceptance.
