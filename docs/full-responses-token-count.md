# DeepSeek exact input counting and compact usage contract

Inspected 2026-10-25. This is an implementation decision note, not an assertion
that the default-model count endpoint already succeeds. No inference or paid API
calls were made. Production source was not changed.

## Decision

There is now a credible official offline implementation path: DeepSeek publishes
`deepseek-recipe` 0.2.0, a Rust protocol converter, V4/V4.1 prompt renderer and
special-range-aware tokenizer integration. The current V4.1-Flash-1017 model
card expressly recommends it for production. This supersedes the weaker finding
that only a legacy tokenizer ZIP was available.

**Use the exact prepared upstream body and its actual protocol, then the matching
versioned recipe conversion/encoding.** Do not count a Chat projection while
sending Responses, omit instructions/tools/reasoning history, or substitute a
character heuristic. Do not start a model run to measure its prompt.

The published formatter is a technically implementable count source; hosted API
parity is a separate acceptance condition. The inspected local telemetry receipts
retain input-token counts but deliberately omit exact prepared request bodies.
Therefore they cannot establish equality against an offline renderer. No numeric
claim of hosted parity is made in this note.

## Primary evidence

| Question | Established finding |
| --- | --- |
| Current default alias | Official first-call page maps `deepseek-flash` to DeepSeek-V4.1-Flash-1017. Legacy Flash aliases are remapped, not stable tokenizer identities. |
| Hosted native count API | No count-only endpoint is documented in the inspected Responses guide, Anthropic compatibility guide or token-usage guide. Search of official docs for `count_tokens` found no supported endpoint contract. Absence of documentation is not proof a private endpoint does not exist; it is insufficient to enable one as public capability. |
| Official prompt renderer | `deepseek-ai/deepseek-recipe` converts Messages, Chat Completions and Responses into a shared Conversation, renders V4/V4.1, and tokenizes with an attached tokenizer. It covers tools, structured schemas and thinking policies. |
| Correct current model source | `deepseek-ai/DeepSeek-V4.1-Flash-1017/encoding/encoding.py` is a self-contained reference renderer. Its model card explicitly links the maintained recipe implementation. |
| V4 Pro | Recipe provides `DeepseekV4Encoding` and its matching V4 tokenizer, separately from V4.1. Do not reuse Flash encoding. |
| Thinking input | V4.1 writes a numeric effort prefix (low/high/max -> 50/75/100). Retained historical reasoning and tool grouping affect the prompt. Output-token reservation is not an input-token count. |
| Image count | Recipe core exposes `ImageTokenSpec::v41`, image resize/count rules and multimodal adjustment. Token count is encoded prompt length plus image tokens beyond the one placeholder per image. OpenCV-backed preprocessing is optional. |
| Hosted image authority | Official API token-usage page explicitly calls dimension-based image calculation an estimate and returned usage authoritative. Exact hosted image parity still needs versioned preprocessing/fixture agreement. |
| Files | DeepSeek Responses guide says file inputs are not supported; platform-owned file extraction must finish before counting and generation and use identical resulting content. |

