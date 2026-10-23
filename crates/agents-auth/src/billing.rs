//! Entitlement policy and exact, idempotent usage accounting. No payment-provider logic.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use thiserror::Error;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Subscription {
    pub id: String,
    pub product_id: String,
    pub plan_id: String,
    pub status: String,
    pub current_period_start: i64,
    pub current_period_end: i64,
    pub entitlements: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    pub agent_configs: u32,
    pub concurrent_sessions: u32,
    pub concurrent_runs: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BillingTier {
    #[default]
    Free,
    Go,
    Plus,
    Pro,
}

impl BillingTier {
    pub const fn monthly_allowance(self) -> MicroUsd {
        MicroUsd(match self {
            Self::Free => 1_000_000,
            Self::Go => 8_000_000,
            Self::Plus => 18_000_000,
            Self::Pro => 180_000_000,
        })
    }
}

/// These are local deployment policy, not claims about the upstream plan catalog.
#[derive(Clone, Debug)]
pub struct PlanPolicy {
    pub product_id: String,
    /// Compatibility field: this is the highest paid tier entitlement, not generic access.
    pub paid_entitlement: String,
    pub go_entitlement: String,
    pub plus_entitlement: String,
    pub free: Limits,
    pub go: Limits,
    pub plus: Limits,
    /// Compatibility field naming for Pro limits.
    pub paid: Limits,
}

impl Default for PlanPolicy {
    fn default() -> Self {
        Self {
            product_id: "agents".into(),
            paid_entitlement: "agents.tier.pro".into(),
            go_entitlement: "agents.tier.go".into(),
            plus_entitlement: "agents.tier.plus".into(),
            free: Limits {
                agent_configs: 2,
                concurrent_sessions: 2,
                concurrent_runs: 1,
            },
            go: Limits {
                agent_configs: 10,
                concurrent_sessions: 5,
                concurrent_runs: 2,
            },
            plus: Limits {
                agent_configs: 30,
                concurrent_sessions: 15,
                concurrent_runs: 5,
            },
            paid: Limits {
                agent_configs: 200,
                concurrent_sessions: 100,
                concurrent_runs: 20,
            },
        }
    }
}

pub fn limits(subscriptions: &[Subscription], now: i64, policy: &PlanPolicy) -> Limits {
    match resolve_tier(subscriptions, now, policy) {
        BillingTier::Free => policy.free,
        BillingTier::Go => policy.go,
        BillingTier::Plus => policy.plus,
        BillingTier::Pro => policy.paid,
    }
}

fn subscription_tier(subscription: &Subscription, policy: &PlanPolicy) -> BillingTier {
    for (tier, entitlement) in [
        (BillingTier::Pro, &policy.paid_entitlement),
        (BillingTier::Plus, &policy.plus_entitlement),
        (BillingTier::Go, &policy.go_entitlement),
    ] {
        if subscription.entitlements.contains(entitlement) {
            return tier;
        }
    }
    BillingTier::Free
}

/// Overlapping grants select the strongest entitlement, never add limits or allowances.
pub fn select_subscription<'a>(
    subscriptions: &'a [Subscription],
    now: i64,
    policy: &PlanPolicy,
) -> Option<&'a Subscription> {
    subscriptions
        .iter()
        .filter(|s| {
            s.product_id == policy.product_id
                && s.status == "active"
                && s.current_period_start <= now
                && now < s.current_period_end
                && subscription_tier(s, policy) != BillingTier::Free
        })
        .max_by_key(|s| {
            (
                subscription_tier(s, policy),
                s.current_period_start,
                s.current_period_end,
                &s.id,
            )
        })
}

