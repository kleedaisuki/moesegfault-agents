# Responses compaction and input-token counting

Implementation slice owned by the full Responses workstream. This note records
the public contract and integration boundary; it is not endpoint acceptance.

## Contract grounded in primary sources

- [Standalone compaction](https://developers.openai.com/api/docs/guides/compaction)
  accepts a complete window or `previous_response_id` and returns
  `object: response.compaction`, an opaque ID, `created_at`, `output` and usage.
  Its output is the complete next input window, not merely a generated assistant
  reply. Callers must be able to pass that window back into Responses unchanged.
- [Compact reference](https://developers.openai.com/api/reference/resources/responses/methods/compact)
  also defines custom instructions, cache key/options/retention and service tier.
- [Token counting](https://developers.openai.com/api/docs/guides/token-counting)
  counts the actual rendered model input, including message boundaries, tools,
  images and files. Counting JSON bytes or dividing characters by four cannot
  implement this endpoint.
- [DeepSeek Responses](https://api-docs.deepseek.com/guides/responses_api/)
  now documents a native Responses endpoint, custom tools, tool search, reasoning
  items and Flash images. Its public guide does **not** document a standalone
  compact or input-token-count endpoint.
- [DeepSeek token usage](https://api-docs.deepseek.com/quick_start/token_usage/)
  publishes an offline tokenizer package, but does not establish exact rendered
  image/tool counts. Its image calculation is expressly an estimate. Do not
  claim that an offline text tokenizer closes the full default-model count gap.
- Anthropic's [count endpoint](https://platform.claude.com/docs/en/api/messages/count_tokens)
  provides vendor-reported counts without generation. Its Messages rendering must
  be the same rendering used by the actual generation adapter.

## First module and explicit interfaces

`crates/agents-worker/src/responses_auxiliary.rs` contains:

- `CompactRequest` with current standalone controls and bounded validation.
- `CountRequest`, keeping full tool/policy values for the shared Responses codec
  rather than duplicating the old function-only schema.
- `CompactionState`, a service-owned `mcmp1.` AES-GCM envelope using existing
  `agents_auth::seal/unseal`. AAD binds owner and item ID; payload validation also
  binds the agent/model identity and pinned-prefix hash. Protected instructions
  and compacted context survive exactly. This is not an OpenAI reasoning blob.
- `compacted_response`, which only emits standard numeric usage when reported
  totals and reasoning details are known. It never invents zero reasoning counts.
- `CountProtocol::{Responses, Anthropic}` and `count_native`, a bounded 30-second,
  redirect-denying, bounded-reply POST to the provider's count-only endpoint.
  Errors never include upstream response text or credentials. Provider calls
  inherit a fresh child trace and native `provider.count` span.

The count caller must first resolve owned conversation/response references,
registered tools/skills, fixed prefix, custom instructions and files. It must pass
the **same prepared context and codec** used by generation, replacing only the
platform agent ID with its upstream model ID. Passing platform IDs to the vendor
or counting native Responses while generating a different Chat rendering is not
an exact count. The module rejects unresolved platform references. It does not
reserve inference capacity or run the model; the route must use existing
resource-only accounting.

`count_prepared` accepts the native adapter's `PreparedRequest`, so counting and
generation share that renderer rather than separately serializing messages/tools.
It uses the same account-only resource reservation as `count_account`; no model
reservation, synthetic Session or execution slot is created by counting.

The parent owns the shared route/codec hook and must add the module to `lib.rs`.
The standalone Rust file is intentionally not wired to an unbilled edge handler.
Pure tests cover controls, token response validation, authenticated-envelope
owner/item/model/prefix separation, and unknown usage rejection. New-source
compilation and actual provider-fixture endpoint tests belong in Actions.

The existing Node SDK 7.43.0 auxiliary-route contract fixture passed two offline
cases (`tests/worker/responses-auxiliary-contract.test.mjs`): count uses only
`POST /responses/input_tokens`, and compact returns a complete window whose
opaque compaction item can be passed unchanged into the next create's `input`.
These use injected fetch and establish SDK compatibility only, not Rust endpoint
acceptance or a successful compaction/model operation.

## Durable compaction integration

Use one defaulted `RunPurpose` discriminator (`Generate` / `Compact`) in the
existing Session DO run, not a parallel harness or several optional task flags.

1. Resolve the exact owned input window and pin before admission. Preserve
   server/agent instructions and this call's custom instruction separately from
   the summary prompt. Validate pending tool groups without executing them.
2. Persist admission identity and purpose before acknowledging or calling a
   provider. `Idempotency-Key` selects this compact operation, not a fresh normal
   response. The caller does not gain an arbitrary workspace or history fork.
3. Reserve/bill the one compaction model call through the existing run/completion
   ledger. Persist the pending summary before settlement or constructing output.
   An uncertain external outcome is not permission to repeat the model call.
4. Return retained input items plus an authenticated compaction item representing
   the compacted active window. Do not reveal private reasoning or fabricate a
   provider's encrypted item. Reusing the returned window must restore the same
   protected instruction context, checked against its pin/model binding.
5. Persist an immutable `CompactedResponse` receipt before successful transport.
   Retrying/observing it does not rerun compaction or alter the original source
   journal, workspace, user-append clock, or previously returned responses.

This requires the parent state integration hook; the current module does not
claim that adding wire types alone implements `/responses/compact`.

## Account model work without fake Sessions

The D1 model reservation/ledger schema already permits an absent Session. New
`store_auxiliary.rs` shares the existing admission/reserve/record implementation
between optional Session scopes. The established `Store::admit_run`,
`reserve_completion` and `record_completion` signatures remain wrappers with
`Some(session)`; their retry hash remains byte-for-byte unchanged. New
`admit_operation`, `reserve_operation_completion` and
`record_operation_completion` use `None` and a separately tagged admission hash.

Both scopes use the same account-plan concurrent execution INSERT, allowance
synchronization, monetary reservation vintage, model pricing snapshot, known
provider usage validation, immutable ledger append and settlement. Account work
does not acquire a workspace/archive cleanup lease or create a `sessions` row.
Null-safe scope checks use `session_id IS ?`, including owner and exact run ID.
This is one financial contract, not an account path that bypasses concurrency.

The four focused SQLite tests execute production SQL against all real migrations:
account/Session work shares a quota, other owners/scopes do not match, known model
charges settle once, and ending execution leaves uncertain model money reserved.
All four pass; the complete SQL suite passed 105 cases. Final compiled integration
remains an Actions gate.

## Remaining exact-count dependency

For an explicitly configured native Responses or Anthropic count capability,
the provider's reported count can be normalized directly. A 404/405/501 is an
explicit capability absence, not a successful zero count. Default DeepSeek and
OpenRouter require either a documented native counting capability or a matching
model-specific renderer/tokenizer with a justified treatment of tools and media.
This remains a completion gap until that contract is implemented and tested.
Never probe it by secretly buying a normal completion, return an estimate under
an exact-count claim, or weaken the financial rule that actual provider usage
remains the billing authority.

## ModelOperation durable implementation

`model_operation.rs` implements the approved `MODEL_OPERATIONS` binding and
exported `ModelOperation` SQLite Durable Object. It is a single paid summary
operation, not a second agent harness: no tool execution, workspace, Session row,
conversation mutation, or user-input clock changes. The edge resolves the same
owned model, pinned prefix, protected system/developer messages and source input
as generation, then constructs `PreparedCompact`. The `instructions` field in
that private structure is the ongoing response-scoped protected instruction;
`guidance` is compact-only guidance and never replaces it.

`auxiliary_routes::compact` accepts this resolved input, hashes the owner and
validated retry key into a deterministic operation identity, and dispatches a
private terminal-only stream. A missing retry key gets a new one, returned as
`Idempotency-Key`, along with `X-Agents-Operation`. A detached stream must retry
the original compact call with that same key, never retrieve a normal Response.
The operation checks the immutable request hash before accepting followers.
Native streaming is reused for the actual provider call; native Responses
models use the shared `PreparedRequest` codec and all provider secrets are
resolved afresh via `provider::resolve_config`, never persisted in actor state.

The paid-call fence is `accepted -> in_flight -> known -> complete`. Only an
accepted operation without a durable provider result can issue inference. The
provider result and measured wall duration are chunked and published before
billing. Recovery finds this result even if the phase write was interrupted.
An in-flight or known operation missing its result becomes failed/unknown and
cannot call the model again. Known model usage uses the same Store ledger and
price vintage as Session inference. Wall observations are declared before I/O,
recorded idempotently from the durable measurement, and settled through the
resource meter, not through unconditional reservation release. A cold I/O loss
with unknown duration deliberately leaves pending funded observations rather
than inventing a zero measurement.

Healthy provider execution has no fixed total wall-clock deadline. Operation
alarms are recovery/observation hooks, not permission to cancel live I/O or retry
a paid request. Private input, provider result and output remain recoverable
while execution/accounting is active. The 24-hour private-content retention
starts at the durable complete/failed transition, not initial acceptance. Active
accepted/in-flight/known phases do not expire merely because creation was long
ago. The alarm first records an expired tombstone, then erases all chunk
content, retaining only owner/operation identity, request hash, timestamps,
phase and non-content resource receipt. Expired keys cannot execute again.
Content erasure precedes final resource settlement so accounting failures do
not indefinitely retain private context. Completed output retains canonical
user items followed by an owner/item/model/prefix-bound service-sealed
compaction item containing protected messages and the summary.

`expand_compaction_items` authenticates and expands a service item using the
current owner/agent/prefix. It preserves ordinary items and returns protected
ongoing instructions separately for the common codec to merge exactly once.
Multiple compaction items in the same window are rejected. Provider-native
encrypted reasoning is a separate item type and is not claimed or forged by
this service token.

Current official required compaction usage details include both cache-write and
reasoning token integers. A missing provider detail is not known zero. The actor
settles authoritative known model usage, then returns
`provider_unsupported_usage_detail` if it cannot construct this required shape.
This capability gap is explicit, including for the default provider; it must not
be described as a successfully completed full native compact implementation.

Actions must compile and test the actor and actual route integration. Local
verification is limited to rustfmt, diff checks, the real SQL suite, and SDK
contract tests. The public source-resolution hooks are owned by the Responses
integration lead; adding this actor alone is not public endpoint acceptance.

## Public route integration checkpoint

`auxiliary_routes::handle` now owns POST-only compact/count routing, current
endpoint-specific schema checks, and sanitized errors. `auxiliary_context`
resolves the account-owned Agent/pin or retained previous Response, reads
Conversations without admission, expands authenticated service compaction state,
and prepares the same native input/tools/control codec as generation. Previous
top-level response instructions are not inherited by count. Compact guidance is
separate from protected ongoing instructions. The latter may be explicitly
restored by a service compaction item; a new request override wins. An omitted
model is inferred from the retained source Response, not from an arbitrary
account Agent. An unresolved model-less standalone request is invalid.

The new `context_prefix` functions contain the old generation renderer unchanged;
Session wrappers delegate to them. The prefix-byte test includes skill markup
and Unicode. No fake Session is needed to construct fixed tools or instructions.
Hosted declarations use the shared `BoundTools` renderer, actual operator
capabilities and read-only ownership validation, including tool-choice rewriting;
counting never starts a Container or creates a retrieval index.

The attachment resolver now separates read-only preparation from execution.
Inline UTF-8 text uses the same pure filename/content rendering in both paths,
without a generated permanent file ID. Existing owner-scoped file references
retain the established renderer. Inline binary workspace attachments still need
a stable owned file reference in auxiliary calls; this explicit capability
failure must not be replaced with a random future workspace path.

Native DeepSeek counting uses the official-recipe Rust counter over exactly the
prepared count projection, with the same resource-only reservation. Content-free
provenance includes renderer revision, tokenizer digest and WASM linear-memory
high-water bytes (not a claim about full isolate heap). Legacy Chat/Anthropic pins
are not relabeled native Responses to make this path appear supported.

The compiled Worker journey `Responses auxiliary compact and count` exercises
real authenticated SDK routes, exact count/generation prepared-input equality,
pure inline text, source export, idempotent compact, sealed-item continuation,
instruction restore/override/non-inheritance, rejected corrupted state, and known
billing despite missing required usage details. It is integrated into the
existing Miniflare harness, not yet claimed passed until Actions runs it. The
independent `DeepSeek exact count-only tokenizer cold warm and profile
replacement` journey uses the vendor's independent expected counts and records
actual workerd memory/timing without paid inference.

## Purpose-specific DeepSeek compaction transport

Direct Responses does not report cache-write tokens; those values remain unknown
for ordinary generation. DeepSeek also exposes a documented
[Anthropic-compatible endpoint](https://api-docs.deepseek.com/guides/anthropic_api/).
A separate, single development capability request on 2026-10-26 returned real
`input_tokens=7`, `cache_read_input_tokens=0`,
`cache_creation_input_tokens=0`, and `output_tokens=2` from that endpoint. This
was not a repeat of the earlier native-Responses usage probe. Neither a key nor
raw prompt/response was recorded, and the request had no retries.

Compaction can therefore select this transport **before dispatch**, reuse the
existing Messages serializer/stream parser and request `thinking: disabled`.
Only an actually non-thinking response can attest zero reasoning tokens; an
absent cache-creation field still fails rather than becoming zero. Caller model
configuration, generation transport, credentials and protected-prefix hashes do
not change. There is no post-failure fallback or hidden repair inference.

The native context is passed as complete JSON data rather than a lossy Chat
projection. The exact outgoing Messages body uses the official renderer and the
same bounded warm vocabulary for the context-window check. Existing sealing
retains protected instructions separately. Resolved text/tool context uses this
path; the media increment below adds image-bearing input. Vendor-opaque inputs
without a portable resolution remain explicit pre-inference capability errors,
not text placeholders. Known partial/failed model output is
accounted for but never installed as a successful summary.

SDK 3.37.0 has a local schema inconsistency: `CompactedResponse.output` documents
all retained user messages, but is annotated with `ResponseOutputItem`, whose
message variant permits only assistants. The endpoint's real user roles must
not be rewritten to satisfy that annotation. Retained messages have stable IDs,
`type: message`, and typed `input_text` content. The controlled validation uses
the SDK's `ResponseInputMessageItem` and `ResponseCompactionItem` for this one
union, while inheriting the SDK's unchanged envelope and usage model. It does
not drop payload fields or claim the unmodified faulty union validated. Normal
SDK calls and passing the entire returned window into generation remain tested.

### Frozen media and owned DeepSeek credentials

`provider_compaction_source` consumes the existing owner-validated, frozen
attachment view. Text files already use the common read-only attachment renderer;
it neither creates a workspace nor uploads a second file. Source JSON keeps roles,
tool identities and content order, while image payloads become location markers
plus actual ordered image blocks in the summarizer's user data message. Base64
bytes are not accidentally billed as text tokens. Screenshot outputs and typed
function/custom output images use that same path, not a second execution bridge.
Original source and retained user media remain unchanged.

The Messages wire has no `detail: low` control. For this purpose-specific
transport, a low image larger than 512 pixels is actually resized to fit 512 by
512 before request rendering and exact counting. Existing `image` codecs perform
the bounded decode/resize, with a 16 MiB decode allocation limit; failed decoding
is a pre-dispatch capability error, never permission to send an original image
while billing for a fictional resize. Other detail modes preserve original bytes.
The shared official recipe renderer counts the actual final image dimensions.
Pro images, unresolved files/URLs and foreign opaque continuations remain explicit
errors rather than the vendor converter's unsupported-image text placeholder.

Purpose selection follows the resolved immutable DeepSeek origin, including an
owned BYOK credential, rather than depending on the platform funding enum. This
does not relabel generation pins or forward credentials to another origin. BYOK
continues to have no platform model API charge; actual infrastructure is metered.

New tests cover source order and immutability, real low-image materialization,
pre-inference rejection, and an actual Rust Worker/current SDK media compact
window followed by continuation after the original file is deleted. Existing
text compact and budget receipts are retained, not replayed by this increment.
Actions `40337239535` at `b8defef` passed all three new media Rust contracts and
produced Worker artifact `12657344011`; the aggregate run failed before compact
execution because the SDK upload prerequisite used an in-process fetch that
does not support its multipart probe. The fixture-only correction uses the
existing explicit-wire upload setup. Actions `40338796050` at `f9a6852` reused
that exact Worker and passed the complete new media/owned-BYOK journey with the
current SDK, file deletion, full-window continuation and final hold closure.
Receipt artifact `12658072275` records that split. There were no external model
or tool requests, and no accepted text/budget/vision journey was replayed.

Low-image re-encoding also retains EXIF orientation, rotating only the small
resized raster. Its decoder path preserves the image library's explicit
allocation reservation rather than bypassing it through `from_decoder`. The
additional orientation contract is separate from the accepted media journey;
the latter is not rerun solely for this bounded transformation correction.

Actions `40339692424`, source `d59d4870c083add6832392027124eee8dbbd8f02`,
passed the new orientation contract with a real JPEG EXIF entry and produced
Worker artifact `12658363089` / verification artifact `12658952375`. It rebuilt
the changed Wasm and executed only that new contract, not the already accepted
media lifecycle or earlier compact/vision cases.

Provider image transport reference:
https://api-docs.deepseek.com/guides/vision/#using-images-with-the-anthropic-api

## Actual default text compact staging closure (2026-10-26)

Normal source CI `40338717364` and staging deployment `40339986401` published
assembly/Worker/daemon `1b7f2cd09fc0be114fbbab673c85b5e0a894811f`. The separately
tracked narrow operator then issued exactly one new default direct-DeepSeek
compact operation and one normal Responses continuation, with no retry or replay.

Operation `cmp_cc471b926db412f9689a1ab105cb74183c9cf7996a03e06a446ca822fb3128a7`
returned one retained real user message followed by its service-sealed compaction
item. Actual compact usage was input 220, cached input 0, cache-write 0, output 7,
reasoning 0, total 227. The SDK 3.37 envelope and usage model validated with only
the documented output union correction; the actual user role was not rewritten.
The entire returned output window was passed unchanged into continuation
`resp_21eb53b500ae4d9ea6159bc39af3b2f6`, which preserved the ongoing developer rule
and corrected decision. Session `9698cef3-5000-4ec4-b60e-586590cc69f8` was normally
archived and its temporary Agent deleted.

The separate account-operation and Session ledger closed at **132 microUSD**,
with zero new reserved holds and zero pending observations. The three baseline
holds (one historical hold and two independently failing Code startup holds)
were excluded and untouched. This is text compact acceptance, not acceptance of
the later media/context-management source changes. Safe local receipts:
`.temp/code-staging/live-compact-receipt.json` and
`.temp/code-staging/compact-postflight-after-terminal.json`. The bounded metadata
collector stopped; neither raw model windows nor credentials were retained.


## Public file inputs: frozen execution and read-only counting

The Responses `input_file.file_url` execution resolver now uses the same native
credential-free global Fetch reader as public image inputs and generated assets.
It accepts public HTTPS origins, including signed queries, but never userinfo,
private/local addresses, non-443 ports, fragments, redirects or caller/provider
headers. The source URL and query do not become model text or telemetry. The
reader bounds both declared and streamed body size (4 MiB per input and aggregate
attachment budget); text materialization is capped at 256 KiB.

Verified UTF-8 becomes the same filename/content JSON renderer as inline text.
PDF/DOCX/XLSX reuse the existing bounded native document-text converter, with no
OCR, image-description model, embedding or invented provider token usage. Other
binary/large inputs require an actual owned File and workspace import; execution
uses normal owner quota and funded R2 publication. Read-only count never creates
such a File, Session or workspace and reports its capability boundary instead.
Both count and execution prepay their actual Worker/DO read interval independently
of model credit. Known failed or rejected HTTP outcomes still settle known I/O.

Execution preparation uses the Session's existing transactional and chunked
storage. A response/part identity pins an owner-bound source digest before
download. A saved renderer result is reused; a dispatched-but-interrupted attempt
cannot download or convert a potentially changed source again. Temporary receipts
are removed after canonical accepted Response/native state owns the frozen view.
Before acceptance they are retained as private Session retry state, not Files or
observability data. Ordinary idempotent response replay reads canonical state and
never revisits the public URL. Count has no execution identity and keeps its bytes
only for that one read-only operation.

`prompt_cache_breakpoint` survives the file-to-text renderer unchanged. `detail`
`auto` selects this adapter's text path; `low`/`high`/`original` receive explicit
422 `file_detail_unavailable`, not an invalid-field error or false page-image
support. OpenAI documents PDF detail as a page-image control, separately from
extracted text: https://developers.openai.com/api/docs/guides/file-inputs .

New verification is scoped to `tests/worker/remote-file-input.test.mjs`: actual
compiled Worker + OpenAI SDK remote text/count/create equivalence, metadata,
owner isolation, no forwarded secrets, redirect/size rejection, prepaid failure
and stable response intent. This source addition does not claim execution until
its new exact-artifact journey passes; prior PDF/embedding/Space/image acceptance
is not rerun.