Sources:
- [DeepSeek first call / aliases](https://api-docs.deepseek.com/)
- [DeepSeek token usage](https://api-docs.deepseek.com/quick_start/token_usage/)
- [DeepSeek Responses compatibility](https://api-docs.deepseek.com/guides/responses_api/)
- [DeepSeek Anthropic compatibility](https://api-docs.deepseek.com/guides/anthropic_api/)
- [Official recipe README](https://github.com/deepseek-ai/deepseek-recipe)
- [Official tokenizer integration](https://github.com/deepseek-ai/deepseek-recipe/blob/935817c66f5d6e64838f419692545f06787e485f/docs/tokenizer.md)
- [1017 model card](https://huggingface.co/deepseek-ai/DeepSeek-V4.1-Flash-1017/blob/main/README.md)
- [1017 reference encoding](https://huggingface.co/deepseek-ai/DeepSeek-V4.1-Flash-1017/blob/main/encoding/encoding.py)

## Version, provenance and security requirements

Recipe HEAD inspected: `935817c66f5d6e64838f419692545f06787e485f`, package 0.2.0,
MIT. Preserve its license and tokenizer notices. Pin the renderer source/crate,
model revision, tokenizer bytes/hash, conversion options and alias resolution as
one capability revision; never download mutable `main` during a request.

The bundled V4.1 tokenizer has SHA-256
`81f64d1248a68ce3663e07ab3ee48b851e5df0e32d27cb98e4c9a268151e8d99`.
Its documented comparison model is the older Vision-Exp tokenizer revision
`6821d6ad3681a4b137b066b76094fa82ebd0a380`. This probe downloaded the current 1017
tokenizer, SHA-256
`c90dfa01249db1be4245780a052ede752e1361c612ac6d08e2bdada7d599476b`,
and compared JSON semantics: **the sole difference is added token 129264**, renamed
from `<｜deepseek_image｜>` (`special=false`) to `<｜image｜>` (`special=true`) in
recipe. All other top-level data and added-token entries agree. This concretely
checks the current alias's text vocabulary, rather than assuming an old match.

Sources: [bundled tokenizer notices](https://github.com/deepseek-ai/deepseek-recipe/blob/935817c66f5d6e64838f419692545f06787e485f/static/tokenizers/README.md),
[V4.1 provenance](https://github.com/deepseek-ai/deepseek-recipe/blob/935817c66f5d6e64838f419692545f06787e485f/static/tokenizers/v41/README.md),
[V4 provenance](https://github.com/deepseek-ai/deepseek-recipe/blob/935817c66f5d6e64838f419692545f06787e485f/static/tokenizers/v4/README.md).

`PromptTokenizer` removes added vocabulary for ordinary text and uses explicit
`special_ranges` from rendering for control-token IDs. A user typing a token's
spelling must not create a control token. Encoding the whole rendered prompt
with a generic tokenizer loses this distinction and is not a production design.

The image fetcher's default transport permits arbitrary destinations. Do not
instantiate it with untrusted URLs without the platform's SSRF filter, redirect
policy, response-byte bounds and media limits. Reuse already-prepared media;
counting must not create an alternate fetch/extraction path.

## Practical deployment slice and capability boundary

1. Add a single `ExactInputCounter` capability keyed by provider, actual upstream
   protocol, model revision and renderer revision. Native count adapters remain
   preferred when provider-configured and protocol-equivalent.
2. Add an official-recipe counter for known V4.1 Flash / V4 Pro models. Feed the
   same `PreparedRequest` body used by generation. Convert with explicit model
   aliases/options and preserve tools, schema format, thinking history and image
   sources. Count `encode(...).len() + image_token_adjustment`; checked arithmetic.
3. Direct Workers WASM fit is **not yet demonstrated**. Recipe's published graph
   includes tokenizers 0.23.2 with default and `http` features, Tokio, rand and
   regex dependencies; image default includes OpenCV. Do not hide a failed WASM
   compile under a portability claim. Use a bounded owned native Rust count
   service/container if necessary, with no inference/GPU requirement and no
   per-session workspace attachment, or upstream a minimal WASM feature split.
   Keep tokenizer warm per process/isolate; never deserialize 6.37 MB per count.
4. Cap request/prepared-media sizes, account count-only Worker/container resources,
   add timeout/concurrency limits, and return only normalized unsigned count/error.
   No model allowance reservation, model execution slot, workspace mutation or
   user append-clock change. No third-party fallback destination.
5. Acceptance should compare the prepared-renderer count with provider usage on
   existing paired fixtures (plain English/Chinese, instructions, functions,
   namespace/custom adapters, schema output, history/reasoning, token-spelling
   adversary, images/details). Current redacted receipts are not paired fixtures.
   Integrate comparison into the next already-authorized bounded end-to-end model
   journey; do not add a paid call solely for counting research.

| Request/model capability | Correct behavior |
| --- | --- |
| Configured native count + matching generation protocol | Count-only request; accept provider-reported integer. |
| Pinned official local renderer/tokenizer and verified request class | Return rendered input count; include all prepared prefix/history/tools. |
| Unknown/remapped model revision, unsupported formatter or media class | Explicit model/input capability error before inference; not a guessed number or permanent global feature gap. |
| Transient counter timeout/unavailable | Retryable operational error, distinct from unsupported model. |
| Provider does not disclose image preprocessing parity | Do not label dimension estimate exact; restrict exact-count capability for that class until a matching contract is established. |

The finite delivery dependency is therefore **integrate the published renderer +
matching assets and run the prepared-fixture gate**, not wait indefinitely for a
hypothetical DeepSeek endpoint. It is legitimate to expose a narrow, documented
unsupported model/input boundary while unsupported classes are being integrated;
it is not legitimate to advertise the default exact-count route as complete while
all default Flash text requests always fail.

## Offline probe and existing receipt findings

Sources and probe are cached in `.cache/full-responses-spec/`;
`deepseek-offline-smoke.py` is fully annotated and makes zero network/model calls
when executed. It uses the official Python renderer plus tokenizers 0.23.1,
installed from binary wheels in `.cache/deepseek-token-count-venv`. Recipe 0.2.0
has Linux/macOS wheels but no Windows wheel; no Rust or native source build was
attempted. Asset download was public metadata/source only.

The fixed corpus deliberately has no user-supplied control-token spellings. It
only tests offline asset viability, not the full special-range production counter
or hosted API parity:

| Fixed corpus | Rendered tokens |
| --- | ---: |
| User `Hello!`, non-thinking | 6 |
| Same user input, thinking/high | 32 |
| Helpful system + Chinese greeting | 13 |
| Helpful system + time function schema + time question | 253 |

Machine output includes prompt SHA-256 and tokenizer semantic comparison in
`.cache/full-responses-spec/deepseek-offline-smoke.json`. Non-thinking tool schemas
alone change the prompt by hundreds of tokens: raw content counting would be wrong.

Read-only inspection of `.temp/stage-errors/stage-655-private/receipt.json`
finds actual `provider.completed` counts such as 308 and 378, plus run/round IDs,
but no complete prepared messages/tools/body. The collectors intentionally retain
sanitized telemetry. Their counts cannot be paired to a reconstructed prompt
without missing instructions, registry/tool versions and prior tool results.
Do not weaken privacy collection just to obtain a prettier retrospective number.

## Current OpenAI compact usage is required, not nullable

Inspected official `openai-python` **v3.8.0** and current main:

- `CompactedResponse` requires `id`, `created_at`, object literal, output list and
  `usage: ResponseUsage`.
- `ResponseUsage` requires integer input/output/total counts plus both detail
  objects.
- `InputTokensDetails` requires **`cache_write_tokens: int`** and
  **`cached_tokens: int`**; neither is `Optional` or defaulted.
- `OutputTokensDetails.reasoning_tokens` is also a required integer.
- v2.6.1 has `cached_tokens` but **no cache_write_tokens field**. Its old tests
  cannot establish acceptance of the new current schema.

Sources:
[v3.8 usage](https://github.com/openai/openai-python/blob/v3.8.0/src/openai/types/responses/response_usage.py),
[v3.8 compact](https://github.com/openai/openai-python/blob/v3.8.0/src/openai/types/responses/compacted_response.py),
[v2.6 usage](https://github.com/openai/openai-python/blob/v2.6.1/src/openai/types/responses/response_usage.py).

Thus `responses_auxiliary::compacted_response` rejecting unknown cache writes or
reasoning details is not accidentally stricter than a nullable SDK field. Runtime
SDK permissive parsing of missing JSON does not change the typed public contract.
DeepSeek's guide and recipe `ResponsesInputTokensDetails` report cached reads but
not cache writes. A provider-specific *documented* absent-charge/known-zero rule
could justify a normalized zero; absence of the field alone cannot. Automatic
DeepSeek caching does not establish zero writes. Do not infer writes as
`input_tokens - cached_tokens`. If exact standard usage is unavailable, fail or
use a provider with the required compact usage contract; do not fabricate it.

## Implemented offline counter slice (2026-10-25)

`crates/agents-worker/src/deepseek_tokenizer.rs` now uses the pinned official
Rust Responses converter and V4/V4.1 renderers with `PromptTokenizer`'s explicit
special ranges. It accepts the exact prepared Responses wire projection,
not legacy Chat messages or raw concatenated text. Only direct DeepSeek HTTPS
native Responses routes and explicit Flash1017/Pro0813 profiles are selected;
old aliases are not rewritten. Unknown model, media, encrypted reasoning,
unsupported hosted/discovery tools and silently ignored unknown item kinds fail
with distinct capability errors before any inference. Extracted files can count
once the normal preparation path has converted them into the same text used by
generation. No image preprocessing parity is claimed.

The managed dependency is `third_party/deepseek-recipe` with MIT notices,
compressed pinned tokenizer assets, source hash inventory and documented narrow
patches. Its insertion-ordered JSON is deliberately isolated as a distinct
vendored mature serde_json package: enabling preserve_order globally would
change historical platform prefix hashes. The bytes-only boundary preserves the
actual sorted platform wire bytes and official renderer construction order.

The tokenizer is cached as one Arc per isolate; switching profile drops the old
vocabulary before loading another. The additive consuming constructor avoids
cloning the full vocabulary during cold initialization. Gzip bytes total
3,727,057; individual uncompressed assets are approximately 6.37 MB. Those are
asset sizes, **not peak heap measurements**. Actual workerd cold/warm/profile
switch execution and final compressed script size remain mandatory CI gates.
No local cold Rust build, paid model probe or user-content network request was
performed by this workstream.

A synthetic independently rendered corpus is committed at
`tests/fixtures/deepseek-token-count.json`, generated by
`scripts/deepseek-tokenizer-reference.py` from the official 1017 Python encoder.
Its exact counts are 6 plain, 32 thinking/high, 13 Chinese+system, 253 function
schema, 17 user-spelled control tokens, and 2053 filler tokens. The special-token
fixture uses a tokenizer with added vocabulary disabled specifically for the
untrusted user region; whole-prompt special-token recognition is not used.
The Rust corpus test compares all six counts, then replaces Pro/Flash profiles.
Asset/source integrity is independently checked by
`python -m unittest discover -s tests -p test_deepseek_tokenizer_assets.py`.
Offline reference equality is useful implementation evidence, not hosted
provider usage parity; pair the same prepared request with the next existing
approved DeepSeek journey rather than generating a paid output just to count.

The actual-Worker helper `tests/worker/deepseek-tokenizer-journey.mjs` covers the
public account-scoped count endpoint using an owned BYOK direct-DeepSeek route
and no external call. It independently checks the six fixtures after the actual
fixed platform prefix (35/60/42/282/46/2082 tokens), warm repeat, Pro replacement,
Pro warm repeat, and Flash replacement. It asserts zero created Sessions, zero
model ledger entries and one settled account-only resource operation per count.
Its receipt records public-route wall times under `.cache/perf/v3`; the
content-free `provider.count.local` event separately reports WASM linear-memory
bytes. WASM pages are a useful high-water allocation measurement, not total JS
isolate heap. A successful workerd execution and configured platform memory
limit still need to accompany that measurement. This gate was authored and
syntax-checked locally, not reported as executed until compiled CI runs it.

## Shared prepared vision input (2026-10-26)

This slice extends the earlier text-only boundary. Count and create both use
`responses_attachments` before constructing the native prepared request. Owned
Files remain owner-checked; public HTTPS is frozen once per preparation with
the existing credential-free, redirect-denying bounded image reader. The count
encoder accepts only prepared bytes/data URLs and never fetches an image.
Preparation preserves original bytes, GIF animation and requested detail across
user/developer/system/assistant messages and function/custom tool outputs.
Bounded `image` header readers cover PNG, JPEG, WebP and GIF without allocating
decoded pixel frames; dimensions use the first GIF frame and no display rotation.

The same `encode_conversation` helper renders once, encodes with the existing
single warm official vocabulary, and adds `image_tokens - 1` for each ordered
rendered image placeholder. The pinned V4.1 dimension/detail recipe remains in
`deepseek_image_tokens`; genuine no-vision Pro is rejected before conversion.
No character-based estimate, second vocabulary, image Files side effect, or
binary workspace alias is introduced.

`scripts/deepseek-image-reference.py` checks the pinned official Python renderer
and tokenizer SHA256 before regenerating the 12-case synthetic corpus. It covers
all four roles, both native tool output families, PNG/GIF/JPEG/WebP, low/high/auto/
original detail and an 8192-square compressed PNG (header inspection only).
The dedicated Worker/SDK case adds owned-file/HTTPS equivalence, one fetch per
preparation, private/redirect/oversize refusals, no count Sessions/model ledger,
and one controlled create observing the same frozen GIF payload. All provider
traffic in that fixture is synthetic; its usage equality is not hosted vision
usage parity. No paid model probe is required. Compiled execution is pending
the new-source incremental CI, not implied by the offline corpus.