pub fn resolve_tier(subscriptions: &[Subscription], now: i64, policy: &PlanPolicy) -> BillingTier {
    select_subscription(subscriptions, now, policy)
        .map_or(BillingTier::Free, |s| subscription_tier(s, policy))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllowancePeriod {
    pub start: i64,
    pub end: i64,
}

impl AllowancePeriod {
    pub fn new(start: i64, end: i64) -> Result<Self, BillingError> {
        if start < 0 || end <= start {
            return Err(BillingError::InvalidPeriod);
        }
        Ok(Self { start, end })
    }

    pub fn contains(self, now: i64) -> bool {
        self.start <= now && now < self.end
    }
}

/// Free funding is keyed by this server UTC month, never by login or browser time.
pub fn free_period(now: i64) -> Result<AllowancePeriod, BillingError> {
    let date = time::OffsetDateTime::from_unix_timestamp(now)
        .map_err(|_| BillingError::InvalidPeriod)?
        .date();
    let start = date
        .replace_day(1)
        .map_err(|_| BillingError::InvalidPeriod)?;
    let year = date.year() + i32::from(date.month() == time::Month::December);
    let end = time::Date::from_calendar_date(year, date.month().next(), 1)
        .map_err(|_| BillingError::InvalidPeriod)?;
    AllowancePeriod::new(
        start.midnight().assume_utc().unix_timestamp(),
        end.midnight().assume_utc().unix_timestamp(),
    )
}

/// Credit-only top-up, rounded down to microUSD. Downgrades never claw back current grants.
pub fn prorated_upgrade(
    from: BillingTier,
    to: BillingTier,
    period: AllowancePeriod,
    now: i64,
) -> Result<MicroUsd, BillingError> {
    let period = AllowancePeriod::new(period.start, period.end)?;
    if !period.contains(now) {
        return Err(BillingError::InvalidPeriod);
    }
    if to <= from {
        return Ok(MicroUsd::ZERO);
    }
    let difference = to.monthly_allowance().0 - from.monthly_allowance().0;
    let credit = i128::from(difference) * i128::from(period.end - now)
        / i128::from(period.end - period.start);
    MicroUsd::new(i64::try_from(credit).map_err(|_| BillingError::Overflow)?)
}

/// SQL INTEGER-compatible millionths of USD; never use binary floats for prices.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct MicroUsd(i64);

impl MicroUsd {
    pub const ZERO: Self = Self(0);

    pub fn new(value: i64) -> Result<Self, BillingError> {
        (value >= 0)
            .then_some(Self(value))
            .ok_or(BillingError::InvalidAmount)
    }

    pub fn get(self) -> i64 {
        self.0
    }

    /// Round once per metered event, upward, using a versioned rational tariff.
    pub fn price(quantity: u64, numerator: u64, denominator: u64) -> Result<Self, BillingError> {
        if denominator == 0 {
            return Err(BillingError::InvalidAmount);
        }
        let product = u128::from(quantity) * u128::from(numerator);
        let rounded = product.div_ceil(u128::from(denominator));
        Self::new(i64::try_from(rounded).map_err(|_| BillingError::Overflow)?)
    }

    fn add(self, other: Self) -> Result<Self, BillingError> {
        self.0
            .checked_add(other.0)
            .map(Self)
            .ok_or(BillingError::Overflow)
    }
}

impl<'de> Deserialize<'de> for MicroUsd {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = i64::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resource {
    Model,
    Container,
    R2Storage,
    R2Operation,
    D1,
    Worker,
    DurableObject,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Charge {
    pub resource: Resource,
    pub quantity: u64,
    pub unit: String,
    pub amount_microusd: MicroUsd,
    pub price_version: String,
    pub provider_request_id: Option<String>,
}

/// A reviewed deployment tariff; the unit identifies the observed metric, e.g. cpu_ms.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceTariff {
    pub resource: Resource,
    pub unit: String,
    pub microusd_numerator: u64,
    pub quantity_denominator: u64,
    pub price_version: String,
}

impl ResourceTariff {
    pub fn charge(&self, quantity: u64) -> Result<Charge, BillingError> {
        if self.unit.is_empty() || self.price_version.is_empty() {
            return Err(BillingError::InvalidTariff);
        }
        Ok(Charge {
            resource: self.resource.clone(),
            quantity,
            unit: self.unit.clone(),
            amount_microusd: MicroUsd::price(
                quantity,
                self.microusd_numerator,
                self.quantity_denominator,
            )?,
            price_version: self.price_version.clone(),
            provider_request_id: None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReservationState {
    Reserved,
    Settled,
    Released,
}

#[derive(Clone, Debug, Serialize)]
pub struct Reservation {
    pub request_hash: String,
    pub reserved_microusd: MicroUsd,
    pub state: ReservationState,
    pub charges: BTreeMap<String, Charge>,
}

impl Reservation {
    fn total(&self) -> Result<MicroUsd, BillingError> {
        self.charges
            .values()
            .try_fold(MicroUsd::ZERO, |sum, c| sum.add(c.amount_microusd))
    }
}

/// Executable reference state machine for the D1 trigger-enforced ledger.
#[derive(Clone, Debug, Serialize)]
pub struct UsageAccount {
    pub budget_microusd: MicroUsd,
    pub spent_microusd: MicroUsd,
    pub reserved_microusd: MicroUsd,
    pub reservations: BTreeMap<String, Reservation>,
}

impl UsageAccount {
    pub fn new(budget: MicroUsd) -> Self {
        Self {
            budget_microusd: budget,
            spent_microusd: MicroUsd::ZERO,
            reserved_microusd: MicroUsd::ZERO,
            reservations: BTreeMap::new(),
        }
    }

    pub fn reserve(&mut self, key: &str, hash: &str, amount: MicroUsd) -> Result<(), BillingError> {
        if key.is_empty() || hash.is_empty() {
            return Err(BillingError::InvalidKey);
        }
        if let Some(existing) = self.reservations.get(key) {
            return if existing.request_hash == hash && existing.reserved_microusd == amount {
                Ok(())
            } else {
                Err(BillingError::IdempotencyConflict)
            };
        }
        let held = self.reserved_microusd.add(amount)?;
        if self.spent_microusd.add(held)? > self.budget_microusd {
            return Err(BillingError::BudgetExceeded);
        }
        self.reserved_microusd = held;
        self.reservations.insert(
            key.into(),
            Reservation {
                request_hash: hash.into(),
                reserved_microusd: amount,
                state: ReservationState::Reserved,
                charges: BTreeMap::new(),
            },
        );
        Ok(())
    }

    pub fn charge(&mut self, key: &str, event: &str, charge: Charge) -> Result<(), BillingError> {
        if event.is_empty() {
            return Err(BillingError::InvalidKey);
        }
        let reservation = self
            .reservations
            .get_mut(key)
            .ok_or(BillingError::NotFound)?;
        if let Some(existing) = reservation.charges.get(event) {
            return if existing == &charge {
                Ok(())
            } else {
                Err(BillingError::IdempotencyConflict)
            };
        }
        if reservation.state != ReservationState::Reserved {
            return Err(BillingError::Finalized);
        }
        if reservation.total()?.add(charge.amount_microusd)? > reservation.reserved_microusd {
            return Err(BillingError::BudgetExceeded);
        }
        reservation.charges.insert(event.into(), charge);
        Ok(())
    }

    pub fn settle(&mut self, key: &str) -> Result<MicroUsd, BillingError> {
        let reservation = self
            .reservations
            .get_mut(key)
            .ok_or(BillingError::NotFound)?;
        let amount = reservation.total()?;
        match reservation.state {
            ReservationState::Settled => return Ok(amount),
            ReservationState::Released => return Err(BillingError::Finalized),
            ReservationState::Reserved => {}
        }
        let spent = self.spent_microusd.add(amount)?;
        self.reserved_microusd.0 -= reservation.reserved_microusd.0;
        self.spent_microusd = spent;
        reservation.state = ReservationState::Settled;
        Ok(amount)
    }

    pub fn release(&mut self, key: &str) -> Result<(), BillingError> {
        let reservation = self
            .reservations
            .get_mut(key)
            .ok_or(BillingError::NotFound)?;
        if reservation.state == ReservationState::Released {
            return Ok(());
        }
        if reservation.state != ReservationState::Reserved || !reservation.charges.is_empty() {
            return Err(BillingError::Finalized);
        }
        self.reserved_microusd.0 -= reservation.reserved_microusd.0;
        reservation.state = ReservationState::Released;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum BillingError {
    #[error("invalid_amount")]
    InvalidAmount,
    #[error("amount_overflow")]
    Overflow,
    #[error("invalid_idempotency_key")]
    InvalidKey,
    #[error("idempotency_conflict")]
    IdempotencyConflict,
    #[error("budget_exceeded")]
    BudgetExceeded,
    #[error("reservation_not_found")]
    NotFound,
    #[error("reservation_finalized")]
    Finalized,
    #[error("invalid_tariff")]
    InvalidTariff,
    #[error("invalid_allowance_period")]
    InvalidPeriod,
}

#[cfg(test)]
mod tests {
    use super::*;
    fn money(n: i64) -> MicroUsd {
        MicroUsd::new(n).unwrap()
    }
    fn charge(n: i64) -> Charge {
        Charge {
            resource: Resource::Model,
            quantity: 1,
            unit: "request".into(),
            amount_microusd: money(n),
            price_version: "test-v1".into(),
            provider_request_id: None,
        }
    }
    #[test]
    fn grants_are_product_scoped_time_bounded_and_not_plan_names() {
        let policy = PlanPolicy::default();
        let mut grant = Subscription {
            id: "s".into(),
            product_id: "agents".into(),
            plan_id: "Free".into(),
            status: "active".into(),
            current_period_start: 10,
            current_period_end: 20,
            entitlements: vec!["agents.access".into(), "agents.tier.pro".into()],
        };
        assert_eq!(limits(&[grant.clone()], 10, &policy), policy.paid);
        assert_eq!(limits(&[grant.clone()], 20, &policy), policy.free);
        grant.product_id = "unrelated".into();
        assert_eq!(limits(&[grant], 15, &policy), policy.free);
    }
    #[test]
    fn sdk_only_paid_grant_survives_refresh_ttl_but_not_subscription_expiry() {
        let fetched_at = 1_000;
        let refresh_after = fetched_at + 300;
        let end = fetched_at + 30 * 86_400;
        let grant = Subscription {
            id: "paid-grant".into(),
            product_id: "agents".into(),
            plan_id: "agents-pro".into(),
            status: "active".into(),
            current_period_start: fetched_at,
            current_period_end: end,
            entitlements: vec!["agents.access".into(), "agents.tier.pro".into()],
        };
        let policy = PlanPolicy::default();
        for now in [refresh_after + 1, fetched_at + 86_400, end - 1] {
            assert_eq!(
                resolve_tier(std::slice::from_ref(&grant), now, &policy),
                BillingTier::Pro
            );
            assert_eq!(
                limits(std::slice::from_ref(&grant), now, &policy),
                policy.paid
            );
        }
        assert_eq!(
            resolve_tier(std::slice::from_ref(&grant), end, &policy),
            BillingTier::Free
        );
        assert_eq!(limits(&[grant], end, &policy), policy.free);
    }
    fn subscription(id: &str, tier: &str) -> Subscription {
        Subscription {
            id: id.into(),
            product_id: "agents".into(),
            plan_id: "misleading-name".into(),
            status: "active".into(),
            current_period_start: 10,
            current_period_end: 100,
            entitlements: vec!["agents.access".into(), format!("agents.tier.{tier}")],
        }
    }
    #[test]
    fn tier_limits_and_allowances_match_reviewed_baseline() {
        let policy = PlanPolicy::default();
        assert_eq!(
            policy.free,
            Limits {
                agent_configs: 2,
                concurrent_sessions: 2,
                concurrent_runs: 1
            }
        );
        assert_eq!(
            policy.go,
            Limits {
                agent_configs: 10,
                concurrent_sessions: 5,
                concurrent_runs: 2
            }
        );
        assert_eq!(
            policy.plus,
            Limits {
                agent_configs: 30,
                concurrent_sessions: 15,
                concurrent_runs: 5
            }
        );
        assert_eq!(
            policy.paid,
            Limits {
                agent_configs: 200,
                concurrent_sessions: 100,
                concurrent_runs: 20
            }
        );
        for (tier, amount) in [
            (BillingTier::Free, 1),
            (BillingTier::Go, 8),
            (BillingTier::Plus, 18),
            (BillingTier::Pro, 180),
        ] {
            assert_eq!(tier.monthly_allowance(), money(amount * 1_000_000));
        }
    }
    #[test]
    fn strongest_active_tier_wins_independent_of_order_and_duplicate_grants() {
        let policy = PlanPolicy::default();
        let go = subscription("go", "go");
        let plus = subscription("plus", "plus");
        let mut pro = subscription("pro", "pro");
        pro.current_period_start = 50;
        let grants = vec![go.clone(), plus.clone(), pro.clone(), plus.clone()];
        assert_eq!(resolve_tier(&grants, 20, &policy), BillingTier::Plus);
        assert_eq!(limits(&grants, 20, &policy), policy.plus);
        assert_eq!(select_subscription(&grants, 50, &policy).unwrap().id, "pro");
        let reversed: Vec<_> = grants.into_iter().rev().collect();
        assert_eq!(resolve_tier(&reversed, 50, &policy), BillingTier::Pro);
        pro.status = "expired".into();
        assert_eq!(resolve_tier(&[go, pro], 60, &policy), BillingTier::Go);
        assert_eq!(resolve_tier(&[plus], 100, &policy), BillingTier::Free);
    }
    #[test]
    fn generic_access_and_unrecognized_tier_do_not_grant_paid_quota() {
        let policy = PlanPolicy::default();
        let mut grant = subscription("s", "unknown");
        grant.plan_id = "agents-pro".into();
        assert_eq!(
            resolve_tier(&[grant.clone()], 20, &policy),
            BillingTier::Free
        );
        grant.entitlements = vec!["agents.tier.pro".into()];
        grant.product_id = "other".into();
        assert_eq!(resolve_tier(&[grant], 20, &policy), BillingTier::Free);
    }
    fn timestamp(year: i32, month: time::Month, day: u8) -> i64 {
        time::Date::from_calendar_date(year, month, day)
            .unwrap()
            .midnight()
            .assume_utc()
            .unix_timestamp()
    }
    #[test]
    fn free_utc_month_handles_leap_days_december_and_repeat_logins() {
        let start = timestamp(2024, time::Month::February, 1);
        let end = timestamp(2024, time::Month::March, 1);
        assert_eq!(free_period(start).unwrap(), AllowancePeriod { start, end });
        assert_eq!(
            free_period(end - 1).unwrap(),
            AllowancePeriod { start, end }
        );
        assert_eq!(end - start, 29 * 86400);
        assert_eq!(
            free_period(timestamp(2026, time::Month::December, 23))
                .unwrap()
                .end,
            timestamp(2027, time::Month::January, 1)
        );
        assert!(free_period(i64::MAX).is_err());
    }
    #[test]
    fn upgrade_credit_is_prorated_not_stacked_and_downgrade_never_claws_back() {
        let period = AllowancePeriod::new(10, 110).unwrap();
        assert_eq!(
            prorated_upgrade(BillingTier::Go, BillingTier::Plus, period, 10),
            Ok(money(10_000_000))
        );
        assert_eq!(
            prorated_upgrade(BillingTier::Go, BillingTier::Plus, period, 60),
            Ok(money(5_000_000))
        );
        assert_eq!(
            prorated_upgrade(BillingTier::Plus, BillingTier::Go, period, 60),
            Ok(money(0))
        );
        assert_eq!(
            prorated_upgrade(BillingTier::Go, BillingTier::Go, period, 60),
            Ok(money(0))
        );
        assert!(prorated_upgrade(BillingTier::Go, BillingTier::Pro, period, 110).is_err());
        for now in 10..110 {
            let credit =
                prorated_upgrade(BillingTier::Free, BillingTier::Pro, period, now).unwrap();
            assert!(credit.get() <= 179_000_000);
            if now > 10 {
                assert!(
                    credit
                        <= prorated_upgrade(BillingTier::Free, BillingTier::Pro, period, now - 1)
                            .unwrap()
                );
            }
        }
    }
    #[test]
    fn retries_never_double_charge_and_conflicts_never_mutate() {
        let mut account = UsageAccount::new(money(100));
        account.reserve("a", "hash", money(80)).unwrap();
        account.reserve("a", "hash", money(80)).unwrap();
        assert_eq!(
            account.reserve("a", "other", money(80)),
            Err(BillingError::IdempotencyConflict)
        );
        assert_eq!(
            account.reserve("b", "hash", money(21)),
            Err(BillingError::BudgetExceeded)
        );
        account.charge("a", "event", charge(30)).unwrap();
        account.charge("a", "event", charge(30)).unwrap();
        assert_eq!(
            account.charge("a", "event", charge(31)),
            Err(BillingError::IdempotencyConflict)
        );
        assert_eq!(account.settle("a"), Ok(money(30)));
        assert_eq!(account.settle("a"), Ok(money(30)));
        assert_eq!(account.spent_microusd, money(30));
        assert_eq!(account.reserved_microusd, money(0));
        assert_eq!(account.release("a"), Err(BillingError::Finalized));
    }
    #[test]
    fn release_cannot_discard_recorded_usage() {
        let mut account = UsageAccount::new(money(100));
        account.reserve("a", "h", money(80)).unwrap();
        account.charge("a", "e", charge(20)).unwrap();
        assert_eq!(account.release("a"), Err(BillingError::Finalized));
        account.settle("a").unwrap();
        account.reserve("b", "h", money(80)).unwrap();
        account.release("b").unwrap();
        account.release("b").unwrap();
        assert_eq!(account.reserved_microusd, money(0));
    }
    #[test]
    fn exact_prices_reject_negative_and_overflow() {
        assert!(serde_json::from_str::<MicroUsd>("-1").is_err());
        assert_eq!(MicroUsd::price(1, 1, 3), Ok(money(1)));
        assert_eq!(MicroUsd::price(3, 1, 3), Ok(money(1)));
        assert_eq!(
            MicroUsd::price(u64::MAX, u64::MAX, 1),
            Err(BillingError::Overflow)
        );
        assert_eq!(MicroUsd::price(1, 1, 0), Err(BillingError::InvalidAmount));
    }
    #[test]
    fn budget_conservation_for_many_interleaved_reservations() {
        for budget in 0..100 {
            let mut account = UsageAccount::new(money(budget));
            for i in 0..100 {
                let key = i.to_string();
                if account.reserve(&key, &key, money(i % 11)).is_ok() {
                    if i % 2 == 0 {
                        account.release(&key).unwrap();
                    } else {
                        account.charge(&key, "usage", charge(i % 11)).unwrap();
                        account.settle(&key).unwrap();
                    }
                }
                assert!(account.spent_microusd.0 + account.reserved_microusd.0 <= budget);
            }
        }
    }
}
