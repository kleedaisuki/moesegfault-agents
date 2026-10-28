//! Tenant-scoped control-plane storage. Admission is one conditional SQL write.

use agents_auth::billing::{
    self, Charge, MicroUsd, PlanPolicy, ReservationState, Resource, Subscription,
};
use agents_core::{
    AgentConfig, ModelConfig, Plan, Provider, SessionConfiguration, SessionRecord, SessionStatus,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use worker::{D1PreparedStatement, Env, Error, Result, wasm_bindgen::JsValue};

#[path = "model_pricing.rs"]
mod model_pricing;

#[path = "store_auxiliary.rs"]
mod auxiliary;

const VERIFIED_SUBSCRIPTIONS_SQL: &str = "SELECT subscription_json AS payload FROM subscription_snapshots WHERE account_id=? AND product_id='agents'";

pub struct Store<'a> {
    env: &'a Env,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct UsageAccount {
    #[serde(deserialize_with = "money_from_text")]
    pub budget_microusd: i64,
    #[serde(deserialize_with = "money_from_text")]
    pub spent_microusd: i64,
    #[serde(deserialize_with = "money_from_text")]
    pub reserved_microusd: i64,
}

#[derive(Clone, Serialize, Deserialize)]
struct ModelPrice {
    input_microusd_per_million: u64,
    cached_input_microusd_per_million: u64,
    output_microusd_per_million: u64,
    revision: String,
}

impl ModelPrice {
    fn valid(&self) -> bool {
        !self.revision.is_empty()
            && self.cached_input_microusd_per_million <= self.input_microusd_per_million
    }
}

#[derive(Serialize, Deserialize)]
struct CompletionReservation {
    model: ModelConfig,
    input_upper_bound: u64,
    price: Option<ModelPrice>,
}

#[derive(Deserialize)]
struct JsonRow {
    payload: String,
}

#[derive(Deserialize)]
struct IdRow {
    id: String,
}

impl<'a> Store<'a> {
    pub fn new(env: &'a Env) -> Self {
        Self { env }
    }

    fn statement(&self, sql: &str, args: &[JsValue]) -> Result<D1PreparedStatement> {
        self.env.d1("DB")?.prepare(sql).bind(args)
    }

    async fn changed(&self, sql: &str, args: &[JsValue]) -> Result<bool> {
        Ok(self
            .statement(sql, args)?
            .first::<IdRow>(None)
            .await?
            .is_some())
    }

    async fn list<T: DeserializeOwned>(&self, sql: &str, owner: &str) -> Result<Vec<T>> {
        self.statement(sql, &[owner.into()])?
            .all()
            .await?
            .results::<JsonRow>()?
            .into_iter()
            .map(|row| decode(&row.payload))
            .collect()
    }

    async fn get<T: DeserializeOwned>(
        &self,
        sql: &str,
        owner: &str,
        id: &str,
    ) -> Result<Option<T>> {
        self.statement(sql, &[owner.into(), id.into()])?
            .first::<JsonRow>(None)
            .await?
            .map(|row| decode(&row.payload))
            .transpose()
    }

    pub async fn list_agents(&self, owner: &str) -> Result<Vec<AgentConfig>> {
        self.list("SELECT config_json AS payload FROM agents WHERE account_id=? AND deleted_at IS NULL ORDER BY created_at DESC,id LIMIT 1000", owner).await
    }

    pub async fn get_agent(&self, owner: &str, id: &str) -> Result<Option<AgentConfig>> {
        self.get("SELECT config_json AS payload FROM agents WHERE account_id=? AND id=? AND deleted_at IS NULL", owner, id).await
    }

    pub async fn create_agent(&self, agent: &AgentConfig, max_agents: u32) -> Result<bool> {
        self.ensure_unblocked(&agent.owner_id).await?;
        self.changed(
            "INSERT INTO agents(id,account_id,name,config_json,created_at,updated_at) SELECT ?,?,?,?,?,? WHERE (SELECT COUNT(*) FROM agents WHERE account_id=? AND deleted_at IS NULL) < ? RETURNING id",
            &[agent.id.as_str().into(), agent.owner_id.as_str().into(), agent.name.as_str().into(), encode(agent)?.into(), number(agent.created_at_ms)?, number(agent.updated_at_ms)?, agent.owner_id.as_str().into(), max_agents.into()],
        ).await
    }

