# Responses MCP and deferred discovery

The Session owns MCP discovery, the immutable schema catalog, caller permissions,
approval intents, dispatch fences, known receipts and canonical publication. Model
providers receive ordinary private functions, never credential-bearing MCP objects.
Tool search uses the same portable local-authority bridge on every Responses
provider; public output remains typed `tool_search_call` / `tool_search_output`.
This avoids a second vendor-owned registry or implicit remote tool harness.

Remote declarations are parsed and bounded before transport. The adapter uses
`rmcp` 3.7.0 protocol DTOs with a Workers Fetch transport: current stateless MCP
and negotiated older Streamable HTTP initialization are separate protocol paths.
Headers are explicit, owner/Response/endpoint-bound AES-GCM ciphertext. A new
approval continuation requires fresh caller authorization. Platform model keys
are not MCP authentication. Redirects are disabled; URL and header authority are
bounded. Global public-origin Fetch, not a private service binding, is used.

A call is schema- and permission-validated before any completed handoff or remote
effect. Approval binds an owned frozen tool/argument/schema/endpoint set. The
actual HTTP call follows a durable dispatch fence. Known results are retained
before continuation; cold in-flight results without a receipt become explicitly
unknown and are never network-replayed. Operation intent/outcome/result records
are purpose-separated encrypted envelopes. Terminal cleanup removes active MCP
credentials from canonical, operation and discovery-cache copies.

Deferred MCP schemas join the Response-local registry as qualified capabilities.
Search loads real schema and caller policy; invocation resolves back to the same
owned MCP adapter. Private aliases and raw authorization never enter public tool
declarations. Client search cannot mint a hosted credential-bearing binding.

`HostedDispatch::Ready` preserves its legacy `From<String>` contract. Its only
rich variant is an opaque MCP decoder result: validated text, decoded raster
images, textual embedded resources and resource links. Audience=user content is
excluded from model input. Audio and binary resources remain available in the
public MCP receipt but return an explicit unsupported-modality model result;
they are never dumped as base64 text or admitted as arbitrary message JSON.

Standalone MCP token counting funds discovery and tokenization as disjoint Worker
intervals. Both are observed and settled, with zero model/tool execution.
Existing non-MCP count/compact billing is unchanged. No migration is required.

## Incremental verification

Only new MCP/discovery contracts run in the bounded MCP workflow. It compiles the
actual Wasm Worker and runs controlled protocol and current SDK journeys with all
unplanned outbound traffic denied. This document records design, not a completed
acceptance receipt; compiler/runtime outcomes must be attached after execution.

## Primary references

- https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/streamable-http
- https://github.com/modelcontextprotocol/rust-sdk
- https://developers.openai.com/api/docs/guides/tools-connectors-mcp
- https://developers.openai.com/api/docs/guides/tools-tool-search

## Modern / legacy HTTP negotiation

The anonymous DeepWiki metadata probe on 2026-10-26 rejected a modern
`server/discover` request with HTTP 400 and JSON-RPC `-32600`, advertising
2025-11-25, 2025-06-18 and 2025-03-26 (plus the older HTTP+SSE revision).
This is an era-detection response, not a failed tool execution. The previous
adapter accepted only method-not-found or non-JSON-RPC errors as fallback signals.

The metadata-only probe now follows the modern HTTP compatibility rule: HTTP 400
without a recognized modern protocol error can enter one legacy initialization.
The typed rmcp error constants distinguish header mismatch, missing client
capability and unsupported modern version, including pre-routing errors with a
null request ID; none authorizes legacy downgrade. Only `server/discover` may
take this transition. `tools/call`, unknown methods and ambiguous effects never
retry or fall back. The fixed endpoint, caller credential envelope and redirect
prohibition remain unchanged.

