"""Independent calendar oracle consumed by the Rust DeepSeek pricing tests."""
from __future__ import annotations

from datetime import date, datetime, timedelta, timezone
import json
from pathlib import Path
import random
import sys
import unittest
from typing import Any

ROOT: Path = Path(__file__).resolve().parents[1]
FIXTURE: Path = ROOT / "tests/fixtures/deepseek-pricing-calendar-2026.json"
BEIJING: timezone = timezone(timedelta(hours=8))
HOLIDAYS: tuple[tuple[date, date], ...] = (
    (date(2026, 1, 1), date(2026, 1, 3)),
    (date(2026, 2, 15), date(2026, 2, 23)),
    (date(2026, 4, 4), date(2026, 4, 6)),
    (date(2026, 5, 1), date(2026, 5, 5)),
    (date(2026, 6, 19), date(2026, 6, 21)),
    (date(2026, 9, 25), date(2026, 9, 27)),
    (date(2026, 10, 1), date(2026, 10, 7)),
)
FIRST: int = int(datetime(2026, 1, 1, tzinfo=BEIJING).timestamp())
LAST: int = int(datetime(2027, 1, 1, tzinfo=BEIJING).timestamp())


def period(unix_seconds: int) -> str:
    instant: datetime = datetime.fromtimestamp(unix_seconds, timezone.utc)
    local_date: date = instant.astimezone(BEIJING).date()
    if not FIRST <= unix_seconds < LAST:
        return "unavailable"
    holiday: bool = any(first <= local_date <= last for first, last in HOLIDAYS)
    if instant.weekday() >= 5 or holiday:
        return "off_peak"
    second: int = instant.hour * 3600 + instant.minute * 60 + instant.second
    return "peak" if 3600 <= second < 4 * 3600 or 6 * 3600 <= second < 10 * 3600 else "off_peak"


def fixture() -> dict[str, Any]:
    values: set[int] = {FIRST - 1, FIRST, FIRST + 1, LAST - 1, LAST, LAST + 1}
    days: set[date] = {date(2026, month, 15) for month in range(1, 13)}
    for first, last in HOLIDAYS:
        days.update({first - timedelta(days=1), first, last, last + timedelta(days=1)})
    days.update({date(2026, 1, 4), date(2026, 2, 14), date(2026, 2, 28), date(2026, 5, 9), date(2026, 9, 20), date(2026, 10, 10)})
    for day in days:
        midnight: int = int(datetime(day.year, day.month, day.day, tzinfo=timezone.utc).timestamp())
        for boundary in (3600, 4 * 3600, 6 * 3600, 10 * 3600, 16 * 3600):
            values.update(midnight + boundary + offset for offset in (-1, 0, 1))
    generator: random.Random = random.Random(20261025)
    values.update(generator.randrange(FIRST, LAST) for _ in range(512))
    return {
        "revision": "deepseek-2026-10-25-cn2026-v1", "calendar_timezone": "Asia/Shanghai", "peak_timezone": "UTC",
        "valid_from_inclusive": FIRST, "valid_until_exclusive": LAST,
        "sources": ["https://api-docs.deepseek.com/quick_start/pricing/", "https://www.beijing.gov.cn/zhengce/zhengcefagui/202511/t20251104_4258873.html"],
        "rates_microusd_per_million": {
            "deepseek-flash": {"off_peak": [3000, 150000, 600000], "peak": [6000, 300000, 1200000]},
            "deepseek-v4-pro": {"off_peak": [22000, 660000, 1980000], "peak": [44000, 1320000, 3960000]},
        },
        "rate_order": ["cached_input", "uncached_input", "output"],
        "cases": [{"unix_seconds": value, "period": period(value)} for value in sorted(values)],
    }


def timestamp(value: str) -> int:
    return int(datetime.fromisoformat(value.replace("Z", "+00:00")).timestamp())


class PricingContract(unittest.TestCase):
    def test_holiday_and_weekend_override_peak_windows(self) -> None:
        assert period(timestamp("2026-02-20T02:00:00Z")) == "off_peak"
        assert period(timestamp("2026-10-05T08:00:00Z")) == "off_peak"
        assert period(timestamp("2026-02-14T02:00:00Z")) == "off_peak"
        assert period(timestamp("2026-10-10T08:00:00Z")) == "off_peak"
        assert period(timestamp("2026-10-12T08:00:00Z")) == "peak"


    def test_windows_are_half_open_and_horizon_is_not_a_silent_peak_fallback(self) -> None:
        for instant, expected in (
            ("2026-10-12T00:59:59Z", "off_peak"), ("2026-10-12T01:00:00Z", "peak"),
            ("2026-10-12T03:59:59Z", "peak"), ("2026-10-12T04:00:00Z", "off_peak"),
            ("2026-10-12T05:59:59Z", "off_peak"), ("2026-10-12T06:00:00Z", "peak"),
            ("2026-10-12T09:59:59Z", "peak"), ("2026-10-12T10:00:00Z", "off_peak"),
        ):
            assert period(timestamp(instant)) == expected
        assert period(FIRST - 1) == period(LAST) == "unavailable"
        assert period(FIRST) == period(LAST - 1) == "off_peak"


    def test_checked_in_vectors_are_reproducible(self) -> None:
        assert json.loads(FIXTURE.read_text(encoding="utf-8")) == fixture()


    def test_deployment_configs_use_verified_schedules_not_static_or_fallback_prices(self) -> None:
        config: dict[str, Any] = json.loads((ROOT / "wrangler.jsonc").read_text(encoding="utf-8"))
        rates: dict[str, Any] = fixture()["rates_microusd_per_million"]
        for environment in ("staging", "production"):
            variables: dict[str, Any] = config["env"][environment]["vars"]
            prices: dict[str, Any] = json.loads(variables["MODEL_PRICING_JSON"])
            models: list[dict[str, Any]] = json.loads(variables["MODEL_REGISTRY_JSON"])
            for model, schedule in (("deepseek-flash", "deepseek_flash_2026_10_25"), ("deepseek-v4-pro", "deepseek_pro_2026_10_25")):
                price: dict[str, Any] = prices["deepseek/" + model]
                assert price["schedule"] == schedule
                assert price["revision"] == fixture()["revision"]
                assert [price["cached_input_microusd_per_million"], price["input_microusd_per_million"], price["output_microusd_per_million"]] == rates[model]["peak"]
                actual: list[dict[str, Any]] = [value for value in models if value["provider"] == "deepseek" and value["model"] == model]
                assert len(actual) == 1 and actual[0]["protocol"] == "responses"
            assert "openrouter/deepseek/deepseek-v4.1-flash-1017" in prices


if __name__ == "__main__":
    if sys.argv[1:] == ["--write"]:
        FIXTURE.write_text(json.dumps(fixture(), indent=2) + "\n", encoding="utf-8")
    else:
        unittest.main()
