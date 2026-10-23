# Plans and included usage

Policy approved for implementation and the staging catalog on **2026-10-23**.
These are release defaults, not proof of a live production payment offering.
Deployment-owned, versioned configuration is authoritative; do not duplicate
prices or entitlement logic in the dashboard or CLI.

## Decision

Provide a usable bounded trial, a low-commitment entry tier, a regular developer
tier and a high-usage tier. All four use the same API, workspace persistence,
steering and BYOK capabilities. Paying increases usable capacity, not correctness,
data ownership or the quality of safety controls.

| Plan | Subscription USD/month | Included USD-equivalent/month | Configurations | Live sessions | Concurrent runs |
| --- | ---: | ---: | ---: | ---: | ---: |
| Free | 0 | 1 | 2 | 2 | 1 |
| Go | 10 | 8 | 10 | 5 | 2 |
| Plus | 20 | 18 | 30 | 15 | 5 |
| Pro | 200 | 180 | 200 | 100 | 20 |

Exact included allocations in microUSD: Free `1000000`, Go `8000000`, Plus
`18000000`, Pro `180000000`. Subscription list prices are `0`, `10000000`,
`20000000`, `200000000` microUSD. Taxes, if applicable, are not usage credit.
The retained subscription portion pays for orchestration and service operation;
the table does not promise a particular task count or provider token allowance.

Live sessions and concurrent runs are distinct: a retained session does not
reserve a running container. Account run limits are maximum admission limits,
not dedicated capacity or a throughput guarantee. The deployment also has a
global workspace instance ceiling (initially staging 5, production 20).
Pro's 100 live sessions therefore does **not** promise 100 simultaneous bash
containers. Return an explicit retryable capacity error when deployment capacity
is unavailable; do not leave a billed workspace waiting invisibly.

## What the credit buys

One wallet pays for two separately visible classes of usage:

1. Platform-funded model inference at the actual provider API price, including
   the provider's cache-read/write and reasoning-token categories where relevant.
   Pin provider/model and the price version to the metered event. Do not charge
   ordinary prompt prices for an observed cheaper cache hit.
2. Workspace, object, storage and database resources at separately published,
   versioned resource tariffs. Any platform tariff that differs from upstream
   invoice cost must say so, rather than claim exact Cloudflare pass-through.

BYOK means no platform-funded model charge for that call; it does not exempt
workspace, R2, D1 or Durable Object costs. The API and dashboard use the same
wallet. Do not add a surcharge merely for using the API.

The first release is prepaid and bounded. No automatic recharge, negative wallet,
postpaid overage, silent plan upgrade or new payment-provider integration. At
insufficient available credit, stop admitting additional paid work with a clear
`402` error while preserving conversation and workspace recovery state.
Existing reservations settle actual observed usage once; an unknown upstream
outcome remains reserved for reconciliation. Safety snapshot/release work needs
a bounded pre-reserved allowance and must not be skipped because inference has
spent the last credit. See [billing accounting](billing.md).

Free must actually complete a small model-and-bash journey within its allocation:
zero-budget signup with an upgrade button is not the intended trial. The default
model should be economical. Do not reserve a paid-plan-sized minimum that makes
a one-dollar wallet unusable.

## Allocation and renewal

Allocate server-side only after authenticated principal resolution, once per
account and eligible period, with an immutable grant key and allocation record.
Logging in again, rotating an API key, refreshing Billing, reopening a session,
replaying a webhook or creating another agent cannot create another grant.

- Free uses a UTC calendar-month window. Paid eligibility and period boundaries
  come from the trusted active Billing grant, not browser dates or plan labels.
- Included credit is a period allowance and does not accumulate indefinitely.
  Any future purchased balance must be distinct from expiring included credit.
- A paid renewal cannot stack with an overlapping Free renewal. Period mapping
  must prevent multiple grant IDs funding the same account/time interval.