Legacy initialization offers 2025-11-25 in its body without sending a premature
`MCP-Protocol-Version` header. rmcp's `InitializeResult` validates the response;
the supported server-selected revision and session ID remain on the owned
binding and its frozen approval intent. Subsequent requests use that exact
version. rmcp already accepts legacy tool results without `resultType`; no JSON
normalization or removal of typed permission/output fields is needed. The
deprecated two-endpoint HTTP+SSE transport is not introduced by this increment.

References:
- https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/streamable-http#backward-compatibility
- https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle#version-negotiation
- https://modelcontextprotocol.io/specification/2025-11-25/basic/transports#protocol-version-header

The production limitations of global Fetch DNS authority remain relevant: a
future deployment adding private origins must review SSRF protection rather than
mistaking hostname syntax validation for resolved-address pinning.
## First remote compiler checkpoint

CI run 40333565874 at source `448faeb` passed the full Worker Wasm type check
and 42 new Rust contracts (15 MCP, 14 registry, 11 grammar, one client-search
completion barrier and one alias/namespace identity fence). Actual release
linking then identified the missing `instant/wasm-bindgen` clock backend while
building llguidance's published cdylib. The target-specific feature is now
explicit. No compiled Worker journey ran in that failed build; those 42 accepted
contracts are retained rather than replayed in the next gate.

## Actual Worker checkpoint and remaining increment

Run 40334959346 built the real Worker at `0b92c0d837962451eba9ceb41603fbb5c03b3f60`
(Wasm SHA-256 `c774b77f87495de3e98c0776bc008da423bc2f96d0aa2578adef5c9cd11bb323`).
Its approval journey passed: exact 356-token catalog count, no effect before
consent, fresh credentials, one SSE RPC, model continuation, immutable raw GET,
idempotency and private terminal credential cleanup. Both real negative journeys
from the authority review passed: invalid client search cannot publish a completed
handoff, and an ordinary programmatic-only function cannot be called directly or
by forged provider caller metadata. The new deferred registry contract also passed.

Using that exact retained Wasm locally with repaired fixture runtime configuration
and ledger join, the server-search/strict-invalid/BYOK bridge journeys passed.
The denial and malformed-HTTP-result journey then passed with current SDK 3.37.0
strict response snapshot validation: denial made zero remote calls, one ambiguous
HTTP call stayed unknown, and replay made no second remote call. These were
controlled transports with all unplanned outbound access denied, not live paid
model or third-party MCP service evidence.

The first actual deferred MCP request exposed an old compile-time refusal before
the new resolved catalog join. That refusal is removed and the registry contract
now includes the original MCP declaration. The remaining increment also consumes
each original approval into exactly one destination Response under the source
operation mutex. Same-destination cold retries remain valid; a second branch is
rejected before another effect. Cleanup cannot overwrite that consumption claim.
Mixed direct/programmatic declarations keep their direct capability, while the
programmatic permission never grants direct execution authority. No JavaScript
executor is claimed by accepting that permission metadata.

## Deferred and single-consumption acceptance

Run 40339077048 compiled source `57c76961ef12817a9c9a76f9e82a0fe5f4153053`
(Wasm SHA-256 `0dead210415cca2a71e5312e9638c0159a032f61937a7c2efc9472d5077be979`).
All four new Rust contracts passed. The actual Worker completed deferred search,
approval, one MCP RPC, typed image/resource continuation and strict SDK SSE
validation. The single-destination approval journey passed, including a 409 for a
second destination, no second RPC/model action, and an immutable public parent.

The remaining deferred assertion incorrectly compared an SDK object containing
its synthesized `output_text` helper with an original HTTP body. New creation
observations now capture `.asResponse().clone().json()` before parsing the same
promise; no payload fields are deleted or filtered. The retained local fixture
was then reopened with the exact Wasm for a read-only tail: raw SDK
`.asResponse().json()` GET, native stored SSE and strict current SDK validation
agreed, with zero model/tool dispatch and no new API grants. The typed SDK helper
was checked separately. This tail passed in 3.97 seconds. Gateway authentication
is not re-claimed from the fixture's internal namespace read.
