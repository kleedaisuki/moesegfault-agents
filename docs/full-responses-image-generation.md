# Image generation service boundary

## Session producer

`session_hosted_image.rs` is the single Session harness producer. Its operation
record retains the original request digest, admitted operator profile, independent
service hold, dispatch fence, metadata/usage receipt, immutable final/preview
candidates, per-I/O attempt identities and published tool result. No raster or
base64 image is serialized into DO state. The final image and each requested
preview have separate deterministic capacity operation IDs and reserve 16 MiB
before the provider POST. Partial index identities never alias the final image.

Admission checks the final public projection envelope and R2 tariffs first.
Unified image-execution admission pins funded run holders before any service
hold or paid operation, including ordinary HTTP `store: false`. Terminal lifecycle
transfers them to authorized stored holders or bounded private observation cache;
private images do not gain public GET/previous-response authority.
The caller writes the dispatch fence before consuming the prepared secret-bearing
operation. A cancelled or cold unknown dispatch is never resubmitted. A known
provider outcome is persisted before financial settlement or asset publication.
Platform Seedream receipts settle against its independent image hold; BYOK
receipts use a zero-platform-model `ReportedCost` budget, without waiving resource
fees. Unknown provider outcomes preserve the independent hold and capacity lease.

Publication follows candidate intent → checksummed R2 storage → actual retained
storage clock → admitted-run execution holder → `ResponseImages` map → durable
canonical event. Public journal values contain null image fields; observer
hydration supplies their exact retained base64 bytes only at the wire boundary.
The shared publication barrier prevents emitting an uncommitted event. Completion
releases capacity slots proven unused by a known final result. A Files view is
optional: deletion does not invalidate canonical images; Files quota/I/O failure
returns bounded `file_link_pending`, not a failed image or a second generation.

`image_artifacts::recover_candidate` recovers a persisted upload candidate through
ready CAS lookup or a newly metered checksum/size HEAD, records the physical
storage clock and promotes the registry. A successful HEAD does not require the
original bytes. A missing object returns None once and converges to a failed,
unknown artifact outcome instead of infinitely charging HEAD retries. If the DO
crashes after persisting an inline candidate but before any R2 PUT, those bytes
are not recoverable from metadata; the producer never pretends otherwise or
regenerates the paid image. The provider receipt remains available for accounting.

Four pure producer fixtures cover independent slot IDs, platform/BYOK funding,
metadata redaction and durable unknown-dispatch fences. The new controlled
`tests/worker/hosted-image-journey.mjs` scenario drives a compiled Worker with
synthetic providers: image generation, public base64 hydration, idempotent replay,
independent Files deletion and exact image ledger charge. It requires suite
installation of its exported operator profile/provider fixture; syntax checks
are not evidence that the compiled journey has run.

## Execution, original delivery, and terminal cache

### Conversation publication and public observation

The image integration keeps Conversation images on the same immutable owner-blob
registry. A private `image:{item_id}` sidecar contains only `OwnedBlobRef` metadata;
canonical items continue to store a null result. The actor validates the complete
new image map, persists candidate intent, binds the existing registry reference,
and writes sidecars before publishing the item header. Recovery discards a
candidate reference only when that header proves it was never published.
Deleting an item persists its canonical tombstone before installing the registry
publication fence and releasing its image reference. Deleting Conversation
metadata alone still preserves its items and their bytes.

Public single-item and list reads use the same `ResponseImages` hydrator as
Responses. It recognizes both `output` and `data` envelopes and checks the whole
page's 32-MiB wire bound before its first physical image read. It does not grant
one maximum-size image allowance per list item. These authorized later reads
fund their own actual I/O; they do not reuse original-observer funding or make a
`store: false` Response publicly retrievable. The pending-cleanup header bit is
defaulted and omitted when false, so ordinary text-only Conversation reads do
not poll/delete image recovery keys on every request.