- An upgrade takes effect immediately after trusted confirmation. Within the
  same period, allocate only the positive allowance difference, prorated for
  remaining time: `floor((new - old) * remaining_seconds / period_seconds)`.
  Persist the transition so retries cannot repeat that increment.
- A voluntary downgrade takes effect next renewal, not as a repeated
  downgrade/upgrade credit faucet. An expired or revoked subscription cannot
  mint a new paid grant. Never delete user configurations on downgrade: keep
  existing data, but reject additional creation above the new cap.
- Renewal does not erase unsettled usage, release unknown provider outcomes,
  reset financial provenance, or cancel an in-flight run merely to clear a cap.

If the upstream subscription product initially provides activation periods rather
than recurring checkout, those periods remain authoritative; do not manufacture
successful recurring payments. Subscribe owns checkout and activation.

## Abuse and rollout boundaries

The dollar ceiling bounds per-account exposure, not multi-account abuse. Use
Identity's stable principal, signup/admission rate controls and a deployment-wide
Free-grant budget/kill switch. Do not infer that a successful login establishes
one human forever. API keys all share their owner's plan, wallet and concurrency.
No anonymous paid-work endpoint or client-controlled allocation amount is valid.

Staging registrations and allocation events are synthetic test entitlements,
isolated from production issuer, accounts, money and secrets. Show test credit
as test credit. Registering the catalog does not enable real charges.
Production promotion follows the real user journey below and a deployment review
of provider prices, resource tariffs and capacity. The catalog is configurable
so measured economics can inform a later release without silently repricing old
events. Do not claim production billing merely from successful unit tests.

## Acceptance journeys

1. A new authenticated Free user sees $1 test credit in staging, creates an agent,
   runs an economical model with bash, reads itemized usage and retains a
   restorable workspace. A second login/API key does not renew the allowance.
2. A trusted staging Go/Plus/Pro grant changes the displayed plan and limits,
   allocates only the intended period credit and is recognized by dashboard and
   CLI as the same account. A forged, expired or other-product grant does not.
3. Two simultaneous admissions cannot exceed the account run cap or available
   wallet. Resource capacity exhaustion is distinguishable from credit exhaustion.
4. A retry, Billing refresh, replayed activation and repeated upgrade transition
   do not inflate credit. Renewal and an in-flight reservation conserve money.
5. BYOK records zero platform model spend and a nonzero resource charge when
   applicable. Missing tariffs fail before new billable resource admission.
6. After ten minutes without a new user message, an executing long task continues;
   on completion it archives and releases its workspace. It still consumes actual
   usage while executing. No new inference is charged just for retained history.
7. Downgrade preserves over-cap data and lets the user view, export and delete it;
   it does not allow additional over-cap creation or silently interrupt work.

## Official Devin reference, checked 2026-10-23

This is a packaging analogy, **not an ACU-to-dollar conversion** or a claim that
our allowance buys equal work. The official current
[pricing page](https://devin.ai/pricing) lists individual Free, Pro at $20/month
and Max at $200/month, with a Teams offering separately. It describes included
quota and purchasing extra usage at API pricing; it does not publish exact
USD-equivalent included allocations.

The [self-serve billing guide](https://docs.devin.ai/admin/billing/self-serve)
documents quota periods and prepaid extra credit. Its
[usage guide](https://docs.devin.ai/admin/billing/usage) distinguishes work from
sleeping. We reuse the useful product principles: a real bounded trial, a
clear step up to heavier use, visible spending and inactive compute release.
Our monthly dollar allocations and ten-minute lifecycle are our own choices.

Cognition's [2026-04-14 migration announcement](https://cognition.com/blog/new-self-serve-plans-for-devin)
retired old Core and Team self-serve plans and moved their overage to dollars.
Current [Enterprise billing](https://docs.devin.ai/admin/billing/enterprise)
still uses contract-priced Agent Compute Units. Old references to $2.25/ACU,
$500 Team or 250 ACUs must not be used to derive present provider-token credits.
