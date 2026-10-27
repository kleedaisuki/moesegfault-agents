# Pinned DeepSeek request rendering dependency

Source: https://github.com/deepseek-ai/deepseek-recipe
Revision: `935817c66f5d6e64838f419692545f06787e485f` (version 0.2.0).
License: MIT; original `LICENSE` and per-tokenizer provenance notices retained.

Only the four required Rust crates are included. Source is upstream verbatim
except `PromptTokenizer::from_owned`, an additive consuming constructor with the
same added-vocabulary separation as `new`. It avoids cloning the full vocabulary
during isolate initialization. Neither protocol conversion nor prompt grammar
is reimplemented. The original constructor and encoding behavior are preserved.

The local workspace restricts members and changes tokenizers to `=0.23.2`,
`default-features = false`, `unstable_wasm`. This is upstream tokenizers' own
WASM feature (fancy-regex and getrandom/wasm_js). HTTP downloads, Oniguruma,
C++ training and OpenCV are not enabled. No tokenizer is fetched during requests.
The complete converter remains present because its protocol conversion and
shared stream types reference each other; unused response/stream code is left
for the release linker to eliminate rather than maintaining a grammar fork.

`assets/manifest.json` pins uncompressed and compressed hashes/byte lengths.
Gzip uses mtime zero. Both vocabulary variants are packaged explicitly: V4 Pro
0813 and V4.1 Flash 1017 do not share the same added-token mapping. Only one warm
vocabulary is retained per isolate. New requests use an Arc, never a vocabulary
clone. The 1017 model tokenizer differs from the recipe's V4.1 asset only at the
image token; image counting is deliberately not enabled by this integration.

The Workers wasm build, final gzip script size and actual isolate peak memory
must pass the deployment gate. Dependency feature selection alone does not prove
WASM compatibility or fit within the 128 MiB isolate limit. Input is bounded at
8 MiB, but the cost of long-input tokenization must also be measured before
expanding that bound. This dependency never calls an LLM, fetches user media,
executes tools, or receives provider credentials.

## JSON feature isolation and source inventory

Recipe depends on insertion-ordered JSON for constructed prompt tool definitions.
Cargo feature unification must not enable `preserve_order` on the platform's
`serde_json`: it would reorder existing pinned prefixes and serialized contracts.
We package unmodified mature serde_json 1.0.154 source as the distinct package
`deepseek-serde-json`, preserving both upstream licenses and VCS metadata. Only
its package name differs. Upstream revision:
`20572a6d066958542d79cdd6cf1a7cd9f0c8e1f8`;
crates.io package SHA-256:
`e7e9cc8b1b85264074fbcc02a88680c4096b1e47df8f739dceb03bf482f04bd6`.
No custom JSON serializer is introduced.

Two additional narrow recipe patches expose a bytes-only Responses request
parser and bridge JSON Schema validation through bytes to jsonschema's registry
JSON type. Thus no ordered Value crosses into platform state. All recipe and
serde-json source hashes, including upstream and locally patched hashes, are in
`source-manifest.json`; only those two files and the consuming tokenizer
constructor differ from recipe upstream. Recompute the local hash inventory
when intentionally changing a patch, then review the upstream/local delta.

### Shared retrieval plain-token accessors

The same patched tokenizer source now exposes `encode_plain_text` (genuine byte
offsets, no special vocabulary/BOS) and `count_plain_text` (same IDs using the
mature tokenizer's fast path). Neither returns or clones the underlying model.
The platform wraps these behind a synchronous borrowed callback so an indexing
job cannot retain an owning Arc across await and keep two model vocabularies
alive during profile replacement. No prompt grammar or model billing counter is
changed by these accessors.

For both pinned assets, setting `added_tokens` to an empty list and encoding
UTF-8 JSON with recursively sorted keys and compact separators produces
4,181,828 bytes with SHA-256
`b64f58e1ec67577c2fc4b6b93689b12da288a974e46c088bad52ae6e5defe5f8`.
The new asset-contract test verifies this equality; profile-specific prompt
special tokens remain distinct. Retrieval sizing is not a CF embedding token
measurement, a chat-template count, or an inference billing amount.
