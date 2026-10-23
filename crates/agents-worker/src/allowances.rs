//! Verified entitlement funding. SQL owns replay, expiry, and concurrent grant arithmetic.
use agents_auth::billing::{self, PlanPolicy};
use serde::Deserialize;
use wasm_bindgen::JsValue;
use worker::{Env, Error, Result};

#[derive(Deserialize)]
struct Window {
    period_start: i64,
    period_end: i64,
    source: String,
}

// A single serialized INSERT computes the increment from the current high-water mark.
// Different overlapping subscription IDs cannot manufacture a second full allowance.
const GRANT: &str = "INSERT INTO allowance_grants(account_id,period_start,period_end,source,ceiling_microusd,credited_microusd,created_at,subsidy_limit_microusd)
SELECT ?1,CAST(?2 AS INTEGER),CAST(?3 AS INTEGER),?4,CAST(?5 AS INTEGER),
CASE WHEN EXISTS(SELECT 1 FROM allowance_windows WHERE account_id=?1 AND period_start=CAST(?2 AS INTEGER))
THEN (CAST(?5 AS INTEGER)-(SELECT ceiling_microusd FROM allowance_windows WHERE account_id=?1 AND period_start=CAST(?2 AS INTEGER)))
 * (CAST(?3 AS INTEGER)-CAST(?6 AS INTEGER))/(CAST(?3 AS INTEGER)-CAST(?2 AS INTEGER))
ELSE MAX(0,CAST(?5 AS INTEGER)-COALESCE((SELECT SUM(credited_microusd) FROM allowance_grants
 WHERE account_id=?1 AND period_start<CAST(?3 AS INTEGER) AND period_end>CAST(?2 AS INTEGER)),0)) END,CAST(?6 AS INTEGER),CAST(?8 AS INTEGER)
WHERE NOT EXISTS(SELECT 1 FROM allowance_windows WHERE account_id=?1 AND period_start=CAST(?2 AS INTEGER) AND ceiling_microusd>=CAST(?5 AS INTEGER))
AND COALESCE((SELECT MAX(period_start) FROM allowance_windows WHERE account_id=?1),-1)=CAST(?7 AS INTEGER)";

/// Call on authentication and immediately before run admission. No user-supplied amounts.
/// A missing/expired grant selects Free; upstream refresh freshness never expires a paid grant.
pub async fn sync(env: &Env, account: &str) -> Result<()> {
    let now = (js_sys::Date::now() / 1000.0) as i64;
    let db = env.d1("DB")?;
    let subscriptions = crate::store::Store::new(env).subscriptions(account).await?;
    let policy = PlanPolicy::default();
    let tier = billing::resolve_tier(&subscriptions, now, &policy);
    let subsidy = if tier == billing::BillingTier::Free {
        let cap = env
            .var("FREE_GRANT_BUDGET_MICROUSD_PER_DAY")
            .ok()
            .map(|v| v.to_string())
            .unwrap_or_else(|| "100000000".into());
        let cap = cap
            .parse::<i64>()
            .map_err(|_| Error::RustError("invalid_trial_capacity".into()))?;
        if cap < 0 {
            return Err(Error::RustError("invalid_trial_capacity".into()));
        }
        JsValue::from_str(&cap.to_string())
    } else {
        JsValue::NULL
    };
    let paid = billing::select_subscription(&subscriptions, now, &policy);
    let current: Option<Window> = db.prepare("SELECT period_start,period_end,source FROM allowance_windows WHERE account_id=?1 ORDER BY period_start DESC LIMIT 1")
        .bind(&[account.into()])?.first(None).await?;
    let expected = current.as_ref().map_or(-1, |w| w.period_start);
    let (mut start, mut end, mut source) = if let Some(subscription) = paid {
        let period = paid_period(
            subscription.current_period_start,
            subscription.current_period_end,
            now,
        )?;
        (period.0, period.1, format!("billing:{}", subscription.id))
    } else {
        let period = billing::free_period(now)
            .map_err(|_| Error::RustError("invalid_allowance_period".into()))?;
        (period.start, period.end, "free".to_owned())
    };
    if let Some(window) = current {
        if now < window.period_end {
            // Pin one high-water mark across tier changes and Free/paid transitions.
            start = window.period_start;
            end = window.period_end;
            source = window.source;
        } else if start < window.period_end {
            start = window.period_end;
        }
    }
    if start > now || end <= now || end <= start {
        return Err(Error::RustError("invalid_allowance_period".into()));
    }
    db.prepare(GRANT)
        .bind(&[
            JsValue::from_str(account),
            start.to_string().into(),
            end.to_string().into(),
            source.into(),
            tier.monthly_allowance().get().to_string().into(),
            now.to_string().into(),
            expected.to_string().into(),
            subsidy,
        ])?
        .run()
        .await?;
    Ok(())
}

fn paid_period(start: i64, end: i64, now: i64) -> Result<(i64, i64)> {
    const MONTH: i64 = 30 * 24 * 60 * 60;
    if start < 0 || now < start || now >= end {
        return Err(Error::RustError("invalid_allowance_period".into()));
    }
    let current = start + ((now - start) / MONTH) * MONTH;
    let next = current
        .checked_add(MONTH)
        .ok_or_else(|| Error::RustError("invalid_allowance_period".into()))?;
    Ok((current, next.min(end)))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paid_renewal_uses_fixed_slices_not_extended_subscription_end() {
        let month = 30 * 86400;
        assert_eq!(
            paid_period(100, 100 + month * 2, 101).unwrap(),
            (100, 100 + month)
        );
        assert_eq!(
            paid_period(100, 100 + month * 2, 100 + month).unwrap(),
            (100 + month, 100 + 2 * month)
        );
        assert!(paid_period(100, 200, 200).is_err());
    }
}
