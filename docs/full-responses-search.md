# Native web search service adapters

This is the search provider boundary for the complete Responses workstream. It is
not an acceptance receipt. The latest user policy authorizes an explicit platform
DeepSeek default as well as BYOK. No other platform search-provider fallback is allowed.

## Provider and execution boundary

The first adapter targets an explicitly configured OpenAI-compatible **Responses
search service**, with its own owner-scoped credential ID and model. It is
independent of the main agent's model. A BYOK language-model credential is not
implicitly a search credential. Platform DeepSeek is a separate explicit route.

`search_byok::prepare` accepts a secret-free `SearchBinding`, native search-tool
options, the harness-selected query and existing tool execution ID. The binding
contains credential ID, model, maximum output tokens and maximum native tool calls.
The credential is resolved through the existing account-scoped encrypted vault at
execution preparation. Its immutable protocol must be in the OpenAI credential family (`openai` or
`responses`); no environment-secret or
platform-provider fallback exists. A missing binding returns `search_byok_required`
before credential lookup or HTTP dispatch. Preparation returns opaque
`PreparedSearch`, containing owned Rust secret/configuration data but no native
request or serializable/debug representation. Admission must perform the same
binding and capability check before starting paid agent work. The harness resolves
preparation first, persists its dispatch fence, then consumes
`PreparedSearch::dispatch` exactly once. Credential lookup/validation failures are
therefore known pre-dispatch failures, not uncertain paid requests.

One external `/responses` request declares one search tool, uses `tool_choice:
"required"`, bounded `max_tool_calls`/`max_output_tokens`, `store:false`, and no
automatic request retry. The native provider may perform search, open-page and
find-in-page actions within that bounded operation. This is a provider tool-service
call, not another local agent harness. The owning DO must persist dispatch intent
before awaiting it, wrap the operation in the normal tool timeout and journal its
known result before publication. A lost response is an uncertain external outcome,
not permission to submit the prompt again. The deterministic idempotency header
does not assert that every compatible provider honors it.

The platform's resource accounting still applies. Search provider/model fees are
paid directly by the BYOK owner; the adapter does not fabricate a platform model
price or infer usage from text. `SearchExchange` retains exact native usage and
raw usage data even when a terminal payload violates protocol. Missing counters
remain unreported, including unknown reasoning tokens. Provider HTTP failures
return a bounded code/status without error-body disclosure; network/truncated
JSON uncertainty must retain the existing unresolved-operation provenance.

## Wire and evidence contract

The typed options preserve the current official variants:

| Variant | Options |
| --- | --- |
| `web_search`, `web_search_2025_08_26` | `external_web_access`, `filters.allowed_domains`, `search_context_size`, `user_location` |
| `web_search_preview`, `web_search_preview_2025_03_11` | `search_content_types`, `search_context_size`, `user_location` |

Location fields are type/city/country/region/timezone; omitted values are not filled
with invented coordinates. Modern filter/cache-only fields are not silently
discarded for a preview provider. The provider receives the configured values and
must reject capabilities it cannot implement.

The result contains typed native `web_search_call` items and their actual action:
`search`, `open_page`, or `find_in_page`. Native result objects, answer text,
sources and URL citations are user data. They are never emitted into telemetry.
Search/source URLs are parsed and checked before being exposed; the adapter never
fetches them itself and never sends the credential to a result URL. The provider
route reuses the vault's public HTTPS validation and refuses redirects.

Citation offsets retain their native message ID and content-part index. They are
not offsets into the outer agent's later answer, or even into a concatenation of
multiple native text parts. The coordinator may use authoritative source URLs to
anchor citations in text actually produced by the outer agent; it must not blindly
copy these offsets onto different text. Raw provider answer/citations remain
available for that decision.

Completed native responses require real search calls, unique call IDs, valid
actions and completed call states. Failed, incomplete, malformed, over-budget or
filter-violating provider results do not become fabricated successful searches.
The adapter bounds response bytes, answer bytes, query size, call count, source
count and annotation ranges.

