# Full Responses contract inventory

This is a bounded contract inventory for the expanded user objective, inspected
on 2026-10-25 against source `6552a93`. It is not an implementation or release
acceptance claim. The former linear-only/"relatively complete" scope in
`responses-spaces-v0.3.md` is historical, not an exclusion for the new objective.

## Contract authority and version boundary

Use the current public OpenAI Responses reference for full OpenAI SDK behavior.
Use OpenResponses as an interoperable core specification, not an interchangeable
snapshot of every current OpenAI extension. In particular:

- [OpenResponses 2026-04-24](https://www.openresponses.org/specification) specifies
  one in-flight response per WebSocket and no multiplexing.
- The current [OpenAI WebSocket guide](https://developers.openai.com/api/docs/guides/websocket-mode)
  supports named `stream_id` lanes, same-lane FIFO and cross-lane concurrency and
  forks. Its limits are 16 active responses, 32 distinct named lanes and a
  60-minute connection. These are materially different contracts.
- The guide calls for Python `openai[realtime]>=3.8.0` and Node `openai>=7.10.0`.
  Keep the existing Python 2.6.1 compatibility tests, but add a current-SDK gate;
  the old package cannot be the completeness oracle for newer types and methods.

Stable Responses/Conversations are distinct from the separately documented beta
multi-agent Responses API, Realtime, Live, Agents, administration, fine-tuning,
standalone audio/image endpoints, and account-specific access programs. A tool
variant in Responses still requires a real capability adapter; acknowledging its
schema without executing/preserving its semantics is not implementation.

The REST create reference's HTML exceeds the web reader's size bound. Its official
Markdown was fetched directly (509,830 bytes) into
`.cache/full-responses-spec/create.md`. Other scoped official pages are cached in
that directory. These caches are source material, not production inputs.

## Resource and transport surface

| Public surface | Baseline | Full contract / acceptance needed |
| --- | --- | --- |
| `POST /v1/responses` | Implemented bounded text/media/functions and generation options | Complete typed item/control coverage; historical parent branches; conversation integration; preserve strict rejection before admission for genuinely invalid requests |
| `GET /v1/responses/{id}` | Immutable stored snapshots and GET SSE cursor replay | Current `include` projections and current response fields; stored/no-store/retention semantics; authentic replay without inference |
| `DELETE /v1/responses/{id}` | Tombstone, no session destruction | Keep deletion receipt/SDK void semantics and independent conversation/file lifetime |
| `POST /v1/responses/{id}/cancel` | Safe cancellation and canonical result | Retain background restrictions/current status/error contract and known/unknown usage boundaries |
| `GET /v1/responses/{id}/input_items` | Ordered bounded pagination | Complete typed items and include projections, not only message/function variants |
| `POST /v1/responses/input_tokens` | Missing | Render the same resolved previous history/conversation, current instructions, tools and media; return `object: response.input_tokens` and authoritative count without generation/admission |
| `POST /v1/responses/compact` | Missing standalone endpoint | Return a compacted input window, **not** a normal response ID/continuation; usage and opaque compaction item must be genuine |
| WebSocket `/v1/responses` | Missing | Native response events, lanes, state/cache/retention, errors, steering, forks, disconnect and connection limits; no second harness |
| Conversations CRUD and items CRUD/list | Missing | Owner-scoped durable conversation identity, immutable response boundaries and defined item mutation concurrency |

Sources: [Responses reference](https://developers.openai.com/api/reference/python/resources/responses),
[input count](https://developers.openai.com/api/reference/python/resources/responses/subresources/input_tokens/methods/count),
[compact](https://developers.openai.com/api/reference/resources/responses/methods/compact),
[Conversations](https://developers.openai.com/api/reference/python/resources/conversations).

### Conversation details that affect state design

`conversation` accepts an ID or `{id}`; its items are prepended to the response
input and both input and output items are added after response completion.
It is not another spelling of `previous_response_id`; do not combine the two
history authorities ambiguously. Creation can seed at most 20 items at once.
Conversation update is `POST /conversations/{id}` with metadata. Items support
create/list and per-ID retrieve/delete under their conversation.
**Deleting a conversation does not delete its items.** Do not cascade away user
history or previously stored responses merely to simplify the schema.

Sources: [create](https://developers.openai.com/api/reference/resources/conversations/methods/create),
[update](https://developers.openai.com/api/reference/resources/conversations/methods/update),
[delete](https://developers.openai.com/api/reference/resources/conversations/methods/delete),
[items](https://developers.openai.com/api/reference/python/resources/conversations/subresources/items).

## Create controls: current public REST inventory

The current reference has 32 top-level fields. This table groups fields rather
than replacing their official schemas with another hand-maintained schema.

| Group | Public fields | Baseline / required delta |
| --- | --- | --- |
| Identity/history | `model`, `input`, `previous_response_id`, `conversation` | Agent-ID model domain is established. Conversation missing; previous history only current head; item references and richer items rejected |
| Instructions/lifecycle | `instructions`, `background`, `store`, `stream`, `stream_options`, `metadata` | Implemented baseline. Audit explicit `null` handling: current bool/map/vector Rust fields reject some documented nullable values. `store` defaults true; streaming/background defaults false |
| Generation | `max_output_tokens`, `temperature`, `top_p`, `text`, `reasoning`, `truncation` | Basic controls exist. `text.verbosity` rejected; reasoning limited to effort; output logprobs/refusals/reasoning/annotations not represented fully. Truncation defaults disabled |
| Tools | `tools`, `tool_choice`, `parallel_tool_calls`, `max_tool_calls` | Function-only tools and simple choice exist; max calls absent. Add custom/namespaces/allowed tool subsets and corresponding item lifecycles. Parallel default is true |
| Projection | `include`, `top_logprobs` | Every nonempty include currently rejected; top logprobs absent. Implement actual data projections, not placeholder arrays |
| Context/cache | `context_management`, `prompt_cache_key`, `prompt_cache_options`, `prompt_cache_retention` | All absent. Public context-management uses token threshold; retain the platform's user-facing percentage configuration separately and resolve it for the pinned model |
| Reusable prompts | `prompt` | Missing prompt ID/version/variable resolution. Reuse a managed versioned registry rather than accepting an unverified ID and ignoring it |
| Routing/safety | `service_tier`, `safety_identifier`, `user`, `moderation`, `access_programs` | Absent. Some are model/provider/account-specific; preserve and enforce supported semantics, report effective values, and fail explicitly for unavailable capabilities |

[Create reference](https://developers.openai.com/api/reference/resources/responses/methods/create).
The Python input-count reference additionally lists `personality`; this is a
source-surface discrepancy worth checking against the current official SDK when
implementing that endpoint, not a reason to invent a create field.

Specific changes hidden by the former allowlists:

- Function names are currently capped at 64. Reconcile the limits of plain,
  namespace and custom tool variants separately rather than assuming one shared
  cap. Latest nested function schemas also describe caller context, asynchronous
  output, deferred loading and output schemas. Their omitted-strictness behavior
  must be checked per variant: some current nested schemas describe attempting
  compatible strict validation with a non-strict fallback, not `None == false`.
- Tool-choice allowed subsets preserve a stable `tools` schema while narrowing
  actual invocation. Enforce the subset in the harness even if a provider ignores
  its hint. Keep client/server names disambiguated without silently changing a
  client's public name or granting permission to shadow a built-in.
- Reasoning includes context, effort (`none` through `max`, model-dependent),
  mode and summary (plus deprecated generate-summary compatibility), not merely
  one effort string. Encrypted reasoning and compaction are opaque state items,
  not text to fabricate or decrypt across providers.
- Cache options include implicit/explicit mode, comparison response, prewarm and
  TTL. Deprecated retention is separate. Explicit content breakpoints and cache
  diagnostics must not alter pinned instructions or leak another owner's prefix.
- Current usage details include `cache_write_tokens` as well as cached tokens.
  Do not invent a zero when a provider does not report the quantity.

## Item and tool unions

Baseline normalization accepts messages, function calls and function results;
text, owned/inline images/files are resolved within those shapes. Public output
is principally text message/function call. The complete item inventory additionally
contains reasoning, compaction and compaction trigger, item references, custom
calls/results, tool search/results, additional tools/configuration updates,
computer calls/results, web/file search, image generation, code interpreter,
local shell and shell, apply-patch, MCP tool lists/approval/calls/results.

The declared tool union contains function, custom and namespace tools; tool search;
MCP; local shell/shell/apply-patch; web search/current+preview; file search;
computer/current+preview; code interpreter; image generation; and programmatic
tool calling. Tool ownership and execution mode are distinct from protocol type.
The root's pending hosting-strategy decision determines provider-backed versus
platform-hosted implementations, not permission to silently drop those item types.

The current typed message-content union inspected here contains text, image and
file input; generic prose mentioning audio must not be used to infer a standalone
audio API requirement. Concrete missing content features include assistant
`phase` (`commentary`/`final_answer`), `input_image.detail: original`, file detail,
content cache breakpoints, output refusals, citations/annotations and logprobs.
The reference accepts fully qualified image URLs and `file_url`; the baseline
rejects arbitrary remote attachments. Implement a bounded, SSRF-safe fetch policy
for those supported forms rather than silently pretending the URL was processed.

Cross-provider handling needs a capability boundary. For example, an opaque
upstream encrypted reasoning item cannot be "supported" by interpreting its bytes
as instructions for an unrelated model. Preserve provenance and fail closed on
incompatible routing. Likewise, image/audio generation requires actual model
support; do not return a plausible empty receipt.

## WebSocket state contract

The current OpenAI transport uses `response.create` without HTTP `stream`,
`stream_options` or `background`; events share the HTTP event model. Distinguish
lane ordering from conversation lineage. Omitted `stream_id` is the default lane;
empty or invalid identifiers are errors. Store-false continuation can use the
connection-local prior response; an unavailable ID returns
`previous_response_not_found`. Same-lane errors evict that parent cache, while a
failed cross-lane fork preserves the source lane's shared parent.

The standalone compact output starts a fresh chain (omit/null parent), whereas
normal server-managed compaction continues the existing parent chain. Enforce
connection/lane/active-response budgets and bounded buffering without using a
volatile socket as execution authority.

### Steering is not ordinary append

`response.steer` accepts only its type, parent response ID and new user input.
Acceptance queues input and produces `response.steer.accepted`; the server waits
for safe output/tool boundaries and creates a successor automatically. An
interrupted response can end incomplete with reason `steered`. Client-owned tool
or approval pauses use `response.steer.pending` and required-input stubs, followed
by an explicit client create with the same parent/lane. Do not send a second model
run merely because acceptance was acknowledged. Current OpenAI model support is
GPT-6-family-specific; the platform's existing safe-boundary steering machinery
can supply its own provider capability policy without claiming upstream support.

Sources: [WebSocket mode](https://developers.openai.com/api/docs/guides/websocket-mode),
[steering](https://developers.openai.com/api/docs/guides/steering),
[WebSocket events](https://developers.openai.com/api/reference/resources/responses/websocket-events).

## Recommended cohesive implementation slices

1. **History and item authority:** immutable response parent graph, explicit
   branch materialization/workspace policy, Conversations/items and ownership.
   Settle this before WebSocket lanes diverge from HTTP semantics.
2. **Typed protocol/provider boundary:** current item/tool unions, controls,
   effective capability routing, faithful output/usage/events and null/default
   handling. Reuse maintained schemas and official SDK contracts.
3. **Auxiliary context APIs:** standalone compact round-trip and exact token
   counting over the same resolved context. Counting must not use a completion
   request as a probe or label the current heuristic as authoritative.
4. **WebSocket transport:** lanes, connection-local no-store cache, create/steer,
   errors and reconnection over the same canonical engine and parent graph.
5. **Acceptance:** retain Python 2.6/Node compatibility and add current official
   SDK coverage; execute cross-lane/historical branches, conversation mutation,
   compaction round-trip, token-count consistency, mixed tool lifecycles,
   cold/no-store recovery, cancellation and exact monetary conservation.

This ordering preserves the existing tested base and lets storage/performance
work optimize shared immutable structures rather than another set of linear-only
special cases. None of the missing slices becomes complete merely because its
request object is accepted or a provider silently ignores it.

## Wiring closure audit: working tree 2026-10-25 12:17 UTC

This section supersedes the **baseline status column**, not the public contract
above. Inspected HEAD `b1edf97746d3902373dbb05e224799f03d9a3a10` plus concurrent
uncommitted subsystem work. It records source wiring, not compiled/staging
acceptance. Active owners can close gaps after this snapshot; use their delivery
notes and selected changed-feature evidence rather than rerunning old baselines.

Legend: **W** = concrete dispatch/consumer exists; **P** = implemented partial path
with a named remaining coupling; **D** = DTO/validation/echo only; **C** = genuinely
provider-dependent effect needs a real capability adapter. D or a global refusal
is not completion of a common portable field.

### All 32 create fields: concrete dispatch status

| Fields | Status and evidence | Next closure / recommended owner |
| --- | --- | --- |
| `model`, `input` | W/P: edge agent ownership and DO admission; attachment resolution and private native input batch wired. Rich reference/opaque classes still fail `unresolved_input`. | State/native: resolve trusted owner references before native preparation. |
| `previous_response_id`, `conversation` | P: branch routes, Conversation admission authority and terminal integration actively being completed by state lead. | State: retain exact immutable history revision, branch/workspace authority and terminal append gate. |
| `instructions` | W: current response's protected-prefix construction; native generation takes current prefix independently from private log. | Preserve compact/branch/steering instruction invariants in changed-feature tests. |
| `background`, `store`, `stream`, `metadata` | W/P: durable response run, observers, background acknowledgement and retrieval/storage lifecycle exist; no-store WS lifetime owned by WS lead. | State/WS: selected no-store/disconnect/lifetime tests, not another full old journey. |
| `stream_options` | W/P: `response_event` reads `include_obfuscation` and adds default-on random padding to delta events. Not DTO-only. | Wire/WS: current SDK retrieval also has independent `include_obfuscation`; retrieval rejects it at snapshot. |
| `max_output_tokens`, `temperature`, `top_p` | W/C: generation options and output-budget admission wired. Chat DeepSeek has mode checks; native path skips them. | Native policy: unify effective model/mode checks without regressing old saved Chat routes. |
| `text` | P/C: text/JSON/schema native forwarding and final JSON/schema validation wired; verbosity forwards only native, DeepSeek explicitly ignores it. | Native policy: capable route for verbosity or honest model boundary; do not echo ignored requested verbosity as effective behavior. |
| `reasoning` | P/C: effort and native summaries/rich output wired. Current native checks do not enforce provider support for context/mode/summary. Public encrypted replay missing. | Native policy + native context: route-bound public envelope/resolver and model-specific supported effects. |
| `truncation` | P: `fit_response_prompt` drops complete legacy message groups for auto and fails disabled overflows; uses working estimate. Private native history remains separate. | Native/context: trim the same prepared/native window and exact count authority; never report successful truncation while sending untrimmed native log. |
| `tools` | P: full unions validate; four hosted families have binding foundation; client custom/namespace native representation and pending code exist. Admission still unconditionally invokes function-only `request.tool_definitions()`. | Hosted/native: first remove this wiring choke point using one resolved tool set shared by generation/count. Additional families below. |
| `tool_choice`, `parallel_tool_calls` | P: legacy simple policy and native none/required/named/single-call checks exist; hosted rewrites available. Native `allowed_tools` subset/mode is not enforced in `validate_policy`. | Tool binding + native policy: actual allowed set governs declaration, native output, execution and continuation. |
| `max_tool_calls` | D/P: validated and forwarded native, no harness-side aggregate counter consumer found. DeepSeek ignores it. | Harness/native: persist per-response built-in call budget, decrement once at accepted tool intent across rounds/retries. Do not count caller function calls incorrectly as hosted budget. |
| `include`, `top_logprobs` | P/C: creation canonical-copy projection and SSE projection are wired; native sanitizer retains reported logprobs/annotations. Retrieve query rejects include and cannot request a new projection. | Wire/state: retrieve include[] against canonical retained data; model capability for logprobs; reference ownership before revealing URLs/ciphertext. |
| `context_management` | D: syntax/response echo and steering rejection only; no execution consumer found. Existing agent percentage compaction is a different policy. | Context/aux: implement requested compaction operation/trigger/opaque item using same actor and exact prepared context; integrate with steering rather than permanent exclusion. |
| `prompt` | D: ID/version/typed variables validated and echoed, but absent from `native_controls`, no registry/resolver consumer found. | Dedicated prompt/registry slice: owned versioned templates resolved at admission, include typed media variables and actual count input, preserve protected-prefix precedence. |
| `prompt_cache_key`, `prompt_cache_options`, `prompt_cache_retention` | P/C: native forwarding exists and prefix pin already helps implicit caches. Explicit provider unsupported controls can silently no-op today. | Native policy/cache: preserve provider-supported cache contract and exact prefix boundaries; support comparison-response ownership, breakpoints/prewarm/TTL with real semantics or specific model boundary. |
| `service_tier` | P/C: native forwarding + requested-value echo, no resolved effective-tier reporting identified. | Native provider: report actual accepted tier from provider or explicitly reject unavailable tier before reservation; request echo is not effective-tier evidence. |
| `safety_identifier`, `user` | P/C: native forwarding only; legacy paths do not consume native-only controls. | Native policy: route-specific documented identity/isolation semantics; do not substitute either for platform auth. |
| `moderation`, `access_programs` | P/C: parsed/forwarded native; no platform moderation execution or entitlement resolver identified. | Native policy: capable configured provider/account or explicit capability refusal; implement portable moderation if advertised platform-wide. Vendor access programs may remain vendor-exclusive. |

Important shared defect: `requires_native_provider()` has **no production
consumer** at this snapshot. Merely setting that predicate does not protect legacy
paths from ignored controls. Conversely, `GenerationOptions::validate_for_config`
early-returns `native.validate_model` for Responses. That validator checks model,
store/history identity and output budget, **not DeepSeek effect support**. A
native-codec allowlist is not a provider capability matrix.

Current official retrieval schema additionally accepts `include` and
`include_obfuscation`; inspected directly in
[official Python ResponseRetrieveParams](https://github.com/openai/openai-python/blob/main/src/openai/types/responses/response_retrieve_params.py).
Current edge `retrieve_query` admits only stream and starting_after. Retrieval
must not mutate the stored canonical item while applying a per-observer include
or padding decision. Existing persisted SSE has creation-time projection; define
replay semantics without inventing events or exposing previously unauthorized data.

### Tool families: execution and canonical item coupling

Hosted lead confirmed four active hosted families; parent requested allocation of
remaining families during this audit. These rows identify required boundaries,
not an instruction to build unrelated hosted VMs.

| Tool family | Current status | Required coherent slice / owner recommendation |
| --- | --- | --- |
| Function | W baseline; schemas/strict validation and call-ID handoff. | Native: preserve nested namespace identity and strict schema validation over shared resolved declaration. |
| Custom text / grammar | P/D: grammar lark/regex shape/size validates; generic native accepts custom; pending string/result pairing exists. No grammar acceptance validator or matching-model guard. DeepSeek documents only custom apply_patch. | Native tools: capable native constrained-format adapter or mature bounded grammar library; do not accept arbitrary grammar on DeepSeek and assume enforcement. |
| Namespace | P: nested function/custom declarations, choice lookup and pending namespace identity exist; function-only admission currently blocks. | Native tools: flatten/rebind stable private aliases when model needs it, preserve public namespace/call identity and count the actual rewritten declaration. |
| Tool search | D: tool/item union and offline tokenizer recognize it, but codec refuses unresolved tool_search. No registry search/load executor. | Registry-discovery worker: server execution loads authorized function/namespace definitions with versioned deferred state; client execution uses typed pending search/result pairing. Never map to web search. |
| MCP | D: secret redaction/DTO only; no mature MCP client, approval state, ownership or dispatch. | MCP worker: existing maintained protocol client, bounded SSRF-safe transport, private credential refs, allowed tools/approval gate and persisted list/call/approval items. No provider-side forwarding under default LLM key. |
| Shell / local_shell | D: unions/environment syntax exist; no native shell pending result or execution mapping. Existing Bash is not automatically the shell API contract. | Workspace-tool worker: map hosted shell to supervised existing workspace; local/client shell remains caller handoff. Preserve environment lifetime, policy, outputs/outcome/limits. |
| Apply patch | D/P: public builtin accepted syntactically; provider codec rejects it. DeepSeek custom apply_patch is a different wire declaration. | Workspace-tool worker: mature patch application/parser + sandbox path policy; adapt native/custom forms without confusing public call/result identity. |
| Computer / computer_use_preview | D: dimensions/actions/results retain payload; no canonical pending computer or supported provider emission adapter. | Client-tool worker: model-capability adapter and caller-executed action/screenshot/safety acknowledgement lifecycle. Full API does not inherently require our own remote desktop service. |
| Programmatic tool calling | D: declaration accepted, no execution capability. Hosted `allowed_callers` rejects unsupported caller kinds and preserves intended rights. | Hosted interpreter/native: only advertise when actual sandbox invokes authorized bound tools; enforce caller rights at dispatch, not merely hidden declarations. |
| Code interpreter / image generation / web search / file search | P: real adapters and pure binding/runtime foundations; authoritative details in corresponding documents; integration active. | Hosted lead: shared resolved binding -> execution intent -> known usage/artifacts -> canonical item/events; outer native provider must not bypass it. |

Concrete central limits at snapshot:

- `provider_responses::validate_tool` permits only function/custom/namespaces;
  correctly refuses all unresolved hosted/client builtins instead of letting
  DeepSeek silently ignore them. Add a real adapter, not an allowlist exception.
- `GeneralPendingCall::from_item` and `validate_returns` handle only
  function/custom. Extend one typed pending-state model for computer, shell,
  local_shell, apply_patch, MCP approval and client tool_search, with exact
  expected result discriminant and call/approval identity.
- `native::completion_failure` currently permits only message/reasoning/function/
  custom native outputs; additional families fail `native_output_binding_required`.
  Their output projection/SSE and terminal pause must be integrated together.
- Native `validate_policy` verifies allowed declarations/strict functions but not
  `allowed_tools` subsets, grammar validity or deferred activation. Do not accept
  these policies on a model merely because it parses their JSON.

### Item classes: storage is not execution

| Items | Current source status / closure |
| --- | --- |
| Message, phase, text/image/file content | Original native item retained; attachments resolved; canonical message phase/refusal/citations/logprobs and typed deltas implemented. Cross-check file-variable/media freezing through shared preparation. |
| Function/custom calls and outputs | Typed native pending pairing implemented; nested namespace preserved; admission/binding choke point above remains. |
| Reasoning | Private log has owner/provider/base/model/credential binding. Canonical output intentionally removes raw reasoning and encrypted_content; public encrypted include cannot restore it. `unresolved_input` rejects reasoning, compaction, item_reference. Need sealed platform-owned opaque envelope + trusted resolver, not upstream ciphertext passthrough. |
| Compaction / compaction_trigger | Authenticated compact helper exists; actor/route integration active. Trigger has no execution semantics in inspected code; private native replacement currently recreates legacy message projection. Preserve rich/opaque context or document the exact valid transformation. |
| Item reference | Validates only ID then blocked at native admission. Resolve retained owner item with original trusted private provider context; never ask shared vendor account to resolve arbitrary user ID. |
| Search/interpreter/image outputs | Four hosted family projection integration active; retaining DTO alone does not execute a tool. |
| Computer/shell/local_shell/apply_patch call/results | Broad wire object acceptance only; no complete typed relationship/effect validation. Add per-family bounded types and shared pending lifecycle before admission. |
| MCP list/call/approval request/response | Approval response validates some keys; no expected-approval relationship/transport discovered. MCP worker must bind approval to the exact owned server/tool arguments. |
| Tool search / additional_tools / configuration | Payload preservation only at snapshot. Registry changes must be owner/version scoped, activate at explicit boundary and feed the subsequent prepared prompt, not silently modify fixed session prefix. |

### Priority integration order

1. **One preparation/capability choke point** (captain + native + hosted): resolve
   prompt/template, references, media, active/deferred tool set and all effective
   policies before financial admission; remove function-only choke point. Share
   resulting immutable input/tool representation with exact counting.
2. **Close common portable controls** (dedicated control/registry workers): prompt
   versions/variables, real context_management, harness max_tool_calls,
   allowed_tools, native truncation synchronization, retrieval include/padding.
   These cannot be declared done by rejecting every caller request.
3. **Typed caller/server tool families** (hosted subteam): shell/patch and registry
   discovery in parallel with MCP; computer caller lifecycle through same pending
   model. Provider capability adapters must emit or translate real calls.
4. **Opaque and current usage** (native/context): owner-bound encrypted replay,
   item references and current cache-write usage. Existing private log retention
   is necessary but not public include/round-trip implementation.
5. **Selective new-feature acceptance**: real current-SDK prompt variables,
   retrieve different include, constrained custom call, deferred registry load,
   approval/result pause, exact count with tools and native truncation, plus
   limit/accounting failure tests only where changed. Reuse already passing
   baseline receipts with exact source/dependency fingerprint; no gratuitous
   old paid/model/UI journeys.
