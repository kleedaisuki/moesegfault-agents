# Prompt-cache engineering

Research and design checkpoint, 2026-10-27. This note connects the current
stateless-provider harness to provider caching mechanisms; it is not a claim of
deployed cache-hit improvement or of a universally supported KV-control API.

## Decision

**Superseded design premise:** the immutable-live-pin approach below describes
the earlier implementation and its measurements. The current direction removes
that invariant: creation snapshots are historical provenance, while effective
configuration/layout evolve. A fixed real dispatcher and ordinary versioned tool
catalog replace repeated mutable top-level registered schemas. See
[dynamic context and dispatch](dynamic-tool-context.md) for the new construction,
provider limits and its separate acceptance; old measurements remain unchanged.

Preserve a deterministic, versioned platform prefix and immutable Session pin.
Represent eligible response-scoped instruction and environment changes as
authoritative append-only context transitions at the point where they take
effect. Reuse the existing native context journal and shared generation/count
representation; do not introduce a parallel conversation store. Tool permission
and effect dispatch remain authoritative even when an old declaration survives
in historical context. A cache optimization must not resurrect a withdrawn tool
or make a previous response's custom instructions apply indefinitely.

The useful target is **unchanged rendered history followed by new input**, not
unchanged JSON formatting alone. Provider-side rendering, model controls, tool
schemas, reasoning state, routing and cache persistence all matter. Prefix
equality is an eligibility observation, not proof of a cache hit.

## Existing mechanisms and the identified failure surface

- [`context_prefix.rs`](../crates/agents-worker/src/context_prefix.rs) builds one
  system string from the platform sentence, pinned agent instructions and skills.
  [`PinnedContext::resolve`](../crates/agents-core/src/context.rs) already sorts
  tools/skills by name and version and serializes a canonical pin hash. Re-sorting
  unrelated caller-owned items every turn is neither necessary nor safe.
- [`session_responses.rs`](../crates/agents-worker/src/session_responses.rs)
  in its legacy renderer appends response custom instructions to the first system string.
  A changed/removed instruction therefore changes the input before all prior
  user, assistant and tool context, even when the conversation itself is stable.
- [`session_native.rs`](../crates/agents-worker/src/session_native.rs) already
  keeps input/output in an operation-keyed append-only log, with replacement
  generations for compaction and immutable historical references for branches.
  This is the right state owner for transitions and their replay boundary.
- [`auxiliary_context.rs`](../crates/agents-worker/src/auxiliary_context.rs),
  [`session_context.rs`](../crates/agents-worker/src/session_context.rs) and the
  compaction adapters render related context for count/compact/history. They must
  share the same effective transition state as inference, rather than reconstruct
  an earlier prefix from the latest live configuration.
- [`provider_anthropic.rs`](../crates/agents-worker/src/provider_anthropic.rs)
  currently collects all system/developer messages into top-level `system` and
  places an ephemeral cache marker on the first block. Adding an ordinary system
  message to its input does **not** currently create an inline cached transition.
- [`provider_controls.rs`](../crates/agents-worker/src/provider_controls.rs)
  already rejects unsupported explicit DeepSeek caching fields instead of
  accepting a provider's silent ignore as successful execution. Preserve this
  capability discipline.

## What the external mechanisms actually allow

| Surface | Reusable mechanism | Boundary for this project |
| --- | --- | --- |
| Direct DeepSeek | Automatic reuse of persisted complete prefix units | No explicit cache-key/retention switch; preserve the full input/output trajectory and inspect actual usage |
| Native OpenAI Responses on capable models | Stable tools and instruction prefix; append-only configuration/tool-loading updates; optional explicit breakpoints | Capability-gate new items/controls; do not translate them to unsupported providers |
| OpenRouter | Provider/model-dependent caching plus sticky routing | Keep existing stable Session routing; a provider fallback can legitimately lose a warm cache |
| Anthropic Messages/BYOK | Explicit/automatic caching; some current models support inline system/tool changes | Existing translator hoists system messages, so preserve old behavior unless a known-capable route explicitly selects a new renderer |
| Self-hosted research engines | Direct KV segment operations | Not exposed by the platform's current external inference providers; not implementable by editing ordinary request JSON |

