//! Durable producer for one image operation. Known provider metadata and CAS
//! candidates precede publication; image bytes never enter the DO journal.

use super::*;
use crate::{
    hosted_bindings::{ResolvedBinding, ResolvedTool},
    hosted_image::HostedImageProfile,
    hosted_service_metering::{self as service_metering, KnownUsage, ServiceBudget, ServiceHold},
    image_artifacts::{self, ImageCandidate, IoAttempt},
    image_generation::{
        ImageObservation, ImageResult, ImageSource, MAX_IMAGE_BYTES, PartialImage, ProviderReply,
        Raster,
    },
    provider_response_receipt::{self as provider_receipts, StoredResponse},
    resources::ResourceMeter,
    workspace_blobs::{self as blobs, OwnedBlobRef},
};
use sha2::{Digest, Sha256};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImageSlot {
    partial: Option<u8>,
    capacity_key: String,
    reserved: bool,
    released: bool,
    candidate: Option<ImageCandidate>,
    blob: Option<OwnedBlobRef>,
    io_attempt: u32,
    recovery_checked: bool,
    published: bool,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImageOperation {
    digest: String,
    admitted_at_ms: u64,
    profile: HostedImageProfile,
    hold: Option<ServiceHold>,
    dispatched: bool,
    slots: Vec<ImageSlot>,
    /// Metadata only: the inline source body is replaced with null before save.
    known: Option<Value>,
    remote: Option<ImageResult>,
    usage: Option<KnownUsage>,
    settled: bool,
    failure: Option<String>,
    result: Option<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    provider_replies: Vec<RetainedReply>,
    #[serde(default)]
    provider_io_attempt: u32,
    #[serde(default)]
    provider_capacity_reserved: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    observation: Option<ImageObservation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    lifecycle_failure: Option<crate::image_generation::ImageFailure>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RetainedReply {
    sequence: u16,
    receipt: StoredResponse,
    complete: bool,
    event_stream: bool,
    expected_format: Option<crate::image_generation::Format>,
    requested_size: Option<String>,
    observation: ImageObservation,
}

impl SessionInner {
    pub(super) async fn dispatch_hosted_image(
        &self,
        record: &SessionRecord,
        execution_id: &str,
        call: &provider::ToolCall,
        binding: &ResolvedBinding,
    ) -> Result<HostedDispatch> {
        let key = operation_key(execution_id);
        let digest = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&(
                &record.owner_id,
                &record.id,
                execution_id,
                binding,
                call
            ))?)
        );
        let mut operation = self.read_image_operation(&key).await?;
        if let Some(saved) = &operation {
            if saved.digest != digest {
                return Err(fault("hosted_image_operation_conflict"));
            }
            if let Some(result) = &saved.result {
                return Ok(HostedDispatch::Ready(result.to_string().into()));
            }
            if !saved.dispatched && saved.failure.is_some() {
                return self
                    .finish_hosted_image(record, execution_id, &key, operation.as_mut().unwrap())
                    .await;
            }
        }
        if operation.as_ref().is_some_and(|saved| saved.dispatched) {
            let saved = operation.as_mut().unwrap();
            if saved.slots[0].blob.is_none() && saved.failure.is_none() {
                if let Some(recovered) = self
                    .recover_provider_reply(record, execution_id, &key, saved)
                    .await?
                {
                    if !self
                        .retain_image_exchange(record, execution_id, &key, saved, recovered)
                        .await?
                    {
                        return pending().await;
                    }
                }
            }
            if saved.known.is_none() {
                saved
                    .failure
                    .get_or_insert_with(|| "image_provider_outcome_unknown".into());
                self.save_image_operation(&key, saved).await?;
            }
            return self
                .finish_hosted_image(record, execution_id, &key, saved)
                .await;
        }
        if self.image_cancelled() {
            if let Some(saved) = operation.as_mut() {
                self.abort_image_admission(record, &key, saved).await?;
            }
            return Ok(HostedDispatch::Ready(
                json!({"status":"failed","error":"image_cancelled"})
                    .to_string()
                    .into(),
            ));
        }
        let trace = {
            let data = self.data.borrow();
            crate::telemetry::TraceContext::from_traceparent(
                data.as_ref()
                    .and_then(|data| data.run.as_ref())
                    .and_then(|run| run.traceparent.as_deref()),
            )
        };
        let prepared = crate::hosted_image::prepare(
            &self.env,
            &record.owner_id,
            binding,
            &call.function.arguments,
            execution_id,
            Some(&trace),
        )
        .await?;
        if operation.is_none() {
            let ResolvedTool::Image { options, .. } = &binding.tool else {
                return Err(fault("hosted_image_binding_mismatch"));
            };
            let saved = ImageOperation {
                digest,
                admitted_at_ms: util::now_ms(),
                profile: prepared.plan().clone(),
                hold: None,
                dispatched: false,
                slots: slots(execution_id, options.partial_images.unwrap_or(0))?,
                known: None,
                remote: None,
                usage: None,
                settled: false,
                failure: None,
                result: None,
                provider_replies: vec![],
                provider_io_attempt: 0,
                provider_capacity_reserved: false,
                observation: None,
                lifecycle_failure: None,
            };
            self.save_image_operation(&key, &saved).await?;
            operation = Some(saved);
        }
        let saved = operation.as_mut().unwrap();
        // A recovery may re-resolve a rotated key, but may not change the paid
        // endpoint/capabilities/price snapshot admitted by this operation.
        if serde_json::to_value(&saved.profile)? != serde_json::to_value(prepared.plan())? {
            self.abort_image_admission(record, &key, saved).await?;
            return Err(fault("hosted_image_profile_changed"));
        }
        if let Err(error) = self.admit_image_resources(record) {
            self.abort_image_admission(record, &key, saved).await?;
            return Err(error);
        }
        if !saved.provider_capacity_reserved {
            let response = self
                .data
                .borrow()
                .as_ref()
                .and_then(|data| data.run.as_ref())
                .and_then(|run| run.response.as_ref())
                .map(|response| response.id.clone())
                .ok_or_else(|| fault("hosted_response_missing"))?;
            if let Err(error) = provider_receipts::reserve_capacity(
                &self.env,
                &record.owner_id,
                Some(&record.id),
                Some(&response),
                execution_id,
                saved.slots.len() as u16,
            )
            .await
            {
                self.abort_image_admission(record, &key, saved).await?;
                return Err(error);
            }
            saved.provider_capacity_reserved = true;
            self.save_image_operation(&key, saved).await?;
        }
        let max_assets =
            u8::try_from(saved.slots.len()).map_err(|_| fault("invalid_image_asset_count"))?;
        if let Err(error) = self.admit_image_execution(execution_id, max_assets).await {
            self.abort_image_admission(record, &key, saved).await?;
            return Err(error);
        }
        if saved.hold.is_none() {
            saved.hold = Some(
                service_metering::prepare_hold(
                    &self.env,
                    &record.owner_id,
                    Some(&record.id),
                    execution_id,
                    &saved.digest,
                    saved.admitted_at_ms,
                    budget(&saved.profile)?,
                )
                .await?,
            );
            self.save_image_operation(&key, saved).await?;
        }
        for index in 0..saved.slots.len() {
            if saved.slots[index].reserved {
                continue;
            }
            let held = blobs::reserve_image_capacity(
                &self.env,
                &record.owner_id,
                &saved.slots[index].capacity_key,
                MAX_IMAGE_BYTES as u64,
            )
            .await;
            if let Err(error) = held {
                self.abort_image_admission(record, &key, saved).await?;
                return Err(error);
            }
            saved.slots[index].reserved = true;
            self.save_image_operation(&key, saved).await?;
        }
        if self.image_cancelled() {
            self.abort_image_admission(record, &key, saved).await?;
            return Ok(HostedDispatch::Ready(
                json!({"status":"failed","error":"image_cancelled"})
                    .to_string()
                    .into(),
            ));
        }
        saved
            .hold
            .as_ref()
            .ok_or_else(|| fault("hosted_image_hold_missing"))?
            .validate_dispatch_time(util::now_ms())?;
        if let Err(error) = self
            .publish_hosted_item(
                &json!({"id":hosted_item_id(execution_id),
            "type":"image_generation_call","status":"in_progress","result":null}),
                false,
            )
            .await
        {
            self.abort_image_admission(record, &key, saved).await?;
            return Err(error);
        }
        saved.dispatched = true;
        self.save_image_operation(&key, saved).await?;
        if self.image_cancelled() {
            // This invocation still proves that no external dispatch occurred.
            saved.dispatched = false;
            self.abort_image_admission(record, &key, saved).await?;
            return Ok(HostedDispatch::Ready(
                json!({"status":"failed","error":"image_cancelled"})
                    .to_string()
                    .into(),
            ));
        }
        let shared = Rc::new(RefCell::new(saved.clone()));
        let callback_state = shared.clone();
        let receipt_state = shared.clone();
        let generated = prepared
            .dispatch(
                |partial| {
                    let shared = callback_state.clone();
                    let key = key.clone();
                    async move {
                        let mut saved = shared.borrow().clone();
                        let result = self
                            .retain_image_partial(record, execution_id, &key, &mut saved, partial)
                            .await;
                        *shared.borrow_mut() = saved;
                        result
                    }
                },
                |reply| {
                    let shared = receipt_state.clone();
                    let key = key.clone();
                    async move {
                        let mut saved = shared.borrow().clone();
                        let result = self
                            .retain_provider_reply(record, execution_id, &key, &mut saved, reply)
                            .await;
                        *shared.borrow_mut() = saved;
                        result
                    }
                },
            )
            .await;
        *saved = shared.borrow().clone();
        if let Err(failure) = &generated {
            if matches!(
                failure.observation.phase,
                crate::image_generation::ImagePhase::RetainBody
            ) && !saved.provider_replies.is_empty()
            {
                // An R2 write may have succeeded before its acknowledgement was
                // lost. Observe those same bytes before publishing a failure.
                saved.lifecycle_failure = Some(failure.clone());
                self.save_image_operation(&key, saved).await?;
                return pending().await;
            }
        }
        if !self
            .retain_image_exchange(record, execution_id, &key, saved, generated)
            .await?
        {
            return pending().await;
        }
        self.finish_hosted_image(record, execution_id, &key, saved)
            .await
    }

    async fn retain_image_exchange(
        &self,
        record: &SessionRecord,
        execution_id: &str,
        key: &str,
        saved: &mut ImageOperation,
        generated: std::result::Result<
            crate::image_generation::ImageExchange,
            crate::image_generation::ImageFailure,
        >,
    ) -> Result<bool> {
        match generated {
            Ok(exchange) => {
                let result = exchange.result;
                util::emit(
                    "tool.image_generation.validated",
                    json!({"execution_id":execution_id,
                    "observation":exchange.observation,"price_revision":saved.profile.price_revision}),
                );
                saved.observation = Some(exchange.observation);
                saved.usage = Some(if saved.profile.platform_funded {
                    KnownUsage::GeneratedImage {
                        reported_cost_usd: result.reported_cost().cloned(),
                        provider_request_id: result.provider_request_id.clone(),
                    }
                } else {
                    KnownUsage::ReportedCost {
                        units: 1,
                        reported_cost_usd: result.reported_cost().cloned(),
                        provider_request_id: result.provider_request_id.clone(),
                    }
                });
                saved.known = Some(metadata(&result));
                if matches!(&result.source, ImageSource::Remote { .. }) {
                    saved.remote = Some(result.clone());
                }
                let live = match result.source {
                    ImageSource::Inline { image } => Some(image),
                    ImageSource::Remote { .. } => None,
                };
                if let Some(image) = &live {
                    ensure_candidate(&mut saved.slots[0], &record.owner_id, image)?;
                }
                self.save_image_operation(key, saved).await?;
                if let Some(image) = live {
                    if let Err(error) = self.store_image_slot(record, key, saved, 0, &image).await {
                        self.observe_image_lifecycle_failure(
                            key,
                            execution_id,
                            saved,
                            crate::image_generation::ImagePhase::StoreImage,
                            &error,
                        )
                        .await?;
                        return Ok(false);
                    }
                }
            }
            Err(failure) => {
                util::emit(
                    "tool.image_generation.failed",
                    json!({"execution_id":execution_id,
                    "error_code":failure.code,"error.type":failure.error_type,"observation":failure.observation,
                    "receipt_count":saved.provider_replies.len()}),
                );
                saved.failure = Some(failure.code);
                let observed = failure.observation;
                if observed.confirmed_no_charge {
                    saved.usage = Some(KnownUsage::ConfirmedNoCharge {
                        provider_request_id: observed
                            .generation_id
                            .clone()
                            .or_else(|| observed.request_id.clone()),
                    });
                } else if observed.provider_completed {
                    saved.usage = Some(if saved.profile.platform_funded {
                        KnownUsage::GeneratedImage {
                            reported_cost_usd: observed.reported_cost_usd.clone(),
                            provider_request_id: observed.generation_id.clone(),
                        }
                    } else {
                        KnownUsage::ReportedCost {
                            units: 1,
                            reported_cost_usd: observed.reported_cost_usd.clone(),
                            provider_request_id: observed.generation_id.clone(),
                        }
                    });
                }
                saved.observation = Some(observed);
                self.save_image_operation(key, saved).await?;
            }
        }
        Ok(true)
    }

    fn image_cancelled(&self) -> bool {
        self.data
            .borrow()
            .as_ref()
            .and_then(|data| data.run.as_ref())
            .and_then(|run| run.response.as_ref())
            .is_some_and(|response| response.cancel_requested)
    }

    fn admit_image_resources(&self, record: &SessionRecord) -> Result<()> {
        let data = self.data.borrow();
        let run = data
            .as_ref()
            .and_then(|data| data.run.as_ref())
            .ok_or_else(|| fault("hosted_image_run_missing"))?;
        let response = run
            .response
            .as_ref()
            .ok_or_else(|| fault("hosted_response_missing"))?;
        response.images.admit_final_with_metadata(
            MAX_IMAGE_BYTES,
            serde_json::to_vec(&response.value())?.len(),
        )?;
        let meter = ResourceMeter::new(&self.env, &record.owner_id, &record.id, &run.id)?;
        for (resource, unit) in [
            (Resource::R2Operation, "class_a"),
            (Resource::R2Operation, "class_b"),
            (Resource::R2Storage, "byte_ms"),
        ] {
            meter.require_tariff(&resource, unit)?;
        }
        Ok(())
    }

    async fn read_image_operation(&self, key: &str) -> Result<Option<ImageOperation>> {
        read_chunks(&self.state.storage(), key)
            .await?
            .map(|bytes| {
                serde_json::from_slice(&bytes).map_err(|_| fault("hosted_image_operation_corrupt"))
            })
            .transpose()
    }

    async fn retain_provider_reply(
        &self,
        record: &SessionRecord,
        execution: &str,
        key: &str,
        saved: &mut ImageOperation,
        reply: ProviderReply,
    ) -> Result<ProviderReply> {
        if usize::from(reply.sequence) >= saved.slots.len() {
            return Err(fault("provider_receipt_count_exceeded"));
        }
        let operation = execution.to_owned();
        let descriptor =
            StoredResponse::prepare(&record.owner_id, &operation, &reply.meta, &reply.body)?;
        let index = usize::from(reply.sequence);
        if index == saved.provider_replies.len() {
            saved.provider_replies.push(RetainedReply {
                sequence: reply.sequence,
                receipt: descriptor.clone(),
                complete: false,
                event_stream: reply.event_stream,
                expected_format: reply.expected_format,
                requested_size: reply.requested_size.clone(),
                observation: reply.observation.clone(),
            });
        } else if saved
            .provider_replies
            .get(index)
            .is_none_or(|stored| stored.receipt != descriptor)
        {
            return Err(fault("provider_receipt_identity_conflict"));
        }
        saved.provider_io_attempt = saved
            .provider_io_attempt
            .checked_add(1)
            .ok_or_else(|| fault("provider_receipt_attempt_limit"))?;
        self.save_image_operation(key, saved).await?;
        provider_receipts::register_prepared(&self.env, &record.owner_id, execution, &descriptor)
            .await?;
        let run = self.image_run_id()?;
        let meter = ResourceMeter::new(&self.env, &record.owner_id, &record.id, &run)?;
        let event = format!("{}:{}", provider_capacity_key(execution), reply.sequence);
        provider_receipts::store_prepared(
            &self.env,
            &meter,
            &record.owner_id,
            &operation,
            &descriptor,
            &reply.body,
            &IoAttempt {
                event: &event,
                attempt: saved.provider_io_attempt,
            },
        )
        .await?;
        saved.provider_replies[index].complete = true;
        self.save_image_operation(key, saved).await?;
        util::emit(
            "tool.image_generation.received",
            json!({"execution_id":execution,
            "sequence":reply.sequence,"upstream_status":reply.meta.status,
            "provider_request_id":reply.meta.request_id,"provider_generation_id":reply.meta.generation_id,
            "content_type":reply.meta.content_type,"duration_ms":reply.meta.duration_ms,
            "received_bytes":descriptor.bytes(),"response_sha256":descriptor.sha256(),
            "encrypted_storage_bytes":descriptor.storage_bytes(),"receipt_stored":true}),
        );
        Ok(reply)
    }

    async fn recover_provider_reply(
        &self,
        record: &SessionRecord,
        execution: &str,
        key: &str,
        saved: &mut ImageOperation,
    ) -> Result<
        Option<
            std::result::Result<
                crate::image_generation::ImageExchange,
                crate::image_generation::ImageFailure,
            >,
        >,
    > {
        let Some(retained) = saved.provider_replies.last().cloned() else {
            return Ok(None);
        };
        saved.provider_io_attempt = saved
            .provider_io_attempt
            .checked_add(1)
            .ok_or_else(|| fault("provider_receipt_attempt_limit"))?;
        self.save_image_operation(key, saved).await?;
        let run = self.image_run_id()?;
        let meter = ResourceMeter::new(&self.env, &record.owner_id, &record.id, &run)?;
        let event = format!("{}:{}", provider_capacity_key(execution), retained.sequence);
        let bytes = match provider_receipts::read(
            &self.env,
            &meter,
            &record.owner_id,
            execution,
            &retained.receipt,
            &IoAttempt {
                event: &event,
                attempt: saved.provider_io_attempt,
            },
        )
        .await
        {
            Ok(bytes) => bytes,
            Err(Error::RustError(code))
                if code.starts_with("provider_receipt_missing")
                    || code.contains("integrity")
                    || code.contains("decrypt") =>
            {
                let observed = ImageObservation {
                    upstream_status: Some(retained.receipt.meta().status),
                    phase: crate::image_generation::ImagePhase::RetainBody,
                    received_bytes: retained.receipt.bytes(),
                    request_id: retained.receipt.meta().request_id.clone(),
                    generation_id: retained.receipt.meta().generation_id.clone(),
                    ..Default::default()
                };
                return Ok(Some(Err(crate::image_generation::ImageFailure::new(
                    fault(&code),
                    observed,
                ))));
            }
            Err(error) => return Err(error),
        };
        let reply = ProviderReply {
            sequence: retained.sequence,
            meta: retained.receipt.meta().clone(),
            body: bytes,
            expected_format: retained.expected_format,
            requested_size: retained.requested_size,
            event_stream: retained.event_stream,
            observation: retained.observation,
        };
        let openrouter = saved.profile.api == crate::image_generation::ImageApi::Openrouter
            && saved.profile.base_url.trim_end_matches('/') == "https://openrouter.ai/api/v1";
        util::emit(
            "tool.image_generation.recover",
            json!({"execution_id":execution,
            "sequence":retained.sequence,"response_sha256":retained.receipt.sha256(),"provider_requests":0}),
        );
        Ok(Some(crate::image_generation::recover_received(
            &reply, openrouter,
        )))
    }

    pub(in crate::session) async fn cleanup_image_provider_replies(
        &self,
        owner: &str,
        execution: &str,
        meter: &ResourceMeter<'_>,
    ) -> Result<()> {
        let key = operation_key(execution);
        let Some(mut saved) = self.read_image_operation(&key).await? else {
            return Ok(());
        };
        // Failed/unpublished data may be the only copy of a paid result. Its
        // recovery/diagnostic policy is not the public observer cache lifetime.
        if saved
            .result
            .as_ref()
            .is_none_or(|result| result["status"] != "completed")
        {
            provider_receipts::finalize_capacity(
                &self.env,
                owner,
                execution,
                if saved
                    .observation
                    .as_ref()
                    .is_some_and(|o| o.confirmed_no_charge)
                {
                    provider_receipts::Retention::Diagnostic
                } else {
                    provider_receipts::Retention::Recovery
                },
            )
            .await?;
            return Ok(());
        }
        saved.provider_io_attempt = saved
            .provider_io_attempt
            .checked_add(1)
            .ok_or_else(|| fault("provider_receipt_attempt_limit"))?;
        self.save_image_operation(&key, &saved).await?;
        for retained in &saved.provider_replies {
            let event = format!("{}:{}", provider_capacity_key(execution), retained.sequence);
            provider_receipts::delete(
                &self.env,
                meter,
                owner,
                execution,
                &retained.receipt,
                &IoAttempt {
                    event: &event,
                    attempt: saved.provider_io_attempt,
                },
            )
            .await?;
        }
        provider_receipts::release_capacity(&self.env, owner, execution).await?;
        saved.provider_replies.clear();
        saved.provider_capacity_reserved = false;
        self.save_image_operation(&key, &saved).await
    }

    async fn save_image_operation(&self, key: &str, saved: &ImageOperation) -> Result<()> {
        write_chunks(&self.state.storage(), key, &serde_json::to_vec(saved)?).await
    }

    async fn observe_image_lifecycle_failure(
        &self,
        key: &str,
        execution: &str,
        saved: &mut ImageOperation,
        phase: crate::image_generation::ImagePhase,
        error: &Error,
    ) -> Result<()> {
        let mut observed = saved.observation.clone().unwrap_or_default();
        observed.phase = phase;
        let failure = crate::image_generation::ImageFailure::from_error(error, observed);
        util::emit(
            "tool.image_generation.lifecycle_failed",
            json!({
                "execution_id":execution,"error_code":failure.code,
                "error.type":failure.error_type,"observation":failure.observation,
                "receipt_count":saved.provider_replies.len(),
                "receipt_stored":saved.provider_replies.iter().any(|reply| reply.complete)
            }),
        );
        saved.lifecycle_failure = Some(failure);
        self.save_image_operation(key, saved).await
    }

    async fn abort_image_admission(
        &self,
        record: &SessionRecord,
        key: &str,
        saved: &mut ImageOperation,
    ) -> Result<()> {
        if saved.dispatched {
            return Err(fault("image_dispatch_already_fenced"));
        }
        // Release all deterministic IDs, including an external hold whose local
        // reserved flag may have been lost before its storage acknowledgement.
        for slot in &mut saved.slots {
            blobs::release_image_capacity(&self.env, &record.owner_id, &slot.capacity_key).await?;
            slot.released = true;
        }
        provider_receipts::release_capacity(&self.env, &record.owner_id, &execution_from_key(key))
            .await?;
        saved.provider_capacity_reserved = false;
        if saved.hold.is_none() {
            saved.hold = service_metering::restore_existing_hold(
                &self.env,
                &record.owner_id,
                Some(&record.id),
                &execution_from_key(key),
                &saved.digest,
                saved.admitted_at_ms,
                budget(&saved.profile)?,
            )
            .await?;
        }
        if let Some(hold) = &saved.hold {
            service_metering::release_known_not_dispatched(
                &self.env,
                &record.owner_id,
                Some(&record.id),
                hold,
            )
            .await?;
        }
        saved.settled = true;
        saved.failure = Some("image_not_dispatched".into());
        self.save_image_operation(key, saved).await
    }

    async fn retain_image_partial(
        &self,
        record: &SessionRecord,
        execution_id: &str,
        key: &str,
        saved: &mut ImageOperation,
        partial: PartialImage,
    ) -> Result<()> {
        let index = saved
            .slots
            .iter()
            .position(|slot| slot.partial == Some(partial.index))
            .ok_or_else(|| fault("invalid_image_partial_index"))?;
        if saved.slots[index].candidate.is_some() {
            return Err(fault("image_partial_identity_conflict"));
        }
        saved.slots[index].candidate =
            Some(ImageCandidate::prepare(&record.owner_id, &partial.image)?);
        self.save_image_operation(key, saved).await?;
        self.store_image_slot(record, key, saved, index, &partial.image)
            .await?;
        self.bind_image_slot(record, execution_id, key, saved, index)
            .await?;
        self.publish_image_partial(execution_id, partial.index)
            .await?;
        saved.slots[index].published = true;
        self.save_image_operation(key, saved).await
    }

    async fn store_image_slot(
        &self,
        record: &SessionRecord,
        key: &str,
        saved: &mut ImageOperation,
        index: usize,
        image: &Raster,
    ) -> Result<()> {
        if saved.slots[index].blob.is_some() {
            return Ok(());
        }
        saved.slots[index].io_attempt = saved.slots[index]
            .io_attempt
            .checked_add(1)
            .ok_or_else(|| fault("image_io_attempt_limit"))?;
        self.save_image_operation(key, saved).await?;
        let run_id = self.image_run_id()?;
        let meter = ResourceMeter::new(&self.env, &record.owner_id, &record.id, &run_id)?;
        let slot = &saved.slots[index];
        let candidate = slot
            .candidate
            .as_ref()
            .ok_or_else(|| fault("image_candidate_missing"))?;
        let blob = image_artifacts::store(
            &self.env,
            &record.owner_id,
            &slot.capacity_key,
            candidate,
            image,
            &meter,
            &IoAttempt {
                event: &slot.capacity_key,
                attempt: slot.io_attempt,
            },
        )
        .await?;
        saved.slots[index].blob = Some(blob);
        self.save_image_operation(key, saved).await
    }

    async fn bind_image_slot(
        &self,
        record: &SessionRecord,
        execution_id: &str,
        key: &str,
        saved: &mut ImageOperation,
        index: usize,
    ) -> Result<()> {
        let item = hosted_item_id(execution_id);
        let slot = &saved.slots[index];
        let blob = slot
            .blob
            .as_ref()
            .ok_or_else(|| fault("image_blob_missing"))?;
        let artifact = slot.partial.map_or_else(
            || format!("{item}:result"),
            |partial| format!("{item}:partial:{partial}"),
        );
        self.bind_image_execution_asset(record, execution_id, &artifact, blob)
            .await?;
        {
            let mut data = self.data.borrow_mut();
            let response = data
                .as_mut()
                .and_then(|data| data.run.as_mut())
                .and_then(|run| run.response.as_mut())
                .ok_or_else(|| fault("hosted_response_missing"))?;
            match slot.partial {
                Some(partial) => response.images.insert_partial(
                    &record.owner_id,
                    &item,
                    partial,
                    blob.clone(),
                )?,
                None => response
                    .images
                    .insert_final(&record.owner_id, &item, blob.clone())?,
            }
        }
        self.persist().await?;
        blobs::release_image_capacity(&self.env, &record.owner_id, &slot.capacity_key).await?;
        saved.slots[index].released = true;
        self.save_image_operation(key, saved).await
    }

    async fn publish_image_partial(&self, execution_id: &str, partial: u8) -> Result<()> {
        let item = hosted_item_id(execution_id);
        self.reserve_response_events();
        let (response_id, event) = {
            let mut data = self.data.borrow_mut();
            let response = data
                .as_mut()
                .and_then(|data| data.run.as_mut())
                .and_then(|run| run.response.as_mut())
                .ok_or_else(|| fault("hosted_response_missing"))?;
            if response.deleted {
                return Ok(());
            }
            if response.events.iter().any(|event| {
                event["type"] == "response.image_generation_call.partial_image"
                    && event["item_id"] == item
                    && event["partial_image_index"] == partial
            }) {
                drop(data);
                self.persist().await?;
                return Ok(());
            }
            let index = response
                .output
                .iter()
                .position(|output| output["id"] == item)
                .ok_or_else(|| fault("image_initial_item_missing"))?;
            let event = response.event("response.image_generation_call.partial_image",
                json!({"item_id":item,"output_index":index,"partial_image_index":partial,"partial_image_b64":null}))?;
            (response.id.clone(), event)
        };
        self.response_publication_barrier
            .borrow_mut()
            .begin(&response_id, &event)?;
        self.persist().await
    }

    async fn finish_hosted_image(
        &self,
        record: &SessionRecord,
        execution_id: &str,
        key: &str,
        saved: &mut ImageOperation,
    ) -> Result<HostedDispatch> {
        if saved.hold.is_none() && !saved.settled {
            saved.hold = service_metering::restore_existing_hold(
                &self.env,
                &record.owner_id,
                Some(&record.id),
                execution_id,
                &saved.digest,
                saved.admitted_at_ms,
                budget(&saved.profile)?,
            )
            .await?;
            self.save_image_operation(key, saved).await?;
        }
        if saved.usage.is_some() && saved.hold.is_none() && !saved.settled {
            return pending().await;
        }
        // A known remote outcome permits only an anonymous asset GET retry.
        if saved.slots[0].blob.is_none() && saved.failure.is_none() {
            if let Some(remote) = &saved.remote {
                let image = match saved.profile.materialize(remote).await {
                    Ok(image) => Some(image),
                    Err(Error::RustError(code)) if permanent_asset_failure(&code) => {
                        saved.failure = Some(code);
                        self.save_image_operation(key, saved).await?;
                        None
                    }
                    Err(error) => {
                        self.observe_image_lifecycle_failure(
                            key,
                            execution_id,
                            saved,
                            crate::image_generation::ImagePhase::FetchImage,
                            &error,
                        )
                        .await?;
                        return pending().await;
                    }
                };
                if let Some(image) = image {
                    if let Some(known) = saved.known.as_mut() {
                        known["output_format"] = json!(image.format());
                    }
                    ensure_candidate(&mut saved.slots[0], &record.owner_id, &image)?;
                    self.save_image_operation(key, saved).await?;
                    if let Err(error) = self.store_image_slot(record, key, saved, 0, &image).await {
                        self.observe_image_lifecycle_failure(
                            key,
                            execution_id,
                            saved,
                            crate::image_generation::ImagePhase::StoreImage,
                            &error,
                        )
                        .await?;
                        return pending().await;
                    }
                }
            }
        }
        for index in 0..saved.slots.len() {
            if saved.slots[index].blob.is_none() && !saved.slots[index].recovery_checked {
                if let Some(candidate) = &saved.slots[index].candidate {
                    let candidate = candidate.clone();
                    saved.slots[index].io_attempt = saved.slots[index]
                        .io_attempt
                        .checked_add(1)
                        .ok_or_else(|| fault("image_io_attempt_limit"))?;
                    self.save_image_operation(key, saved).await?;
                    let run_id = self.image_run_id()?;
                    let meter =
                        ResourceMeter::new(&self.env, &record.owner_id, &record.id, &run_id)?;
                    let slot = &saved.slots[index];
                    saved.slots[index].blob = image_artifacts::recover_candidate(
                        &self.env,
                        &record.owner_id,
                        &slot.capacity_key,
                        &candidate,
                        &meter,
                        &IoAttempt {
                            event: &slot.capacity_key,
                            attempt: slot.io_attempt,
                        },
                    )
                    .await?;
                    saved.slots[index].recovery_checked = true;
                    self.save_image_operation(key, saved).await?;
                }
            }
            if saved.slots[index].blob.is_some() {
                if let Err(error) = self
                    .bind_image_slot(record, execution_id, key, saved, index)
                    .await
                {
                    self.observe_image_lifecycle_failure(
                        key,
                        execution_id,
                        saved,
                        crate::image_generation::ImagePhase::BindImage,
                        &error,
                    )
                    .await?;
                    if let Error::RustError(code) = &error {
                        if matches!(
                            code.as_str(),
                            "image_projection_identity_conflict"
                                | "invalid_image_item_id"
                                | "image_projection_limit"
                        ) {
                            return Err(error);
                        }
                    }
                    return pending().await;
                }
                if let Some(partial) = saved.slots[index].partial {
                    if !saved.slots[index].published {
                        self.publish_image_partial(execution_id, partial).await?;
                        saved.slots[index].published = true;
                        self.save_image_operation(key, saved).await?;
                    }
                }
            }
        }
        if saved.known.is_some() && saved.slots[0].blob.is_none() {
            saved.failure = Some("image_artifact_outcome_unknown".into());
            self.save_image_operation(key, saved).await?;
        }
        if !saved.settled {
            if let (Some(hold), Some(usage)) = (&saved.hold, &saved.usage) {
                if let Err(error) = service_metering::settle_hold(
                    &self.env,
                    &record.owner_id,
                    Some(&record.id),
                    hold,
                    usage,
                )
                .await
                {
                    self.observe_image_lifecycle_failure(
                        key,
                        execution_id,
                        saved,
                        crate::image_generation::ImagePhase::SettleUsage,
                        &error,
                    )
                    .await?;
                    return pending().await;
                }
                saved.settled = true;
                self.save_image_operation(key, saved).await?;
            }
        }
        let success = saved.known.is_some() && saved.slots[0].blob.is_some();
        if saved.usage.is_some() {
            // Release only slots whose absence is proved by a complete provider
            // outcome. An ambiguous POST retains unused capacity and API holds.
            for slot in &mut saved.slots {
                if slot.candidate.is_none() && !slot.released {
                    blobs::release_image_capacity(&self.env, &record.owner_id, &slot.capacity_key)
                        .await?;
                    slot.released = true;
                }
            }
            self.save_image_operation(key, saved).await?;
        }
        let item_id = hosted_item_id(execution_id);
        let mut item = saved.known.clone().unwrap_or_else(|| json!({}));
        item["id"] = json!(item_id);
        item["type"] = json!("image_generation_call");
        item["status"] = json!(if success { "completed" } else { "failed" });
        item["result"] = Value::Null;
        self.publish_hosted_item(&item, true).await?;
        let mut result = json!({"type":"image_generation","item_id":item_id,
            "status":if success {"completed"} else {"failed"}});
        if success {
            let blob = saved.slots[0].blob.as_ref().unwrap();
            let filename = format!(
                "{item_id}.{}",
                match blob.content_type.as_str() {
                    "image/jpeg" => "jpg",
                    "image/webp" => "webp",
                    _ => "png",
                }
            );
            match crate::space::link_image_blob(
                &self.env,
                &record.owner_id,
                &item_id,
                &filename,
                blob,
            )
            .await
            {
                Ok(file) => result["file_id"] = json!(file.id),
                Err(Error::RustError(code)) if code == "file_publication_deleted" => {}
                Err(error) => {
                    self.observe_image_lifecycle_failure(
                        key,
                        execution_id,
                        saved,
                        crate::image_generation::ImagePhase::BindImage,
                        &error,
                    )
                    .await?;
                    result["file_link_pending"] = json!(true);
                }
            }
        } else {
            result["error"] = json!(
                saved
                    .failure
                    .as_deref()
                    .unwrap_or("image_provider_outcome_unknown")
            );
            result["outcome_unknown"] = json!(saved.usage.is_none());
            result["recoverable"] =
                json!(saved.provider_replies.iter().any(|reply| reply.complete));
        }
        saved.result = Some(result.clone());
        if !success {
            provider_receipts::finalize_capacity(
                &self.env,
                &record.owner_id,
                execution_id,
                if saved
                    .observation
                    .as_ref()
                    .is_some_and(|o| o.confirmed_no_charge)
                {
                    provider_receipts::Retention::Diagnostic
                } else {
                    provider_receipts::Retention::Recovery
                },
            )
            .await?;
        }
        self.save_image_operation(key, saved).await?;
        Ok(HostedDispatch::Ready(result.to_string().into()))
    }

    fn image_run_id(&self) -> Result<String> {
        self.data
            .borrow()
            .as_ref()
            .and_then(|data| data.run.as_ref())
            .map(|run| run.id.clone())
            .ok_or_else(|| fault("hosted_image_run_missing"))
    }
}