    pub async fn update_agent(
        &self,
        agent: &AgentConfig,
        configuration: Option<&SessionConfiguration>,
    ) -> Result<bool> {
        self.ensure_unblocked(&agent.owner_id).await?;
        let update = self.statement(
            "UPDATE agents SET name=?,config_json=?,updated_at=? WHERE account_id=? AND id=? AND deleted_at IS NULL AND CAST(json_extract(config_json,'$.revision') AS INTEGER) = ? RETURNING id",
            &[agent.name.as_str().into(), encode(agent)?.into(), number(agent.updated_at_ms)?, agent.owner_id.as_str().into(), agent.id.as_str().into(), number(agent.revision.saturating_sub(1))?],
        )?;
        let Some(configuration) = configuration else {
            return Ok(update.first::<IdRow>(None).await?.is_some());
        };
        let append = self.statement(
            "INSERT INTO agent_configuration_revisions(account_id,agent_id,revision,payload,created_at) SELECT account_id,id,?, ?,? FROM agents WHERE account_id=? AND id=? AND config_json=? ON CONFLICT(account_id,agent_id,revision) DO NOTHING",
            &[number(agent.revision)?,encode(configuration)?.into(),number(agent.updated_at_ms)?,agent.owner_id.as_str().into(),agent.id.as_str().into(),encode(agent)?.into()],
        )?;
        let results = self.env.d1("DB")?.batch(vec![update, append]).await?;
        Ok(!results
            .first()
            .ok_or_else(|| fail("configuration_write_missing"))?
            .results::<IdRow>()?
            .is_empty())
    }