### DeepSeek: append-only is more important than a shared opening sentence

DeepSeek's current [KV-cache guide](https://api-docs.deepseek.com/guides/kv_cache/)
describes sliding-window-attention prefix units. Request and output boundaries
can persist complete prefixes; common prefixes can also be detected and stored.
Thus `A+B`, then `A+B+C` can reuse the first unit, while `A+B`, then `A+C` may
initially miss. A later `A+D` can benefit after shared `A` is persisted. Do not
hard-code old cache-block-size assumptions or claim the second call must hit.

Its [Responses compatibility table](https://api-docs.deepseek.com/guides/responses_api/)
says `prompt_cache_key`/retention are unsupported, `previous_response_id` is not
an upstream state store, and `developer` is treated as `user`. Authoritative
instruction transitions therefore need a supported system-role rendering, not
an assumption that every OpenAI-compatible endpoint preserves developer
authority. Native cache reads are reported in
`usage.input_tokens_details.cached_tokens`.

### OpenAI: stable prefix, dynamic suffix, provider-specific additions

The [prompt-caching guide](https://developers.openai.com/api/docs/guides/prompt-caching)
describes caching over the provider's rendered context, including tools and
controls. It recommends stable declarations, restricted availability through
supported tool-choice controls, and append-only tool-loading history. Current
`additional_tools` and explicit-breakpoint mechanisms are model/API capabilities,
not portable function schemas. A cache key influences routing/accounting; it is
not evidence of a hit or permission to share private context between owners.

The [Prompt Caching 201 production account](https://developers.openai.com/cookbook/examples/prompt_caching_201)
describes Codex keeping instructions, tools, sandbox and environment consistently
ordered and appending runtime changes instead of rewriting earlier messages.
Move volatile times/IDs out of the prefix, but do not add padding merely to reach
a provider minimum: the goal is less paid work, not a larger cached-token count.

### Industry experience and research

[Manus's engineering report](https://manus.im/blog/Context-Engineering-for-AI-Agents-Lessons-from-Building-Manus)
emphasizes deterministic serialization, append-only observations and avoiding
tool-list churn. Its tool masking technique depends on model-serving controls;
a textual statement that a tool is disabled is not a substitute for the harness
refusing its execution. Do not apply its separate suggestion of intentional
action-format variation to stable prompt prefixes.

[Anthropic's context-engineering account](https://www.anthropic.com/research/effective-context-engineering-for-ai-agents)
treats context as a limited attention resource, not free storage. Preserve source
records, but bound the active window through the existing percentage policy;
an infinite instruction-update log would trade cache misses for growing input
cost and confusion. Its current [inline-system/tool documentation](https://platform.claude.com/docs/en/build-with-claude/mid-conversation-system-messages)
is explicitly model-specific, with additional beta requirements for tool changes.

The October 2026 paper [Cache Engineering for Agents](https://arxiv.org/html/2610.28577v1)
proposes direct `drop`/reposition operations in a co-designed AKV serving system.
Its useful lesson here is that rewriting history forces re-prefill and that
cache identity can depend on operation history. Its reported benefits are not
portable performance estimates for a Cloudflare harness using DeepSeek. We do
not own those providers' KV tensors or eviction policy.

[OpenRouter's cache documentation](https://openrouter.ai/docs/guides/best-practices/prompt-caching)
describes `session_id`/`x-session-id` sticky routing, provider fallback and actual
cache-read/write usage. Reuse existing routing identity; do not add randomly
changing routing keys, disable fallbacks globally, or confuse identical-response
caching with reusable prompt KV caching.

## Concrete transition model

An effective context has five distinct owners:

1. **Platform policy**: versioned, deterministic built-in instructions.
2. **Session pin**: resolved agent instructions, registered tools/skills,
   workspace/model settings. Existing Sessions retain this original base prefix.
3. **Live Agent configuration**: durable instruction/tool revisions appended at
   safe boundaries without rewriting the base; see [live updates](session-config-updates.md).
   Model/workspace authority and pinned skill changes still require a new Session.
4. **Response scope**: top-level custom instructions and admitted tool controls.
   This state is selected for the current Response, not inherited accidentally.
5. **Environment observations**: actual working directory, workspace lifecycle or
   execution-policy changes. Append facts only when changed and observed; do not
   inject a fresh timestamp or random container generation at the front each turn.

For an append-capable renderer, persist a typed internal event with an operation
identity, source boundary and full replacement value for each changed scope.
Render it as supported authoritative provider input, not an unknown public item
or ordinary untrusted user content. The fixed policy explains that later
transitions replace only their named scope. An unchanged value creates no event.
The transition on omitted/empty/null custom instructions is important: append a
reset of the **response** scope, without deleting the agent's pinned instructions.
Do not rely on the model inferring that the old instruction silently expired.

Native role history and custom instructions are not interchangeable. User-supplied
historical developer/system items keep their original public/source semantics;
generated context transitions must be identifiable internally and must not be
accepted as trusted policy merely because a client copied their textual marker.

### Tools: preserve capability and authorization, not just cache eligibility

Keep pinned registry definitions stable. For a provider with verified inline
tool-addition/removal support, translate active registry changes into that native
mechanism and preserve its history. For a provider without it, arbitrary changed
schemas cannot honestly promise an unchanged rendered tool prefix. Use a safe
explicit cache epoch/boundary where needed; do not leave obsolete tools callable
or invent unsupported `allowed_tools`. A stable dispatcher would be a separate
API/identity design and is not a free replacement for existing function calls.

Availability policy is always rechecked at dispatch. Approval grants, BYOK
credential deletion, permissions, ownership, account admission and file pins are
not cached authorization. A changed schema for the same name needs a new binding
identity or explicit epoch so a pending call cannot execute under another schema.

## Compatibility, compaction and recovery

- Version the layout for new Sessions. An absent version keeps the old renderer,
  serialized pin bytes/hash and provider binding. Existing pins are not a license
  to change their rendered prefix on a later deployment.
- A same-provider branch imports the exact source-boundary layout, events and
  native history. Never read later events from the live head into an old Response.
- Compaction is an intentional cache epoch. Carry forward the effective policy
  state and protected instructions, replace old transition history with the
  existing compacted generation, and keep immutable source generations readable.
- Do not insert transition items between a tool call and its required matching
  result, or reinterpret a failed/unknown provider turn as accepted new history.
- Append transitions idempotently under the same admitted Response/run operation;
  a cold retry completes publication instead of duplicating the event.
- Exact token counting and generation use the same prepared representation.
  Standalone count is read-only and must not publish events or mutate the cache
  epoch just to obtain a number.

## Discriminating verification and accounting

Use new targeted cases, not a replay of every accepted Responses journey:

- Same instructions, changed instructions, explicit reset and unchanged reset:
  compare actual rendered message/tool prefixes and event counts, not only final
  text. Repeat inside one tool loop to catch per-round prefix mutation.
- Withdraw/replace a tool: no stale dispatch effect; an incompatible provider
  reports an explicit cache boundary rather than false prefix preservation.
- Cold restart, historical branch and compaction: effective instruction state is
  correct, source data immutable, generated events never duplicated.
- Count and inference: exact prepared input equality; count creates no execution
  or persistent transition. Existing legacy pin bytes remain byte-identical.
- A small real direct-DeepSeek trajectory records every provider request ID,
  input/output/cache-read/cache-write tokens, timing and exact known charges.
  Preserve missing fields as unknown. Include initial establishment and any
  deliberate cache-epoch cost, not only favorable warm turns.

Report token-weighted cache reads as `sum(cache_read)/sum(input)` only over records
with known compatible usage; separately report unknown coverage. Also report
uncached input, output and cache-write charges, cost per completed user turn,
TTFT and end-to-end time. Provider routing/expiry can explain a miss even with
identical bytes. Stable-prefix hashes, lengths, layout/epoch and reset reasons
are useful diagnostics, but private instructions and raw prompt bodies belong
to protected user/provider records, not metric labels or public reports.

## Implementation checkpoint

New native Session pins select the versioned layout. The active renderer also
checks model capability: the vendored official DeepSeek recipe preserves middle
system messages for V4.1 Flash, but hoists them for V4 Pro. Direct Pro and unknown
DeepSeek profiles therefore retain response instructions in the compatible legacy
position rather than pretending an appended JSON message preserves their KV
prefix. Anthropic Messages is not silently switched to a new authority renderer.

`prompt_context.rs` constructs a complete replacement snapshot of response-only
instructions, ordered effective tool fingerprints and semantic workspace/runtime
profile. The existing native journal owns the event, its idempotent operation
receipt and trusted item indices. Client text cannot manufacture those indices.
Unchanged snapshots append nothing; `null` resets the response-only scope, while
an empty string remains distinct. No clock, request ID or container-generation
identifier is injected into the stable prefix.

The same pure planner prepares count and generation. Explicit empty Session
counting uses the actual owned pin, including a pre-upgrade legacy pin, rather
than re-resolving the latest Agent. A nonempty unanchored Session is rejected
instead of returning a guessed count; use its immutable previous Response.
Compaction consumes trusted updates as data, retains only the current snapshot
for continuation, and never executes an old response-specific instruction as
summary guidance. Automatic truncation can drop superseded internal snapshots
without dropping ordinary protected system/developer history.

The new compiled acceptance selectors cover instruction changes/reset, exact
count, cold idempotency, tool-schema changes and historical branches, compaction,
and explicit legacy/new Session pins. They use controlled providers and **do not
claim simulated cache hits**. Normal Actions `40517024757`, source
`3799809521a64a287aa10a037e017760228e9422`, passed all seven new Rust guards, four
named compiled-Worker journeys, and four isolated tooling assertions. The
downloaded plan contains only those new checks; dashboard/container artifacts
were reused. The cancelled duplicate PR observation is not a passing run.

### Actual legacy staging baseline

`tests/e2e/prompt-cache-sdk.mjs` submitted three new direct DeepSeek Flash Responses
to staging build `bb18283f80cdc1a9cf87a020de174eb828fc0a8c`, with a fixed architecture
reference and instructions set to ALPHA, changed to BRAVO, then omitted. All three
labels were correct; actual native usage was available through `agents_usage`
(standard `usage` was null because cache-write usage remained unknown).

| Turn | Input tokens | Reported cached tokens | First text (ms) | End-to-end (ms) |
| --- | ---: | ---: | ---: | ---: |
| Set ALPHA | 2,674 | 0 | 6,296 | 10,164 |
| Change to BRAVO | 2,694 | 0 | 6,394 | 9,643 |
| Omit/reset | 2,704 | 0 | 4,988 | 8,184 |

Total: 8,072 input tokens, zero reported cache reads. These are three observations,
not a latency percentile or a guarantee of cold provider cache. The candidate
must reuse the byte-identical saved workload. The protected `.temp/prompt-cache-probe/`
receipt retains original response/session identities and no-retry intent; no
production inference or existing accepted business journey was replayed.

Incremental CI also reuses the downloaded normal-main `40471168005` tooling
receipt for the unchanged draft-release permission check (`passed_current`, one
executed test). It does not rerun that release check as part of cache work.

### Actual append-only staging result

Standard deployment `40519135055` succeeded with the same immutable source,
Worker artifact `12741190507` and all six previously enabled tool features.
Public health returned that exact build. The daemon remained source `3539c1c`
and dashboard `1b7f2cd`; neither was rebuilt or relabeled. Deployment selected no
old paid acceptance profile. The local driver then reused the exact saved
baseline workload SHA-256, not a newly favorable prompt.

| Turn | Input tokens | Reported cached tokens | First text (ms) | End-to-end (ms) | Model charge (microUSD) |
| --- | ---: | ---: | ---: | ---: | ---: |
| Set ALPHA | 2,863 | 0 | 5,986 | 9,307 | 432 |
| Change to BRAVO | 2,976 | 2,816 | 8,747 | 11,580 | 34 |
| Omit/reset | 3,087 | 2,944 | 4,648 | 7,758 | 32 |

All labels were correct and the Session actually pinned `append_only_v1`.
Including the initial cold establishment, actual cache reads were 5,760/8,926
input tokens (**64.53%**); the two follow-ups reused 5,760/6,063 (**95.00%**).
The additional policy/snapshots increased total input by 854 tokens (10.58%), so
this is not a claim of free context or lower first-turn cost.

Exact model ledger rows, independently read for the two owned Session IDs,
totaled **1,216 → 498 microUSD (−59.05%)** for the three completed turns. Both
used `deepseek-2026-10-25-cn2026-v1:offpeak`; each side had three settled model
rows with real provider request IDs, no new reserved holds and no pending
resource observations. Both Sessions closed and their configurations were
deleted. Infrastructure is separately billed; these figures deliberately exclude
agent creation, account-level API overhead and incomparable retained-storage
time, and are **model charges**, not total invoice comparisons. Cache-write
usage remains unknown/null, not zero.

This small paired observation establishes cache reuse and correct scope changes,
not a latency SLO: the second candidate turn routed through FRA and had a much
larger admission delay. The observed end-to-end times do not establish a speedup.
No successful request was replayed. A transient read-only D1 fetch failure was
retried as the same SELECT, without rerunning any model or tool work.

Protected evidence: `.temp/prompt-cache-probe/{baseline,candidate}.json`, the
byte-identical workload, both `*-finance.json` receipts, normal selected-check
artifacts under `.temp/cache-first-ci-*`, and the deployment receipt/provenance
under `.temp/cache-staging-deployment/`. The staging acceptance itself did not
change production; its separate rollout is recorded below.

### Production rollout

After the actual staging usage/ledger closure, protected production deployment
`40521777891` succeeded from the immutable `codex/cache-verified-release` reference
at the same tested `3799809` source and normal CI `40517024757`. It reused Worker
artifact `12741190507` and the already accepted daemon/dashboard artifacts; there
was no promotion rebuild. All six configured tool capabilities remained enabled.
The public production health returned HTTP 200 and exact build
`3799809521a64a287aa10a037e017760228e9422`.

The standard deployment performed its nonbillable credential/dependency checks
and selected `acceptance_profile=none`: no old paid/model, workspace, login,
browser or CLI journey was replayed. No new credential was generated or
intentionally rotated. Existing Sessions retain their original pin/layout; a new
eligible native Session obtains the cache layout. For the local chat example,
choosing “新对话” is enough—no key or agent configuration change is required.
Production deployment provenance/receipt is retained under
`.temp/cache-production-deployment/`.

## Long-context transition experiment — 2026-10-28

The earlier 3K-token startup probe cannot quantify the impact at a mature
conversation length. A new direct DeepSeek Flash experiment used the deployed
Worker `bf0fd027ff685ae501593295a8a77304c14d8455` and a frozen, imported synthetic
reference history from repository source `f27897bd9084a477054c8e782be9387ddd95d7a9`.
The count-only renderer calibrated **109,992 input tokens** across 65 messages
(510,651 reference characters). This was not 65 paid conversations or repeated
padding. Initial model usage independently matched the exact count.

Reproducible runner: [`cache-long-context.mjs`](../tests/e2e/cache-long-context.mjs).
Structured observations, per-operation tariffs/provider IDs and response IDs:
[`prompt-cache-110k-2026-10-28.json`](benchmarks/prompt-cache-110k-2026-10-28.json).
The workload hash is
`638b2eb431f54cf2d4fbc8a1636c6adeed9ecdababbe5af5cef0f43761d582d5`.
Protected full workload/receipts remain under `.temp/cache-110k/` and
`.temp/cache-110k-tools/`.

### Method and controls

- Establish two warm append-only controls around 110K before changing only the
  durable Agent instructions. Keep model, output policy and all tools unchanged.
- Keep the model's output to a short policy label to minimize output variance.
  Output cap is 64 tokens, except the deliberate compaction request's 2,048-token
  budget. Wait five seconds between submissions. Record actual reported cache
  reads, input/missed tokens, SDK first-text timing and separately settled costs.
- **Do not use `tool_choice:none` to claim active tool-schema behavior.** The
  vendored official DeepSeek Responses converter drops tools under that choice
  (`responses/request/tools.rs::convert_tools`). The initial declaration-edit
  observation under `none` therefore measures a configuration notification, not
  the real callable-schema change.
- For the valid tool case, fork the already sealed pre-tool-change long history
  into an owned test Agent. Establish that fork's own warm baseline, then use
  `tool_choice:auto` while changing `[]` to `[time]`, followed by a warm follow-up.
  No tool execution is requested. One settled model operation per response and
  completed label-only outputs distinguish this from a tool-execution benchmark.
- The original accepted Responses are not replayed. Historical context is reused
  locally; the fork has its own measured cold/warm establishment. No
  cross-Session cache-sharing assumption is made.

### Actual cache and model-charge observations

The table deliberately separates the two warm baselines instead of mixing cold
startup into their rates. Prices below are actual model-ledger micro-USD, all
under `deepseek-2026-10-25-cn2026-v1:offpeak`, not a public pricing promise.

| Stage | Input tokens | Cached | Missed | Hit rate | Model micro-USD | First text ms |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Ordinary warm control | 110,034 | 109,824 | 210 | 99.81% | 363 | 9,817 |
| Agent instructions changed | 110,256 | 109,952 | 304 | 99.72% | 377 | 9,704 |
| Instructions follow-up | 110,276 | 110,080 | 196 | 99.82% | 361 | 11,523 |
| Active-tools warm control (`auto`) | 110,517 | 110,336 | 181 | 99.84% | 360 | 4,273 |
| Enable callable time schema (`auto`) | 111,168 | 0 | 111,168 | 0.00% | 16,677 | 5,586 |
| Active-tools follow-up (`auto`) | 111,189 | 110,976 | 213 | 99.81% | 367 | 4,388 |
| Compact: summary + continuation aggregate | 117,636 | 0 | 117,636 | 0.00% | 17,649 | 25,300 |
| First post-compact append | 596 | 384 | 212 | 64.43% | 35 | 11,268 |
| Second post-compact append | 616 | 384 | 232 | 62.34% | 38 | 9,505 |

Instruction changes cost about 0.09 percentage points against the adjacent warm
control, with 304 rather than 210 uncached tokens. Actual model cost increased
by 14 micro-USD, then returned to 361 micro-USD on the next append. This supports
the append-only instruction design at 110K, not merely at startup.

Callable tool-schema mutation **did lose the warm prefix in this route**. The
first changed request cost 46.3 times its warm control, followed by 99.81% reuse
and near-baseline cost. The official vendored Flash renderer explains the
mechanism: `deepseek-recipe-encoding/src/v4/dsv41.rs::render_conversation` appends
the tool prompt to the **first system message**, ahead of the long history.
Appending a trusted configuration event cannot by itself prevent that upstream
schema-prefix change. The disabled-tool observation must not be substituted for
this measured active-tool result. Other models/transports require their own
capability-specific conclusion.

Compaction is not one ordinary generation: its summary operation consumed
117,060 input tokens under a distinct source-as-data transport and cost 17,561
micro-USD; the continuation consumed 576 and cost 88. Both reported zero cache
reads in their aggregate. Later requests missed only 212/232 tokens, comparable
to the long warm controls, despite their much smaller percentage denominators.
The lower post-compact percentage is not evidence of a large ongoing bill.

**Scope boundary:** this is a cache/prefill microbenchmark, not task quality or
summary-fidelity acceptance. The policy-label workload produced only two summary
output tokens and a sub-1K active context. That is not a representative retained
working set for a real 110K engineering task; do not extrapolate its compression
ratio or post-compact total cost to such a task. All updated policy labels were
correct, but that does not establish preservation of the reference corpus.
First-text timings are individual end-to-end observations across AMS/HKG routes,
not latency percentiles or a causal speedup claim. Unknown cache-write usage
remains null, not zero.

There were 14 successful Responses submissions and 15 settled model operations,
including one summary, costing **70,434 micro-USD** in model charges. Resource
charges at the scoped closure observations brought the total to 70,970
micro-USD. Both test Sessions closed, their conversations were archived and own
test Agents deleted; no reserved holds or pending resource observations remained.
No successful operation or previously accepted product regression was replayed.

The next optimization target is the active tool-declaration path, not rewriting
the already effective append-only instruction mechanism. Any capability-specific
incremental loading/masking solution must preserve actual execution revocation;
retaining revoked tools as callable solely to improve a cache metric is invalid.