fn ensure_candidate(slot: &mut ImageSlot, owner: &str, image: &Raster) -> Result<()> {
    // Its original UUID already owns capacity even if the PUT never created an
    // object. The store boundary verifies hash, MIME, dimensions and ownership.
    if slot.candidate.is_none() {
        slot.candidate = Some(ImageCandidate::prepare(owner, image)?);
    }
    Ok(())
}

fn permanent_asset_failure(code: &str) -> bool {
    matches!(
        code,
        "image_generation_asset_origin_denied"
            | "invalid_image_generation_asset"
            | "image_generation_format_mismatch"
            | "image_generation_output_mismatch"
            | "image_generation_image_too_large"
            | "invalid_generated_image"
    )
}

fn metadata(result: &ImageResult) -> Value {
    json!({"output_format":result.output_format,"background":result.background,
        "quality":result.quality,"size":result.requested_size,
        "revised_prompt":result.revised_prompt})
}

fn slots(execution_id: &str, partials: u8) -> Result<Vec<ImageSlot>> {
    if partials > 3 {
        return Err(fault("invalid_image_partial_index"));
    }
    let id = format!("{:x}", Sha256::digest(execution_id.as_bytes()));
    let mut slots = vec![ImageSlot {
        partial: None,
        capacity_key: format!("image:{id}:final"),
        reserved: false,
        released: false,
        candidate: None,
        blob: None,
        io_attempt: 0,
        recovery_checked: false,
        published: false,
    }];
    for partial in 0..partials {
        slots.push(ImageSlot {
            partial: Some(partial),
            capacity_key: format!("image:{id}:partial:{partial}"),
            reserved: false,
            released: false,
            candidate: None,
            blob: None,
            io_attempt: 0,
            recovery_checked: false,
            published: false,
        });
    }
    Ok(slots)
}

