//! Versioned provider tariffs select a static, durable reservation receipt.
use super::ModelPrice;
use serde::Deserialize;
use time::{OffsetDateTime, UtcOffset, Weekday};

#[derive(Clone, Deserialize)]
pub(super) struct PriceDefinition {
    #[serde(flatten)]
    pub price: ModelPrice,
    #[serde(default)]
    schedule: Option<Schedule>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Schedule {
    #[serde(rename = "deepseek_flash_2026_10_25")]
    DeepseekFlash2026_10_25,
    #[serde(rename = "deepseek_pro_2026_10_25")]
    DeepseekPro2026_10_25,
}

impl PriceDefinition {
    pub fn valid(&self) -> bool {
        self.price.valid()
            && self.schedule.is_none_or(|schedule| {
                let (input, cached, output) = schedule.peak();
                (
                    self.price.input_microusd_per_million,
                    self.price.cached_input_microusd_per_million,
                    self.price.output_microusd_per_million,
                ) == (input, cached, output)
                    && self.price.revision == "deepseek-2026-10-25-cn2026-v1"
            })
    }

    pub fn supports_key(&self, key: &str) -> bool {
        match self.schedule {
            None => true,
            Some(Schedule::DeepseekFlash2026_10_25) => matches!(
                key,
                "deepseek/deepseek-flash"
                    | "deepseek/deepseek-v4-flash"
                    | "deepseek/deepseek-v4-flash-vision-exp"
            ),
            Some(Schedule::DeepseekPro2026_10_25) => key == "deepseek/deepseek-v4-pro",
        }
    }

    pub fn select(&self, now: i64) -> Result<ModelPrice, &'static str> {
        if !self.valid() {
            return Err("invalid_model_price");
        }
        let Some(_) = self.schedule else {
            return Ok(self.price.clone());
        };
        let peak = deepseek_peak(now)?;
        let mut price = self.price.clone();
        if !peak {
            price.input_microusd_per_million /= 2;
            price.cached_input_microusd_per_million /= 2;
            price.output_microusd_per_million /= 2;
        }
        price.revision = format!(
            "{}:{}",
            price.revision,
            if peak { "peak" } else { "offpeak" }
        );
        Ok(price)
    }
}

impl Schedule {
    fn peak(self) -> (u64, u64, u64) {
        match self {
            Self::DeepseekFlash2026_10_25 => (300_000, 6_000, 1_200_000),
            Self::DeepseekPro2026_10_25 => (1_320_000, 44_000, 3_960_000),
        }
    }
}