    pub(crate) async fn configuration_revisions(
        &self,
        owner: &str,
        agent: &str,
        after: u64,
        observation: Option<(&crate::resources::ResourceMeter<'_>, &str)>,
    ) -> Result<Vec<SessionConfiguration>> {
        if let Some((meter, event)) = observation {
            meter.require_d1(event).await?;
        }
        let result = self.statement(
            "SELECT payload FROM agent_configuration_revisions WHERE account_id=? AND agent_id=? AND revision>? ORDER BY revision LIMIT 128",
            &[owner.into(),agent.into(),number(after)?],
        )?.all().await?;
        if let Some((meter, event)) = observation {
            meter.record_d1(event, &result).await?;
        }
        result
            .results::<JsonRow>()?
            .into_iter()
            .map(|row| decode(&row.payload))
            .collect()
    }

    pub async fn delete_agent(&self, owner: &str, id: &str, now_ms: u64) -> Result<bool> {
        self.changed("UPDATE agents SET deleted_at=?,updated_at=? WHERE account_id=? AND id=? AND deleted_at IS NULL RETURNING id", &[number(now_ms)?,number(now_ms)?,owner.into(),id.into()]).await
    }

    pub async fn list_sessions(&self, owner: &str) -> Result<Vec<SessionRecord>> {
        self.statement("SELECT config_snapshot_json AS payload FROM sessions WHERE account_id=? ORDER BY created_at DESC,id LIMIT 1000", &[owner.into()])?
            .all().await?.results::<JsonRow>()?.into_iter().map(|row| decode_session(&row.payload)).collect()
    }

    pub async fn list_agent_sessions(
        &self,
        owner: &str,
        agent: &str,
    ) -> Result<Vec<SessionRecord>> {
        self.statement("SELECT config_snapshot_json AS payload FROM sessions WHERE account_id=? AND agent_id=? ORDER BY created_at DESC,id LIMIT 1000", &[owner.into(), agent.into()])?
            .all()
            .await?
            .results::<JsonRow>()?
            .into_iter()
            .map(|row| decode_session(&row.payload))
            .collect()
    }

    pub async fn get_session(&self, owner: &str, id: &str) -> Result<Option<SessionRecord>> {
        self.statement(
            "SELECT config_snapshot_json AS payload FROM sessions WHERE account_id=? AND id=?",
            &[owner.into(), id.into()],
        )?
        .first::<JsonRow>(None)
        .await?
        .map(|row| decode_session(&row.payload))
        .transpose()
    }

    pub async fn create_session(
        &self,
        session: &SessionRecord,
        do_id: &str,
        max_sessions: u32,
    ) -> Result<bool> {
        self.ensure_unblocked(&session.owner_id).await?;
        crate::allowances::sync(self.env, &session.owner_id).await?;
        if session.status == SessionStatus::Closed {
            return Err(Error::RustError("new session cannot be closed".into()));
        }
        self.changed(
            "INSERT INTO sessions(id,account_id,agent_id,do_id,status,config_snapshot_json,instructions_hash,created_at,last_input_at,runtime_profile) SELECT ?,?,?,?,'active',?,?,?,?,? WHERE EXISTS(SELECT 1 FROM agents WHERE id=? AND account_id=? AND deleted_at IS NULL) AND (SELECT COUNT(*) FROM sessions WHERE account_id=? AND status!='closed') < ? RETURNING id",
            &[session.id.as_str().into(), session.owner_id.as_str().into(), session.agent_id.as_str().into(), do_id.into(), encode(session)?.into(), session.pinned.prefix_hash().into(), number(session.created_at_ms)?, number(session.last_input_at_ms)?, session.runtime_profile.name().into(), session.agent_id.as_str().into(), session.owner_id.as_str().into(), session.owner_id.as_str().into(), max_sessions.into()],
        ).await
    }

    /// Only the owning Durable Object writes snapshots. Revision and prefix guards
    /// also reject stale writes after steering or closure.
    pub async fn update_session(&self, session: &SessionRecord) -> Result<()> {
        self.update_session_inner(session, None).await
    }

    /// Preserve actual D1 rows-read/written metadata for run resource accounting.
    pub async fn update_session_observed(
        &self,
        session: &SessionRecord,
        meter: &crate::resources::ResourceMeter<'_>,
        event: &str,
    ) -> Result<()> {
        meter.require_d1(event).await?;
        self.update_session_inner(session, Some((meter, event)))
            .await
    }

    async fn update_session_inner(
        &self,
        session: &SessionRecord,
        observation: Option<(&crate::resources::ResourceMeter<'_>, &str)>,
    ) -> Result<()> {
        let status = if session.status == SessionStatus::Closed {
            "closed"
        } else {
            "active"
        };
        let workspace = session
            .workspace_snapshot
            .as_ref()
            .map(|s| JsValue::from_str(&s.manifest_key))
            .unwrap_or(JsValue::NULL);
        let result = self.statement(
            "UPDATE sessions SET status=?,config_snapshot_json=?,last_input_at=?,workspace_r2_key=?,closed_at=CASE WHEN ?='closed' THEN ? ELSE NULL END WHERE account_id=? AND id=? AND (status!='closed' OR ?='closed') AND instructions_hash=? AND CAST(json_extract(config_snapshot_json,'$.input_revision') AS INTEGER)<=? RETURNING id",
            &[status.into(), encode(session)?.into(), number(session.last_input_at_ms)?, workspace, status.into(), number(worker::Date::now().as_millis())?, session.owner_id.as_str().into(), session.id.as_str().into(), status.into(), session.pinned.prefix_hash().into(), number(session.input_revision)?],
        )?.all().await?;
        if let Some((meter, event)) = observation {
            meter.record_d1(event, &result).await?;
        }
        let changed = !result.results::<IdRow>()?.is_empty();
        if !changed {
            return Err(Error::RustError(
                "session snapshot is stale, closed, or not owned".into(),
            ));
        }
        Ok(())
    }

    pub async fn close_session(&self, owner: &str, id: &str, now_ms: u64) -> Result<bool> {
        self.changed("UPDATE sessions SET status='closed',closed_at=?,config_snapshot_json=json_set(config_snapshot_json,'$.status','closed') WHERE account_id=? AND id=? AND status!='closed' RETURNING id", &[number(now_ms)?,owner.into(),id.into()]).await
    }

    /// Reopening is explicit admission, never a side effect of a stale DO write.
    pub async fn resume_session(&self, owner: &str, id: &str, max_sessions: u32) -> Result<bool> {
        self.ensure_unblocked(owner).await?;
        crate::allowances::sync(self.env, owner).await?;
        let now = number(worker::Date::now().as_millis())?;
        // D1 binds JS numbers as REAL; JSON has no column affinity to restore integers.
        self.changed("UPDATE sessions SET status='active',closed_at=NULL,last_input_at=CAST(? AS INTEGER),config_snapshot_json=json_set(config_snapshot_json,'$.status','idle','$.last_input_at_ms',CAST(? AS INTEGER),'$.input_revision',CAST(json_extract(config_snapshot_json,'$.input_revision') AS INTEGER)+1) WHERE account_id=? AND id=? AND status='closed' AND (SELECT COUNT(*) FROM sessions WHERE account_id=? AND status!='closed') < ? RETURNING id", &[now.clone(),now,owner.into(),id.into(),owner.into(),max_sessions.into()]).await
    }

    pub async fn account(&self, owner: &str) -> Result<Option<Account>> {
        self.statement(
            "SELECT id,created_at,updated_at FROM accounts WHERE id=?",
            &[owner.into()],
        )?
        .first(None)
        .await
    }

    pub async fn usage(&self, owner: &str) -> Result<UsageAccount> {
        Ok(self.statement("SELECT CAST(budget_microusd AS TEXT) AS budget_microusd,CAST(spent_microusd AS TEXT) AS spent_microusd,CAST(reserved_microusd AS TEXT) AS reserved_microusd FROM usage_accounts WHERE account_id=?", &[owner.into()])?.first(None).await?.unwrap_or_default())
    }

    /// Billing grants expire at their own period end, not at the refresh hint.
    /// The current issuer has no subscription cancellation/revocation surface.
    pub async fn subscriptions(&self, owner: &str) -> Result<Vec<Subscription>> {
        let rows = self
            .statement(VERIFIED_SUBSCRIPTIONS_SQL, &[owner.into()])?
            .all()
            .await?
            .results::<JsonRow>()?;
        Ok(rows
            .into_iter()
            .map(|r| decode::<Vec<Subscription>>(&r.payload))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect())
    }

    pub async fn account_plan(&self, owner: &str) -> Result<Plan> {
        let now = now_seconds();
        let subscriptions = self.subscriptions(owner).await?;
        let policy = PlanPolicy::default();
        Ok(match billing::resolve_tier(&subscriptions, now, &policy) {
            billing::BillingTier::Free => Plan::Free,
            billing::BillingTier::Go => Plan::Go,
            billing::BillingTier::Plus => Plan::Plus,
            billing::BillingTier::Pro => Plan::Pro,
        })
    }

    /// Resource credit is held for the run; individual model requests reserve
    /// separately, avoiding a worst-case 32-turn prepayment for a one-turn task.
    pub async fn admit_run(
        &self,
        owner: &str,
        session: &str,
        run: &str,
        model: &ModelConfig,
    ) -> Result<bool> {
        self.scoped_admit_run(owner, Some(session), run, model)
            .await
    }

    pub async fn cleanup_generation(&self, owner: &str, session: &str) -> Result<u64> {
        #[derive(Deserialize)]
        struct Archive {
            conversation_r2_key: Option<String>,
        }
        let row: Archive = self
            .statement(
                "SELECT conversation_r2_key FROM sessions WHERE account_id=? AND id=?",
                &[owner.into(), session.into()],
            )?
            .first(None)
            .await?
            .ok_or_else(|| fail("session_not_found"))?;
        archive_generation(owner, session, row.conversation_r2_key.as_deref())
    }

    pub async fn reserve_completion(
        &self,
        owner: &str,
        session: &str,
        run: &str,
        event: &str,
        model: &ModelConfig,
        input_tokens_upper_bound: u64,
    ) -> Result<bool> {
        self.scoped_reserve_completion(
            owner,
            Some(session),
            run,
            event,
            model,
            input_tokens_upper_bound,
        )
        .await
    }

    pub async fn record_completion(
        &self,
        owner: &str,
        session: &str,
        run: &str,
        event: &str,
        usage: &crate::provider::Usage,
        provider: Provider,
        model: &str,
    ) -> Result<()> {
        self.scoped_record_completion(owner, Some(session), run, event, usage, provider, model)
            .await
    }

    pub async fn finish_run(&self, owner: &str, run: &str) -> Result<()> {
        let owned: Option<IdRow> = self
            .statement(
                "SELECT id FROM usage_reservations WHERE account_id=? AND id=?",
                &[owner.into(), run.into()],
            )?
            .first(None)
            .await?;
        if owned.is_none() {
            return Err(fail("run_not_found"));
        }
        crate::metering::settle(&self.env.d1("DB")?, run, now_seconds()).await?;
        Ok(())
    }

    async fn ensure_unblocked(&self, owner: &str) -> Result<()> {
        let blocked = self
            .statement(
                "SELECT account_id AS id FROM resource_account_blocks WHERE account_id=?",
                &[owner.into()],
            )?
            .first::<IdRow>(None)
            .await?
            .is_some();
        if blocked {
            return Err(fail("resource_account_blocked"));
        }
        Ok(())
    }

    pub fn available_platform_models(&self, models: &[ModelConfig]) -> Vec<ModelConfig> {
        let pricing = self
            .env
            .var("MODEL_PRICING_JSON")
            .ok()
            .map(|value| value.to_string());
        platform_models(models, pricing.as_deref())
    }

    fn model_price(&self, provider: Provider, model: &str) -> Result<ModelPrice> {
        self.model_price_at(provider, model, now_seconds())
    }

    fn model_price_at(&self, provider: Provider, model: &str, now: i64) -> Result<ModelPrice> {
        let registry: std::collections::BTreeMap<String, model_pricing::PriceDefinition> =
            decode(&self.env.var("MODEL_PRICING_JSON")?.to_string())?;
        let key = platform_price_key(provider, model)
            .ok_or_else(|| fail("byok_has_no_platform_model_price"))?;
        let price = registry
            .get(&key)
            .cloned()
            .ok_or_else(|| fail("model_price_unconfigured"))?;
        if !price.supports_key(&key) {
            return Err(fail("invalid_model_price_provider"));
        }
        price.select(now).map_err(fail)
    }
}

fn platform_price_key(provider: Provider, model: &str) -> Option<String> {
    let provider = match provider {
        Provider::OpenRouter => "openrouter",
        Provider::DeepSeek => "deepseek",
        Provider::Byok => return None,
    };
    Some(format!("{provider}/{model}"))
}

fn platform_models(models: &[ModelConfig], pricing: Option<&str>) -> Vec<ModelConfig> {
    let registry = pricing
        .and_then(|value| {
            serde_json::from_str::<
                    std::collections::BTreeMap<String, model_pricing::PriceDefinition>,
                >(value)
                .ok()
        })
        .unwrap_or_default();
    let priced: std::collections::HashSet<&str> = registry
        .iter()
        .filter(|(key, price)| price.valid() && price.supports_key(key))
        .map(|(key, _)| key.as_str())
        .collect();
    models
        .iter()
        .filter(|model| {
            platform_price_key(model.provider, &model.model)
                .is_some_and(|key| priced.contains(key.as_str()))
        })
        .cloned()
        .collect()
}

fn now_seconds() -> i64 {
    (worker::Date::now().as_millis() / 1000) as i64
}
fn archive_generation(owner: &str, session: &str, key: Option<&str>) -> Result<u64> {
    let Some(key) = key else { return Ok(0) };
    let prefix = format!("accounts/{owner}/sessions/{session}/archives/");
    key.strip_prefix(&prefix)
        .and_then(|v| v.strip_suffix(".json"))
        .and_then(|v| v.parse::<u64>().ok())
        .and_then(|v| v.checked_add(1))
        .ok_or_else(|| fail("invalid_archive_generation"))
}
fn money_from_text<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<i64, D::Error> {
    let value = String::deserialize(deserializer)?;
    let amount = value.parse::<i64>().map_err(serde::de::Error::custom)?;
    if amount < 0 {
        return Err(serde::de::Error::custom("negative money"));
    }
    Ok(amount)
}

/// Convert the provider's decimal USD amount without a binary floating-point
/// multiplication. Positive sub-micro-USD charges round upward once per event.
pub(crate) fn usd_to_microusd(decimal: &str) -> Result<i64> {
    let (mantissa, exponent) = decimal.split_once(['e', 'E']).unwrap_or((decimal, "0"));
    let exponent = exponent
        .parse::<i64>()
        .map_err(|_| fail("invalid_provider_cost"))?;
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if whole.is_empty()
        || !whole
            .bytes()
            .chain(fraction.bytes())
            .all(|byte| byte.is_ascii_digit())
    {
        return Err(fail("invalid_provider_cost"));
    }
    let digits = format!("{whole}{fraction}");
    let digits = digits.trim_start_matches('0');
    if digits.is_empty() {
        return Ok(0);
    }
    let position = i64::try_from(digits.len())
        .map_err(|_| fail("price_overflow"))?
        .checked_add(exponent)
        .and_then(|n| n.checked_sub(fraction.len() as i64))
        .and_then(|n| n.checked_add(6))
        .ok_or_else(|| fail("price_overflow"))?;
    if position <= 0 {
        return Ok(1);
    }
    if position > 19 {
        return Err(fail("price_overflow"));
    }
    let position = position as usize;
    let (integer, round_up) = if position < digits.len() {
        (
            digits[..position].to_owned(),
            digits[position..].bytes().any(|b| b != b'0'),
        )
    } else {
        (
            format!("{digits}{}", "0".repeat(position - digits.len())),
            false,
        )
    };
    let amount = integer.parse::<i64>().map_err(|_| fail("price_overflow"))?;
    amount
        .checked_add(i64::from(round_up))
        .ok_or_else(|| fail("price_overflow"))
}
fn fail(code: &str) -> Error {
    Error::RustError(code.into())
}
fn token_cost(tokens: u64, rate: u64) -> Result<i64> {
    MicroUsd::price(tokens, rate, 1_000_000)
        .map(|n| n.get())
        .map_err(|_| fail("price_overflow"))
}
fn model_cost(input: u64, cached: u64, output: u64, price: &ModelPrice) -> Result<i64> {
    let weighted = u128::from(input) * u128::from(price.input_microusd_per_million);
    let weighted = weighted
        .checked_add(u128::from(cached) * u128::from(price.cached_input_microusd_per_million))
        .and_then(|n| {
            n.checked_add(u128::from(output) * u128::from(price.output_microusd_per_million))
        })
        .ok_or_else(|| fail("price_overflow"))?;
    i64::try_from(weighted.div_ceil(1_000_000)).map_err(|_| fail("price_overflow"))
}
fn completion_id(owner: &str, run: &str, event: &str) -> String {
    use sha2::{Digest, Sha256};
    format!(
        "completion_{:x}",
        Sha256::digest(encode(&(owner, run, event)).expect("strings serialize"))
    )
}

fn encode<T: Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value).map_err(|error| Error::RustError(error.to_string()))
}

