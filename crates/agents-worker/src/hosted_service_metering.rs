//! Independent upstream API holds. Resource observations stay on the run meter.
//! A known receipt must be durable before settlement; an uncertain dispatch is
//! never evidence that a reservation may be released or the service retried.

use agents_auth::billing::{Charge, MicroUsd, ReservationState, Resource};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use wasm_bindgen::JsValue;
use worker::{Env, Error, Result};

use crate::{metering, search_deepseek::SearchRequestReceipt};

pub(crate) const DEEPSEEK_PAID_FROM_MS: u64 = 1_793_462_400_000;
pub(crate) const SEEDREAM5_IMAGE_MICROUSD: i64 = 18_000;
const MAX_ADMISSION_WINDOW_MS: u64 = 300_000;

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "service", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ServiceBudget {
    Seedream5 {
        platform_funded: bool,
        ceiling_microusd: i64,
        price_revision: String,
    },
    DeepseekRequest {
        platform_funded: bool,
        not_after_ms: u64,
        price_revision: String,
    },
    /// A separately reviewed route must declare its actual non-token unit and
    /// a sufficient ceiling. Missing cost in its receipt cannot become zero.
    ReportedCost {
        platform_funded: bool,
        ceiling_microusd: i64,
        unit: String,
        price_revision: String,
    },
}

impl ServiceBudget {
    fn platform_funded(&self) -> bool {
        match self {
            Self::Seedream5 {
                platform_funded, ..
            }
            | Self::DeepseekRequest {
                platform_funded, ..
            }
            | Self::ReportedCost {
                platform_funded, ..
            } => *platform_funded,
        }
    }