## Integration and verification

The Session producer in `session_hosted_search.rs` now shares the existing
`session_hosted_publication.rs` publisher and dispatch boundary with Code. This
independent search vertical does not import the image, MCP or discovery WIP.
It retains a secret-free operation digest, original admission/price snapshot,
independent fee hold, durable dispatch fence, known provider receipt and canonical
result. Known receipt settlement and publication retry those stored values only.
An unknown dispatch publishes a bounded failed item, preserves its unresolved
hold and cannot repeat provider HTTP on replay.

The canonical lifecycle is `in_progress` → `searching` → `completed`/`failed`.
The searching item and native phase events commit before external dispatch;
only a searching **web search** item accepts that terminal update. Canonical
query provenance for dedicated DeepSeek comes from the retained request; ranked
sources come from its actual result. A native Responses BYOK service's own
observed action is not overwritten with the outer query. Dedicated API results
contain no answer-span offsets, and no offsets are manufactured for the outer
assistant's separate text.

A Responses BYOK service can return multiple search/open/find calls within its
one bounded HTTP operation. Their original order, actual actions and individual
terminal statuses are projected to separate stable canonical items rather than
collapsing to the last call. The first uses the pre-dispatch item identity;
additional identities derive from the retained operation and call index, never
from upstream IDs. Settlement still consumes one original provider-request
receipt, and replay publishes no duplicate item or external request. The new
`responses_multi` compiled journey verifies three native actions from one HTTP
request; it awaits the next build, independently of the six accepted cases below.

`tests/worker/hosted-search.test.mjs` is the compiled-Worker acceptance entrypoint:
platform search/open/find, an independent owner search credential, proved empty
zero-unit results, and a lost response that retains its hold without redispatch.
It uses the actual OpenAI Node SDK, strict current Python SDK wire validation,
canonical SSE replay/JSON retrieval and actual D1 ledger/hold assertions. The
fixture has no external network access. Select one increment with
`MAGENTS_HOSTED_SEARCH_CASE=platform|byok|responses_byok|responses_multi|empty|http_error|unknown`.

On 2026-10-26 all six cases passed locally against the actual Worker artifact
`12636014402` from source `2cc1e0cb4b06c997212d1bbfa97c6a238e7977b6`, CI build
`40291651029`; Wasm SHA-256
`ee351088fce81db2950fbe569c3645e86307c0cfda21a50d76bb3fe0badfcc80`.
The validator was locked current Python SDK `3.37.0`, not the legacy CI parser.
Platform search/open/find produced three posts, four outer model turns and
three 750-microUSD ledger entries; both BYOK routes produced one post and zero
platform upstream debit. Empty and HTTP-429 results settled zero units. A
truncated HTTP-200 body retained one unresolved hold and replay made no second
post. Every case passed native phase order, exact SSE replay, matching JSON
retrieval and strict wire validation. Missing reasoning/cache-write counters
remained nullable under `agents_usage`; no tool tokens were manufactured.
Per-case local receipts/logs are in `.temp/media-delivery/search-*` (the platform
receipt's original source field is null; its identical Wasm checksum binds the
retained artifact). This is controlled execution, not live provider acceptance.
The original CI journey did not run because its Worker job installed only the
legacy SDK environment; fixing that test environment reuses the same build.

The adapter slice owns `crates/agents-worker/src/search_byok.rs` and
`crates/agents-worker/src/search_deepseek.rs`.
Shared tool schemas, binding management, canonical item/event publication,
operation metering and provider-result fences are integrated by the hosted-tools
coordinator. Module wiring and actual-Wasm provider fixtures remain required;
formatting or native payload fixtures alone are not end-to-end acceptance.

Pure contracts cover exact option forwarding, current/preview distinctions,
search/open/find results, citation identity, known versus missing usage, terminal
status validation, source filter enforcement and safe failure metadata. No real
search query, model request, credential read, deployment or local Rust build was
performed while implementing this slice.

## Dedicated DeepSeek search and fetch (2026-10-25 policy update)

