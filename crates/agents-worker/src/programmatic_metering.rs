//! Dynamic Worker allocation and known requests share the existing immutable
//! wallet. Financial funding never replaces the actor's JavaScript effect fence.

use agents_auth::billing::{ReservationState, Resource, ResourceTariff};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use wasm_bindgen::JsValue;
use worker::{Env, Error, Result};

use crate::{metering, resources::ResourcePricing};

const DAY_MS: u64 = 86_400_000;
const START_WINDOW_MS: u64 = 30_000;
const CPU_GAP_MICROUSD: i64 = 2;
const CREATION_UNIT: &str = "dynamic_worker_creation_day";
const REQUEST_UNIT: &str = "dynamic_worker_request";
const CPU_ALLOCATION_UNIT: &str = "dynamic_worker_cpu_slot_100ms";
const CPU_ALLOCATION_VERSION: &str = "2026-10-25-dynamic-allocation-v1";

/// Persist these trusted actor parameters before any reservation or Loader call.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TurnAdmission {
    pub owner: String,
    pub session: String,
    pub worker_id: String,
    pub code_revision: String,
    pub attempt_id: String,
    pub admitted_at_ms: u64,
    pub cpu_limit_ms: u32,
}

impl TurnAdmission {
    fn validate(&self) -> Result<()> {
        if !text(&self.owner, 512)
            || !opaque(&self.session, 256)
            || !opaque(&self.worker_id, 128)
            || !self.worker_id.starts_with("program-")
            || !hex_digest(&self.code_revision)
            || !opaque(&self.attempt_id, 512)
            || self.cpu_limit_ms != 100
        {
            return Err(fail("invalid_programmatic_funding_admission"));
        }
        Ok(())
    }

    fn validate_start(&self, now: u64) -> Result<()> {
        if now < self.admitted_at_ms
            || now.saturating_sub(self.admitted_at_ms) > START_WINDOW_MS
            || now / DAY_MS != self.admitted_at_ms / DAY_MS
        {
            return Err(fail("programmatic_funding_admission_expired"));
        }
        Ok(())
    }

    fn identity(&self) -> Identity {
        Identity {
            owner: self.owner.clone(),
            worker_id: self.worker_id.clone(),
            code_revision: self.code_revision.clone(),
        }
    }