    fn service(&self) -> &'static str {
        match self {
            Self::Seedream5 { .. } => "seedream5",
            Self::DeepseekRequest { .. } => "deepseek_request",
            Self::ReportedCost { .. } => "reported_cost",
        }
    }

    fn price_revision(&self) -> &str {
        match self {
            Self::Seedream5 { price_revision, .. }
            | Self::DeepseekRequest { price_revision, .. }
            | Self::ReportedCost { price_revision, .. } => price_revision,
        }
    }

    fn ceiling(&self) -> i64 {
        if !self.platform_funded() {
            return 0;
        }
        match self {
            Self::Seedream5 {
                ceiling_microusd, ..
            }
            | Self::ReportedCost {
                ceiling_microusd, ..
            } => *ceiling_microusd,
            // Reserve the one-request maximum even in the advertised free
            // period. A disconnected call can outlive a local deadline, and
            // the provider receipt has no authoritative billing timestamp.
            Self::DeepseekRequest { .. } => 750,
        }
    }

    fn validate(&self, admitted_at_ms: u64) -> Result<()> {
        if !text(self.price_revision(), 256) {
            return Err(fail("invalid_tool_service_price"));
        }
        match self {
            Self::Seedream5 {
                platform_funded,
                ceiling_microusd,
                ..
            } => {
                if *ceiling_microusd < 0
                    || *platform_funded && *ceiling_microusd < SEEDREAM5_IMAGE_MICROUSD
                {
                    return Err(fail("insufficient_image_service_ceiling"));
                }
            }
            Self::DeepseekRequest {
                not_after_ms,
                price_revision,
                ..
            } => {
                if *price_revision != crate::search_deepseek::PRICE_REVISION
                    || *not_after_ms < admitted_at_ms
                    || not_after_ms.saturating_sub(admitted_at_ms) > MAX_ADMISSION_WINDOW_MS
                {
                    return Err(fail("invalid_search_service_admission"));
                }
            }
            Self::ReportedCost {
                ceiling_microusd,
                unit,
                ..
            } => {
                if *ceiling_microusd < 0
                    || !matches!(unit.as_str(), "generated_image" | "provider_request")
                {
                    return Err(fail("invalid_tool_service_price"));
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ServiceHold {
    id: String,
    owner: String,
    session: Option<String>,
    operation: String,
    request_digest: String,
    admitted_at_ms: u64,
    budget: ServiceBudget,
}

impl ServiceHold {
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    /// Persist the start timestamp before dispatch. Expired admission requires a
    /// new operation admission, not a paid request under an obsolete free hold.
    pub(crate) fn validate_dispatch_time(&self, started_at_ms: u64) -> Result<()> {
        if started_at_ms < self.admitted_at_ms
            || matches!(&self.budget, ServiceBudget::DeepseekRequest { not_after_ms, .. } if started_at_ms > *not_after_ms)
        {
            return Err(fail("tool_service_admission_expired"));
        }
        Ok(())
    }

    fn key(&self) -> String {
        format!("tool:{}:{}", self.budget.service(), self.operation)
    }

    fn hash(&self) -> Result<String> {
        digest(&(
            &self.owner,
            &self.session,
            &self.operation,
            &self.request_digest,
            self.admitted_at_ms,
            &self.budget,
        ))
    }

    fn validate_identity(&self, owner: &str, session: Option<&str>) -> Result<()> {
        self.budget.validate(self.admitted_at_ms)?;
        if self.owner != owner
            || self.session.as_deref() != session
            || self.id != hold_id(owner, &self.operation, self.budget.service())?
            || !text(&self.operation, 512)
            || !sha256_hex(&self.request_digest)
        {
            return Err(fail("tool_service_hold_identity_conflict"));
        }
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "service", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum KnownUsage {
    /// A provider-specific adapter has durable, authoritative no-charge evidence.
    /// This must not be constructed from a timeout or an unparsed error body.
    ConfirmedNoCharge { provider_request_id: Option<String> },
    GeneratedImage {
        reported_cost_usd: Option<Value>,
        provider_request_id: Option<String>,
    },
    DeepseekRequest {
        receipt: SearchRequestReceipt,
        started_at_ms: u64,
        completed_at_ms: u64,
        provider_request_id: Option<String>,
    },
    ReportedCost {
        units: u64,
        reported_cost_usd: Option<Value>,
        provider_request_id: Option<String>,
    },
}

/// Restore identity without authorizing dispatch or creating a reservation.
/// Expired and finalized holds remain recoverable for known-result settlement
/// replay or release proven never to have dispatched. The caller's durable
/// operation fence, not the existence of a financial hold, governs retries.
pub(crate) async fn restore_existing_hold(
    env: &Env,
    owner: &str,
    session: Option<&str>,
    operation: &str,
    request_digest: &str,
    admitted_at_ms: u64,
    budget: ServiceBudget,
) -> Result<Option<ServiceHold>> {
    let hold = make_hold(
        owner,
        session,
        operation,
        request_digest,
        admitted_at_ms,
        budget,
    )?;
    Ok(existing_hold_state(env, owner, session, &hold)
        .await?
        .map(|_| hold))
}

/// Persist the original intent before reserving. Existing reserved holds are
/// restored even after admission expires; validate_dispatch_time is still
/// mandatory immediately before any new HTTP dispatch. Finalized holds may be
/// restored explicitly for known-result replay but cannot admit a new request.
pub(crate) async fn prepare_hold(
    env: &Env,
    owner: &str,
    session: Option<&str>,
    operation: &str,
    request_digest: &str,
    admitted_at_ms: u64,
    budget: ServiceBudget,
) -> Result<ServiceHold> {
    let hold = make_hold(
        owner,
        session,
        operation,
        request_digest,
        admitted_at_ms,
        budget,
    )?;
    if let Some(state) = existing_hold_state(env, owner, session, &hold).await? {
        return prepared_hold(hold, state);
    }
    hold.validate_dispatch_time(crate::util::now_ms())?;
    let reserved = metering::reserve(
        &env.d1("DB")?,
        owner,
        session,
        hold.id(),
        &hold.key(),
        &hold.hash()?,
        hold.budget.ceiling(),
        now_seconds()?,
    )
    .await?;
    let state = verify_hold(env, owner, session, &hold).await?;
    if reserved.id != hold.id {
        return Err(fail("tool_service_hold_identity_conflict"));
    }
    prepared_hold(hold, state)
}

fn make_hold(
    owner: &str,
    session: Option<&str>,
    operation: &str,
    request_digest: &str,
    admitted_at_ms: u64,
    budget: ServiceBudget,
) -> Result<ServiceHold> {
    let hold = ServiceHold {
        id: hold_id(owner, operation, budget.service())?,
        owner: owner.into(),
        session: session.map(str::to_owned),
        operation: operation.into(),
        request_digest: request_digest.into(),
        admitted_at_ms,
        budget,
    };
    hold.validate_identity(owner, session)?;
    Ok(hold)
}

fn prepared_hold(hold: ServiceHold, state: ReservationState) -> Result<ServiceHold> {
    if state != ReservationState::Reserved {
        return Err(fail("tool_service_hold_finalized"));
    }
    Ok(hold)
}

/// The caller must first durably store the exact KnownUsage. A ledger timeout
/// then replays this same receipt; it never repeats provider generation/search.
pub(crate) async fn settle_hold(
    env: &Env,
    owner: &str,
    session: Option<&str>,
    hold: &ServiceHold,
    usage: &KnownUsage,
) -> Result<()> {
    verify_hold(env, owner, session, hold).await?;
    let charge = charge(hold, usage)?;
    metering::append(
        &env.d1("DB")?,
        hold.id(),
        "upstream",
        &charge,
        now_seconds()?,
    )
    .await?;
    metering::settle(&env.d1("DB")?, hold.id(), now_seconds()?).await?;
    Ok(())
}

/// This is deliberately not an `on_error` helper: only the harness's durable
/// proof that no provider dispatch happened permits releasing the hold.
pub(crate) async fn release_known_not_dispatched(
    env: &Env,
    owner: &str,
    session: Option<&str>,
    hold: &ServiceHold,
) -> Result<()> {
    verify_hold(env, owner, session, hold).await?;
    metering::release(&env.d1("DB")?, hold.id(), now_seconds()?).await?;
    Ok(())
}

async fn verify_hold(
    env: &Env,
    owner: &str,
    session: Option<&str>,
    hold: &ServiceHold,
) -> Result<ReservationState> {
    existing_hold_state(env, owner, session, hold)
        .await?
        .ok_or_else(|| fail("tool_service_hold_not_found"))
}

async fn existing_hold_state(
    env: &Env,
    owner: &str,
    session: Option<&str>,
    hold: &ServiceHold,
) -> Result<Option<ReservationState>> {
    hold.validate_identity(owner, session)?;
    #[derive(Deserialize)]
    struct Held {
        request_hash: String,
        idempotency_key: String,
        reserved_microusd: String,
        state: ReservationState,
    }
    let row = env.d1("DB")?.prepare("SELECT request_hash,idempotency_key,CAST(reserved_microusd AS TEXT) AS reserved_microusd,state FROM usage_reservations WHERE id=? AND account_id=? AND session_id IS ?")
        .bind(&[JsValue::from_str(hold.id()),JsValue::from_str(owner),session.map(JsValue::from_str).unwrap_or(JsValue::NULL)])?
        .first::<Held>(None).await?;
    let Some(row) = row else {
        return Ok(None);
    };
    if row.request_hash != hold.hash()?
        || row.idempotency_key != hold.key()
        || row.reserved_microusd != hold.budget.ceiling().to_string()
    {
        return Err(fail("tool_service_hold_identity_conflict"));
    }
    Ok(Some(row.state))
}

fn charge(hold: &ServiceHold, usage: &KnownUsage) -> Result<Charge> {
    let (quantity, unit, actual, request_id) = match (&hold.budget, usage) {
        (
            budget,
            KnownUsage::ConfirmedNoCharge {
                provider_request_id,
            },
        ) => {
            let unit = match budget {
                ServiceBudget::Seedream5 { .. } => "generated_image",
                ServiceBudget::DeepseekRequest { .. } => "successful_search_request",
                ServiceBudget::ReportedCost { unit, .. } => unit,
            };
            (0, unit, 0, provider_request_id)
        }
        (
            ServiceBudget::Seedream5 { .. },
            KnownUsage::GeneratedImage {
                reported_cost_usd,
                provider_request_id,
            },
        ) => (
            1,
            "generated_image",
            if hold.budget.platform_funded() {
                reported_cost_usd
                    .as_ref()
                    .map(cost)
                    .transpose()?
                    .unwrap_or(SEEDREAM5_IMAGE_MICROUSD)
            } else {
                0
            },
            provider_request_id,
        ),
        (
            ServiceBudget::DeepseekRequest { .. },
            KnownUsage::DeepseekRequest {
                receipt,
                started_at_ms,
                completed_at_ms,
                provider_request_id,
            },
        ) => {
            hold.validate_dispatch_time(*started_at_ms)?;
            if receipt.provider != "deepseek"
                || !matches!(receipt.operation.as_str(), "search" | "fetch")
                || receipt.price_revision != crate::search_deepseek::PRICE_REVISION
                || receipt.successful_nonempty_requests > 1
            {
                return Err(fail("invalid_search_service_receipt"));
            }
            let quantity = u64::from(receipt.successful_nonempty_requests);
            if completed_at_ms < started_at_ms {
                return Err(fail("invalid_search_service_time"));
            }
            if matches!(&hold.budget, ServiceBudget::DeepseekRequest{not_after_ms,..} if completed_at_ms > not_after_ms)
            {
                return Err(fail("tool_service_admission_expired"));
            }
            if hold.budget.platform_funded()
                && quantity != 0
                && *started_at_ms < DEEPSEEK_PAID_FROM_MS
                && *completed_at_ms >= DEEPSEEK_PAID_FROM_MS
            {
                return Err(fail("search_service_price_boundary_uncertain"));
            }
            let amount = if *started_at_ms >= DEEPSEEK_PAID_FROM_MS {
                quantity * 750
            } else {
                0
            };
            (
                quantity,
                "successful_search_request",
                amount as i64,
                provider_request_id,
            )
        }
        (
            ServiceBudget::ReportedCost { unit, .. },
            KnownUsage::ReportedCost {
                units,
                reported_cost_usd,
                provider_request_id,
            },
        ) => {
            if *units > i64::MAX as u64 {
                return Err(fail("invalid_tool_service_units"));
            }
            let actual = if hold.budget.platform_funded() {
                cost(
                    reported_cost_usd
                        .as_ref()
                        .ok_or_else(|| fail("tool_service_cost_unknown"))?,
                )?
            } else {
                // BYOK upstream costs remain in the private original receipt;
                // they never become an invented debit on the platform wallet.
                0
            };
            (*units, unit.as_str(), actual, provider_request_id)
        }
        _ => return Err(fail("tool_service_receipt_kind_conflict")),
    };
    if request_id.as_ref().is_some_and(|id| !text(id, 512)) {
        return Err(fail("invalid_tool_service_request_id"));
    }
    let amount = if hold.budget.platform_funded() {
        actual
    } else {
        0
    };
    if amount > hold.budget.ceiling() {
        return Err(fail("tool_service_cost_exceeds_hold"));
    }
    Ok(Charge {
        resource: Resource::Model,
        quantity,
        unit: unit.into(),
        amount_microusd: MicroUsd::new(amount).map_err(|_| fail("invalid_tool_service_cost"))?,
        price_version: hold.budget.price_revision().into(),
        provider_request_id: request_id.clone(),
    })
}

fn cost(value: &Value) -> Result<i64> {
    let decimal = match value {
        Value::String(value) => value.clone(),
        Value::Number(value) => value.to_string(),
        _ => return Err(fail("invalid_provider_cost")),
    };
    if decimal.len() > 1024 {
        return Err(fail("invalid_provider_cost"));
    }
    crate::store::usd_to_microusd(&decimal)
}

fn hold_id(owner: &str, operation: &str, service: &str) -> Result<String> {
    if !text(owner, 512) || !text(operation, 512) {
        return Err(fail("invalid_tool_service_operation"));
    }
    Ok(format!(
        "tool_{:x}",
        Sha256::digest(serde_json::to_vec(&(owner, operation, service))?)
    ))
}

fn digest<T: Serialize>(value: &T) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(value)?)))
}

fn sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn text(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}

fn now_seconds() -> Result<i64> {
    i64::try_from(crate::util::now_ms() / 1000).map_err(|_| fail("invalid_tool_service_time"))
}

fn fail(code: &str) -> Error {
    Error::RustError(code.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::json;

    fn hold(budget: ServiceBudget, admitted_at_ms: u64) -> ServiceHold {
        ServiceHold {
            id: hold_id("owner", "operation-a", budget.service()).unwrap(),
            owner: "owner".into(),
            session: Some("session-a".into()),
            operation: "operation-a".into(),
            request_digest: "a".repeat(64),
            admitted_at_ms,
            budget,
        }
    }

    fn search_hold(platform_funded: bool, admitted_at_ms: u64) -> ServiceHold {
        hold(
            ServiceBudget::DeepseekRequest {
                platform_funded,
                not_after_ms: admitted_at_ms + 1_000,
                price_revision: crate::search_deepseek::PRICE_REVISION.into(),
            },
            admitted_at_ms,
        )
    }

    fn search_usage(started_at_ms: u64, completed_at_ms: u64, units: u32) -> KnownUsage {
        KnownUsage::DeepseekRequest {
            receipt: SearchRequestReceipt {
                provider: "deepseek".into(),
                operation: "search".into(),
                successful_nonempty_requests: units,
                price_revision: crate::search_deepseek::PRICE_REVISION.into(),
            },
            started_at_ms,
            completed_at_ms,
            provider_request_id: Some("provider-request-a".into()),
        }
    }

    #[test]
    fn search_boundary_reserves_maximum_but_only_charges_known_actual_fee() {
        let before = search_hold(true, DEEPSEEK_PAID_FROM_MS - 1_000);
        assert_eq!(before.budget.ceiling(), 750);
        let free = charge(
            &before,
            &search_usage(DEEPSEEK_PAID_FROM_MS - 100, DEEPSEEK_PAID_FROM_MS - 1, 1),
        )
        .unwrap();
        assert_eq!(free.amount_microusd, MicroUsd::ZERO);
        assert_eq!(free.quantity, 1);
        assert_eq!(free.unit, "successful_search_request");
        assert!(
            charge(
                &before,
                &search_usage(DEEPSEEK_PAID_FROM_MS - 1, DEEPSEEK_PAID_FROM_MS, 1)
            )
            .is_err()
        );
        let after = search_hold(true, DEEPSEEK_PAID_FROM_MS);
        assert_eq!(
            charge(
                &after,
                &search_usage(DEEPSEEK_PAID_FROM_MS, DEEPSEEK_PAID_FROM_MS + 1, 1)
            )
            .unwrap()
            .amount_microusd
            .get(),
            750
        );
        assert_eq!(
            charge(
                &after,
                &search_usage(DEEPSEEK_PAID_FROM_MS, DEEPSEEK_PAID_FROM_MS + 1, 0)
            )
            .unwrap()
            .amount_microusd,
            MicroUsd::ZERO
        );
    }

    #[test]
    fn expired_window_does_not_authorize_dispatch_or_settlement() {
        let held = search_hold(true, DEEPSEEK_PAID_FROM_MS);
        assert!(
            held.validate_dispatch_time(DEEPSEEK_PAID_FROM_MS - 1)
                .is_err()
        );
        assert!(
            held.validate_dispatch_time(DEEPSEEK_PAID_FROM_MS + 1_001)
                .is_err()
        );
        assert!(
            charge(
                &held,
                &search_usage(DEEPSEEK_PAID_FROM_MS, DEEPSEEK_PAID_FROM_MS + 1_001, 1)
            )
            .is_err()
        );
        assert!(
            charge(
                &held,
                &search_usage(DEEPSEEK_PAID_FROM_MS + 1, DEEPSEEK_PAID_FROM_MS, 1)
            )
            .is_err()
        );
    }

    #[test]
    fn expired_reserved_hold_restores_original_identity_without_new_dispatch_permission() {
        let original = search_hold(true, DEEPSEEK_PAID_FROM_MS);
        let restored = make_hold(
            "owner",
            Some("session-a"),
            "operation-a",
            &"a".repeat(64),
            DEEPSEEK_PAID_FROM_MS,
            original.budget.clone(),
        )
        .unwrap();
        let restored = prepared_hold(restored, ReservationState::Reserved).unwrap();
        assert_eq!(restored.hash().unwrap(), original.hash().unwrap());
        assert_eq!(restored.admitted_at_ms, DEEPSEEK_PAID_FROM_MS);
        assert!(
            restored
                .validate_dispatch_time(DEEPSEEK_PAID_FROM_MS + 300_001)
                .is_err()
        );
        // A durable known receipt retains its original dispatch/completion
        // times; a later wall clock must not erase its settlement evidence.
        assert_eq!(
            charge(
                &restored,
                &search_usage(DEEPSEEK_PAID_FROM_MS, DEEPSEEK_PAID_FROM_MS + 1, 1)
            )
            .unwrap()
            .amount_microusd
            .get(),
            750
        );
    }

    #[test]
    fn finalized_hold_identity_is_restorable_but_cannot_prepare_another_request() {
        let original = search_hold(true, DEEPSEEK_PAID_FROM_MS);
        for state in [ReservationState::Settled, ReservationState::Released] {
            let restored: ServiceHold =
                serde_json::from_str(&serde_json::to_string(&original).unwrap()).unwrap();
            restored
                .validate_identity("owner", Some("session-a"))
                .unwrap();
            assert!(prepared_hold(restored, state).is_err());
        }
    }

    #[test]
    fn image_catalog_is_non_token_usage_and_reported_cost_keeps_decimal_precision() {
        let held = hold(
            ServiceBudget::Seedream5 {
                platform_funded: true,
                ceiling_microusd: 18_001,
                price_revision: "catalog-seed-2026-10-25".into(),
            },
            1,
        );
        let usage = KnownUsage::GeneratedImage {
            reported_cost_usd: None,
            provider_request_id: None,
        };
        let catalog = charge(&held, &usage).unwrap();
        assert_eq!(catalog.quantity, 1);
        assert_eq!(catalog.unit, "generated_image");
        assert_eq!(catalog.amount_microusd.get(), 18_000);
        let usage = KnownUsage::GeneratedImage {
            reported_cost_usd: Some(serde_json::from_str("0.01800000000000000000001").unwrap()),
            provider_request_id: None,
        };
        assert_eq!(charge(&held, &usage).unwrap().amount_microusd.get(), 18_001);
        assert!(cost(&json!("-0.018")).is_err());
        assert!(cost(&json!(null)).is_err());
    }

    #[test]
    fn hold_identity_is_durable_owner_session_operation_and_budget_bound() {
        let held = search_hold(true, DEEPSEEK_PAID_FROM_MS);
        let restored: ServiceHold =
            serde_json::from_str(&serde_json::to_string(&held).unwrap()).unwrap();
        assert_eq!(held.id(), restored.id());
        assert_eq!(held.hash().unwrap(), restored.hash().unwrap());
        assert!(
            restored
                .validate_identity("other-owner", Some("session-a"))
                .is_err()
        );
        assert!(
            restored
                .validate_identity("owner", Some("other-session"))
                .is_err()
        );
        assert!(restored.validate_identity("owner", None).is_err());
        let mut changed = restored.clone();
        changed.request_digest = "b".repeat(64);
        assert_ne!(held.hash().unwrap(), changed.hash().unwrap());
        changed.operation = "operation-b".into();
        assert!(
            changed
                .validate_identity("owner", Some("session-a"))
                .is_err()
        );
    }

    #[test]
    fn unknown_cost_is_not_a_zero_charge_or_a_different_service_receipt() {
        let held = hold(
            ServiceBudget::ReportedCost {
                platform_funded: true,
                ceiling_microusd: 10_000,
                unit: "provider_request".into(),
                price_revision: "owned-profile-v1".into(),
            },
            1,
        );
        let unknown = KnownUsage::ReportedCost {
            units: 1,
            reported_cost_usd: None,
            provider_request_id: None,
        };
        assert!(charge(&held, &unknown).is_err());
        assert!(
            charge(
                &held,
                &KnownUsage::GeneratedImage {
                    reported_cost_usd: None,
                    provider_request_id: None
                }
            )
            .is_err()
        );
        let no_charge = charge(
            &held,
            &KnownUsage::ConfirmedNoCharge {
                provider_request_id: Some("known-not-billed".into()),
            },
        )
        .unwrap();
        assert_eq!(no_charge.quantity, 0);
        assert_eq!(no_charge.amount_microusd, MicroUsd::ZERO);
    }

    proptest! {
        #[test]
        fn byok_never_debits_platform_upstream_credit(units in 0u64..=10000, dollars in 0u64..=10000) {
            let held = hold(ServiceBudget::ReportedCost {
                platform_funded: false,
                ceiling_microusd: 0,
                unit: "provider_request".into(),
                price_revision: "byok-v1".into(),
            }, 1);
            let charge = charge(&held, &KnownUsage::ReportedCost {
                units,
                reported_cost_usd: Some(json!(dollars)),
                provider_request_id: None,
            }).unwrap();
            prop_assert_eq!(held.budget.ceiling(), 0);
            prop_assert_eq!(charge.amount_microusd, MicroUsd::ZERO);
            prop_assert_eq!(charge.quantity, units);
            prop_assert_eq!(charge.unit, "provider_request");
        }

        #[test]
        fn unrelated_operations_and_owners_have_distinct_hold_ids(operation in "[a-z0-9_-]{1,64}") {
            prop_assert_ne!(hold_id("owner-a", &operation, "seedream5").unwrap(), hold_id("owner-b", &operation, "seedream5").unwrap());
            prop_assert_ne!(hold_id("owner-a", &operation, "seedream5").unwrap(), hold_id("owner-a", &operation, "deepseek_request").unwrap());
        }
    }
}