The shared ledger's `created_at`/`updated_at` values are Unix **seconds**. Image
lifecycle deadlines, physical storage clocks and measured wall intervals remain
milliseconds. Cleanup reservations convert at that boundary; the image journey
asserts the shared ledger timestamp contract instead of silently writing
millisecond values into seconds-based recovery indexes.

The four image-only SQL suites (`test_image_blob_holders_sql.py`,
`test_image_execution_blob_sql.py`, `test_conversation_image_blob_sql.py`, and
`test_original_image_read_sql.py`) passed 20 tests against an isolated copy of
the actual candidate registry queries and tracked migration set plus `0022`.
That run deliberately excluded the future `0021` Conversation quota schema and
archive-workspace implementation. It establishes the physical-holder, ownership,
publication-fence and prepaid-read invariants, not compiled-Worker acceptance.
Conversation metadata deletion remains a real SDK journey gate rather than a
synthetic test of an unrelated future archive-deletion query.

`session_image_lifecycle.rs` owns the Session-side lifecycle; the shared immutable
blob registry remains the sole physical reference/GC boundary. Before each paid
image operation, the Session persists a planned, tariff-pinned account cleanup
hold, reserves that exact identity, then persists its funded acknowledgment. Its
quote covers at most one final plus three previews, bounded original image reads,
D1 metadata and measured Durable Object wall time. It is neither a new model
charge nor a new tariff. Cold recovery reads the original scope/price snapshot;
completion never requires fresh account credit.

The original create observer alone receives a private per-item funding map. JSON
hydration and stream frame hydration use that already funded hold; independent
GET/replay continues to fund its own actual read. A new physical GET has a unique
durable observation before I/O. An atomic count guard includes pending/unknown
attempts and enforces eight original reads per planned slot; prepaid reads have a
30-second bound. Unknown read metrics retain funding, never become zero and never
cause another image generation. The controller's observation nonce and actor
instance generation are private receipts, not public Response or parent authority.

Terminal preparation persists a compact handoff outbox and private delivery ID.
The first canonical run-clear acknowledgment stamps the delivery hour; retries do
not reset it. Execution holders and the prefunded hold remain while original
hydration can still perform I/O. On known EOF/cancel, the terminal D1 transaction
transfers exact slots to stored Response, already-published Conversation item,
and/or bounded `image-delivery` holders, then removes the operation's execution
holders. The cache expires no later than that terminal acknowledgment plus one
hour, even if finalization cleanup was delayed. Files links remain independently
owned and deletable. Public `store: false` retrieval and previous-response rules
are unchanged: the private delivery ID is not a `ConnectionLease` or an API token.

Known deferred reference checks retain and bill their exact D1 receipts, then
retry with a distinct attempt. Transport/measurement uncertainty keeps the old
observation pending. Canonical public/item deletion fences prevent late handoff
from resurrecting removed publication. Conversation metadata deletion alone does
not delete its retained image items. This implementation is awaiting the new
compiled-Worker lifecycle, zero-balance original-delivery, disconnect and fake-clock
journeys; syntax/format checks are not substitutes for that gate.

## Default Seedream output delivery