    fn turn_id(&self, resource: &str) -> Result<String> {
        funding_id(&(
            resource,
            &self.owner,
            &self.session,
            &self.worker_id,
            &self.code_revision,
            &self.attempt_id,
        ))
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Identity {
    owner: String,
    worker_id: String,
    code_revision: String,
}

impl Identity {
    fn daily_id(&self, utc_day: u64) -> Result<String> {
        funding_id(&(CREATION_UNIT, self, utc_day))
    }

    fn cpu_gap_id(&self) -> Result<String> {
        funding_id(&("dynamic_worker_cpu_gap", self))
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Record {
    Creation {
        identity: Identity,
        utc_day: u64,
        claim_attempt: String,
        tariff: ResourceTariff,
    },
    Request {
        admission: TurnAdmission,
        tariff: ResourceTariff,
    },
    CpuGap {
        admission: TurnAdmission,
        ceiling_microusd: i64,
        price_revision: String,
        #[serde(default)]
        allocation_tariff: Option<ResourceTariff>,
    },
}

impl Record {
    fn validate(&self) -> Result<()> {
        match self {
            Self::Creation {
                identity,
                claim_attempt,
                tariff,
                ..
            } => {
                if !text(&identity.owner, 512)
                    || !opaque(&identity.worker_id, 128)
                    || !identity.worker_id.starts_with("program-")
                    || !hex_digest(&identity.code_revision)
                    || !opaque(claim_attempt, 512)
                {
                    return Err(fail("programmatic_funding_identity_conflict"));
                }
                validate_tariff(tariff, CREATION_UNIT, 2000, 1)
            }
            Self::Request { admission, tariff } => {
                admission.validate()?;
                validate_tariff(tariff, REQUEST_UNIT, 3, 10)
            }
            Self::CpuGap {
                admission,
                ceiling_microusd,
                price_revision,
                allocation_tariff,
            } => {
                admission.validate()?;
                if *ceiling_microusd != CPU_GAP_MICROUSD || !text(price_revision, 256) {
                    return Err(fail("invalid_programmatic_cpu_gap_budget"));
                }
                if let Some(tariff) = allocation_tariff {
                    validate_tariff(tariff, CPU_ALLOCATION_UNIT, 2, 1)?;
                    if tariff.price_version != *price_revision {
                        return Err(fail("invalid_programmatic_cpu_allocation_revision"));
                    }
                }
                Ok(())
            }
        }
    }

    fn owner(&self) -> &str {
        match self {
            Self::Creation { identity, .. } => &identity.owner,
            Self::Request { admission, .. } | Self::CpuGap { admission, .. } => &admission.owner,
        }
    }

    fn session(&self) -> Option<&str> {
        match self {
            Self::Creation { .. } => None,
            Self::Request { admission, .. } | Self::CpuGap { admission, .. } => {
                Some(&admission.session)
            }
        }
    }

    fn id(&self) -> Result<String> {
        match self {
            Self::Creation {
                identity, utc_day, ..
            } => identity.daily_id(*utc_day),
            Self::Request { admission, .. } => admission.turn_id(REQUEST_UNIT),
            Self::CpuGap { admission, .. } => admission.identity().cpu_gap_id(),
        }
    }

    fn ceiling(&self) -> Result<i64> {
        match self {
            Self::Creation { tariff, .. } | Self::Request { tariff, .. } => tariff
                .charge(1)
                .map(|charge| charge.amount_microusd.get())
                .map_err(|_| fail("invalid_programmatic_tariff")),
            Self::CpuGap {
                ceiling_microusd, ..
            } => Ok(*ceiling_microusd),
        }
    }

    fn matches(&self, admission: &TurnAdmission) -> Result<()> {
        let matches = match self {
            Self::Creation {
                identity, utc_day, ..
            } => *identity == admission.identity() && *utc_day == admission.admitted_at_ms / DAY_MS,
            Self::Request {
                admission: original,
                ..
            } => serde_json::to_vec(original)? == serde_json::to_vec(admission)?,
            Self::CpuGap {
                admission: original,
                ..
            } => {
                original.identity() == admission.identity()
                    && original.session == admission.session
                    && original.cpu_limit_ms == admission.cpu_limit_ms
            }
        };
        if !matches {
            return Err(fail("programmatic_funding_identity_conflict"));
        }
        Ok(())
    }
}

struct Stored {
    record: Record,
    state: ReservationState,
}

/// Not Deserialize: only scoped ledger verification can mint this authority.
/// It still does not authorize repeating an ambiguous actor/runtime invocation.
pub(crate) struct FundedTurn {
    admission: TurnAdmission,
    creation: Stored,
    request: Stored,
    cpu_gap: Stored,
}

#[derive(Clone, Serialize)]
pub(crate) struct MeteringGap {
    pub code: &'static str,
    pub cpu_hold_id: String,
    pub reserved_cpu_microusd: i64,
    pub creation_day_pending: bool,
}

#[derive(Clone, Serialize)]
pub(crate) struct CapacityAllocationReceipt {
    pub allocation_profile: &'static str,
    pub hold_id: String,
    pub unit: String,
    pub slots: u64,
    pub amount_microusd: i64,
    pub price_version: String,
}

struct CapacityIdentity<'a> {
    worker_id: &'a str,
    code_revision: &'a str,
    cpu_limit_ms: u32,
}

fn capacity_charge(
    record: &Record,
    admission: &TurnAdmission,
    proof: &CapacityIdentity<'_>,
) -> Result<agents_auth::billing::Charge> {
    record.validate()?;
    record.matches(admission)?;
    if proof.worker_id != admission.worker_id
        || proof.code_revision != admission.code_revision
        || proof.cpu_limit_ms != admission.cpu_limit_ms
    {
        return Err(fail("programmatic_cpu_allocation_identity_conflict"));
    }
    let Record::CpuGap {
        allocation_tariff: Some(tariff),
        ..
    } = record
    else {
        return Err(fail("programmatic_cpu_allocation_price_missing"));
    };
    let slots = u64::from(proof.cpu_limit_ms).div_ceil(100);
    let charge = tariff
        .charge(slots)
        .map_err(|_| fail("invalid_programmatic_tariff"))?;
    if charge.amount_microusd.get() > record.ceiling()? {
        return Err(fail("programmatic_cpu_allocation_exceeds_hold"));
    }
    Ok(charge)
}

impl FundedTurn {
    /// Call again synchronously immediately before the native Loader effect.
    /// A previous check cannot survive an intervening funding/storage await.
    pub(crate) fn verify_dispatch_time(&self, now: u64) -> Result<()> {
        self.admission.validate_start(now)
    }

    pub(crate) async fn verify_before_dispatch(
        &self,
        env: &Env,
        worker_id: &str,
        code_revision: &str,
        cpu_limit_ms: u32,
        now: u64,
    ) -> Result<()> {
        self.verify_dispatch_time(now)?;
        if worker_id != self.admission.worker_id
            || code_revision != self.admission.code_revision
            || cpu_limit_ms != self.admission.cpu_limit_ms
        {
            return Err(fail("programmatic_funding_identity_conflict"));
        }
        let restored = restore_turn(env, &self.admission)
            .await?
            .ok_or_else(|| fail("programmatic_funding_missing"))?;
        verify_dispatch_clocks(&self.admission, now, crate::util::now_ms())?;
        creation_dispatchable(&restored.creation, &self.admission)?;
        if restored.request.state != ReservationState::Reserved
            || restored.cpu_gap.state != ReservationState::Reserved
        {
            return Err(fail("programmatic_funding_finalized"));
        }
        Ok(())
    }
}

fn verify_dispatch_clocks(admission: &TurnAdmission, before_io: u64, after_io: u64) -> Result<()> {
    admission.validate_start(before_io)?;
    admission.validate_start(after_io)
}

/// Read-only pre-model admission; neither reserves money nor creates runtime state.
pub(crate) fn validate_configuration(env: &Env) -> Result<()> {
    configuration(env).map(|_| ())
}

fn configuration(env: &Env) -> Result<ResourcePricing> {
    let pricing = ResourcePricing::parse(&env.var("RESOURCE_PRICING_JSON")?.to_string())?;
    validate_tariff(
        pricing.tariff(&Resource::Worker, CREATION_UNIT)?,
        CREATION_UNIT,
        2000,
        1,
    )?;
    validate_tariff(
        pricing.tariff(&Resource::Worker, REQUEST_UNIT)?,
        REQUEST_UNIT,
        3,
        10,
    )?;
    let cpu = pricing.tariff(&Resource::Worker, CPU_ALLOCATION_UNIT)?;
    validate_tariff(cpu, CPU_ALLOCATION_UNIT, 2, 1)?;
    Ok(pricing)
}

pub(crate) async fn prepare_turn(env: &Env, admission: &TurnAdmission) -> Result<FundedTurn> {
    admission.validate()?;
    if let Some(restored) = restore_turn(env, admission).await? {
        return Ok(restored);
    }
    admission.validate_start(crate::util::now_ms())?;
    let pricing = configuration(env)?;
    let creation_tariff = pricing.tariff(&Resource::Worker, CREATION_UNIT)?.clone();
    let request_tariff = pricing.tariff(&Resource::Worker, REQUEST_UNIT)?.clone();
    let cpu_tariff = pricing
        .tariff(&Resource::Worker, CPU_ALLOCATION_UNIT)?
        .clone();
    let candidates = candidates(
        admission,
        creation_tariff,
        request_tariff,
        pricing.version,
        Some(cpu_tariff),
    );
    let creation = load_or_reserve(env, admission, candidates.0).await?;
    creation_dispatchable(&creation, admission)?;
    let request = load_or_reserve(env, admission, candidates.1).await?;
    let cpu_gap = load_or_reserve(env, admission, candidates.2).await?;
    Ok(FundedTurn {
        admission: admission.clone(),
        creation,
        request,
        cpu_gap,
    })
}

/// This read-only path also works after UTC/admission expiry or settlement.
/// None includes partial funding: release_partial_known_not_dispatched can clean
/// such rows without ever constructing a new hold after an uncertain execution.
pub(crate) async fn restore_turn(
    env: &Env,
    admission: &TurnAdmission,
) -> Result<Option<FundedTurn>> {
    admission.validate()?;
    let ids = lookup_ids(admission)?;
    let creation = load(env, &admission.owner, None, &ids.0).await?;
    let request = load(env, &admission.owner, Some(&admission.session), &ids.1).await?;
    let cpu_gap = load(env, &admission.owner, Some(&admission.session), &ids.2).await?;
    for stored in [&creation, &request, &cpu_gap].into_iter().flatten() {
        stored.record.matches(admission)?;
    }
    let (Some(creation), Some(request), Some(cpu_gap)) = (creation, request, cpu_gap) else {
        return Ok(None);
    };
    Ok(Some(FundedTurn {
        admission: admission.clone(),
        creation,
        request,
        cpu_gap,
    }))
}

/// The actor persists this trusted native outcome before calling. A valid reply
/// proves invocation, not a cold startup nor an actual measured CPU quantity.
pub(crate) async fn record_known_execution(
    env: &Env,
    funded: &FundedTurn,
    started_at_ms: u64,
    completed_at_ms: u64,
) -> Result<MeteringGap> {
    funded.admission.validate_start(started_at_ms)?;
    if completed_at_ms < started_at_ms {
        return Err(fail("invalid_programmatic_receipt_time"));
    }
    let restored = restore_turn(env, &funded.admission)
        .await?
        .ok_or_else(|| fail("programmatic_funding_missing"))?;
    creation_known(&restored.creation, &funded.admission)?;
    settle_known(env, &restored.request).await?;
    let creation_day_pending = completed_at_ms / DAY_MS != started_at_ms / DAY_MS;
    if !creation_day_pending {
        settle_known(env, &restored.creation).await?;
    }
    Ok(MeteringGap {
        code: "programmatic_cpu_measurement_unavailable",
        cpu_hold_id: restored.cpu_gap.record.id()?,
        reserved_cpu_microusd: restored.cpu_gap.record.ceiling()?,
        creation_day_pending,
    })
}

/// Any known native fetch response establishes one request, even when its HTTP
/// status or body cannot establish successful isolate execution. Record the
/// durable native receipt first; neither a JS assertion nor elapsed await time
/// establishes this observation.
pub(crate) async fn record_known_request(
    env: &Env,
    funded: &FundedTurn,
    started_at_ms: u64,
    completed_at_ms: u64,
) -> Result<MeteringGap> {
    funded.admission.validate_start(started_at_ms)?;
    if completed_at_ms < started_at_ms {
        return Err(fail("invalid_programmatic_receipt_time"));
    }
    let restored = restore_turn(env, &funded.admission)
        .await?
        .ok_or_else(|| fail("programmatic_funding_missing"))?;
    settle_known(env, &restored.request).await?;
    Ok(MeteringGap {
        code: "programmatic_cpu_measurement_unavailable",
        cpu_hold_id: restored.cpu_gap.record.id()?,
        reserved_cpu_microusd: restored.cpu_gap.record.ceiling()?,
        creation_day_pending: restored.creation.state != ReservationState::Settled,
    })
}

/// The native single-origin runtime alone can mint this scope witness. Charge
/// the published reserved capacity once, not an invented measured CPU duration.
/// The actor persists the known native outcome before calling this method.
pub(crate) async fn record_capacity_allocation(
    env: &Env,
    funded: &FundedTurn,
    proof: &crate::programmatic_runtime::EnforcedProgramBudget,
) -> Result<CapacityAllocationReceipt> {
    let restored = restore_turn(env, &funded.admission)
        .await?
        .ok_or_else(|| fail("programmatic_funding_missing"))?;
    let charge = capacity_charge(
        &restored.cpu_gap.record,
        &funded.admission,
        &CapacityIdentity {
            worker_id: proof.worker_id(),
            code_revision: proof.code_revision(),
            cpu_limit_ms: proof.cpu_limit_ms(),
        },
    )?;
    let id = restored.cpu_gap.record.id()?;
    metering::append(
        &env.d1("DB")?,
        &id,
        "capacity-allocation",
        &charge,
        seconds()?,
    )
    .await?;
    metering::settle(&env.d1("DB")?, &id, seconds()?).await?;
    Ok(CapacityAllocationReceipt {
        allocation_profile: CPU_ALLOCATION_VERSION,
        hold_id: id,
        unit: charge.unit,
        slots: charge.quantity,
        amount_microusd: charge.amount_microusd.get(),
        price_version: charge.price_version,
    })
}

/// A durable actor proof of no dispatch is required. Unknown execution is not
/// cleanup permission; creation's shared known daily allocation is never freed.
pub(crate) async fn release_partial_known_not_dispatched(
    env: &Env,
    admission: &TurnAdmission,
) -> Result<()> {
    admission.validate()?;
    let ids = lookup_ids(admission)?;
    if load(env, &admission.owner, Some(&admission.session), &ids.1)
        .await?
        .is_some_and(|stored| stored.state == ReservationState::Settled)
    {
        return Err(fail("programmatic_known_request_conflicts_no_dispatch"));
    }
    for (id, session) in [
        (&ids.1, Some(admission.session.as_str())),
        (&ids.2, Some(admission.session.as_str())),
        (&ids.0, None),
    ] {
        let Some(stored) = load(env, &admission.owner, session, id).await? else {
            continue;
        };
        stored.record.matches(admission)?;
        if matches!(&stored.record, Record::Creation { claim_attempt, .. } if claim_attempt != &admission.attempt_id)
            || matches!(&stored.record, Record::CpuGap { admission: original, .. } if original.attempt_id != admission.attempt_id)
        {
            continue;
        }
        if stored.state == ReservationState::Reserved {
            metering::release(&env.d1("DB")?, &stored.record.id()?, seconds()?).await?;
        }
    }
    Ok(())
}

fn candidates(
    admission: &TurnAdmission,
    creation: ResourceTariff,
    request: ResourceTariff,
    revision: String,
    allocation_tariff: Option<ResourceTariff>,
) -> (Record, Record, Record) {
    (
        Record::Creation {
            identity: admission.identity(),
            utc_day: admission.admitted_at_ms / DAY_MS,
            claim_attempt: admission.attempt_id.clone(),
            tariff: creation,
        },
        Record::Request {
            admission: admission.clone(),
            tariff: request,
        },
        Record::CpuGap {
            admission: admission.clone(),
            ceiling_microusd: CPU_GAP_MICROUSD,
            price_revision: revision,
            allocation_tariff,
        },
    )
}

fn lookup_ids(admission: &TurnAdmission) -> Result<(String, String, String)> {
    // IDs exclude tariffs. Existing rows supply their frozen reviewed profiles,
    // so a later configuration revision cannot double-charge a known daily ID.
    Ok((
        admission
            .identity()
            .daily_id(admission.admitted_at_ms / DAY_MS)?,
        admission.turn_id(REQUEST_UNIT)?,
        admission.identity().cpu_gap_id()?,
    ))
}

fn funding_id<T: Serialize>(identity: &T) -> Result<String> {
    Ok(format!(
        "program_funding_{:x}",
        Sha256::digest(serde_json::to_vec(identity)?)
    ))
}

fn creation_dispatchable(stored: &Stored, admission: &TurnAdmission) -> Result<()> {
    if stored.state == ReservationState::Settled {
        return Ok(());
    }
    creation_known(stored, admission)
}

fn creation_known(stored: &Stored, admission: &TurnAdmission) -> Result<()> {
    match &stored.record {
        Record::Creation { claim_attempt, .. }
            if stored.state != ReservationState::Released
                && (stored.state == ReservationState::Settled
                    || claim_attempt == &admission.attempt_id) =>
        {
            Ok(())
        }
        _ => Err(fail("programmatic_creation_outcome_unknown")),
    }
}

async fn load_or_reserve(
    env: &Env,
    admission: &TurnAdmission,
    candidate: Record,
) -> Result<Stored> {
    candidate.validate()?;
    let id = candidate.id()?;
    if let Some(stored) = load(env, candidate.owner(), candidate.session(), &id).await? {
        stored.record.matches(admission)?;
        return Ok(stored);
    }
    admission.validate_start(crate::util::now_ms())?;
    metering::reserve(
        &env.d1("DB")?,
        candidate.owner(),
        candidate.session(),
        &id,
        &id,
        &serde_json::to_string(&candidate)?,
        candidate.ceiling()?,
        seconds()?,
    )
    .await?;
    load(env, candidate.owner(), candidate.session(), &id)
        .await?
        .ok_or_else(|| fail("programmatic_funding_missing"))
}

async fn load(env: &Env, owner: &str, session: Option<&str>, id: &str) -> Result<Option<Stored>> {
    #[derive(Deserialize)]
    struct Row {
        request_hash: String,
        idempotency_key: String,
        reserved_microusd: String,
        state: ReservationState,
    }
    let row = env.d1("DB")?.prepare("SELECT request_hash,idempotency_key,CAST(reserved_microusd AS TEXT) AS reserved_microusd,state FROM usage_reservations WHERE id=? AND account_id=? AND session_id IS ?")
        .bind(&[JsValue::from_str(id), JsValue::from_str(owner), session.map(JsValue::from_str).unwrap_or(JsValue::NULL)])?.first::<Row>(None).await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let record: Record = serde_json::from_str(&row.request_hash)
        .map_err(|_| fail("programmatic_funding_identity_conflict"))?;
    record.validate()?;
    if record.owner() != owner
        || record.session() != session
        || record.id()? != id
        || row.idempotency_key != id
        || record.ceiling()?.to_string() != row.reserved_microusd
    {
        return Err(fail("programmatic_funding_identity_conflict"));
    }
    Ok(Some(Stored {
        record,
        state: row.state,
    }))
}

async fn settle_known(env: &Env, stored: &Stored) -> Result<()> {
    let tariff = match &stored.record {
        Record::Creation { tariff, .. } | Record::Request { tariff, .. } => tariff,
        Record::CpuGap { .. } => return Err(fail("programmatic_cpu_measurement_unavailable")),
    };
    let charge = tariff
        .charge(1)
        .map_err(|_| fail("invalid_programmatic_tariff"))?;
    let id = stored.record.id()?;
    metering::append(&env.d1("DB")?, &id, "known", &charge, seconds()?).await?;
    metering::settle(&env.d1("DB")?, &id, seconds()?).await?;
    Ok(())
}

fn validate_tariff(
    tariff: &ResourceTariff,
    unit: &str,
    numerator: u64,
    denominator: u64,
) -> Result<()> {
    if tariff.resource != Resource::Worker
        || tariff.unit != unit
        || !text(&tariff.price_version, 256)
        || tariff.quantity_denominator == 0
        || u128::from(tariff.microusd_numerator) * u128::from(denominator)
            != u128::from(numerator) * u128::from(tariff.quantity_denominator)
        || tariff.charge(1).is_err()
    {
        return Err(fail("invalid_programmatic_tariff"));
    }
    Ok(())
}

fn text(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}
fn opaque(value: &str, max: usize) -> bool {
    text(value, max)
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-:".contains(&b))
}
fn hex_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn seconds() -> Result<i64> {
    i64::try_from(crate::util::now_ms() / 1000).map_err(|_| fail("invalid_programmatic_time"))
}
fn fail(code: &str) -> Error {
    Error::RustError(code.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn admission() -> TurnAdmission {
        TurnAdmission {
            owner: "account-a".into(),
            session: "session-a".into(),
            worker_id: "program-opaque-operation-a".into(),
            code_revision: "a".repeat(64),
            attempt_id: "attempt-a".into(),
            admitted_at_ms: DAY_MS + 1_000,
            cpu_limit_ms: 100,
        }
    }

    fn tariff(unit: &str, numerator: u64, denominator: u64) -> ResourceTariff {
        ResourceTariff {
            resource: Resource::Worker,
            unit: unit.into(),
            microusd_numerator: numerator,
            quantity_denominator: denominator,
            price_version: "reviewed-public-allocation-v1".into(),
        }
    }

    fn records(admission: &TurnAdmission) -> (Record, Record, Record) {
        candidates(
            admission,
            tariff(CREATION_UNIT, 2000, 1),
            tariff(REQUEST_UNIT, 3, 10),
            "reviewed-public-allocation-v1".into(),
            None,
        )
    }

    #[test]
    fn creation_is_daily_identity_not_an_await_or_request_charge() {
        let original = admission();
        let first = records(&original);
        let mut next = original.clone();
        next.attempt_id = "attempt-resume".into();
        next.admitted_at_ms += 10_000;
        let resume = records(&next);
        assert_eq!(first.0.id().unwrap(), resume.0.id().unwrap());
        assert_ne!(first.1.id().unwrap(), resume.1.id().unwrap());
        assert_eq!(first.2.id().unwrap(), resume.2.id().unwrap());
        next.admitted_at_ms += DAY_MS;
        assert_ne!(first.0.id().unwrap(), records(&next).0.id().unwrap());
        next = original.clone();
        next.code_revision = "b".repeat(64);
        assert_ne!(first.0.id().unwrap(), records(&next).0.id().unwrap());
    }

    #[test]
    fn unknown_creation_cannot_fund_a_different_attempt() {
        let original = admission();
        let mut next = original.clone();
        next.attempt_id = "attempt-next".into();
        let record = records(&original).0;
        let pending = Stored {
            record: record.clone(),
            state: ReservationState::Reserved,
        };
        assert!(creation_dispatchable(&pending, &original).is_ok());
        assert!(creation_dispatchable(&pending, &next).is_err());
        let known = Stored {
            record: record.clone(),
            state: ReservationState::Settled,
        };
        assert!(creation_dispatchable(&known, &next).is_ok());
        let cancelled = Stored {
            record,
            state: ReservationState::Released,
        };
        assert!(creation_dispatchable(&cancelled, &original).is_err());
    }

    #[test]
    fn exact_reviewed_rates_use_existing_rounding_without_included_usage_guessing() {
        let records = records(&admission());
        assert_eq!(records.0.ceiling().unwrap(), 2000);
        assert_eq!(records.1.ceiling().unwrap(), 1);
        assert_eq!(records.2.ceiling().unwrap(), 2);
        assert!(validate_tariff(&tariff(CREATION_UNIT, 1000, 1), CREATION_UNIT, 2000, 1).is_err());
        assert!(validate_tariff(&tariff(REQUEST_UNIT, 3, 100), REQUEST_UNIT, 3, 10).is_err());
        assert!(validate_tariff(&tariff(REQUEST_UNIT, 30, 100), REQUEST_UNIT, 3, 10).is_ok());
        assert!(validate_tariff(&tariff("request", 3, 10), REQUEST_UNIT, 3, 10).is_err());
    }

    #[test]
    fn expired_identity_can_restore_but_cannot_start_in_another_utc_day() {
        let original = admission();
        let record = records(&original).1;
        let restored: Record =
            serde_json::from_str(&serde_json::to_string(&record).unwrap()).unwrap();
        restored.validate().unwrap();
        restored.matches(&original).unwrap();
        assert!(
            original
                .validate_start(original.admitted_at_ms - 1)
                .is_err()
        );
        assert!(
            original
                .validate_start(original.admitted_at_ms + START_WINDOW_MS + 1)
                .is_err()
        );
        let mut midnight = original.clone();
        midnight.admitted_at_ms = DAY_MS * 2 - 1;
        assert!(midnight.validate_start(DAY_MS * 2).is_err());
        let mut changed = original.clone();
        changed.admitted_at_ms += 1;
        assert!(restored.matches(&changed).is_err());
    }

    #[test]
    fn delayed_readonly_funding_verification_rechecks_window_and_utc_day() {
        let mut current = admission();
        let mut clocks = [
            current.admitted_at_ms,
            current.admitted_at_ms + START_WINDOW_MS + 1,
        ]
        .into_iter();
        assert!(
            verify_dispatch_clocks(&current, clocks.next().unwrap(), clocks.next().unwrap())
                .is_err()
        );
        current.admitted_at_ms = DAY_MS * 2 - 1;
        let mut clocks = [current.admitted_at_ms, DAY_MS * 2].into_iter();
        assert!(
            verify_dispatch_clocks(&current, clocks.next().unwrap(), clocks.next().unwrap())
                .is_err()
        );
        assert!(
            verify_dispatch_clocks(&current, current.admitted_at_ms, current.admitted_at_ms)
                .is_ok()
        );
        // Restoring an old financial identity still succeeds. Rechecking time
        // denies dispatch; it never invents a fresh identity or new UTC hold.
        let original_id = records(&current).0.id().unwrap();
        let restored: Record =
            serde_json::from_str(&serde_json::to_string(&records(&current).0).unwrap()).unwrap();
        assert_eq!(restored.id().unwrap(), original_id);
    }

    #[test]
    fn frozen_profile_revision_does_not_create_another_daily_charge_identity() {
        let mut revised = tariff(CREATION_UNIT, 2000, 1);
        revised.price_version = "reviewed-public-allocation-v2".into();
        let original = admission();
        let first = records(&original).0;
        let next = candidates(
            &original,
            revised,
            tariff(REQUEST_UNIT, 3, 10),
            "v2".into(),
            None,
        )
        .0;
        assert_eq!(first.id().unwrap(), next.id().unwrap());
        assert_ne!(
            serde_json::to_string(&first).unwrap(),
            serde_json::to_string(&next).unwrap()
        );
    }

    #[test]
    fn cpu_gap_never_claims_a_measured_or_reserved_slot_quantity() {
        let gap = MeteringGap {
            code: "programmatic_cpu_measurement_unavailable",
            cpu_hold_id: "hold".into(),
            reserved_cpu_microusd: 2,
            creation_day_pending: false,
        };
        let encoded = serde_json::to_value(gap).unwrap();
        assert!(encoded.get("quantity").is_none());
        assert!(encoded.get("cpu_ms").is_none());
        assert_eq!(encoded["reserved_cpu_microusd"], 2);
        let mut unsupported = admission();
        unsupported.cpu_limit_ms = 1000;
        assert!(unsupported.validate().is_err());
    }

    #[test]
    fn enforced_program_capacity_uses_named_slots_not_actual_cpu_or_await_duration() {
        let admission = admission();
        let mut cpu_tariff = tariff(CPU_ALLOCATION_UNIT, 2, 1);
        cpu_tariff.price_version = CPU_ALLOCATION_VERSION.into();
        let record = candidates(
            &admission,
            tariff(CREATION_UNIT, 2000, 1),
            tariff(REQUEST_UNIT, 3, 10),
            CPU_ALLOCATION_VERSION.into(),
            Some(cpu_tariff),
        )
        .2;
        let proof = CapacityIdentity {
            worker_id: &admission.worker_id,
            code_revision: &admission.code_revision,
            cpu_limit_ms: 100,
        };
        let charge = capacity_charge(&record, &admission, &proof).unwrap();
        assert_eq!(charge.unit, CPU_ALLOCATION_UNIT);
        assert_eq!(charge.quantity, 1);
        assert_eq!(charge.amount_microusd.get(), 2);
        assert_eq!(charge.price_version, CPU_ALLOCATION_VERSION);
        assert!(capacity_charge(&records(&admission).2, &admission, &proof).is_err());
        assert!(
            capacity_charge(
                &record,
                &admission,
                &CapacityIdentity {
                    worker_id: "program-other",
                    ..proof
                }
            )
            .is_err()
        );
    }

    proptest! {
        #[test]
        fn funding_ids_are_owner_isolated_and_have_separate_resource_dimensions(owner in "[a-z]{1,32}") {
            let mut first = admission();
            first.owner = format!("first-{owner}");
            let mut second = first.clone();
            second.owner = format!("second-{owner}");
            let ids = lookup_ids(&first).unwrap();
            prop_assert_ne!(&ids.0, &ids.1);
            prop_assert_ne!(&ids.1, &ids.2);
            prop_assert_ne!(ids.0, lookup_ids(&second).unwrap().0);
        }
    }
}
