//! Two accounting round trips per measured D1 query, independent of row counts.
//! Conversation and Vector Store management opt in; other resource callers stay unchanged.
use super::*;

const DECLARE_SQL: &str = "INSERT INTO resource_observations(run_id,account_id,session_id,event_key,resource,unit,quantity,price_version,observed_at,status) SELECT ?1,?2,?3,?4,'d1',?5,NULL,?6,NULL,'pending' WHERE EXISTS(SELECT 1 FROM usage_reservations WHERE id=?1 AND account_id=?2 AND session_id IS ?3 AND state='reserved') ON CONFLICT(run_id,event_key) DO NOTHING";
const READ_SQL: &str = "SELECT resource,unit,quantity,price_version,status FROM resource_observations WHERE run_id=?1 AND account_id=?2 AND session_id IS ?3 AND event_key=?4";
const APPEND_SQL: &str = "INSERT INTO usage_ledger(id,reservation_id,event_key,resource,quantity,unit,amount_microusd,price_version,provider_request_id,created_at) SELECT ?9,?1,?8,'d1',CAST(?7 AS INTEGER),?5,CAST(?10 AS INTEGER),?6,NULL,CAST(?11 AS INTEGER) WHERE EXISTS(SELECT 1 FROM resource_observations WHERE run_id=?1 AND account_id=?2 AND session_id IS ?3 AND event_key=?4 AND resource='d1' AND unit=?5 AND price_version=?6 AND status='pending') AND NOT EXISTS(SELECT 1 FROM usage_ledger WHERE reservation_id=?1 AND event_key=?8)";
const OBSERVE_SQL: &str = "UPDATE resource_observations SET quantity=?7,status='observed',observed_at=CAST(?11 AS INTEGER) WHERE run_id=?1 AND account_id=?2 AND session_id IS ?3 AND event_key=?4 AND resource='d1' AND unit=?5 AND price_version=?6 AND status='pending' AND EXISTS(SELECT 1 FROM usage_ledger WHERE reservation_id=?1 AND event_key=?8 AND resource='d1' AND quantity=CAST(?7 AS INTEGER) AND unit=?5 AND amount_microusd=CAST(?10 AS INTEGER) AND price_version=?6 AND provider_request_id IS NULL)";
const VERIFY_SQL: &str = "SELECT o.resource,o.unit,o.quantity,o.price_version,o.status,l.resource AS ledger_resource,CAST(l.quantity AS TEXT) AS ledger_quantity,l.unit AS ledger_unit,CAST(l.amount_microusd AS TEXT) AS ledger_amount,l.price_version AS ledger_price,l.provider_request_id AS ledger_provider FROM resource_observations o LEFT JOIN usage_ledger l ON l.reservation_id=o.run_id AND l.event_key=?8 WHERE o.run_id=?1 AND o.account_id=?2 AND o.session_id IS ?3 AND o.event_key=?4";

#[derive(Deserialize)]
struct Recorded {
    #[serde(flatten)]
    observation: Observation,
    ledger_resource: Option<String>,
    ledger_quantity: Option<String>,
    ledger_unit: Option<String>,
    ledger_amount: Option<String>,
    ledger_price: Option<String>,
    ledger_provider: Option<String>,
}