fn decode<T: DeserializeOwned>(value: &str) -> Result<T> {
    serde_json::from_str(value).map_err(|error| Error::RustError(error.to_string()))
}

fn decode_session(value: &str) -> Result<SessionRecord> {
    if let Ok(session) = serde_json::from_str::<SessionRecord>(value) {
        return Ok(session);
    }
    let mut value: serde_json::Value = serde_json::from_str(value)?;
    if let Some(serde_json::Value::Number(number)) = value.get("last_input_at_ms") {
        if number.as_u64().is_none() {
            value["last_input_at_ms"] = legacy_timestamp(&number.to_string())?.into();
        }
    }
    serde_json::from_value(value).map_err(|error| Error::RustError(error.to_string()))
}

/// Historical D1 JSON writes bound integral milliseconds as REAL. Accept only
/// exact decimal integers representable by that JavaScript binding, not rounded f64s.
fn legacy_timestamp(value: &str) -> Result<u64> {
    let (mantissa, exponent) = value.split_once(['e', 'E']).unwrap_or((value, "0"));
    let exponent = exponent
        .parse::<i64>()
        .map_err(|_| fail("invalid_session_timestamp"))?;
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if whole.is_empty()
        || !whole
            .bytes()
            .chain(fraction.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return Err(fail("invalid_session_timestamp"));
    }
    let digits = format!("{whole}{fraction}");
    let digits = digits.trim_start_matches('0');
    if digits.is_empty() {
        return Ok(0);
    }
    let position = (digits.len() as i64)
        .checked_add(exponent)
        .and_then(|n| n.checked_sub(fraction.len() as i64))
        .ok_or_else(|| fail("invalid_session_timestamp"))?;
    if !(1..=16).contains(&position) {
        return Err(fail("invalid_session_timestamp"));
    }
    let position = position as usize;
    let integer = if position < digits.len() {
        if digits[position..].bytes().any(|b| b != b'0') {
            return Err(fail("invalid_session_timestamp"));
        }
        digits[..position].to_owned()
    } else {
        format!("{digits}{}", "0".repeat(position - digits.len()))
    };
    let timestamp = integer
        .parse::<u64>()
        .map_err(|_| fail("invalid_session_timestamp"))?;
    if timestamp > 9_007_199_254_740_991 {
        return Err(fail("invalid_session_timestamp"));
    }
    Ok(timestamp)
}