The dedicated OpenRouter Image API documents **inline base64 bytes** as its
buffered response contract (`data[].b64_json`), and inline `b64_json` for native
image completion events. Its request parameter table does not expose a
`response_format` selector. See the primary
[Response Format contract](https://openrouter.ai/docs/guides/overview/multimodal/image-generation#response-format)
and [request parameters](https://openrouter.ai/docs/guides/overview/multimodal/image-generation#request-parameters),
checked 2026-10-25. This applies to the dedicated `/api/v1/images` route used for
the Seedream profile; it is not inferred from a model-specific CDN or the legacy
chat multimodal response shape.

Consequently the default Seedream operator profile should have `asset_hosts: []`.
No CDN hostname needs to be guessed before the first generation, and the adapter
does not invent or globally add `response_format: b64_json` (modern native
OpenAI GPT Images also does not accept that legacy parameter). The new controlled
transport fixture builds the pinned Seedream request, parses the documented
buffered inline response, preserves its exact `usage.cost`, and materializes it
with an empty asset-host list without an HTTP asset read.

If an upstream violates this inline contract and instead supplies a remote URL,
the known outcome is still preserved before materialization; an empty host list
explicitly denies that download. This is a provider compatibility diagnostic,
not permission to widen the output allowlist or repeat an already-paid POST.
An eventual paid staging image acceptance test should verify the actual chosen
endpoint's inline response; the controlled fixture is not evidence of a live
Seedream generation.

This is the adapter/interface decision for the full Responses workstream. The
new modules require parent integration and compiled Actions verification; their
presence is not a claim that the public tool is already deployed.

## Route and operation ownership

### Single live Seedream delivery increment

`tests/e2e/hosted-image-sdk.mjs` is the separately gated real-provider journey.
It reuses the existing Responses SDK/provenance/direct-DeepSeek route helpers,
persists one non-replayable create intent, and makes exactly one Responses create.
The outer LLM remains direct DeepSeek; only the hosted image service uses
OpenRouter. Original canonical SSE, JSON GET, GET event replay, current Python SDK
wire validation, Files bytes and Files deletion are checked against the same
image. Receipts retain hashes and resource handles, never raster/base64 content.
The runner does not perform a paid direct-provider probe or repeat the six
already accepted controlled image cases. Financial closure is a subsequent
operator ledger/trace check, not inferred from SDK success.

The additive staging scope is
`web_search,file_search,code_interpreter,image_generation`; it preserves accepted
services, requires the normal verified deployment, and does not alter production.
The selected profile is pinned to the free
[Seedream endpoint discovery](https://openrouter.ai/api/v1/images/models/bytedance-seed/seedream-5-0-flash/endpoints)
checked 2026-10-26: `seed` provider/tag, no fallbacks, no streaming, 14 references,
1K/2K, and USD 0.018 per output image. Common `size` and `output_format`
normalization fields match `Capabilities::from_openrouter_endpoint`; these are
documented Image API fields, not guessed endpoint-specific knobs. No
`response_format` or output CDN hostname is added.

`image_generation.rs` validates native image-tool options and resolves them into
one prepared image-service request. `image_generation_transport.rs` performs the
actual HTTPS request, incrementally parses native image SSE when requested, and
returns a known result. The caller owns account authorization, credential lookup,
reservation/intent durability, tool execution identity, Response item/event IDs,
Space publication and financial settlement.

The invocation-only `ImageConfig` is intentionally neither serializable nor
`Debug`: it contains the service key. `ImageInput` contains only an already
authorized prompt, validated reference rasters and a resolved mask. The adapter
does not resolve arbitrary file IDs, run shell commands, create another agent,
or receive an R2 credential. Reuse the model-provider credential's owner-scoped
OpenAI-compatible base/key for BYOK; select the image API dialect explicitly.

The native public model alias can differ from its provider slug. For example,
the public image-tool model may be `gpt-image-2.5-flare`, while an explicitly
resolved OpenRouter route uses `openai/gpt-image-2.5-flare`. A request cannot
silently override the resolved profile with another model or credential.

## Supported transport behavior

| Route | Generation | Reference edits | Partial images |
| --- | --- | --- | --- |
| OpenAI Images-compatible | JSON `POST /images/generations` | Binary multipart `POST /images/edits`; generated filenames, optional owned PNG mask | Native `image_generation.partial_image` / `.completed` SSE |
| OpenRouter Image API | JSON `POST /images` | Owned rasters encoded into `input_references` | Same native SSE when the selected endpoint advertises streaming |

Modern GPT Image requests must **not** send the retired `response_format`
parameter: the native API always returns base64. OpenRouter's dedicated Image
API likewise returns `data[].b64_json`; do not substitute its older image-through-
chat examples for this protocol.

Preserve requested fields rather than treating them as hints. A resolved
capability map rejects unavailable explicit quality/background/fidelity/mask/
compression options before request construction. OpenRouter's endpoint model
capabilities are authoritative, not the union of every provider route. Pin
`provider.only` with `allow_fallbacks:false` when using a selected endpoint.
Allowed provider-specific fields belong under `provider.options[provider_slug]`;
for example the observed OpenAI endpoint advertises `moderation` there, not as a
generic top-level Image API field. The `output_format` and exact-pixel `size`
normalization fields are part of the common Image API; validate the returned
raster against the requested format and explicit dimensions as well.

The returned native completed event is the known outcome. A later connection
close does not undo it. Partial-only EOF, malformed ordering and oversized bodies
do not produce a completed result. A declared provider error never includes its
raw body in the public error or logs. A maximum of the requested 0–3 previews is
delivered to the caller's asynchronous persistence callback.

## Known outcome and asset lifetime

`ImageResult` is a durable-friendly result containing one `ImageSource`:

- `Inline`: one validated base64 raster representation, never a JSON byte array.
- `Remote`: a known provider asset URL, not a request to fetch it automatically.

It also preserves the original request identity, effective metadata when
reported, and arbitrary-precision upstream `usage` / `usage.cost`. Missing usage
or cost is not a zero-price image. The existing accounting layer must convert
reported decimal USD; do not introduce another float-based price parser.

The caller first fences generation with the tool execution ID. After receiving a
known result, persist it before publishing a Space file or settling/reconciling
the provider charge. The sent `Idempotency-Key` helps upstreams that implement
that facility; it is not permission to retry a generation after an uncertain
external outcome. Network timeout, invalid output or an unreported provider cost
requires evidence-based reconciliation, not another paid POST.

Only after that known-result commit may `materialize` read a URL result. It uses
an exact pinned public-HTTPS hostname allowlist, no redirects, no API key,
cookies or trace context, and a bounded anonymous GET. Signed query strings may
be necessary for an asset but never enter diagnostics. Empty allowlists reject
remote materialization. A transient CDN failure can retry this read of the same
known asset; it cannot repeat image generation.

Publish validated bytes through the existing owner-scoped
`space::save_bytes_idempotent` boundary and a stable execution/ordinal identity.
The parent must give canonical image output a strong lifetime independent of
ordinary Space file visibility: deleting a generated file must not make a retained
Response's `image_generation_call.result` disappear. Input-only active pins are
not sufficient for immutable generated output. Also avoid copying a 16 MiB image's
base64 into the baseline 4 MiB Response output snapshot; use the parent artifact
reference/hydration design or an explicitly revised bounded representation.

## Limits and verification

One image per hosted call. Encoded images are bounded to 16 MiB decoded, matching
the existing Space bytes publisher. References plus mask are bounded in aggregate
to 16 MiB and at most 16 images (further reduced by the resolved provider profile).
The `image` 0.25.10 library, with only PNG/JPEG/WebP features, checks raster format
and dimensions without full pixel decoding. Header limits cap each edge at 16,384
and total pixels at 64 Mi; these are defensive transport limits, not a claim that
every selected model supports those dimensions. The codec allocation budget is
64 MiB. A raster restored from pending state is revalidated through the same path.

Provider JSON/events are bounded; generation has a 300-second operation deadline,
and anonymous asset materialization a separate 30-second deadline. Neither bound
implies a missing result was free or that the task should be replayed.

Focused tests cover modern generation fields, multipart binary edits/masks,
provider pinning/passthrough, invalid capability preflight, base64 state restore,
Unicode/size malformed-input properties, exact decimal-cost preservation,
MIME/format mismatch, remote URL policy, genuine partial/final SSE, semantic
terminal-before-disconnect, and partial-only uncertainty. Formatting has been run;
the tests still require compilation/execution in the parent's integrated CI.

## Verified discovery, 2026-10-25

Public metadata (no credentials or generation request) was saved under
`.cache/full-responses-image/`. The current catalog listed
`openai/gpt-image-2.5-flare` and `openai/gpt-image-2.5-sunburst`, both streaming,
including transparent backgrounds and low/medium/high/xhigh/max quality. The
Flare endpoint record reported provider slug/tag `openai`, up to 16 references,
and `moderation` as allowed passthrough. Its pricing lines were per-token USD:
input image `0.000008`, input text `0.000005`, output image `0.00003`.
These are not a fixed image price or the platform default. The user's selected
platform default is the Seedream 5 family, not GPT Image. The verified concrete
candidate is `bytedance-seed/seedream-5-0-flash`, provider slug/tag `seed`, with
1K/2K resolution, one output and up to fourteen references. Its endpoint currently
advertises no native streaming, quality/background knobs or passthrough fields.
Its published pricing lines are USD `0.018` per output image and `0` per input
image; settle actual reported cost rather than inferring every invocation has the
same bill. A platform profile should pin that provider and its verified capability
and reservation snapshot. Explicit native `auto` knobs select its default rather
than demanding an unavailable provider-specific knob. Richer streaming/mask or
quality controls remain available through capable BYOK routes, not invented for
Seedream. No paid image request has been made during this implementation.

Configure the verified model through `IMAGE_GENERATION_MODEL` and its
reservation/capability snapshot. Paid language-model tests use direct DeepSeek;
OpenRouter paid calls are reserved for separately authorized image testing.
The older tutorial's
`gpt-image-1` example is not a safe default: current OpenAI documentation records
its 2026-10-23 shutdown, even though an aggregator may retain a legacy route name.

Primary sources:

- [OpenAI image generation request](https://developers.openai.com/api/reference/resources/images/methods/generate)
- [OpenAI image streaming events](https://developers.openai.com/api/reference/resources/images/generation-streaming-events)
- [OpenRouter Image API](https://openrouter.ai/docs/guides/overview/multimodal/image-generation)
- [Public image model catalog](https://openrouter.ai/api/v1/images/models)
- [Observed Flare endpoint capabilities and pricing](https://openrouter.ai/api/v1/images/models/openai/gpt-image-2.5-flare/endpoints)
- [Verified Seedream 5 Flash endpoint](https://openrouter.ai/api/v1/images/models/bytedance-seed/seedream-5-0-flash/endpoints)
- [Rust image reader](https://docs.rs/image/0.25.10/image/struct.ImageReader.html)
- [Codec allocation/dimension limits](https://docs.rs/image/0.25.10/image/struct.Limits.html)
# Hosted preparation interface

`hosted_image.rs` prepares one invocation, without adding a second agent loop.
`prepare(env, owner, binding, arguments,
execution_id, trace)` loads the versioned operator registry, resolves only the
owner's selected credential, and loads only file IDs declared in the original
request. A mask file must also be declared. Space reads verify ownership, byte
count and SHA-256; raster validation is shared with the transport. Inline masks
accept only bounded PNG data URLs. No provider secret reaches the workspace.

`PreparedHostedImage::plan()` exposes a serializable `HostedImageProfile` before
the caller writes the durable dispatch intent and reserves funds. It contains
the actual API/base/model, optional native alias, capability snapshot, endpoint
pin, anonymous asset-host allowlist, price revision, reservation and funding
kind. It never contains the API key, credential ID, prompt or input images.
`dispatch(self, partial_sink)` consumes the opaque key-bearing preparation and
performs one generation POST. Any dispatch error may be uncertain: the caller
must preserve its fence rather than invoke `prepare` and generate again.

Persist the known `ImageResult` before artifact publication or asset retrieval.
`fenced_profile.materialize(&known_result)` restores anonymous retrieval from
that same snapshot without reading today's credential or operator registry.
Generation admission, durable result storage, reservations, cost settlement,
strong blob references and public output hydration remain the Session owner’s
responsibility. This wrapper deliberately does not publish ordinary Space files
or replace durable image results with deletable file pointers.

## Operator registry

### BYOK self-service contract

An owner can configure a self-hosted, unknown model without global administrator
JSON. The immutable owner credential's explicit `openai_images` or
`openrouter_images` service dialect claims the minimal v1 generation contract:
`model`, `prompt`, one requested image, and a buffered inline PNG `b64_json`
response. The minimal POST sends only `model`, `prompt`, `n: 1`: it invents no
`output_format`, `response_format`, size, transparency, edit, provider routing or
streaming support. Incompatible actual upstream behavior fails honestly; neither
a model-name prefix nor a compatible credential grants advanced capabilities.
No platform key is ever a fallback for BYOK.

For a controlled endpoint with advanced capabilities, the owner can declare:

```json
{
  "image_generation": {
    "provider": "openai_images",
    "credential_id": "owned-image-credential",
    "model": "my-selfhost-model",
    "capabilities": {
      "version": 1,
      "edit": true,
      "max_references": 2,
      "mask": true,
      "partial_images": 1,
      "output_formats": ["png", "webp"],
      "transparent": true,
      "sizes": ["1024x1024"],
      "qualities": ["high"]
    }
  }
}
```

All omitted advanced fields default off/empty; `output_formats` defaults to
`["png"]`. Additional optional sets are `input_fidelity` (`low`, `high`) and
`moderation` (`auto`, `low`); `output_compression` defaults false and permits
0–100 for declared JPEG/WebP. Masks require edit and are unavailable through the
current OpenRouter dialect. Edits require 1–16 declared reference capacity;
previews are bounded at 0–3. Size sets contain at most 32 exact pixel dimensions,
each within the fixed 16384 edge / 64 Mi-pixel envelope. Neither declarations nor
operator profiles raise the platform's byte, reference, preview or raster limits.
The default implicit format remains PNG; a JPEG/WebP-only declaration must select
that format explicitly. None of these fields can carry an API key or credential
origin override.

The frozen profile records `verified_operator`, `owner_declared_minimal`, or
`owner_declared_contract` provenance and retains the owner's original contract.
Verified operator matches take precedence over owner declarations. Metadata-only
`resolve_profile` and pure `validate_profile_options` are shared by counting and
generation: they do not decrypt keys, read image files, or access an upstream.
Unsupported options fail before outer model spend. Generation rechecks the
owner's immutable credential family/origin, then uses the pinned profile rather
than silently importing a changed operator registry. Unknown paid outcomes keep
the normal durable fence and cannot trigger regeneration.

`IMAGE_GENERATION_PROFILE_JSON` is strict JSON with `version: 1` and `profiles`
(1–64 `HostedImageProfile` records). Profiles are selected by **API + complete
normalized credential base URL + actual model + funding kind**, never a model
prefix or an unreviewed provider URL. Multiple matches are an ambiguity error.
Native aliases must match a selected profile. Operators can install reviewed
OpenAI Images profiles for actual BYOK origins/models, or reviewed OpenRouter
endpoint snapshots. A matching verified profile takes precedence; absent one,
BYOK can use the owner's explicit minimal API dialect or versioned capability
contract described below. Platform funding still requires a verified profile.
No native GPT profile is inferred from a compatible credential protocol. Credential family
must be OpenAI-compatible, including the separately negotiated Responses family.
There are no per-invocation catalog GETs.

Platform profiles must use `api: openrouter`,
`base_url: https://openrouter.ai/api/v1`,
`model: bytedance-seed/seedream-5-0-flash`, endpoint slug/tag `seed`, no provider
passthrough, and reservation **18000 µUSD** per requested image. The platform
key is `OPENROUTER_API_KEY`. BYOK profiles must have zero model reservation;
this does not waive compute/storage fees owned by the caller. Settlement still
prefers authoritative actual `usage.cost`; a reservation is not an invoice.
Changing the published model price requires reviewing this explicit policy and
price revision, not silently reusing a stale number.

`asset_hosts` may be narrowed by the user's service declaration, never widened
past the selected operator profile. Empty means no remote downloads. Known
result URLs still undergo the transport's independent public-HTTPS and exact
host checks at materialization.

## Endpoint fields versus normalized Image API controls

The 2026-10-25 Seedream endpoint snapshot advertises resolution tiers `1K`/`2K`,
14 references and no native streaming. It does not advertise a `size` key.
This does **not** mean the API rejects explicit pixels: OpenRouter's
[primary Image API documentation](https://openrouter.ai/docs/guides/overview/multimodal/image-generation#resolution-and-aspect-ratio)
separately defines `size` as a normalized convenience shorthand; explicit pixels
are authoritative, and conflicting resolution/aspect ratio fields fail with
400. The adapter therefore sends the documented shorthand instead of inventing
a provider-specific conversion. Native `auto` is omitted. The hosted Seedream
preparation rejects explicit edges above 2048 rather than treating the generic
presence descriptor as proof of 4K support. Other non-catalog pixel combinations
are still subject to provider validation; tests establish the documented square
1024/2048 path, not arbitrary-resolution support.

Five pure fixtures cover versioning and secret-free snapshots, exact-origin
BYOK routing, fixed platform model/provider/price, declared file and execution
IDs, and the bounded size shorthand. They are written for the integration CI;
local checks only ran rustfmt and diff validation, not a Rust build or paid API.

## Controlled runtime acceptance (2026-10-26)

All six producer journeys passed with the actual compiled Worker, OpenAI Node
SDK and strict current Python SDK `3.37.0`. Outbound providers were controlled
fixtures; this is not a live Seedream or BYOK-provider acceptance receipt.

| Case | Observed boundary |
| --- | --- |
| `stored` | One generation POST, two model rounds, actual inline base64, 18000-microUSD image ledger entry, Files deletion independent of cold GET/idempotent SSE, canonical deletion fencing late publication. |
| `byok` | Independent owner key and unknown model without operator registry; one POST, platform image debit zero; three undeclared advanced controls rejected before additional model/funding effects. |
| `no_store` | Ordinary HTTP execution holds do not expire as observer caches; terminal cache grants neither public GET nor previous-response authority. |
| `conversation` | Independent final-image holder; item/list reads survive Files deletion and Conversation metadata deletion; item deletion releases its holder and fences late publication. |
| `lifecycle` | Separate preview/final bytes, original-observer disconnect, 70-minute fake-clock active protection, terminal-started one-hour expiry. A separate deliberately ambiguous POST remains held at 18000 microUSD and cold observation never resubmits it. |
| `funded_delivery` | Original JSON delivery reads through its prepaid account hold at zero unreserved balance; a later fresh GET returns 402, not a free read or generic 500. |

The first five use Worker artifact `12645959577`, source
`4f767153449cb0601a562c2412eee832ffa2e2c1`, CI build `40315096372`, Wasm SHA-256
`a3fe5ed73ea724f59f7b13d59c0182b254626fad5a9e1d7846fac05987a3bcf1`.
The final credit-error repair uses artifact `12648952278`, source
`8c4889aecadf2b04832fe57158d41e0c536f3611`, build `40320354539`, Wasm SHA-256
`a04b5d9c13a1c20367b01c6c4320616145fe2a692af59cb89832e58d13813bec`.
The original CI attempts stopped at reporter-argument duplication and then a
JSON-only fixture incorrectly invoking an SSE validator; repaired fixtures were
run locally against those exact retained binaries, not a speculative rebuild.
JSON-only delivery validates its actual Response model without invented events.

Native function-result metadata is JSON **text** (or its normalized singleton
`input_text` representation), never the structured durable receipt passed as a
content object. Public image items and durable blob references remain separate.
The stored canonical record measured 12636 bytes with no inline image payload;
this is not a peak-memory measurement. Separate retained-projection journeys
passed a 16-MiB image and distinct previews against artifact `12645045621` after
correcting seeded file IDs to the existing 32-hex contract. Raw per-case receipts
and their unchanged-source binding supplement are retained under the topic's
`.temp/media-results/`; the immutable source/artifact/checksum identities above
preserve the acceptance context if that scratch directory is cleaned.
