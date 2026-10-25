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

## Integration checkpoint: 2026-10-25

The captain owns the index; no delegated lease is active. Foundation commits
`34ab493` (supervised interpreter) and `e4bf137` (protocol/state foundations)
are on the feature branch. Normal CI `40190532245` passed tooling/dashboard
but exposed a pure wire-module import error, daemon Clippy failures, and a
real-container Matplotlib display failure. These are not acceptance evidence.

Hotfix `e9b73a5` removes the runtime credential dependency from pure attachment
validation, normalizes bounded MIME whitespace before strict base64 validation,
and retires killed interpreter contexts without discarding unpublished artifacts.
The latter closes independently reviewed eight-reset resource exhaustion. Ten
bridge tests passed locally; normal CI `40191551856` is the compiled/container
gate. It contains only completed hotfix files, not concurrent feature work.

Next integration ownership is explicit:

- State lead: Conversation admission-to-Response coupling and terminal append.
- Native context lead: rich canonical output/events and general pending tools.
- Hosted-tools lead: secret-free preflight bindings, runtime execution, artifacts,
  and sole manifest/lock ownership.
- Auxiliary lead: public count/compact context preparation and route dispatch.
- WebSocket lead: authenticated public upgrade and the shared-harness executor.
- Pricing/tokenizer workers: versioned direct-DeepSeek tariffs and the official
  offline prompt encoder/tokenizer. Existing receipt and pinned-prefix bytes
  must remain stable. In particular, upstream `serde_json/preserve_order` cannot
  unify into the platform dependency; any ordered JSON dependency is isolated.
- Release operations: current/legacy SDK acceptance, pre-paid routing guards,
  and an operator-only single-attempt usage-capability probe. No paid execution
  or deployment is authorized by source readiness alone.

The next meaningful milestone is a coherent compiled slice with real route
journeys, not a count of independently completed modules. Production promotion
and public release remain root decisions.
## Incremental verification policy

The user's subsequent instruction replaces the default full-regression gate:
reuse previously passing behavior unless there is concrete failure evidence.
Every checkpoint must select new/changed behavior and targeted regressions for
actual failures. Compilation, type checks and artifact builds still establish
that the changed artifact is usable; skipped old tests are never described as
passing at the new SHA.

The incremental-CI owner records deterministic selectors, the exact candidate,
and reused baseline/component-dependency fingerprints. Release operations uses
the same evidence distinction for staged journeys: do not repay for old core
journeys merely to aggregate newly collected tool evidence. New Responses
state, rich items, SDK schemas, WebSocket, auxiliary routes, hosted execution,
pricing and tokenizer paths remain unaccepted until their own meaningful
incremental checks execute.

The in-flight `40191955285` run is allowed to finish under its original plan.
Subsequent gates use the incremental plan. The preceding `40191551856` run
passed Rust, real-container, tooling and dashboard checks; its Worker test build
revealed two missing compatibility-fixture fields, repaired by exact two-line
commit `b1edf97` without including concurrent runtime work.
## Accepted interfaces before the next compiler checkpoint

Foundation `b1edf97746d3902373dbb05e224799f03d9a3a10`, run `40191955285`,
built the native Worker and passed 363 library tests. Its real Worker suite had
55 passing checks and three failing leaf cases (five including parent suites).
The independently passing workspace-blob and historical/concurrent-branch
selectors are reusable evidence; the failed aggregate is not a release gate.

Two fixture defects were diagnosed at the actual boundary: the installed Node
SDK did not send `Idempotency-Key` for its convenience option, and its parsed
object augmentation was compared with raw HTTP output. Repairs explicitly send
the header and compare complete raw snapshots before/after forks. A real
compatibility regression was also repaired: tool-choice availability is checked
after injecting the actual server registry, rather than against only client tools.
Only these documented failures and new behavior are selected in the next gate.

Independent review found and drove repairs for steering prompt reapplication,
non-atomic source-end/exclusion sampling, operation-independent compact item IDs,
and omitted effective template instructions. Source-level closure is distinct
from the upcoming compiled journeys.

The approved physical runtime profiles are legacy `basic` (1 GiB) and a new
`Session4G` thin Durable Object wrapper using the exact same Session engine in a
separate `standard-1` Container application. Binding `SESSION_4G` and migration
`0018` carry trusted persisted routing. Append-only Cloudflare migration
`v3-runtime-profiles` must not rewrite `v2-full-responses`. Both applications use
the same immutable daemon image. Existing basic pins, live leases and tariff
bytes remain unchanged. New 4 GiB allocation is published at 21 microUSD per
container-second (`2026-10-25-platform-standard4g-v1`), a platform reserved-capacity
rate, not a claim about actual active-CPU vendor invoices.

Two later safeguards have separate owners and must close before their relevant
paid staging gates: independent retained-Conversation logical quota (initial cap
uses existing plan storage-capacity values, never merges Space physical storage),
and migration `0019` separating trusted local-execution capacity from unknown
financial holds. Healthy long operations do not expire from elapsed lease time.