fn number(value: u64) -> Result<JsValue> {
    if value > 9_007_199_254_740_991 {
        return Err(Error::RustError(
            "integer exceeds D1 JavaScript binding precision".into(),
        ));
    }
    Ok(JsValue::from_f64(value as f64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_agent_json_keeps_hosted_routes_without_a_second_config_column() {
        let input: agents_core::CreateAgent = serde_json::from_value(serde_json::json!({
            "name":"Hosted services",
            "tool_services":{
                "image_generation":{"provider":"openai_images","credential_id":"cred_image"},
                "web_search":{"provider":"platform_deepseek"},
            },
        }))
        .unwrap();
        input.validate().unwrap();
        let agent = input.into_config("agent".into(), "owner".into(), 123);
        // Agent CRUD stores this full config_json through the same codec.
        let stored = encode(&agent).unwrap();
        let loaded: AgentConfig = decode(&stored).unwrap();
        assert_eq!(loaded, agent);
        assert_eq!(loaded.tool_services, agent.tool_services);
        let legacy: AgentConfig = serde_json::from_value(serde_json::json!({
            "id":"old","owner_id":"owner","name":"Old agent","revision":1,
            "model":ModelConfig::default(),"instructions":"","tools":["time"],"skills":[],
            "compaction_percent":80,"workspace":false,"created_at_ms":1,"updated_at_ms":1,
        }))
        .unwrap();
        assert!(legacy.tool_services.is_empty());
        assert!(!encode(&legacy).unwrap().contains("tool_services"));
    }
    use proptest::prelude::*;

    #[test]
    fn platform_catalog_requires_a_valid_price_for_the_exact_provider_and_model() {
        let openrouter = ModelConfig {
            model: "test-model".into(),
            ..ModelConfig::default()
        };
        let direct = ModelConfig {
            provider: Provider::DeepSeek,
            ..openrouter.clone()
        };
        let byok = ModelConfig {
            provider: Provider::Byok,
            ..openrouter.clone()
        };
        let unpriced = ModelConfig {
            model: "unpriced".into(),
            ..openrouter.clone()
        };
        let models = vec![openrouter.clone(), direct.clone(), byok, unpriced];
        let pricing = serde_json::json!({
            "openrouter/test-model":{"input_microusd_per_million":10,"cached_input_microusd_per_million":1,"output_microusd_per_million":20,"revision":"v1"},
            "byok/test-model":{"input_microusd_per_million":10,"cached_input_microusd_per_million":1,"output_microusd_per_million":20,"revision":"v1"},
            "deepseek/unregistered":{"input_microusd_per_million":10,"cached_input_microusd_per_million":1,"output_microusd_per_million":20,"revision":"v1"}
        }).to_string();
        assert_eq!(platform_models(&models, Some(&pricing)), vec![openrouter]);
        assert_eq!(models.len(), 4);
        let direct_pricing = serde_json::json!({"deepseek/test-model":{
            "input_microusd_per_million":10,"cached_input_microusd_per_million":1,"output_microusd_per_million":20,"revision":"v1"
        }}).to_string();
        assert_eq!(
            platform_models(&models, Some(&direct_pricing)),
            vec![direct]
        );
    }

    #[test]
    fn platform_catalog_uses_the_same_price_validity_as_inference() {
        let models = [ModelConfig {
            model: "test-model".into(),
            ..ModelConfig::default()
        }];
        for (input, cached, revision, valid) in [
            (10, 1, "v1", true),
            (0, 0, "free", true),
            (10, 11, "v1", false),
            (10, 1, "", false),
        ] {
            let price = ModelPrice {
                input_microusd_per_million: input,
                cached_input_microusd_per_million: cached,
                output_microusd_per_million: 20,
                revision: revision.into(),
            };
            assert_eq!(price.valid(), valid);
            let pricing = serde_json::json!({"openrouter/test-model":price}).to_string();
            assert_eq!(
                platform_models(&models, Some(&pricing)).len(),
                usize::from(valid)
            );
        }
    }

    #[test]
    fn missing_or_malformed_pricing_never_advertises_platform_support() {
        let models = [ModelConfig::default()];
        for pricing in [
            None,
            Some(""),
            Some("{}"),
            Some("[]"),
            Some("{\"broken\":{}}"),
            Some("private invalid config"),
        ] {
            assert!(platform_models(&models, pricing).is_empty());
        }
    }

    fn session_json(timestamp: &str) -> String {
        let agent =
            serde_json::from_value::<agents_core::CreateAgent>(serde_json::json!({"name":"test"}))
                .unwrap()
                .into_config("a".into(), "owner".into(), 1);
        let record = SessionRecord {
            id: "s".into(),
            owner_id: "owner".into(),
            agent_id: "a".into(),
            status: SessionStatus::Idle,
            created_at_ms: 1,
            last_input_at_ms: 0,
            input_revision: 1,
            pinned: agents_core::PinnedContext::resolve(&agent, vec![], vec![]),
            configuration: None,
            context_layout: None,
            workspace_snapshot: None,
            runtime_profile: agents_core::RuntimeProfile::Basic1g,
        };
        encode(&record).unwrap().replace(
            "\"last_input_at_ms\":0",
            &format!("\"last_input_at_ms\":{timestamp}"),
        )
    }

    #[test]
    fn legacy_session_timestamp_normalizes_only_exact_safe_integers() {
        for (literal, expected) in [
            ("1792776157970.0", 1_792_776_157_970),
            ("1.792776157970e12", 1_792_776_157_970),
            ("9007199254740991.0", 9_007_199_254_740_991),
            ("0.0000", 0),
        ] {
            let session = decode_session(&session_json(literal)).unwrap();
            assert_eq!(session.last_input_at_ms, expected, "{literal}");
            let canonical = serde_json::to_value(session).unwrap();
            assert_eq!(canonical["last_input_at_ms"].as_u64(), Some(expected));
        }
        for literal in [
            "-1.0",
            "0.1",
            "1792776157970.0000000001",
            "9007199254740990.5",
            "9007199254740992.0",
            "1e309",
            "NaN",
            "null",
            "\"1792776157970.0\"",
        ] {
            assert!(decode_session(&session_json(literal)).is_err(), "{literal}");
        }
    }

    #[test]
    fn session_compatibility_does_not_coerce_other_fields_or_generic_json() {
        let legacy = session_json("1792776157970.0");
        for (before, after) in [
            ("\"created_at_ms\":1", "\"created_at_ms\":1.0"),
            ("\"input_revision\":1", "\"input_revision\":1.0"),
            ("\"compaction_percent\":80", "\"compaction_percent\":80.0"),
        ] {
            assert!(legacy.contains(before));
            assert!(decode_session(&legacy.replace(before, after)).is_err());
        }
        assert!(decode::<SessionRecord>(&legacy).is_err());
        assert_eq!(
            decode_session(&session_json("18446744073709551615"))
                .unwrap()
                .last_input_at_ms,
            u64::MAX
        );
    }

    proptest! {
        #[test]
        fn every_legacy_safe_integral_timestamp_roundtrips(timestamp in 0u64..=9_007_199_254_740_991) {
            let session = decode_session(&session_json(&format!("{timestamp}.000000"))).unwrap();
            prop_assert_eq!(session.last_input_at_ms,timestamp);
        }

        #[test]
        fn fractional_timestamps_below_float_precision_are_never_rounded(timestamp in 0u64..=9_007_199_254_740_991) {
            let fractional = format!("{timestamp}.0000000001");
            prop_assert!(legacy_timestamp(&fractional).is_err());
        }
    }

    #[test]
    fn cleanup_generation_follows_archives_not_each_append_or_run() {
        assert_eq!(archive_generation("u", "s", None).unwrap(), 0);
        assert_eq!(
            archive_generation("u", "s", Some("accounts/u/sessions/s/archives/17.json")).unwrap(),
            18
        );
        assert!(
            archive_generation("u", "s", Some("accounts/other/sessions/s/archives/17.json"))
                .is_err()
        );
        assert!(
            archive_generation(
                "u",
                "s",
                Some("accounts/u/sessions/s/archives/18446744073709551615.json")
            )
            .is_err()
        );
    }

    #[test]
    fn public_tier_limits_match_verified_billing_policy() {
        let policy = PlanPolicy::default();
        for (plan, limits) in [
            (Plan::Free, policy.free),
            (Plan::Go, policy.go),
            (Plan::Plus, policy.plus),
            (Plan::Pro, policy.paid),
        ] {
            let actual = plan.limits();
            assert_eq!(actual.max_agents, limits.agent_configs);
            assert_eq!(actual.max_concurrent_sessions, limits.concurrent_sessions);
            assert_eq!(actual.max_concurrent_runs, limits.concurrent_runs);
        }
    }

    #[test]
    fn upstream_usd_decimal_preserves_exact_amount_and_rounds_up() {
        let upstream: serde_json::Value =
            serde_json::from_str("0.00000100000000000000001").unwrap();
        assert_eq!(
            usd_to_microusd(&upstream.as_number().unwrap().to_string()).unwrap(),
            2
        );
        for (value, expected) in [
            ("0", 0),
            ("0.000001", 1),
            ("0.000001000001", 2),
            ("0.00000000001", 1),
            ("1.234567", 1_234_567),
            ("1.2345671", 1_234_568),
            ("1e-8", 1),
            ("4.2E-5", 42),
            ("12e2", 1_200_000_000),
            ("9223372036854.775807", i64::MAX),
            ("0000001.000000", 1_000_000),
        ] {
            assert_eq!(usd_to_microusd(value).unwrap(), expected, "{value}");
        }
        for value in [
            "-1",
            "NaN",
            "inf",
            "9223372036854.775808",
            "1e100",
            "1e999999999999999999999",
        ] {
            assert!(usd_to_microusd(value).is_err(), "{value}");
        }
    }

    #[test]
    fn d1_money_survives_javascript_integer_precision_boundary() {
        let usage: UsageAccount = serde_json::from_str(r#"{"budget_microusd":"9223372036854775807","spent_microusd":"9007199254740993","reserved_microusd":"0"}"#).unwrap();
        assert_eq!(usage.budget_microusd, i64::MAX);
        assert_eq!(usage.spent_microusd, 9_007_199_254_740_993);
    }

    #[test]
    fn round_once_per_model_event_not_once_per_token_category() {
        let price = ModelPrice {
            input_microusd_per_million: 1,
            cached_input_microusd_per_million: 1,
            output_microusd_per_million: 1,
            revision: "v1".into(),
        };
        assert_eq!(model_cost(1, 1, 1, &price).unwrap(), 1);
    }

    #[test]
    fn cached_tokens_and_output_stay_within_reserved_upper_bound() {
        let price = ModelPrice {
            input_microusd_per_million: 280_000,
            cached_input_microusd_per_million: 28_000,
            output_microusd_per_million: 420_000,
            revision: "test".into(),
        };
        for input in 0..100 {
            let reserved = token_cost(input, price.input_microusd_per_million).unwrap()
                + token_cost(50, price.output_microusd_per_million).unwrap();
            for cached in 0..=input {
                assert!(model_cost(input - cached, cached, 50, &price).unwrap() <= reserved);
            }
        }
    }
}

// Return one consistent database snapshot. Money becomes text inside SQLite before
// crossing D1's JavaScript boundary, where INTEGER values may exceed 2^53.
const USAGE_SUMMARY_SQL: &str = r#"
SELECT json_object(
  'wallet', json_object(
    'budget_microusd', CAST(COALESCE(wallet.budget_microusd, 0) AS TEXT),
    'spent_microusd', CAST(COALESCE(wallet.spent_microusd, 0) AS TEXT),
    'reserved_microusd', CAST(COALESCE(wallet.reserved_microusd, 0) AS TEXT),
    'available_microusd', CAST(COALESCE(wallet.budget_microusd - wallet.spent_microusd - wallet.reserved_microusd, 0) AS TEXT)
  ),
  'by_resource', json((
    SELECT json_group_array(json_object('resource', resource, 'cost_microusd', CAST(cost_microusd AS TEXT)))
    FROM (
      SELECT ledger.resource, SUM(ledger.amount_microusd) AS cost_microusd
      FROM usage_ledger AS ledger
      JOIN usage_reservations AS reservation ON reservation.id = ledger.reservation_id
      WHERE reservation.account_id = tenant.account_id
      GROUP BY ledger.resource
      ORDER BY ledger.resource
    )
  ))
) AS payload
FROM (SELECT ? AS account_id) AS tenant
LEFT JOIN usage_accounts AS wallet ON wallet.account_id = tenant.account_id
"#;

impl Store<'_> {
    pub async fn usage_summary(&self, owner: &str) -> Result<serde_json::Value> {
        let row = self
            .statement(USAGE_SUMMARY_SQL, &[owner.into()])?
            .first::<JsonRow>(None)
            .await?
            .ok_or_else(|| fail("usage_summary_unavailable"))?;
        decode(&row.payload)
    }
}