impl ResourceMeter<'_> {
    pub(crate) async fn require_d1_batched(&self, event: &str) -> Result<()> {
        let started = crate::util::now_ms();
        let db = self.db()?;
        let mut statements = Vec::with_capacity(4);
        for (suffix, unit) in [("read", "rows_read"), ("write", "rows_written")] {
            let key = format!("{event}:{suffix}");
            if key.is_empty() || key.len() > 256 {
                return Err(fail("invalid_resource_event"));
            }
            let tariff = self.pricing.tariff(&Resource::D1, unit)?;
            let args = [
                text(self.run),
                text(self.owner),
                self.session_binding(),
                text(&key),
                text(unit),
                text(&tariff.price_version),
            ];
            statements.push(db.prepare(DECLARE_SQL).bind(&args)?);
            statements.push(db.prepare(READ_SQL).bind(&args[..4])?);
        }
        let results = db.batch(statements).await?;
        if results.len() != 4 {
            return Err(fail("resource_observation_missing"));
        }
        for (index, unit) in [(1, "rows_read"), (3, "rows_written")] {
            let rows = results[index].results::<Observation>()?;
            let row = rows
                .first()
                .ok_or_else(|| fail("resource_observation_missing"))?;
            if row.resource != "d1"
                || row.unit != unit
                || row.price_version != self.pricing.tariff(&Resource::D1, unit)?.price_version
            {
                return Err(fail("resource_observation_conflict"));
            }
        }
        crate::util::emit(
            "resource.d1.batch",
            serde_json::json!({"operation_id":self.run,"session_id":self.session,
            "event_key":event,"phase":"funded","duration_ms":crate::util::now_ms().saturating_sub(started)}),
        );
        Ok(())
    }

    pub(crate) async fn record_d1_batched(&self, event: &str, result: &D1Result) -> Result<()> {
        let started = crate::util::now_ms();
        let meta = result
            .meta()?
            .ok_or_else(|| fail("d1_usage_metadata_missing"))?;
        let read = meta
            .rows_read
            .ok_or_else(|| fail("d1_usage_metadata_missing"))? as u64;
        let written = meta
            .rows_written
            .ok_or_else(|| fail("d1_usage_metadata_missing"))? as u64;
        let db = self.db()?;
        let now = crate::util::now_ms().to_string();
        let mut statements = Vec::with_capacity(6);
        let mut expected = Vec::with_capacity(2);
        for (suffix, unit, quantity) in [
            ("read", "rows_read", read),
            ("write", "rows_written", written),
        ] {
            let key = format!("{event}:{suffix}");
            if key.is_empty() || key.len() > 256 || quantity > i64::MAX as u64 {
                return Err(fail("invalid_resource_event"));
            }
            let tariff = self.pricing.tariff(&Resource::D1, unit)?;
            let charge = tariff
                .charge(quantity)
                .map_err(|_| fail("invalid_resource_tariff"))?;
            let quantity = quantity.to_string();
            let amount = charge.amount_microusd.get().to_string();
            let ledger_key = format!("resource:{key}");
            let args = [
                text(self.run),
                text(self.owner),
                self.session_binding(),
                text(&key),
                text(unit),
                text(&tariff.price_version),
                text(&quantity),
                text(&ledger_key),
                text(&uuid::Uuid::new_v4().to_string()),
                text(&amount),
                text(&now),
            ];
            for sql in [APPEND_SQL, OBSERVE_SQL, VERIFY_SQL] {
                statements.push(db.prepare(sql).bind(if sql == VERIFY_SQL {
                    &args[..8]
                } else {
                    &args
                })?);
            }
            expected.push((unit, quantity, amount, tariff.price_version.as_str()));
        }
        // The ledger insert, exact-charge check and observed transition share a
        // D1 transaction. Lost batch acknowledgement is a safe same-event retry.
        let results = db.batch(statements).await?;
        if results.len() != 6 {
            return Err(fail("resource_observation_missing"));
        }
        for (index, (unit, quantity, amount, price)) in expected.into_iter().enumerate() {
            let rows = results[index * 3 + 2].results::<Recorded>()?;
            let row = rows
                .first()
                .ok_or_else(|| fail("resource_observation_missing"))?;
            let observed = &row.observation;
            if observed.resource != "d1"
                || observed.unit != unit
                || observed.price_version != price
                || observed
                    .quantity
                    .as_deref()
                    .is_some_and(|previous| previous != quantity)
            {
                return Err(fail("resource_observation_conflict"));
            }
            if row.ledger_resource.as_deref() != Some("d1")
                || row.ledger_quantity.as_deref() != Some(quantity.as_str())
                || row.ledger_unit.as_deref() != Some(unit)
                || row.ledger_amount.as_deref() != Some(amount.as_str())
                || row.ledger_price.as_deref() != Some(price)
                || row.ledger_provider.is_some()
            {
                return Err(fail("idempotency_conflict"));
            }
            if observed.status != "observed" {
                return Err(fail("resource_observation_pending"));
            }
        }
        crate::util::emit(
            "resource.d1.batch",
            serde_json::json!({"operation_id":self.run,"session_id":self.session,
            "event_key":event,"phase":"observed","rows_read":read,"rows_written":written,
            "duration_ms":crate::util::now_ms().saturating_sub(started)}),
        );
        Ok(())
    }
}