fn deepseek_peak(now: i64) -> Result<bool, &'static str> {
    let utc = OffsetDateTime::from_unix_timestamp(now).map_err(|_| "invalid_pricing_timestamp")?;
    let beijing = utc.to_offset(UtcOffset::from_hms(8, 0, 0).expect("fixed China offset"));
    if beijing.year() != 2026 {
        return Err("model_price_calendar_expired");
    }
    let holiday = matches!(
        (u8::from(beijing.month()), beijing.day()),
        (1, 1..=3)
            | (2, 15..=23)
            | (4, 4..=6)
            | (5, 1..=5)
            | (6, 19..=21)
            | (9, 25..=27)
            | (10, 1..=7)
    );
    Ok(!holiday
        && !matches!(utc.weekday(), Weekday::Saturday | Weekday::Sunday)
        && matches!(utc.hour(), 1..=3 | 6..=9))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn at(month: u8, day: u8, hour: u8, minute: u8, second: u8) -> i64 {
        time::Date::from_calendar_date(2026, month.try_into().unwrap(), day)
            .unwrap()
            .with_hms(hour, minute, second)
            .unwrap()
            .assume_utc()
            .unix_timestamp()
    }
    fn flash() -> PriceDefinition {
        serde_json::from_value(serde_json::json!({
            "input_microusd_per_million":300000,"cached_input_microusd_per_million":6000,
            "output_microusd_per_million":1200000,"revision":"deepseek-2026-10-25-cn2026-v1",
            "schedule":"deepseek_flash_2026_10_25"
        }))
        .unwrap()
    }
    #[test]
    fn utc_half_open_peak_boundaries() {
        for (h, m, s, peak) in [
            (0, 59, 59, false),
            (1, 0, 0, true),
            (3, 59, 59, true),
            (4, 0, 0, false),
            (5, 59, 59, false),
            (6, 0, 0, true),
            (9, 59, 59, true),
            (10, 0, 0, false),
        ] {
            assert_eq!(deepseek_peak(at(10, 26, h, m, s)), Ok(peak));
        }
    }
    #[test]
    fn holiday_weekend_and_makeup_workday_are_offpeak() {
        for (month, day) in [
            (1, 1),
            (2, 16),
            (4, 6),
            (5, 4),
            (6, 19),
            (9, 25),
            (10, 1),
            (10, 7),
            (10, 10),
            (10, 25),
        ] {
            assert_eq!(deepseek_peak(at(month, day, 2, 0, 0)), Ok(false));
        }
        assert_eq!(deepseek_peak(at(10, 8, 2, 0, 0)), Ok(true));
    }
    #[test]
    fn calendar_expires_at_beijing_new_year_and_static_prices_do_not() {
        let last = at(12, 31, 15, 59, 59);
        assert!(deepseek_peak(last).is_ok());
        assert_eq!(deepseek_peak(last + 1), Err("model_price_calendar_expired"));
        let mut definition = flash();
        definition.schedule = None;
        assert!(definition.select(last + 1).is_ok());
    }
    #[test]
    fn selected_receipt_is_static_and_exact() {
        let off = flash().select(at(10, 26, 0, 0, 0)).unwrap();
        let peak = flash().select(at(10, 26, 1, 0, 0)).unwrap();
        assert_eq!(off.input_microusd_per_million, 150000);
        assert_eq!(off.cached_input_microusd_per_million, 3000);
        assert_eq!(off.output_microusd_per_million, 600000);
        assert_eq!(peak.output_microusd_per_million, 1200000);
        let wire = serde_json::to_value(&off).unwrap();
        assert_eq!(wire.as_object().unwrap().len(), 4);
        assert_eq!(wire["revision"], "deepseek-2026-10-25-cn2026-v1:offpeak");
    }
    #[test]
    fn scheduled_tariffs_cannot_be_attached_to_another_provider_or_changed() {
        let mut definition = flash();
        assert!(definition.supports_key("deepseek/deepseek-v4-flash"));
        assert!(!definition.supports_key("openrouter/deepseek-flash"));
        assert!(!definition.supports_key("deepseek/deepseek-v4-pro"));
        definition.price.output_microusd_per_million += 1;
        assert!(!definition.valid());
        assert_eq!(
            definition.select(at(10, 26, 1, 0, 0)).err(),
            Some("invalid_model_price")
        );
    }

    #[test]
    fn independent_release_oracle_matches_both_models() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/deepseek-pricing-calendar-2026.json"
        ))
        .unwrap();
        for (model, schedule) in [
            ("deepseek-flash", "deepseek_flash_2026_10_25"),
            ("deepseek-v4-pro", "deepseek_pro_2026_10_25"),
        ] {
            let rates = &fixture["rates_microusd_per_million"][model];
            let peak = rates["peak"].as_array().unwrap();
            let definition: PriceDefinition = serde_json::from_value(serde_json::json!({
                "input_microusd_per_million":peak[1], "cached_input_microusd_per_million":peak[0],
                "output_microusd_per_million":peak[2], "revision":fixture["revision"], "schedule":schedule,
            })).unwrap();
            for case in fixture["cases"].as_array().unwrap() {
                let timestamp = case["unix_seconds"].as_i64().unwrap();
                let period = case["period"].as_str().unwrap();
                let selected = definition.select(timestamp);
                if period == "unavailable" {
                    assert_eq!(selected.err(), Some("model_price_calendar_expired"));
                    continue;
                }
                let price = selected.unwrap();
                assert_eq!(
                    serde_json::json!([
                        price.cached_input_microusd_per_million,
                        price.input_microusd_per_million,
                        price.output_microusd_per_million
                    ]),
                    rates[period],
                    "{model} {timestamp}"
                );
            }
        }
    }

    proptest::proptest! {
        #[test]
        fn every_second_in_a_week_obeys_utc_schedule(second in 0i64..604800) {
            let timestamp = at(10,19,0,0,0)+second;
            let utc = OffsetDateTime::from_unix_timestamp(timestamp).unwrap();
            let expected = utc.weekday().number_from_monday() <= 5
                && ((3600..14400).contains(&(second%86400)) || (21600..36000).contains(&(second%86400)));
            proptest::prop_assert_eq!(deepseek_peak(timestamp),Ok(expected));
        }
    }
}
