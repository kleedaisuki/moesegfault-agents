# Published platform resource tariffs

Revision: `2026-10-23-platform-v1`. Staging balances are synthetic test credit, not invoices. The machine-readable source is `deploy/resource-tariffs.json`; its exact rational values are embedded in environment configuration. Prices must be pinned to an observation/reservation before an operation. Changing a deployment must not reprice old observations.

| Meter | Platform price | Measurement |
| --- | ---: | --- |
| Basic workspace allocation | $0.036 / running hour | Container running wall milliseconds, including retained idle allocation and idle controller overhead |
| Active harness orchestration | $0.0072 / hour | Observed active Durable Object operation wall milliseconds |
| API orchestration | $0.00036 / hour | Observed Worker operation wall milliseconds |
| R2 Class A | $4.50 / million operations | Actual PUT/multipart/list operations |
| R2 Class B | $0.36 / million operations | Actual GET/HEAD operations |
| R2 retained data | $0.015 / decimal GB / 30 days | Bytes multiplied by retained milliseconds |
| D1 reads | $0.001 / million rows | D1 result metadata, not returned-row count |
| D1 writes | $1.00 / million rows | D1 result metadata |

**The first three entries are published platform tariffs, not exact Cloudflare invoice pass-through.** In particular, wall time is not CPU utilization. A basic workspace has provisioned memory/disk and a CPU capacity cap; a predictable allocation tariff avoids falsely presenting elapsed time as measured CPU time. The upstream Containers API does not expose synchronous CPU measurements. Delayed Cloudflare analytics are useful for operator margin/reconciliation, not an invented in-request `getMetrics` call.

R2 and D1 rates reference [Cloudflare R2 pricing](https://developers.cloudflare.com/r2/pricing/) and [D1 pricing](https://developers.cloudflare.com/d1/platform/pricing/) checked on 2026-10-23. Platform prices do not apportion the operator account's included free usage or vendor invoice rounding among individual customers. R2 byte-time is a published fine-grained platform meter rather than claiming to reproduce the vendor's daily peak-storage rounding. Monetary events round upward to integer microUSD; implementations should aggregate cheap events in deterministic batches rather than create ledger writes for individual tokens or filesystem accesses.

Other upstream dimensions are documented at [Containers pricing](https://developers.cloudflare.com/containers/platform/pricing/) and [Durable Objects pricing](https://developers.cloudflare.com/durable-objects/platform/pricing/). Allocation telemetry and backend metering must agree before production promotion. Unmeasured/uncertain operations retain a visible reconciliation hold; they must not silently become free or be billed from fabricated measurements.

## Model prices

The initial default is OpenRouter's `deepseek/deepseek-v4.1-flash-1017`, exercised through the platform's existing OpenRouter credential. OpenRouter's [model metadata](https://openrouter.ai/api/v1/models) reports current context/output limits and supports provider-reported cost. The registry records **peak reservation ceilings** for Flash ($0.30 input, $0.006 cached input, $1.20 output per million tokens) and the observed GPT-4.1-mini prices ($0.40/$0.10/$1.60). The reservation ceiling is not the bill: settle the actual reported OpenRouter `usage.cost`, preserving discounted/cache prices and upstream request provenance.

Direct DeepSeek Flash/Pro are registered as supported model identities for BYOK and direct-provider integration, but no guessed static direct-provider price is configured. [DeepSeek's current pricing](https://api-docs.deepseek.com/quick_start/pricing/) varies by UTC peak windows and Chinese public holidays. Direct platform-funded requests must stay fail-closed until the time-dependent tariff resolver is configured; BYOK still pays platform resources but no duplicated model charge. This distinction is intentional and must be surfaced as provider availability, not an obscure internal error.
