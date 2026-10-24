# BYOK protocol adapters

## Runtime boundary

Provider identity and wire protocol are different concepts. Platform-funded
OpenRouter and DeepSeek routes remain priced platform providers. User credentials
select only `openai` or `anthropic`, a public HTTPS base URL and a secret API key.
The core model remains `provider = "byok"` with an owner-scoped credential ID;
the credential resolves its immutable protocol and route at execution time.

`provider.rs` retains a single `Completion` / `Message` / `Usage` contract for the
durable harness. `provider_anthropic.rs` translates only at the HTTP boundary, so
the tool loop, steering, compaction, admission and resource accounting do not grow
provider-specific branches. Provider credentials are not serialized with messages,
exposed in user exports, or injected into workspace subprocesses.

| Property | OpenAI-compatible | Anthropic Messages |
| --- | --- | --- |
| Endpoint | normalized base + `/chat/completions` | normalized base + `/messages` |
| Authentication | `Authorization: Bearer` | `x-api-key`, `anthropic-version: 2023-06-01` |
| System prefix | ordered system message | top-level ordered `system` text blocks |
| Tools | `type=function`, function schema | `name`, `description`, `input_schema` |
| Results | tool messages linked by call ID | user content blocks with `tool_result` / `tool_use_id` |
| Successful stream boundary | `[DONE]` after finish reason | `message_stop` after closed blocks and stop reason |

Anthropic base normalization follows the SDK convention: an origin or gateway
prefix such as `https://api.deepseek.com/anthropic` gains `/v1`; an existing `/v1`
is retained. OpenAI-compatible routes retain their supplied versioned path, while
legacy DeepSeek's bare origin remains valid. Explicit stable caching is attached
to the first, pinned system block; it also covers preceding tool definitions while
excluding later mutable compaction summaries. The marker does not move with each
user append, and supported upstreams may reuse the pinned prefix. No vendor-specific session ID or OpenAI streaming option is sent to
Anthropic endpoints.

## Durable continuation and failure semantics

The Anthropic adapter retains the exact ordered content blocks, including signed
thinking and redacted thinking, in `Message.extra.anthropic_content`. The next
request replays these blocks unchanged. Only user-visible text deltas are emitted
to the downstream OpenAI-compatible stream. Adjacent tool results become one user
content array, in their original order and before any subsequent steering text.

Streamed tool input is accumulated as JSON fragments and parsed only when its
content block closes. A complete assistant message is released to the harness
only after `message_stop`. Truncation, malformed JSON, open blocks, mismatched
delta types, duplicate tool IDs, unsupported server-tool stop reasons, and
inconsistent tool/stop boundaries fail closed with `provider_protocol_error`.
There is no automatic protocol fallback or replay of a possibly dispatched tool.
Unknown nonterminal event types are ignored as required by protocol versioning.
Completion content is bounded to 16 MiB for both SSE and compaction JSON.

Anthropic input usage is normalized as uncached input + cache creation + cache
reads. Cache reads separately populate `cached_tokens`; output usage in
`message_delta` is cumulative and replaces, rather than adds to, the earlier
counter. Missing required usage stays unreported, never silently "free reported
usage." BYOK model charges remain zero regardless of protocol; separately metered
container, storage and other resources remain billable through the existing
ledger.

## Security and compatibility

The management and inference paths share base-URL validation. They require public
HTTPS hostnames and reject userinfo, query/fragment, IP literals and reserved local
names. Native fetch uses manual redirects and refuses non-success responses,
without forwarding a key to the redirect destination or exposing provider error
bodies. This is not DNS pinning: Cloudflare's originless Workers fetch boundary is
the network authority; no private/VPC service binding is granted to a custom URL.
Protocol and base URL are immutable for a credential ID, preserving existing
session continuations while allowing API-key rotation.

Migration 0010 preserves existing credential IDs, ciphertext bytes and the legacy
`byok:{account}:{vendor}` encryption context; new generic rows use the immutable
credential ID instead of a vendor in that context. The database's `provider`
column remains a compatibility discriminator, not the new protocol abstraction.
Legacy rows keep `openrouter` or `deepseek`; generic rows use their own unique ID
there and put the actual wire choice in `protocol`. Public metadata maps that
internal discriminator back to the protocol, never treating it as another UI
provider choice.

The full `(account_id, provider)` unique constraint is deliberately retained so
the previous binary's exact legacy UPSERT continues working during migration and
rollback. Default values and a vendor-only insert trigger populate metadata when
that older binary omits the new columns. A partial unique index alone would break
its conflict target even though existing data remained readable. Real SQLite
tests execute the old INSERT/UPSERT after migration, including new legacy rows;
AES tests verify old ciphertext still decrypts and new credentials reject another
owner/credential's encryption context. Older servers do not gain support for the
new generic protocols merely because their legacy operations remain compatible.

The compatibility scope remains text and client-tool messages. Images, server
tools, provider-managed compaction and forced tool-choice/thinking configuration
are not advertised by this adapter. Existing platform-provider payloads, cache
continuation fields and model billing remain unchanged.

## Verification and primary references

Native tests cover request/schema translation, signed-block replay, multi-tool
result grouping, cache-token accounting, cumulative usage, terminal stream fences,
invalid tool JSON and mismatched content. A property test varies Unicode tool
arguments and fragment boundaries over 32 generated cases. Actual compiled-WASM
HTTP journeys separately exercise authorization headers, two tool rounds,
checkpointed session state and ledger behavior; native parser tests do not replace
those integration gates or a real upstream acceptance.

- [Anthropic streaming event contracts](https://platform.claude.com/docs/en/build-with-claude/streaming)
- [Anthropic client-tool definitions](https://platform.claude.com/docs/en/agents-and-tools/tool-use/define-tools)
- [Anthropic fine-grained tool JSON streaming](https://platform.claude.com/docs/en/agents-and-tools/tool-use/fine-grained-tool-streaming)
- [Anthropic prompt caching and usage](https://platform.claude.com/docs/en/build-with-claude/prompt-caching)
- [DeepSeek Anthropic-compatible API](https://api-docs.deepseek.com/guides/anthropic_api/)
