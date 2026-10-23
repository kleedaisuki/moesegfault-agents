# Allowance wallet

`allowances::sync` accepts only an authenticated account ID, reads server-owned,
verified Billing grants through `Store::subscriptions`, and resolves the reviewed `PlanPolicy` using each grant's own active period. A snapshot refresh deadline never expires a paid grant or requires SDK-only users to keep a browser signed in. See [the refresh/validity boundary](billing.md#refresh-freshness-is-not-grant-validity).
Client request bodies cannot select a funding amount. Free receives $1 per UTC
calendar month; Go / Plus / Pro receive $8 / $18 / $180 per paid window.

Paid windows are deterministic 30-day slices anchored at Billing's authoritative
`current_period_start`, bounded by `current_period_end`. Upstream early renewal
extends the latter without changing the former, so hashing both bounds as a grant
key would mint duplicate funds. Slices prevent this. A short final validity window
receives at most one allowance; it never funds work after the verified end.

Free-to-paid activation **replaces**, rather than adds to, the Free allocation.
The paid window ends at the authoritative paid slice end, never at the old Free
calendar-month end. Existing actual spend and outstanding holds remain inside
the new paid ceiling. A $1 Free allocation becoming Pro therefore has a total
allocation of $180, not $181, and any already spent/reserved amounts reduce its
available balance. This also repairs the former calendar-pinned upgrade state.

The existing window identity and reservation vintages are retained during this
handoff. Immutable `allowance_replacements` records the original source, end,
ceiling and credited amount, the authoritative paid anchor/end/source, and exact
replacement delta. The budget increment is `paid_target - previous_credited`;
the effective window records the paid target. Historical grant events remain
unchanged. Later hold release restores only funds already within that same cap.
If an old legacy higher-tier allocation is no longer backed by the currently
verified lower paid tier, it is retired instead: its unused balance expires,
old holds keep their original isolated vintage, and the new paid window receives
its own allowance. An old upper-bound hold must never turn into a larger new-tier
allowance merely because its eventual charge was lower than reserved.

Within one paid slice, an upgrade credits only the positive difference from the
highest previously seen nominal tier, prorated using the **paid anchor** and
remaining seconds, rounded down. Downgrades do not issue credits or revoke
already allocated current-cycle credit. A renewal cannot issue the next slice
before its actual boundary. At paid expiry, the unused paid balance expires and
the effective source becomes Free, never a still-spendable paid window. Free
subsidy grants across one UTC calendar month total at most $1, including Free
granted before a paid activation; returning to Free that same month does not
grant another $1. The next UTC month receives its normal $1.

`usage_accounts` remains a cumulative exact microUSD budget/spent/reserved wallet.
On a new window, budget is reduced to spent + outstanding holds before the new
allowance is added. This expires only unspent credit, not unknown upstream usage.
Reservation vintages record the funding window at admission. Settlement or release
of an expired hold removes its unused portion from budget rather than making it
spendable again. Holds created before the migration use an expired legacy vintage.
Financial timestamps are Unix seconds; expiry comparison tolerates legacy
millisecond reservation timestamps during the ongoing runtime migration.

Funding uses one serialized SQLite INSERT and triggers, not a balance read followed
by a write. Grant amounts and upgrade arithmetic come from the current SQL
high-water mark. An expected-window comparison prevents a competing stale rollover
from burning newly issued funds. Immutable grant events and reservations are
financial provenance, not TTL-cleanable intermediate state.

`FREE_GRANT_BUDGET_MICROUSD_PER_DAY` caps all new Free grants globally per UTC day.
The default is `100000000` ($100); staging should use `10000000` ($10). The trigger
checks the total and issues credit in one transaction, so concurrent new accounts
cannot exceed the subsidy. At capacity, a new grant fails with
`trial_capacity_exhausted`; replaying an existing grant still succeeds. Paid grants
are not drawn from this Free trial subsidy. This bounds subsidy exposure but is
not a replacement for request rate limiting or Identity's account-abuse controls.

Integration calls `sync` on login/account retrieval and before run admission.
Admission additionally rejects expired funding even when a caller omits sync.
Apply all migrations, including `0009_authoritative_paid_allowance.sql`, before
deploying these call sites. Migration 0009 is additive: existing financial rows
are not rewritten, and normal authenticated synchronization journals the repair.

## October 2026 staging-cycle reproduction

The observed `48744159` microUSD balance matches this synthetic, identity-free
fixture (all times UTC): Free calendar window October 1--November 1, activation
October 23 at 17:33:18, paid validity ending November 22 at 17:33:18. The timestamp
is inferred from the reported amount, not asserted as an independently inspected
live Billing timestamp. The old calculation was
`1000000 + floor(179000000 * 714402 / 2678400) = 48744159`.

| Event | Previous implementation | Corrected allocation, before usage |
| --- | ---: | ---: |
| Free signup | $1 | $1 |
| Pro activation | $48.744159 until November 1 | $180 through verified paid end |
| November 1, 00:00 UTC | Burns old remainder, grants a new full $180 | No additional grant |
| Verified paid end, November 22, 17:33:18 UTC | Returns to Free | Paid remainder expires; November Free allocation up to $1 |

The old first paid term could issue $227.744159 of additional paid funding, plus
the original $1, if the early portion was spent before November 1. This was an
over-allocation bug, not just a harmless partial balance display. Repairing an
unused legacy balance journals a delta of `131255841` microUSD, resulting in
exactly `180000000` total credited allocation; prior real spend/holds are retained.
Repeated synchronization and the November 1 boundary issue zero extra credit.
The next paid $180 slice is available only when Billing has extended validity
past the verified November 22 boundary.

Verification: `python -m unittest discover -s tests -p 'test_*sql.py' -v` exercises
login, upgrade, downgrade, replay, rollover, old unknown-outcome settlement, legacy
release, expiry, immutable provenance, and concurrent global subsidy admission.
Worker's `paid_renewal_uses_fixed_slices_not_extended_subscription_end` test covers
the upstream renewal shape. These checks use production SQL rather than a second
wallet implementation.
