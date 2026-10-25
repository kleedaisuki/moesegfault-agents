# Image generation service boundary

This is the adapter/interface decision for the full Responses workstream. The
new modules require parent integration and compiled Actions verification; their
presence is not a claim that the public tool is already deployed.

## Route and operation ownership

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
