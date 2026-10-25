//! Account and Session model work share one admission and settlement contract.
use super::*;

fn session_binding(session: Option<&str>) -> JsValue {
    session.map(JsValue::from_str).unwrap_or(JsValue::NULL)
}

fn run_hash(owner: &str, session: Option<&str>, model: &ModelConfig) -> Result<String> {
    match session {
        // Preserve the established Session retry identity byte for byte.
        Some(session) => Ok(format!("{owner}:{session}:{}", encode(model)?)),
        None => Ok(format!(
            "account-operation:{}:{}",
            owner.len(),
            encode(&(owner, model))?
        )),
    }
}

impl Store<'_> {
    /// A paid account operation has concurrency and funding, but no invented
    /// conversation, workspace, or archive-maintenance lease.
    pub(crate) async fn admit_operation(
        &self,
        owner: &str,
        run: &str,
        model: &ModelConfig,
    ) -> Result<bool> {
        self.scoped_admit_run(owner, None, run, model).await
    }

    pub(crate) async fn reserve_operation_completion(
        &self,
        owner: &str,
        run: &str,
        event: &str,
        model: &ModelConfig,
        input_tokens_upper_bound: u64,
    ) -> Result<bool> {
        self.scoped_reserve_completion(owner, None, run, event, model, input_tokens_upper_bound)
            .await
    }

    pub(crate) async fn record_operation_completion(
        &self,
        owner: &str,
        run: &str,
        event: &str,
        usage: &crate::provider::Usage,
        provider: Provider,
        model: &str,
    ) -> Result<()> {
        self.scoped_record_completion(owner, None, run, event, usage, provider, model)
            .await
    }
    pub(super) async fn scoped_admit_run(
        &self,
        owner: &str,
        session: Option<&str>,
        run: &str,
        model: &ModelConfig,
    ) -> Result<bool> {
        self.ensure_unblocked(owner).await?;
        crate::allowances::sync(self.env, owner).await?;
        if let Some(session) = session {
            if self.get_session(owner, session).await?.is_none() {
                return Err(fail("session_not_found"));
            }
            let generation = self.cleanup_generation(owner, session).await?;
            crate::cleanup::ensure(self.env, owner, session, generation).await?;
        }
        if model.provider != Provider::Byok {
            self.model_price(model.provider, &model.model)?;
        }
        let amount = self
            .env
            .var("RUN_RESOURCE_RESERVATION_MICROUSD")?
            .to_string()
            .parse::<i64>()
            .map_err(|_| fail("invalid_resource_reservation"))?;
        if amount <= 0 {
            return Err(fail("resource_reservation_required"));
        }
        let limit = self.account_plan(owner).await?.limits().max_concurrent_runs;
        let key = format!("run:{run}");
        let hash = run_hash(owner, session, model)?;
        let reservation = crate::metering::reserve_run(
            &self.env.d1("DB")?,
            owner,
            session,
            run,
            &key,
            &hash,
            amount,
            now_seconds(),
            limit,
        )
        .await?;
        if reservation.state != ReservationState::Reserved {
            return Err(fail("run_already_finalized"));
        }
        Ok(true)
    }

    pub(super) async fn scoped_reserve_completion(
        &self,
        owner: &str,
        session: Option<&str>,
        run: &str,
        event: &str,
        model: &ModelConfig,
        input_tokens_upper_bound: u64,
    ) -> Result<bool> {
        self.ensure_unblocked(owner).await?;
        let db = self.env.d1("DB")?;
        let active: Option<IdRow> = self.statement("SELECT id FROM usage_reservations WHERE id=? AND account_id=? AND session_id IS ? AND state='reserved'", &[run.into(),owner.into(),session_binding(session)])?.first(None).await?;
        if active.is_none() {
            return Err(fail("run_not_admitted"));
        }
        if input_tokens_upper_bound > u64::from(model.context_window) {
            return Err(fail("context_budget_exceeded"));
        }
        let price = if model.provider == Provider::Byok {
            None
        } else {
            Some(self.model_price(model.provider, &model.model)?)
        };
        let amount = if let Some(price) = &price {
            token_cost(input_tokens_upper_bound, price.input_microusd_per_million)?
                .checked_add(token_cost(
                    u64::from(model.max_output_tokens),
                    price.output_microusd_per_million,
                )?)
                .ok_or_else(|| fail("price_overflow"))?
        } else {
            0
        };
        let key = format!("completion:{run}:{event}");
        let id = completion_id(owner, run, event);
        let hash = encode(&CompletionReservation {
            model: model.clone(),
            input_upper_bound: input_tokens_upper_bound,
            price,
        })?;
        let reservation =
            crate::metering::reserve(&db, owner, session, &id, &key, &hash, amount, now_seconds())
                .await?;
        if reservation.state != ReservationState::Reserved {
            return Err(fail("completion_already_finalized"));
        }
        Ok(true)
    }

    pub(super) async fn scoped_record_completion(
        &self,
        owner: &str,
        session: Option<&str>,
        run: &str,
        event: &str,
        usage: &crate::provider::Usage,
        provider: Provider,
        model: &str,
    ) -> Result<()> {
        if !usage.reported {
            return Err(fail("provider_usage_missing"));
        }
        if usage.cached_tokens > usage.prompt_tokens {
            return Err(fail("invalid_provider_usage"));
        }
        let id = completion_id(owner, run, event);
        let row: Option<JsonRow> = self.statement("SELECT request_hash AS payload FROM usage_reservations WHERE id=? AND account_id=? AND session_id IS ?", &[id.as_str().into(),owner.into(),session_binding(session)])?.first(None).await?;
        let reserved: CompletionReservation =
            decode(&row.ok_or_else(|| fail("completion_not_reserved"))?.payload)?;
        if reserved.model.provider != provider
            || reserved.model.model != model
            || usage.prompt_tokens > reserved.input_upper_bound
            || usage.completion_tokens > u64::from(reserved.model.max_output_tokens)
        {
            return Err(fail("completion_reservation_mismatch"));
        }
        let (amount, revision) = if let Some(price) = reserved.price {
            let amount = if provider == Provider::OpenRouter {
                let number = usage
                    .cost
                    .as_ref()
                    .and_then(serde_json::Value::as_number)
                    .ok_or_else(|| fail("openrouter_cost_missing"))?;
                usd_to_microusd(&number.to_string())?
            } else {
                model_cost(
                    usage.prompt_tokens - usage.cached_tokens,
                    usage.cached_tokens,
                    usage.completion_tokens,
                    &price,
                )?
            };
            let revision = if provider == Provider::OpenRouter {
                format!("openrouter:{}", price.revision)
            } else {
                price.revision
            };
            (amount, revision)
        } else {
            (0, "byok".to_owned())
        };
        let charge = Charge {
            resource: Resource::Model,
            quantity: usage
                .prompt_tokens
                .checked_add(usage.completion_tokens)
                .ok_or_else(|| fail("usage_overflow"))?,
            unit: "tokens".into(),
            amount_microusd: MicroUsd::new(amount).map_err(|_| fail("invalid_amount"))?,
            price_version: revision,
            provider_request_id: usage.provider_request_id.clone(),
        };
        let db = self.env.d1("DB")?;
        crate::metering::append(&db, &id, event, &charge, now_seconds()).await?;
        crate::metering::settle(&db, &id, now_seconds()).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_retry_hash_stays_exact_and_account_work_has_a_distinct_scope() {
        let model = ModelConfig::default();
        let old = format!("owner:session:{}", encode(&model).unwrap());
        assert_eq!(run_hash("owner", Some("session"), &model).unwrap(), old);
        let account = run_hash("owner", None, &model).unwrap();
        assert_ne!(account, old);
        assert_ne!(account, run_hash("other", None, &model).unwrap());
        assert_eq!(account, run_hash("owner", None, &model).unwrap());
    }
}