fn budget(profile: &HostedImageProfile) -> Result<ServiceBudget> {
    let ceiling = i64::try_from(profile.reservation_microusd)
        .map_err(|_| fault("invalid_image_service_ceiling"))?;
    Ok(if profile.platform_funded {
        ServiceBudget::Seedream5 {
            platform_funded: true,
            ceiling_microusd: ceiling,
            price_revision: profile.price_revision.clone(),
        }
    } else {
        ServiceBudget::ReportedCost {
            platform_funded: false,
            ceiling_microusd: 0,
            unit: "generated_image".into(),
            price_revision: profile.price_revision.clone(),
        }
    })
}

fn operation_key(execution: &str) -> String {
    format!("hosted:image:{execution}")
}
fn provider_capacity_key(execution: &str) -> String {
    format!("image-provider:{:x}", Sha256::digest(execution.as_bytes()))
}
fn execution_from_key(key: &str) -> String {
    key.trim_start_matches("hosted:image:").into()
}
fn fault(code: &str) -> Error {
    Error::RustError(code.into())
}
async fn pending() -> Result<HostedDispatch> {
    Delay::from(std::time::Duration::from_millis(500)).await;
    Ok(HostedDispatch::Pending)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image_generation::{Capabilities, Format, ImageApi};

    #[test]
    fn received_image_reprocessing_keeps_the_candidate_that_already_owns_capacity() {
        let image = image::DynamicImage::ImageRgb8(image::RgbImage::new(1, 1));
        let mut bytes = std::io::Cursor::new(Vec::new());
        image.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
        let raster = Raster::from_bytes(bytes.get_ref()).unwrap();
        let mut slot = slots("original-execution", 0).unwrap().remove(0);
        ensure_candidate(&mut slot, "original-owner", &raster).unwrap();
        let original = serde_json::to_value(&slot.candidate).unwrap();
        let mut cold: ImageSlot =
            serde_json::from_slice(&serde_json::to_vec(&slot).unwrap()).unwrap();
        ensure_candidate(&mut cold, "original-owner", &raster).unwrap();
        assert_eq!(serde_json::to_value(&cold.candidate).unwrap(), original);
    }

    fn profile(funded: bool) -> HostedImageProfile {
        HostedImageProfile {
            api: ImageApi::Openrouter,
            base_url: "https://openrouter.ai/api/v1".into(),
            model: "bytedance-seed/seedream-5-0-flash".into(),
            public_model: None,
            capabilities: Capabilities::default(),
            router_endpoint: None,
            asset_hosts: vec![],
            price_revision: "fixture".into(),
            reservation_microusd: if funded { 18_000 } else { 0 },
            platform_funded: funded,
            provenance: crate::hosted_image::ProfileProvenance::VerifiedOperator,
            declared_contract: None,
        }
    }

    #[test]
    fn every_preview_has_an_independent_stable_capacity_and_identity() {
        let slots = slots("execution", 3).unwrap();
        assert_eq!(slots.len(), 4);
        assert_eq!(
            slots
                .iter()
                .map(|slot| &slot.capacity_key)
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            4
        );
        assert_eq!(slots[0].partial, None);
        assert_eq!(slots[3].partial, Some(2));
        assert!(super::slots("execution", 4).is_err());
        assert_eq!(
            slots[0].capacity_key,
            super::slots("execution", 0).unwrap()[0].capacity_key
        );
    }

    #[test]
    fn model_credit_is_independent_of_resource_capacity_and_byok_zero() {
        assert!(matches!(
            budget(&profile(true)).unwrap(),
            ServiceBudget::Seedream5 {
                platform_funded: true,
                ceiling_microusd: 18_000,
                ..
            }
        ));
        assert!(matches!(
            budget(&profile(false)).unwrap(),
            ServiceBudget::ReportedCost {
                platform_funded: false,
                ceiling_microusd: 0,
                ..
            }
        ));
        assert_eq!(slots("byok", 3).unwrap().len(), 4);
    }

    #[test]
    fn known_metadata_cannot_serialize_provider_image_bytes_or_secrets() {
        let result = ImageResult {
            source: ImageSource::Remote {
                url: "https://cdn.example.com/private?token=secret".into(),
                media_type: None,
            },
            provider_request_id: Some("request_private".into()),
            created_at: Some(10),
            revised_prompt: Some("revised".into()),
            output_format: Some(Format::Png),
            background: None,
            quality: None,
            requested_size: None,
            usage: Some(json!({"cost":0.018})),
        };
        let serialized = metadata(&result).to_string();
        for forbidden in [
            "cdn.example.com",
            "secret",
            "request_private",
            "b64_json",
            "source",
        ] {
            assert!(!serialized.contains(forbidden));
        }
        assert_eq!(metadata(&result)["revised_prompt"], "revised");
    }

    #[test]
    fn cold_unknown_state_retains_dispatch_fence_and_no_inline_payload() {
        let saved = ImageOperation {
            digest: "a".repeat(64),
            admitted_at_ms: 100,
            profile: profile(true),
            hold: None,
            dispatched: true,
            slots: slots("execution", 1).unwrap(),
            known: None,
            remote: None,
            usage: None,
            settled: false,
            failure: Some("image_provider_outcome_unknown".into()),
            result: None,
            provider_replies: vec![],
            provider_io_attempt: 0,
            provider_capacity_reserved: false,
            observation: None,
            lifecycle_failure: None,
        };
        let encoded = serde_json::to_vec(&saved).unwrap();
        let restored: ImageOperation = serde_json::from_slice(&encoded).unwrap();
        assert!(restored.dispatched);
        assert!(restored.known.is_none() && restored.hold.is_none());
        assert!(!restored.settled);
        assert!(restored.slots.iter().all(|slot| !slot.released));
        assert!(!String::from_utf8(encoded).unwrap().contains("encoded"));
    }
}
