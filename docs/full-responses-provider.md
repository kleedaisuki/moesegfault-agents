# Native Responses provider boundary

This is the provider-codec foundation for the full Responses workstream, not a
claim that the complete public contract or a new deployment has passed acceptance.
The current Session DO remains the authority for response IDs, conversation and
workspace lineage, instruction pins, execution fences, public events and billing.

## Private codec interface

`provider::responses::PreparedRequest::new` takes fully resolved native input,
resolved caller/harness tool declarations and generation controls. It never
accepts provider conversation IDs, `previous_response_id`, `background`, metadata
or a caller-selected upstream model. Generation always sends `store:false`.
`payload(stream)` and `count_body()` use the same input/tool representation;
counting cannot silently tokenize a different Chat projection.

Hosted capabilities must be resolved by the harness before preparation. The codec
accepts function/custom and their namespace declarations, not raw web search,
image generation, code interpreter or MCP tools. This prevents a default model
credential from silently buying an upstream search or bypassing the platform's
tool-service routing, resource limits and cost ownership.

`NativeCompletion` contains the existing normalized text/function `Completion`
and a separate `NativeTurn`; adapter dispatch moves that turn into the defaulted
`Completion.native` sidecar without duplicating it. The latter retains all upstream output items, raw
usage details, optional cache-write count, terminal status/error and incomplete
details. Reasoning/encrypted items, annotations, refusal parts and future rich
item types are not stuffed into `Message.extra` or exposed by serializing an
upstream response. The harness must project these items to its own stable IDs and
public policy, and retain opaque continuations with owner/provider-route binding.

The parser preserves reported cost as its original JSON number. Missing usage or
an absent cache breakdown is `reported:false` in the compatibility projection;
absent reasoning/cache-write detail stays absent rather than a fabricated zero.
Full-contract integration can consult the original `NativeTurn.usage_details`
for partially reported counts. It must not price an unknown cache breakdown as
an authoritative cache miss.

Native streams terminate on an authentic response terminal event, not `[DONE]`.
Their sequence numbers and response identity must remain monotonic/stable. Text
deltas are checked against the corresponding terminal output parts. Upstream
events are private inputs to harness event projection: an upstream completion is
not proof that the whole agent run completed or that server tools executed.

## Provider facts and integration boundary

