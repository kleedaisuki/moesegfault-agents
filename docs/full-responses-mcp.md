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

The production limitations of global Fetch DNS authority remain relevant: a
future deployment adding private origins must review SSRF protection rather than
mistaking hostname syntax validation for resolved-address pinning.
