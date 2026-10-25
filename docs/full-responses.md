# Full Responses implementation work graph

The user's expanded objective supersedes the earlier relative/linear-only scope.
This document coordinates implementation; it is not a completion or release claim.
The current public contract inventory is [full-responses-contract.md](full-responses-contract.md).

## Authority and integration

- `responses_full` is the integration captain: shared index, coherent commits,
  feature CI, and cross-subsystem interfaces. Root owns user/global decisions,
  final production promotion and public release.
- Only one index writer may act at a time. An explicit delegated lease permits a
  subsystem lead to commit its complete isolated increment. Working-tree feature
  changes remain intact; never checkout/reset merely to construct a test tree.
- Existing userspace contracts, user source journals, financial vintages,
  immutable results, owner checks and last-user-append clocks are invariants.
- Heavy compilation and broad checks run in Actions. Local verification is
  limited to inexpensive probes, metadata/syntax/property setup and real journeys.

## Cohesive workstreams

| Workstream | Lead / owned boundary | Next useful checkpoint |
| --- | --- | --- |
| Baseline observation | `native_error_diagnosis`: terminal followers/private subscription and Chat relay | Healthy long task observation has bounded reconnection, not an arbitrary task-duration deadline |
| State / branches / conversations | `branch_conversations`: immutable boundaries, deterministic child sessions, Conversation authority, migrations 0015/0016 | Real text forks + model-independent Conversation CRUD, then trusted workspace/native-context references |
| Native model codec | `native_provider`: native Responses transport, credential-family separation, provider mapping | Existing text/function harness using native provider with known usage and no upstream history authority |
| Native context | `session_native`: operation-fenced append/replace native item log and reference boundaries | Preserve opaque reasoning/rich items without public Message metadata leaks or whole-history snapshot copies |
| HTTP auxiliaries | `auxiliary_api`: count/compact contracts and account-scoped model operation | Genuine native/matching count and durable stateless compact, never a fabricated user Session |
| WebSocket | `websocket_api`: protocol/lane scheduler and native socket gateway | Same harness executor, 32 named lanes / 16 active cap, FIFO, forks and steering |
| Hosted tools | `hosted_tools`: interpreter, image generation, web/file search and artifacts | Executed tools with correct public items/events, service isolation and physical storage references |
| Runtime / storage performance | Parent `perf_runtime` / `perf_space` leads | Measured clone, redundant-write, archive-read and physical-byte improvements without contract changes |
| Clients | Parent `perf_clients` | Measured query/render/stream improvements and progressive recovery |

## Global interface constraints

1. Wire transport is separate from credential authentication family. Native
   Responses can reuse an owned OpenAI-family credential at the same immutable
   origin; do not add a fake credential protocol or silently change old pins.
2. The Session DO remains the execution authority. Upstream native Responses is
   a codec/provider operation, not another agent harness or conversation store.
3. Historical/racing/cross-agent continuations create independent child Sessions.
   Current-head same-agent continuation remains a fast path. A workspace is
   sealed before its response boundary can become a trustworthy fork source.
4. Conversation identity is model-independent. Accepted history revision and new
   input are distinct; append only new input/output at terminal, never duplicate
   the conversation's previously prepended history.
5. Hosted output blobs and workspace roots share an owner-scoped immutable blob
   registry with explicit strong references. Deleting a user file must not erase
   a retained response image or a historical workspace anchor.
6. Protected server/agent instructions remain pinned. Request customizations
   survive compaction during that response, but explicit later requests do not
   accidentally inherit a previous top-level override.

## Current user service policy

- Platform language models: current verified DS4.1 Flash / DeepSeek Pro through
  DeepSeek or OpenRouter routing; existing pinned aliases stay immutable.
- All paid language-model tests use direct DeepSeek, never OpenRouter.
- Default image generation uses our OpenRouter Seedream 5 route; BYOK remains
  supported. Image tests may use that image route after the staged test gate.
- Platform web search can use DeepSeek's genuine Search/Fetch service, with BYOK
  alternatives. Function/namespace `tool_search` is registry discovery, not web
  search. Platform Space file search is a separate first-party capability.
- Code interpretation runs in our supervised workspace service. Provider keys,
  R2 credentials and supervisor secrets never enter user code environments.

## Completion gates

Every advertised stable Responses field, item/tool variant and endpoint needs
implemented semantics and provider-capability boundaries, not only a permissive
schema. Preserve older SDK compatibility while testing a current SDK for newly
introduced surfaces. A source-only review or prior narrow baseline is not full
acceptance. Required end state includes real SDK/lane/fork/steering/tool journeys,
state and financial recovery, measured performance/storage results, exact tested
deployment artifacts, native clients, and production/public-release clearance.