[DeepSeek's current official guide](https://api-docs.deepseek.com/guides/responses_api/)
documents native `/responses` and semantic events. Its compatibility table is
explicitly partial: state/storage/background are unsupported, several parameters
are silently ignored, custom tools are limited to `apply_patch`, and hosted tool
types are largely ignored. Native format support therefore does **not** imply
that every requested policy is implemented by the provider. The harness must keep
its own tool-count/selection/schema policy, and capability validation must reject
or implement unsupported effects rather than forwarding an ignored field and
claiming success. The existing DeepSeek Chat behavior must not silently change.

[OpenAI's migration guide](https://developers.openai.com/api/docs/guides/migrate-to-responses)
documents stateless use with `store:false` and preserved reasoning items. Do not
decrypt, manufacture or replay an opaque item through a different provider route.
OpenRouter's [official API overview](https://github.com/OpenRouterTeam/docs/blob/main/api_reference/overview.mdx)
links its live [OpenAPI schema](https://openrouter.ai/openapi.json). A public,
credential-free fetch of that schema exposed `/responses` and
`/presets/{slug}/responses`, not a documented token-count route. The observed
path list is retained at `.cache/full-responses-provider/openrouter-response-paths.json`.
The online create-reference route returned 404 during this inspection, so no
additional per-model support is inferred from an old search snippet.

Native routing is explicit through optional `ModelConfig.protocol="responses"`.
Omitting it preserves the existing adapter and serialized pin bytes. Credentials
remain OpenAI or Anthropic authentication families: an existing OpenAI-family key
at its immutable origin can serve Responses; selecting an Anthropic-family key
for that wire transport is rejected before generation. Key rotation cannot flip
credential metadata into a new route. No credential-table migration is needed.

`provider::resolve_config(env, owner, model)` is the shared operation-boundary
resolver. It returns secrets only in the non-serializable `ProviderConfig`;
Session adds its own trace/sticky-routing context. Auxiliary count/compact actors
use the same resolver rather than cloning credential policy.

The normal provider dispatcher now supports native text/function completions and
semantic native terminals, using the existing text-delivery/coalescing mechanism.
`GenerationOptions.native` is a trusted fully rendered override for Session's
private native history; its model/output budget and stateless constraints are
revalidated before dispatch. A legacy-message fallback supplies deterministic
call/result aliases only when that private context is absent. Newly accepted
logical inputs/results can use `message_items` without changing native call IDs.

Session persistence of `NativeTurn`, owner/provider-bound native history and rich
public item projection remain coordinated integration work, not implied by a
successful text projection. A normalized text-only completion must never replace
rich/custom items or opaque reasoning context. Existing OpenAI Chat and Anthropic
adapters remain unchanged for old pins.

## Verification boundary

### Provider control effects

`provider_controls.rs` validates native effects both when building a prepared
request and when reusing one. DeepSeek's documented native format does not imply
support for cache controls, vendor tiers, safety identifiers, moderation/access
programs, or reasoning summaries. Unsupported explicit requests fail before
reservation rather than silently succeeding. Its supported `user` isolation,
structured output, logprobs, and model/mode-specific sampling remain available.
Verbosity is adapted into a response-scoped system instruction in the exact
counted input, leaving the protected prefix unchanged.

`HarnessControlEffects` allows trusted Session preparation to attest real
counter/window/single-call enforcement; defaults attest nothing. Merely accepting
`max_tool_calls`, automatic truncation, or single-tool policy in request JSON is
not proof of an implementation. Active integration owners must supply these
effects only after their corresponding runtime behavior is connected.

OpenRouter's live public `ResponsesRequest` schema is retained in
`.cache/full-responses-provider/openrouter-response-request.json` for the
bounded compatibility check. Explicit OpenAI/custom native BYOK routes retain
their provider controls; the platform does not assume every OpenRouter route
supports OpenAI-only extensions. Native actual service tier is preserved
separately from the requested value for canonical response projection.

Sources: [DeepSeek native compatibility](https://api-docs.deepseek.com/guides/responses_api/),
[mode effects](https://api-docs.deepseek.com/guides/thinking_mode/),
[OpenRouter schema](https://openrouter.ai/openapi.json), and
[OpenAI create contract](https://developers.openai.com/api/reference/resources/responses/methods/create).

### Incremental native delivery

`complete_stream_with_options_and_native` adds a separate private semantic
callback without changing legacy Chat text callbacks. It delivers accepted item,
content-part, text/refusal, reasoning-summary, annotation, and function/custom
input events before the provider's terminal response. Session assigns canonical
item IDs, output ordinals, and event sequence numbers; upstream lifecycle events
and sequence numbers are not public events.

The typed `SemanticDelta` allowlists public fields, including nested citation
and logprob fields. It excludes provider metadata, raw reasoning content,
encrypted continuations, and hosted items requiring a separate service
authority. A zero-delay cooperation point after delivery lets the Worker stream
reader observe the first accepted semantic event without waiting for a whole
provider turn. Native terminal reconciliation checks item identities and
text/refusal/summary/function/custom fragments, including done-versus-delta
agreement. Usage accounting and terminal failure handling remain Session's
responsibility; incremental publication does not imply successful completion.
Semantic-integrity disagreements suppress subsequent semantic callbacks while
the provider drains. A valid terminal carrying known usage is returned as a
failed `NativeTurn` with a safe protocol error rather than discarding its usage.

Additional pure tests cover early delivery without a terminal turn, private-field
suppression, tool/part event coverage, and inconsistent semantic fragments.
These tests are added for Actions, not claimed as locally executed.

Focused pure tests cover preparation/count context identity, forbidden state and
hosted capabilities, output-budget ownership, private-item preservation and
redacted Debug output, exact known usage/cost with absent details, failed terminal
preservation, native terminal-without-DONE, sequence/identity and text mismatch.
Formatting and diff checks are inexpensive local checks. New Rust compilation,
actual Wasm HTTP fixtures and live provider acceptance belong in Actions and the
root's controlled staging gate; none is claimed by this note yet.
