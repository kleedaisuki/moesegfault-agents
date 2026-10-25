# Responses hosted tools: execution, credentials and artifacts

This is the active integration contract for the expanded full Responses scope,
not a deployment or acceptance receipt. The user explicitly requests platform
Code Interpreter and image generation with BYOK support. The later authoritative
clarification additionally permits platform DeepSeek Search, requires default
platform file search, and selects Seedream 5 for default platform image generation.
Search BYOK remains supported. Existing time/Bash and workspace-version tools
remain available under their established contracts.

## One harness, separate tool services

The Session DO still owns model execution, external-operation fences, accounting,
workspace lifetime and the canonical response. Hosted tools are implementations
behind that harness, not another agent runner. A provider-facing reserved function
alias selects each implementation when the selected outer LLM does not natively
emit the hosted tool. Public output uses the native hosted item/event types, not
a client `function_call` handoff.

| Native tool | Private provider alias | Execution boundary |
| --- | --- | --- |
| `code_interpreter` | `__agents_code_interpreter` | Managed Jupyter kernel in the session's physical Container |
| `image_generation` | `__agents_image_generation` | Worker-held platform or owned BYOK image-service route |
| `web_search` / preview aliases | `__agents_web_search` | Dedicated platform DeepSeek Search/Fetch, or explicit owned BYOK route |
| `file_search` | `__agents_file_search` | Owner-scoped Space collections and real lexical/semantic indexes |

These names are reserved against caller-local tools. Admission resolves and
validates the declaration and service binding before paying for outer inference.
An LLM spelling an alias does not grant that capability: dispatch requires the
same response's declared, resolved binding. Native-capable outer providers must
not bypass platform service/billing/credential policy by executing the hosted
tool themselves.

## Credential and configuration boundary

The outer LLM and a paid tool service are independent routes. A DeepSeek outer
model may use an owned OpenAI image/search credential, or a platform image route.
No API key is part of a tool definition, model prompt, Container environment,
public response or telemetry.

The proposed additive `AgentConfig.tool_services` and corresponding pinned field
hold only service kind, immutable credential ID, model/capability policy and
resource bounds. Empty values must be omitted when serializing old pins so
previous prefix hashes remain stable. The root coordinates these core fields;
individual adapters do not alter credential tables or generic LLM protocols.
Existing encrypted OpenAI-protocol credentials can serve the first image and
native-search adapters without relabeling a search API as an LLM endpoint.

Search never falls back to an unrelated provider key. The platform default uses
DeepSeek's dedicated Search/Fetch JSON API rather than another language-model
call. Explicit BYOK supports that service or a configured OpenAI-compatible
Responses search service. Native search/open/find actions, sources and answer
annotations remain meaningful; unsupported provider-specific location/cache/image
options are not silently dropped. Each dedicated request retains its exact known
successful-nonempty request unit. The official tariff is USD 0.75 per thousand
such requests after 2026-11-01 00:00 Beijing time; the preceding advertised free
window has a zero provider charge, not invented zero token usage. Account
capability and configured service availability still require staging acceptance.
Annotation offsets in that service's answer must not be copied onto a different
outer model answer. Only known evidence URLs/markers can be anchored in the
actual final text.

Image generation supports a verified platform route and an independently chosen
owned BYOK route. Current OpenRouter has dedicated image generation and model
catalog endpoints; a deployment must select an actual catalog model instead of
assuming a text-model alias can generate images. Model capability and pricing
snapshots remain explicit. The public catalog inspected on 2026-10-25 lists
`bytedance-seed/seedream-5-0-flash`, provider `seed`, with one output image priced
at USD 0.018 and no input-image fee. It advertises 1K/2K resolution and up to 14
references, without streaming or transparency/quality controls. This is the
user-selected default family; no GPT default is substituted. Modern OpenAI image
BYOK routes retain richer edit/mask/partial-image capabilities when supported.
A catalog entry is not an execution-success claim. Exact deployment selection,
pricing reservation and staging acceptance are still required.

## Code Interpreter lifecycle

The logical Python container is separate from the Bash process namespace while
sharing the physical CF Container and operation gate. Its managed Jupyter kernel
and code directory use UID 1001; Bash remains UID 1000. Exempting an untrusted
same-UID kernel from Bash's existing quiescence would let background threads write
during snapshots and is forbidden.

All UID 1001 processes are paused between cells and before immutable artifact
capture. Imports occur while paused. The supervisor alone resumes/kills that UID.
Timeout, explicit cancellation, kernel death or reset invalidates the logical
container ID; a stale ID must not silently receive a fresh namespace. The code
scratch filesystem and Python memory have the logical container's volatile
lifetime, while exported artifacts are owner data in Space. Files must remain
discoverable/exportable before expiry; limit errors cannot silently drop files.

The Rust supervisor provides accepted execution IDs and GET result lookup. The
Python bridge uses `jupyter_client`, not a home-grown ZMQ implementation. Provider
and R2 credentials never enter it. The planned internal endpoints are
`POST /v1/python`, `GET`/`DELETE /v1/python/{execution_id}`, attachment import and
opaque immutable artifact download. They retain the same authenticated supervisor
and shared execution/checkpoint gate. Publication-pending known artifacts must be
resolved before intentional physical release.

A request for Code Interpreter is an explicit per-response compute capability;
it must not flip `PinnedContext.workspace` and accidentally inject Bash or rewrite
the stable prefix. Funding/preflight and cleanup must account for the actual
physical Container even when Bash workspace was disabled. Requested native memory
profiles must be checked against real configured Container capacity. The current
platform SKUs are `1g` and `4g`; `16g` and `64g` must fail admission before compute
or model reservations. A request for `4g` also fails if its selected physical
mapping cannot supply it. A label is not an allocation by itself.

