//! Image execution owns bytes until canonical terminal publication. Observer
//! cache expiry is a separate, terminal-started lifetime and never run authority.

use super::*;
use crate::resources::{ResourceMeter, ResourcePricing};
use agents_auth::billing::ReservationState;
use sha2::{Digest, Sha256};

const DELIVERY_MS: u64 = 60 * 60 * 1000;
const RETRY_MS: u64 = 30_000;
const MAX_OPERATIONS: usize = 128;

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct DeliveryLease {
    pub(super) id: String,
    pub(super) acknowledged_at_ms: u64,
    pub(super) expires_at_ms: u64,
}

impl DeliveryLease {
    fn new(id: String, now: u64) -> Self {
        Self {
            id,
            acknowledged_at_ms: now,
            expires_at_ms: now.saturating_add(DELIVERY_MS),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct FundingPlan {
    owner: String,
    session: String,
    response: String,
    execution: String,
    slots: u8,
    pricing: ResourcePricing,
    budget_microusd: i64,
}

#[derive(Clone, Serialize, Deserialize)]
struct RowsReceipt {
    event: String,
    read: u64,
    written: u64,
    started_at_ms: u64,
    ended_at_ms: u64,
}

#[derive(Clone, Serialize, Deserialize)]
struct ImageOperation {
    plan: FundingPlan,
    #[serde(default)]
    assets: Vec<crate::workspace_blobs::ImageExecutionAsset>,
    funded: bool,
    #[serde(default)]
    funding_rejected: bool,
    attempt: u32,
    receipt: Option<RowsReceipt>,
    transferred: bool,
    finished: bool,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct ImageLifecycle {
    operations: Vec<ImageOperation>,
    prepared: bool,
    delivery_id: Option<String>,
    delivery: Option<DeliveryLease>,
    #[serde(default)]
    observation: Option<(String, String)>,
    #[serde(default)]
    original_closed: bool,
}

impl ImageLifecycle {
    fn acknowledge(&mut self, now: u64) {
        if let Some(id) = &self.delivery_id {
            self.delivery
                .get_or_insert_with(|| DeliveryLease::new(id.clone(), now));
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct ImageHandoff {
    pub(super) response_id: String,
    observation: Option<(String, String)>,
    acknowledged: bool,
}

fn fault(code: &'static str) -> Error {
    Error::RustError(code.into())
}

fn funding_id(plan: &FundingPlan) -> String {
    let identity = (&plan.owner, &plan.session, &plan.response, &plan.execution);
    let digest = Sha256::digest(serde_json::to_vec(&identity).expect("string tuple"));
    format!("image-cleanup:{digest:x}")
}

fn budget(pricing: &ResourcePricing, slots: u8) -> Result<i64> {
    if !(1..=4).contains(&slots) {
        return Err(fault("invalid_image_cleanup_slots"));
    }
    let mut amount = 0i64;
    for (resource, unit, quantity) in [
        (
            Resource::DurableObject,
            "wall_ms",
            120_000u64
                + u64::from(slots)
                    * crate::response_image_projection::ORIGINAL_IMAGE_READS_PER_SLOT
                    * crate::response_image_projection::ORIGINAL_IMAGE_READ_TIMEOUT_MS,
        ),
        (Resource::D1, "rows_read", 128 * u64::from(slots)),
        (Resource::D1, "rows_written", 16 * u64::from(slots)),
        (
            Resource::R2Operation,
            "class_b",
            u64::from(slots) * crate::response_image_projection::ORIGINAL_IMAGE_READS_PER_SLOT,
        ),
    ] {
        let charge = pricing
            .tariff(&resource, unit)?
            .charge(quantity)
            .map_err(|_| fault("invalid_image_cleanup_tariff"))?;
        amount = amount
            .checked_add(charge.amount_microusd.get())
            .ok_or_else(|| fault("image_cleanup_budget_overflow"))?;
    }
    amount
        .checked_add(amount / 4 + i64::from(amount % 4 != 0))
        .filter(|amount| *amount > 0)
        .ok_or_else(|| fault("image_cleanup_budget_overflow"))
}

#[derive(Deserialize)]
struct FundingRow {
    request_hash: String,
    state: ReservationState,
}

async fn funding(env: &Env, plan: &FundingPlan) -> Result<Option<ReservationState>> {
    let id = funding_id(plan);
    let row = env.d1("DB")?.prepare("SELECT request_hash,state FROM usage_reservations WHERE id=? AND account_id=? AND session_id IS NULL")
        .bind(&[id.as_str().into(),plan.owner.as_str().into()])?.first::<FundingRow>(None).await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let actual: FundingPlan = serde_json::from_str(&row.request_hash)?;
    if serde_json::to_value(&actual)? != serde_json::to_value(plan)? {
        return Err(fault("image_cleanup_funding_conflict"));
    }
    Ok(Some(row.state))
}

async fn reserve(env: &Env, plan: &FundingPlan) -> Result<()> {
    if let Some(state) = funding(env, plan).await? {
        return if state == ReservationState::Reserved {
            Ok(())
        } else {
            Err(fault("image_cleanup_already_finalized"))
        };
    }
    let id = funding_id(plan);
    let row = crate::metering::reserve(
        &env.d1("DB")?,
        &plan.owner,
        None,
        &id,
        &id,
        &serde_json::to_string(plan)?,
        plan.budget_microusd,
        i64::try_from(util::now_ms() / 1000).map_err(|_| fault("invalid_image_cleanup_time"))?,
    )
    .await?;
    if row.state != ReservationState::Reserved {
        return Err(fault("image_cleanup_already_finalized"));
    }
    Ok(())
}

fn meter<'a>(env: &'a Env, plan: &'a FundingPlan, id: &'a str) -> Result<ResourceMeter<'a>> {
    ResourceMeter::with_account_pricing(env, &plan.owner, id, plan.pricing.clone())
}

fn row_totals(results: &[D1Result]) -> Result<(u64, u64)> {
    results
        .iter()
        .try_fold((0u64, 0u64), |(read, written), result| {
            let meta = result
                .meta()?
                .ok_or_else(|| fault("d1_usage_metadata_missing"))?;
            let next_read =
                meta.rows_read
                    .ok_or_else(|| fault("d1_usage_metadata_missing"))? as u64;
            let next_written =
                meta.rows_written
                    .ok_or_else(|| fault("d1_usage_metadata_missing"))? as u64;
            Ok((
                read.checked_add(next_read)
                    .ok_or_else(|| fault("image_cleanup_usage_overflow"))?,
                written
                    .checked_add(next_written)
                    .ok_or_else(|| fault("image_cleanup_usage_overflow"))?,
            ))
        })
}

async fn record_rows(meter: &ResourceMeter<'_>, event: &str, results: &[D1Result]) -> Result<()> {
    let (read, written) = row_totals(results)?;
    meter
        .record_observation(&format!("{event}:read"), Resource::D1, "rows_read", read)
        .await?;
    meter
        .record_observation(
            &format!("{event}:write"),
            Resource::D1,
            "rows_written",
            written,
        )
        .await
}

impl SessionInner {
    /// Plan first, reserve second, acknowledge third. A crash between these
    /// stages never leaves an unowned cleanup hold or authorizes paid work.
    pub(super) async fn admit_image_execution(&self, execution: &str, slots: u8) -> Result<()> {
        let (owner, session, response, existing) = {
            let data = self.data.borrow();
            let data = data
                .as_ref()
                .ok_or_else(|| fault("session_not_initialized"))?;
            let run = data
                .run
                .as_ref()
                .filter(|run| run.admitted && run.ended_at_ms.is_none())
                .ok_or_else(|| fault("image_execution_not_admitted"))?;
            let response = run
                .response
                .as_ref()
                .ok_or_else(|| fault("hosted_response_missing"))?;
            let existing = response
                .image_lifecycle
                .as_ref()
                .and_then(|lifecycle| {
                    lifecycle
                        .operations
                        .iter()
                        .find(|op| op.plan.execution == execution)
                })
                .cloned();
            (
                data.record.owner_id.clone(),
                data.record.id.clone(),
                response.id.clone(),
                existing,
            )
        };
        let mut operation = match existing {
            Some(operation) => {
                if operation.plan.slots != slots {
                    return Err(fault("image_cleanup_slots_changed"));
                }
                operation
            }
            None => {
                let pricing =
                    ResourcePricing::parse(&self.env.var("RESOURCE_PRICING_JSON")?.to_string())?;
                let plan = FundingPlan {
                    owner,
                    session,
                    response: response.clone(),
                    execution: execution.into(),
                    slots,
                    budget_microusd: budget(&pricing, slots)?,
                    pricing,
                };
                let operation = ImageOperation {
                    plan,
                    assets: vec![],
                    funded: false,
                    funding_rejected: false,
                    attempt: 0,
                    receipt: None,
                    transferred: false,
                    finished: false,
                };
                let observation = self.original_image_identity(&response);
                {
                    let mut data = self.data.borrow_mut();
                    let response = data
                        .as_mut()
                        .unwrap()
                        .run
                        .as_mut()
                        .unwrap()
                        .response
                        .as_mut()
                        .unwrap();
                    let lifecycle = response
                        .image_lifecycle
                        .get_or_insert_with(ImageLifecycle::default);
                    if lifecycle.observation.is_none() {
                        lifecycle.original_closed = observation.is_none();
                        lifecycle.observation = observation;
                    }
                    if lifecycle.operations.len() >= MAX_OPERATIONS {
                        return Err(fault("image_cleanup_operation_limit"));
                    }
                    lifecycle.operations.push(operation.clone());
                }
                self.persist().await?;
                operation
            }
        };
        if let Err(error) = reserve(&self.env, &operation.plan).await {
            if error
                .to_string()
                .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .any(|token| token == "budget_exceeded")
            {
                operation.funding_rejected = true;
                self.set_live_image_operation(&response, operation)?;
                self.persist().await?;
            }
            return Err(error);
        }
        if operation.funded {
            return Ok(());
        }
        operation.funding_rejected = false;
        operation.funded = true;
        self.set_live_image_operation(&response, operation)?;
        self.persist().await
    }

    pub(super) fn original_image_funding(
        &self,
        response: &ResponseRun,
    ) -> crate::response_image_projection::ImageReadFundingMap {
        use crate::response_image_projection::{ImageReadFunding, ImageReadScope};
        response
            .image_lifecycle
            .as_ref()
            .into_iter()
            .flat_map(|lifecycle| &lifecycle.operations)
            .filter(|operation| operation.funded && !operation.finished)
            .map(|operation| {
                (
                    super::hosted_publication::hosted_item_id(&operation.plan.execution),
                    ImageReadFunding {
                        hold_id: funding_id(&operation.plan),
                        pricing: operation.plan.pricing.clone(),
                        event_prefix: "image.original".into(),
                        scope: ImageReadScope::Account,
                        max_reads: u16::from(operation.plan.slots)
                            * crate::response_image_projection::ORIGINAL_IMAGE_READS_PER_SLOT
                                as u16,
                    },
                )
            })
            .collect()
    }

    pub(super) fn conversation_image_assets(
        &self,
        response: &ResponseRun,
    ) -> std::collections::BTreeMap<String, crate::workspace_blobs::OwnedBlobRef> {
        response
            .output
            .iter()
            .filter(|item| item["type"] == "image_generation_call" && item["status"] == "completed")
            .filter_map(|item| item["id"].as_str())
            .flat_map(|item| response.images.assets_for_item(item))
            .filter(|asset| asset.partial_index.is_none())
            .map(|asset| (asset.item_id, asset.blob))
            .collect()
    }

    fn set_live_image_operation(&self, response_id: &str, operation: ImageOperation) -> Result<()> {
        let mut data = self.data.borrow_mut();
        let response = data
            .as_mut()
            .and_then(|data| data.run.as_mut())
            .and_then(|run| run.response.as_mut())
            .filter(|response| response.id == response_id)
            .ok_or_else(|| fault("image_execution_changed"))?;
        let lifecycle = response
            .image_lifecycle
            .as_mut()
            .ok_or_else(|| fault("image_lifecycle_missing"))?;
        let existing = lifecycle
            .operations
            .iter_mut()
            .find(|op| op.plan.execution == operation.plan.execution)
            .ok_or_else(|| fault("image_lifecycle_missing"))?;
        *existing = operation;
        Ok(())
    }

    pub(super) async fn bind_image_execution_asset(
        &self,
        record: &SessionRecord,
        execution: &str,
        artifact: &str,
        blob: &crate::workspace_blobs::OwnedBlobRef,
    ) -> Result<()> {
        let run_id = {
            let data = self.data.borrow();
            let data = data
                .as_ref()
                .ok_or_else(|| fault("session_not_initialized"))?;
            let run = data
                .run
                .as_ref()
                .filter(|run| run.admitted && run.ended_at_ms.is_none())
                .ok_or_else(|| fault("image_execution_not_admitted"))?;
            let operation = run
                .response
                .as_ref()
                .and_then(|response| response.image_lifecycle.as_ref())
                .and_then(|lifecycle| {
                    lifecycle
                        .operations
                        .iter()
                        .find(|op| op.plan.execution == execution)
                })
                .filter(|op| op.funded)
                .ok_or_else(|| fault("image_cleanup_funding_missing"))?;
            if operation.plan.owner != record.owner_id || operation.plan.session != record.id {
                return Err(fault("image_execution_identity_changed"));
            }
            run.id.clone()
        };
        let meter = ResourceMeter::new(&self.env, &record.owner_id, &record.id, &run_id)?;
        let event = format!("image.bind:{}", uuid::Uuid::new_v4());
        meter.require_d1(&event).await?;
        let bound = crate::workspace_blobs::bind_image_execution(
            &self.env,
            &record.owner_id,
            &record.id,
            &run_id,
            execution,
            artifact,
            blob,
        )
        .await?;
        record_rows(&meter, &event, &bound.results).await?;
        if !bound.complete {
            return Err(fault("image_execution_not_active"));
        }
        Ok(())
    }

    pub(super) async fn prepare_image_terminal(&self) -> Result<()> {
        let response_id = {
            let data = self.data.borrow();
            let Some(response) = data
                .as_ref()
                .and_then(|data| data.run.as_ref())
                .and_then(|run| run.response.as_ref())
            else {
                return Ok(());
            };
            if response
                .image_lifecycle
                .as_ref()
                .is_none_or(|lifecycle| lifecycle.prepared)
            {
                return Ok(());
            }
            response.id.clone()
        };
        let mut data = self.data.borrow_mut();
        let data = data.as_mut().unwrap();
        let response = data.run.as_mut().unwrap().response.as_mut().unwrap();
        let lifecycle = response.image_lifecycle.as_mut().unwrap();
        for operation in &mut lifecycle.operations {
            operation.assets =
                response
                    .images
                    .assets_for_item(&super::hosted_publication::hosted_item_id(
                        &operation.plan.execution,
                    ));
            if operation.assets.len() > usize::from(operation.plan.slots) {
                return Err(fault("image_cleanup_asset_limit"));
            }
        }
        lifecycle.prepared = true;
        if !response.request.store {
            lifecycle.delivery_id = Some(util::id("image_delivery"));
        }
        if !data
            .image_handoffs
            .iter()
            .any(|entry| entry.response_id == response_id)
        {
            data.image_handoffs.push(ImageHandoff {
                response_id,
                observation: lifecycle.observation.clone(),
                acknowledged: false,
            });
        }
        Ok(())
    }

    pub(super) async fn close_original_image_delivery(
        &self,
        response_id: &str,
        observation_id: &str,
        generation: &str,
    ) -> Result<()> {
        let _guard = self.persistence.lock().await;
        let Some(mut response) = self.read_response(response_id).await? else {
            return Err(fault("image_observation_not_found"));
        };
        let lifecycle = response
            .image_lifecycle
            .as_mut()
            .ok_or_else(|| fault("image_observation_not_found"))?;
        if lifecycle
            .observation
            .as_ref()
            .is_none_or(|(id, actor)| id != observation_id || actor != generation)
        {
            return Err(fault("image_observation_not_found"));
        }
        lifecycle.original_closed = true;
        self.mark_original_image_closed(response_id);
        let live = self
            .data
            .borrow()
            .as_ref()
            .and_then(|data| data.run.as_ref())
            .is_some_and(|run| run.id == response_id);
        if live {
            self.data
                .borrow_mut()
                .as_mut()
                .unwrap()
                .run
                .as_mut()
                .unwrap()
                .response
                .as_mut()
                .unwrap()
                .image_lifecycle = response.image_lifecycle;
            let mut data = self.snapshot()?;
            save_state(&self.state.storage(), &mut data).await?;
        } else {
            self.write_response(&response).await?;
        }
        self.handoff_images_locked().await
    }

    pub(super) async fn reconcile_image_handoffs(&self) -> Result<()> {
        let ready = self.data.borrow().as_ref().is_some_and(|data| {
            !data.image_handoffs.is_empty() && data.image_handoff_retry_at <= util::now_ms()
        });
        if !ready {
            return Ok(());
        }
        let _guard = self.persistence.lock().await;
        self.handoff_images_locked().await
    }

    pub(super) async fn handoff_images_locked(&self) -> Result<()> {
        let pending = self
            .data
            .borrow()
            .as_ref()
            .map(|data| data.image_handoffs.clone())
            .unwrap_or_default();
        for entry in pending.into_iter().take(8) {
            let live = self
                .data
                .borrow()
                .as_ref()
                .and_then(|data| data.run.as_ref())
                .is_some_and(|run| run.id == entry.response_id);
            if live {
                continue;
            }
            let closed = entry.observation.as_ref().is_none_or(|(_, generation)| {
                self.original_image_observation_closed(&entry.response_id, generation)
            });
            if entry.acknowledged && !closed {
                continue;
            }
            if let Err(error) = self.handoff_one_image_response(&entry, closed).await {
                let mut data = self.snapshot()?;
                data.image_handoff_retry_at = util::now_ms().saturating_add(RETRY_MS);
                save_state(&self.state.storage(), &mut data).await?;
                self.data
                    .borrow_mut()
                    .as_mut()
                    .unwrap()
                    .image_handoff_retry_at = data.image_handoff_retry_at;
                util::emit(
                    "image.terminal_handoff_pending",
                    json!({"session_id":data.record.id,
                    "response_id":entry.response_id,"error_code":safe_error_code(&error)}),
                );
                return Ok(());
            }
        }
        Ok(())
    }

    async fn handoff_one_image_response(&self, entry: &ImageHandoff, closed: bool) -> Result<()> {
        let Some(mut response) = self.read_response(&entry.response_id).await? else {
            return Err(fault("image_handoff_response_missing"));
        };
        if !matches!(
            response.status.as_str(),
            "completed" | "failed" | "incomplete" | "cancelled"
        ) {
            return Err(fault("image_handoff_response_not_terminal"));
        }
        let lifecycle = response
            .image_lifecycle
            .as_mut()
            .ok_or_else(|| fault("image_lifecycle_missing"))?;
        if !lifecycle.prepared {
            return Err(fault("image_handoff_not_prepared"));
        }
        if !entry.acknowledged {
            lifecycle.acknowledge(util::now_ms());
            self.write_response(&response).await?;
            let mut data = self.snapshot()?;
            if let Some(current) = data
                .image_handoffs
                .iter_mut()
                .find(|current| current.response_id == entry.response_id)
            {
                current.acknowledged = true;
            }
            save_state(&self.state.storage(), &mut data).await?;
            self.data.borrow_mut().as_mut().unwrap().image_handoffs = data.image_handoffs;
        }
        let lifecycle = response.image_lifecycle.as_mut().unwrap();
        lifecycle.original_closed |= closed;
        if !lifecycle.original_closed {
            return Ok(());
        }
        self.write_response(&response).await?;
        let count = response.image_lifecycle.as_ref().unwrap().operations.len();
        for index in 0..count {
            self.handoff_image_operation(&mut response, index).await?;
        }
        let mut data = self.snapshot()?;
        data.image_handoffs
            .retain(|current| current.response_id != response.id);
        data.image_handoff_retry_at = 0;
        save_state(&self.state.storage(), &mut data).await?;
        {
            let mut current = self.data.borrow_mut();
            let current = current.as_mut().unwrap();
            current.image_handoffs = data.image_handoffs;
            current.image_handoff_retry_at = 0;
        }
        self.forget_original_image_observation(&response.id);
        Ok(())
    }

    async fn handoff_image_operation(
        &self,
        response: &mut ResponseRun,
        index: usize,
    ) -> Result<()> {
        let mut operation = response.image_lifecycle.as_ref().unwrap().operations[index].clone();
        if operation.finished {
            return Ok(());
        }
        let state = funding(&self.env, &operation.plan).await?;
        if state.is_none() && operation.funding_rejected {
            operation.finished = true;
            response.image_lifecycle.as_mut().unwrap().operations[index] = operation;
            return self.write_response(response).await;
        }
        let state = state.ok_or_else(|| fault("image_cleanup_reservation_unconfirmed"))?;
        if state == ReservationState::Settled && operation.transferred {
            operation.finished = true;
            response.image_lifecycle.as_mut().unwrap().operations[index] = operation;
            return self.write_response(response).await;
        }
        if state != ReservationState::Reserved {
            return Err(fault("image_cleanup_funding_finalized"));
        }
        let hold = funding_id(&operation.plan);
        let meter = meter(&self.env, &operation.plan, &hold)?;
        if operation.receipt.is_none() {
            operation.attempt = operation
                .attempt
                .checked_add(1)
                .ok_or_else(|| fault("image_handoff_attempt_limit"))?;
            response.image_lifecycle.as_mut().unwrap().operations[index] = operation.clone();
            self.write_response(response).await?;
            let event = format!("handoff:{}", operation.attempt);
            meter.require_d1(&event).await?;
            meter
                .require_observation(&format!("{event}:wall"), Resource::DurableObject, "wall_ms")
                .await?;
            let started = util::now_ms();
            let conversation = response
                .conversation
                .as_ref()
                .filter(|binding| binding.prepared)
                .map(|binding| binding.id.as_str());
            let delivery = response
                .image_lifecycle
                .as_ref()
                .unwrap()
                .delivery
                .as_ref()
                .filter(|lease| !response.deleted && lease.expires_at_ms > util::now_ms())
                .map(|lease| crate::workspace_blobs::ImageDelivery {
                    id: lease.id.clone(),
                    expires_at_ms: lease.expires_at_ms,
                });
            let assets = if !response.request.store && conversation.is_none() && delivery.is_none()
            {
                &[][..]
            } else {
                operation.assets.as_slice()
            };
            let transfer = crate::workspace_blobs::transfer_image_execution(
                &self.env,
                &operation.plan.owner,
                &operation.plan.session,
                &operation.plan.response,
                &operation.plan.execution,
                &response.id,
                assets,
                response.request.store && !response.deleted,
                conversation,
                delivery.as_ref(),
            )
            .await?;
            let (read, written) = row_totals(&transfer.results)?;
            operation.receipt = Some(RowsReceipt {
                event,
                read,
                written,
                started_at_ms: started,
                ended_at_ms: util::now_ms(),
            });
            operation.transferred = transfer.complete;
            response.image_lifecycle.as_mut().unwrap().operations[index] = operation.clone();
            self.write_response(response).await?;
        }
        let receipt = operation.receipt.as_ref().unwrap();
        meter
            .record_observation(
                &format!("{}:read", receipt.event),
                Resource::D1,
                "rows_read",
                receipt.read,
            )
            .await?;
        meter
            .record_observation(
                &format!("{}:write", receipt.event),
                Resource::D1,
                "rows_written",
                receipt.written,
            )
            .await?;
        meter
            .record_wall_time(
                &format!("{}:wall", receipt.event),
                Resource::DurableObject,
                receipt.started_at_ms,
                receipt.ended_at_ms,
            )
            .await?;
        if !operation.transferred {
            operation.receipt = None;
            response.image_lifecycle.as_mut().unwrap().operations[index] = operation;
            self.write_response(response).await?;
            return Err(fault("image_handoff_not_ready"));
        }
        meter.settle_run().await?;
        operation.finished = true;
        response.image_lifecycle.as_mut().unwrap().operations[index] = operation;
        self.write_response(response).await
    }
}

pub(super) fn cleanup_deadline(data: &PersistedSession, now: u64) -> Option<u64> {
    (!data.image_handoffs.is_empty())
        .then(|| data.image_handoff_retry_at.max(now.saturating_add(1000)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agents_auth::billing::ResourceTariff;
    use proptest::prelude::*;

    fn pricing() -> ResourcePricing {
        ResourcePricing {
            version: "v1".into(),
            tariffs: [
                (Resource::DurableObject, "wall_ms"),
                (Resource::D1, "rows_read"),
                (Resource::D1, "rows_written"),
                (Resource::R2Operation, "class_b"),
            ]
            .into_iter()
            .map(|(resource, unit)| ResourceTariff {
                resource,
                unit: unit.into(),
                microusd_numerator: 1,
                quantity_denominator: 1,
                price_version: "v1".into(),
            })
            .collect(),
        }
    }

    #[test]
    fn delivery_hour_starts_at_terminal_ack_not_request_or_connection_creation() {
        let ack = 4 * DELIVERY_MS;
        let lease = DeliveryLease::new("private".into(), ack);
        assert_eq!(lease.acknowledged_at_ms, ack);
        assert_eq!(lease.expires_at_ms, ack + DELIVERY_MS);
    }

    #[test]
    fn cleanup_quote_uses_existing_tariffs_and_grows_with_accepted_slots() {
        assert_eq!(budget(&pricing(), 1).unwrap(), 450_190);
        assert_eq!(budget(&pricing(), 4).unwrap(), 1_350_760);
        assert!(budget(&pricing(), 0).is_err());
        assert!(budget(&pricing(), 5).is_err());
    }

    #[test]
    fn a_terminal_retry_never_renews_private_delivery() {
        let mut lifecycle = ImageLifecycle {
            delivery_id: Some("private".into()),
            ..Default::default()
        };
        lifecycle.acknowledge(4 * DELIVERY_MS);
        lifecycle.acknowledge(9 * DELIVERY_MS);
        assert_eq!(
            lifecycle.delivery.as_ref().unwrap().acknowledged_at_ms,
            4 * DELIVERY_MS
        );
        assert_eq!(
            lifecycle.delivery.as_ref().unwrap().expires_at_ms,
            5 * DELIVERY_MS
        );
    }

    #[test]
    fn operation_funding_identity_is_owner_session_run_and_execution_scoped() {
        let original = FundingPlan {
            owner: "owner".into(),
            session: "session".into(),
            response: "resp_a".into(),
            execution: "call_a".into(),
            slots: 1,
            pricing: pricing(),
            budget_microusd: 450_190,
        };
        for field in ["owner", "session", "response", "execution"] {
            let mut changed = original.clone();
            match field {
                "owner" => changed.owner.push('x'),
                "session" => changed.session.push('x'),
                "response" => changed.response.push('x'),
                "execution" => changed.execution.push('x'),
                _ => unreachable!(),
            }
            assert_ne!(funding_id(&original), funding_id(&changed), "{field}");
        }
        let mut new_deployment = original.clone();
        new_deployment.pricing.version = "v2".into();
        assert_eq!(funding_id(&original), funding_id(&new_deployment));
        assert_ne!(
            serde_json::to_value(&original).unwrap(),
            serde_json::to_value(&new_deployment).unwrap()
        );
    }

    proptest! {
        #[test]
        fn delivery_expiry_is_fixed_to_first_terminal_ack(
            ack in 0u64..=u64::MAX-DELIVERY_MS, later in any::<u64>(),
        ) {
            let mut lifecycle = ImageLifecycle { delivery_id: Some("private".into()), ..Default::default() };
            lifecycle.acknowledge(ack);
            lifecycle.acknowledge(later);
            let delivery = lifecycle.delivery.unwrap();
            prop_assert_eq!(delivery.acknowledged_at_ms, ack);
            prop_assert_eq!(delivery.expires_at_ms, ack + DELIVERY_MS);
        }
    }
}
