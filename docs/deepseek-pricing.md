# DeepSeek model tariff selection

## Contract and sources

The deployment registry observes the [DeepSeek pricing table](https://api-docs.deepseek.com/quick_start/pricing/)
on 2026-10-25. Rates below are integer microUSD per million tokens, in
**cache-miss input / cache-hit input / output** order:

| Model | Off-peak | Peak |
| --- | --- | --- |
| `deepseek-flash` (V4.1 Flash 1017) | 150000 / 3000 / 600000 | 300000 / 6000 / 1200000 |
| `deepseek-v4-pro` (V4 Pro 0813) | 660000 / 22000 / 1980000 | 1320000 / 44000 / 3960000 |

The provider explicitly maps legacy `deepseek-v4-flash` and
`deepseek-v4-flash-vision-exp` requests to current Flash and its tariff. These
aliases retain their original request/pin identity; pricing does not rewrite models.
OpenRouter remains a distinct route: its authoritative reported native request
cost is settled under the existing OpenRouter policy, never this DeepSeek tariff.
BYOK model requests still have zero platform model charge.

## Calendar and validity

Peak is UTC Monday–Friday, half-open `[01:00,04:00)` and `[06:00,10:00)`, except
Chinese public holidays. Natural weekends are off-peak even on makeup workdays.
The holiday calendar is the State Council's **国办发明电〔2025〕7号**, published
2025-11-04, [official Beijing government republication](https://www.beijing.gov.cn/zhengce/zhengcefagui/202511/t20251104_4258873.html)
of [central government notice](https://www.gov.cn/zhengce/content/202511/content_7047090.htm).
The full published holiday periods are January 1–3, February 15–23, April 4–6,
May 1–5, June 19–21, September 25–27, and October 1–7, 2026.
Holiday dates and calendar coverage use Beijing's fixed UTC+08 civil time;
weekday and peak windows use UTC. The `time` library owns timestamp/calendar
conversion. All peak windows fall on the same civil date in both time zones.

Revision `deepseek-2026-10-25-cn2026-v1` covers Beijing civil year 2026 only.
Outside that coverage, new scheduled requests fail closed with
`model_price_calendar_expired`; they do not guess holidays, silently charge a
more expensive fallback, or use an off-peak guess. Before 2027, publish the
next official calendar as a new schedule/revision and update deployment config.
Already reserved requests remain settleable and replayable after expiry.

## Representation and financial lifetime

`MODEL_PRICING_JSON` retains the four legacy static fields. A scheduled entry
adds `schedule: "deepseek_flash_2026_10_25"` or
`"deepseek_pro_2026_10_25"`; its base rates must exactly match that schedule's
published **peak** rates and revision. A schedule is valid only for its approved
direct DeepSeek model/alias keys. Unknown schedules, mismatched rates and routes
are invalid, not a fallback to static prices. Legacy entries without `schedule`
retain their original behavior and serialized representation.

At each model completion reservation, selection produces the existing static
four-field `ModelPrice`, with `:peak` or `:offpeak` appended to the revision.
The existing durable reservation receipt stores those selected rates. Settlement
reads that receipt, never the current wall clock or deployment tariff. A retry
first loads the account/session-scoped receipt, validates the same pinned model
and input bound, and reuses its exact price/hash. Peak crossings, calendar expiry
and tariff deployments therefore cannot reprice reserved work. Concurrent first
reservation races remain protected by the unique idempotency key and request
hash: a conflicting loser is rejected before model dispatch and can retry the
same operation against the persisted winner.

No D1 schema or historical ledger migration is needed. Token charges retain the
existing checked integer arithmetic and aggregate ceiling-rounding semantics.
The new receipt-read query adds one indexed reservation lookup per completion;
platform run admission also checks for an existing scoped reservation before
validating current tariff availability, preserving old run-admission retries.
This prevents expensive duplicate provider dispatch and avoids calendar re-selection
on replay.

## Capacity is not the default budget

The provider publishes 1M context and 384K **maximum** output. The deployment's
configured output budget remains **8192**. `ModelConfig.max_output_tokens` is the
platform per-request budget/default, not a claim that the upstream cannot emit
more. Raising it to the provider maximum would unnecessarily reserve much of a
Free wallet. Existing agent/session pins remain unchanged; newly selected direct
DeepSeek registry models advertise the Responses wire protocol. The registry does
not change authentication credential families.

## Verification

Fixed-time Rust tests cover both half-open UTC windows, all holiday families,
natural/makeup weekends, the Beijing calendar horizon, exact static receipt
shape, and legacy timeless prices. A seeded property test samples every second
of a full non-holiday week. Independent Python-generated calendar cases are
checked against the Rust selector in CI. Reservation replay tests preserve old
prices and reject changed model budgets/input bounds. No live model request is
needed for this policy test; deployment and paid acceptance belong to ReleaseOps.