`search_deepseek` targets the documented first-party JSON APIs, not a speculative
`enable_search` flag, a proxy, or another paid model turn:

- `POST https://api.deepseek.com/v1/tools/search`
- `POST https://api.deepseek.com/v1/tools/fetch`

The platform route resolves only `DEEPSEEK_API_KEY`. The separate dedicated BYOK
route resolves an explicitly selected owner credential, requires the OpenAI
credential family and the official DeepSeek host/base, and never falls back to
the platform key. Both return the same opaque prepared-dispatch type. Parent
admission must select the route before any main-model work. Official documentation
establishes public API availability; this implementation has not verified the
deployment account's entitlement with a live query. Such a probe needs the root
coordinator's separate approval.

One `DeepseekAction` is one fenced external operation. `Search` calls search once;
`OpenPage` calls fetch once; `FindInPage` calls fetch once and performs bounded
literal matching against its returned Markdown. There are no hidden follow-up
page requests, model requests, automatic retries or local URL fetches. Actions
need distinct execution IDs if a tool workflow performs more than one operation.
Fetch URLs use the shared public-host policy and configured domain constraints;
the credential is sent exclusively to DeepSeek, never the fetched target.

Search queries are limited to 128 Unicode scalar values, including server-added
`site:` clauses. `query_limit` exposes the remaining model-visible budget; queries
are rejected rather than truncated. Context size maps to 5/10/20 results.
Cache-only search, user geolocation and image results have no corresponding
dedicated API capability and fail before secret resolution/dispatch, rather than
being silently ignored. Source domains are additionally checked on results.
Untrusted summaries and Markdown are user data, not telemetry or instructions.
DeepSeek returns source URLs rather than answer-span citation offsets; this
adapter emits no invented citation annotations. The coordinator can anchor these
sources in the outer model's actual answer.

`DeepseekSearchExchange.receipt` records the operation and exact successful
nonempty request units independently of model-token usage. A nonempty HTTP-200
result still counts if our publication policy rejects its contents. A malformed
or lost response has no trustworthy receipt, preserving an unresolved outcome;
it is not silently settled as zero or retried. Known HTTP failures and empty
results produce zero successful units. The adapter does not invent token counts
or use language-model token prices for this API.

The direct API tariff is `$0.75 / 1,000` successful nonempty search or fetch
requests, i.e. **750 microdollars per unit now**. The November 1 promotion boundary
in the official guide concerns separate server-side Harness/Claude Code/Codex
calls, not this application's direct standalone requests. No such entitlement
is inferred. Receipts pin `price_revision=deepseek-direct-search-fetch-2026-10-26`;
known empty/failed responses have zero units, while an unknown outcome remains
held. BYOK provider charges are paid by the owner, not the platform wallet.
Infrastructure remains separately metered. Search
and fetch have separate account QPS budgets (100 and 25 respectively); admission
and resource reservations belong to the owning coordinator.

Five added pure fixtures cover the dedicated request contract/Unicode limits,
nonempty-unit charging/deduplicated sources, rejected-publication receipts,
fetch/find payloads and pre-dispatch safety/capability failures. Formatting and
diff checks pass. Module wiring, compilation, actual-Wasm fixtures, configured
tariff ledger integration and live account acceptance remain parent-owned; no
live or paid operation was made in this slice.

## Primary references

- [OpenAI web search guide](https://developers.openai.com/api/docs/guides/tools-web-search)
- [Create response reference](https://developers.openai.com/api/reference/resources/responses/methods/create)
- Repository contract cache: `.cache/full-responses-spec/create.md` (official
  Markdown fetched by the contract-inventory owner on 2026-10-25).
- [DeepSeek Search & Fetch guide](https://api-docs.deepseek.com/guides/search/)
- [DeepSeek Search API](https://api-docs.deepseek.com/api/search/)
- [DeepSeek Fetch API](https://api-docs.deepseek.com/api/fetch/)
- Existing vault policy: [BYOK protocol adapters](byok-protocols.md).