## Known outcomes, publication and billing

1. Persist a tool execution intent keyed by run, round and provider call ID.
2. Dispatch the declared implementation once.
3. Persist its known outcome (including usage/cost and artifact source) before
   settlement or publication. An unknown paid result is not retry permission.
4. Publish each known artifact into the owner's Space through the existing quota,
   storage-clock and failure-recovery boundary.
5. Publish canonical hosted items/terminal state, then release references and
   finish accounting normally. Client transport does not own the task.

`space::save_bytes_idempotent` already provides the artifact publication primitive.
`hosted_artifacts.rs` exposes a small safe reference and uses a stable execution
plus ordinal key. **Content is not part of that idempotency identity**: changed
bytes for an existing slot must conflict rather than create another file after
an ambiguous reply. Existing owner checks, storage debt, plan quota, R2 accounting
and retained-byte registration remain authoritative. No fake authenticated HTTP
upload or unmetered R2 write is introduced.

The initial materialization bound is 16 MiB per artifact, matching that primitive.
The daemon may return a bounded collection of descriptors; the Worker should
materialize/publish one at a time, not buffer every artifact simultaneously.
Published references never expose R2 object keys, supervisor paths/tokens or owner
identity. A known artifact that cannot yet be published stays recoverable; it does
not justify rerunning Python or generating another paid image.

Images differ from ordinary Files artifacts: the canonical response retains an
immutable owner blob independently of optional Files metadata. The prepared
`image_artifacts.rs` helper creates a small serializable candidate identity for
the harness to persist, consumes the pre-provider physical-byte capacity hold,
uploads with R2 SHA-256 verification, registers the actual physical storage clock,
and returns the registry's ready deduplication winner. Retry after an ambiguous
PUT reuses the persisted candidate and checks its existing checksum rather than
creating a new object key. Each actual HEAD/PUT/GET attempt has a distinct durable
resource-observation identity. A known R2 failure is still a request observation;
an interrupted observation remains pending for reconciliation.

The caller binds that ready blob to the response/item before releasing a remaining
capacity hold or publishing a final result. A Files-count limit must not discard
the already paid canonical image: linking is optional, and `file_id` exists only
after an actual successful link. A bounded `file_link_pending` diagnostic may
describe a retryable metadata failure without regenerating bytes. User-deleted
Files links and deleted Responses must never be resurrected by late publication.
The file link and response each hold their own strong reference; only registry GC
deletes physical bytes after all applicable holders and leases are gone.

Canonical response/event storage keeps a private blob reference, not repeated
base64 copies. Public hydration authorizes its live Response first and then uses
the helper's bounded, SHA-256-checked read; knowledge of a blob key alone is not
download authority. The helper and its three pure tests are prepared for the next
compiler checkpoint, not wired or accepted as a working public image journey.

BYOK upstream model/tool fees are paid to that provider by the user; platform
model debit remains zero as in existing BYOK inference. Platform infrastructure
is still metered. Platform-funded image generation needs its own adequate cost
reservation and exact reported provider charge or an authoritative versioned
tariff; do not invent a per-image price from a blog example or image dimensions.
Resource-only artifact/read work must not mutate a settled outer-model hold.

File search uses real D1 FTS5/BM25 and configured embedding/vector retrieval, not
keyword ranks mislabeled as semantic similarity. Vector results must be filtered
by owner/store and rechecked against the current D1 file SHA, indexing generation,
membership and tombstone before returning text. Derived extraction/index state is
recoverable intermediate state; the owner-authorized R2 source remains authority.
Embedding tokens are not estimated from character counts when the service reports
no token usage. AI/Vectorize bindings, real model dimensions, resource tariff units
and static token-aware chunking require their own configuration/verification.

## Wire, events and provenance

The public items are `code_interpreter_call`, `image_generation_call` and
`web_search_call` with standard statuses, output formats, actions and event names.
Private aliases, API endpoint keys and workspace implementation paths do not
escape into those objects. Tool progress joins the existing response's ordered,
durably fenced event identity. In particular, image previews cannot cause an
unbounded duplicated base64 journal; intermediate preview policy is distinct
from the retained final artifact.

Operational events contain only allowed run/session/call IDs, tool kind, outcome,
bounded counters and pricing/provenance references. Queries, code, generated
images, logs and search results are user data, not telemetry.

## Sources and current implementation ownership

- [OpenAI Code Interpreter](https://developers.openai.com/api/docs/guides/tools-code-interpreter)
- [OpenAI image generation tool](https://developers.openai.com/api/docs/guides/tools-image-generation)
- [OpenAI web search tool](https://developers.openai.com/api/docs/guides/tools-web-search)
- [DeepSeek dedicated Search/Fetch](https://api-docs.deepseek.com/guides/search/)
- Existing [workspace supervisor contract](workspace.md), [Space](space.md),
  [billing](billing.md), and [provider credential contract](byok-protocols.md).

The tool workstream owns new image/search/artifact/host-dispatch modules. Its
interpreter leaf owns new daemon interpreter/process modules, the typed Jupyter
bridge and minimal daemon route/state hooks. The full-Responses leader owns wire
admission/output and shared provider dispatch integration. Runtime and workspace
performance owners retain their agreed storage-helper/download files. Root alone
commits, runs full CI and deploys; passing adapter fixtures is not live acceptance.
