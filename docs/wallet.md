# Allowance wallet

`allowances::sync` accepts only an authenticated account ID, reads server-owned,
unexpired verified Billing snapshots, and resolves the reviewed `PlanPolicy`.
Client request bodies cannot select a funding amount. Free receives $1 per UTC
calendar month; Go / Plus / Pro receive $8 / $18 / $180 per paid window.

Paid windows are deterministic 30-day slices anchored at Billing's authoritative
`current_period_start`, bounded by `current_period_end`. Upstream early renewal
extends the latter without changing the former, so hashing both bounds as a grant
key would mint duplicate funds. Slices prevent this. A short final validity window
receives at most one allowance; it never funds work after the verified end.

One active account window is pinned across overlapping Free and paid subscriptions.
An upgrade credits only the positive difference from the highest previously seen
nominal tier, prorated by remaining window seconds and rounded down. Downgrades
do not issue credits or revoke already allocated credit; the next window uses
the then-current verified tier. Switching subscriptions or replaying an older
tier cannot recreate an allowance. After a transition, the next paid slice may
be clipped at the previous account window end to prevent overlapping funds.

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
Apply migration `0004_allowance_wallet.sql` before deploying these call sites.

Verification: `python -m unittest discover -s tests -p 'test_*sql.py' -v` exercises
login, upgrade, downgrade, replay, rollover, old unknown-outcome settlement, legacy
release, expiry, immutable provenance, and concurrent global subsidy admission.
Worker's `paid_renewal_uses_fixed_slices_not_extended_subscription_end` test covers
the upstream renewal shape. These checks use production SQL rather than a second
wallet implementation.
