# Dynamic Agent context and stable tool dispatch

Protocol investigation, 2026-10-28. This note supports replacing the frozen-Agent
pin invariant with versioned, effective Session configuration. It is not a claim
that a new runtime or provider capability has already been deployed.

## Decision

For the direct DeepSeek production path, use a **genuine, fixed-schema dispatch
function** plus an append-only, server-owned configuration/catalog record. Keep
only the minimal platform contract and stable dispatch protocol at system level.
Treat ordinary Agent instructions, skills and changing tool descriptions as
versioned context rather than repeatedly rewriting the first system message.

Do not implement this by inventing an `additional_tools`/`tool_loading` item or
fabricating a discovery call/result pair to carry a configuration annotation.
The current public HTTP contract does not establish those as a generic
add/replace/remove operation.

## Evidence and capability boundary

The project's vendored official recipe is DeepSeek revision
`935817c66f5d6e64838f419692545f06787e485f`, version 0.2.0; see
[`VENDORED.md`](../third_party/deepseek-recipe/VENDORED.md).

| Mechanism | Evidence | Consequence |
| --- | --- | --- |
| Top-level function `tools` | `deepseek-recipe-encoding/src/v4/dsv41.rs::render_conversation` appends their prompt to the first system message | Changing a callable schema changes the early rendered prefix even when the platform's stored pin is unchanged. The 110K measurement observed a warm-cache drop from 99.84% to 0%. |
| `tool_choice:none` | `responses/request/tools.rs::convert_tools` returns an empty tool vector | It cannot validate the cache behavior of an active tool-schema change. |
| `tool_search_call` / `tool_search_output` | `responses/request/schema.rs`; `tools.rs::from_declarations`; `convert.rs::transform_input_items` | The official recipe supports client-executed search on Flash, with matched call IDs. Discovered schemas become deferred global definitions and an inline `System { tools: Some(...) }` message. This is additive discovery, not general revocation/replacement. |
| Repeated schema names | `protocol/client_tools.rs::convert_client_tools` rejects duplicate names across the collected declarations | Appending a changed definition with the same name can invalidate the entire request. Removing a declaration cannot erase its historical call/output obligations. |
| `additional_tools` / `tool_loading` | Not variants in the current recipe's `ResponsesTypedInputItem`; unknown item types deserialize as unsupported and are ignored | Do not infer model-visible loading merely because a custom JSON object was accepted by the HTTP endpoint. |
| `developer` role | Public Responses guide says it is treated as user; current recipe `convert.rs::push_message` maps Flash developer to system and Pro developer to user | This documented/source mismatch forbids a universal authority claim. Moving dynamic content from `system` to `developer` is not a reliable way to avoid system rendering. Use ordinary context for ordinary Agent settings, keeping security decisions in the backend. |

The official [Responses guide](https://api-docs.deepseek.com/guides/responses_api/)
lists supported input items and says other types are ignored. It supports normal
function tools and `none`/`auto`/`required`/named choice, but does not document a
general incremental tool-replacement control. The official
[recipe README](https://github.com/deepseek-ai/deepseek-recipe) explicitly lists
Flash client tool search. The recipe provides conversion/rendering, not the live
HTTP service; the stronger local capability is not by itself proof of endpoint
acceptance. No paid capability probe was performed for this investigation.

The [Tool Calls guide](https://api-docs.deepseek.com/guides/tool_calls/) explicitly
permits inserting actual call/result history in Responses and Anthropic APIs.
That permits truthful externally initiated operations; it does not make a fake
discovery transaction an appropriate representation of an ordinary config edit.

## Portable construction

### Evaluation consequence

The [BFCL ICML 2025 paper](https://proceedings.mlr.press/v267/patil25a.html)
separates valid function/argument generation, abstention and stateful multi-step
behavior. A fixed outer dispatcher removes provider-side constraints on each
inner tool schema, so cache-prefix equality alone is insufficient acceptance.
Our targeted increment must additionally execute a real registered tool, reject
invalid inner arguments without an effect, correct a stale/revoked selection,
abstain for an empty catalog, preserve caller-local ownership and recall a
concrete earlier tool result after compaction. This is a local design-informed
test selection, not a claim to reproduce BFCL scores or run its benchmark.

The provider sees one reserved server dispatcher with a stable name, description
and JSON schema. Its arguments identify the logical tool and its input object.
The provider decision boundary records the observed catalog digest as trusted
server provenance; the model does not need to copy a hash into every call. This
avoids treating model-written version text as authority or causing needless
argument errors. Do not put revisions, names, inventories or changing
descriptions in the outer schema or description.

Compute the catalog revision from effective tool schemas/availability, not the
overall Agent revision: an instructions-only edit must not stale an otherwise
valid tool call. The current execution grant is still checked independently.

For a free-form `arguments` object, leave vendor `strict` unset/false. DeepSeek's
[strict-mode guide](https://api-docs.deepseek.com/guides/tool_calls/) requires
closed object schemas and required properties; that is not a per-tool schema
validator for an arbitrary nested argument dictionary. A JSON-string argument
could satisfy a closed outer schema but adds escaping/parsing burden. Backend
validation of the real inner schema is required in either representation.

```text
stable provider declaration: call_registered_tool(name, arguments)

history:
  ordinary user/task messages and actual tool results
  server-owned Agent configuration revision N:
    instructions, tool descriptions/schemas, additions, replacements, removals
  subsequent model call:
    call_registered_tool("bash", {"command": "..."})
  server-owned call provenance: observed catalog N
```

This is analogous to MCP's separation of
[`tools/list`, list-change notification and `tools/call {name, arguments}`](https://modelcontextprotocol.io/specification/2025-11-25/server/tools).
The catalog revision is an Agents concurrency/provenance extension, not a claim
that MCP requires this field. The official
[Go SDK server guide](https://go.sdk.modelcontextprotocol.io/server/) makes the
corresponding server obligations explicit: validate input, execute the handler,
validate structured output and represent tool errors as results.

Required invariants:

1. The dispatcher declaration exists even when the active catalog is empty. A
   conditional outer declaration would reintroduce the same prefix transition.
2. Catalog/config state is owned by the Session and versioned independently of
   model observations. Text resembling a catalog message cannot grant a tool.
   Ordinary-context placement is not an authentication mechanism.
3. Before issuing an effect, resolve the logical name against the active
   registration, validate its **inner** schema and enforce ownership, workspace,
   caller and revision authority. The vendor's fixed outer schema no longer
   performs per-tool argument validation.
4. A stale or revoked call returns a structured correction with current revision
   and availability, rather than a terminal `tool_not_found` fiction. Do not
   execute a removed tool merely because its old schema remains in history.
5. An already-issued effect is recovered under the original execution ID and
   registration provenance. Revocation must neither replay it nor claim it never
   ran. A budget-admission record alone is not an issued-effect receipt.
6. Count and generation use the same catalog events, fixed declaration and role
   representation. Compaction retains the latest effective catalog/settings,
   not every obsolete version or only a textual promise to remember them.
7. One dispatch is one logical tool operation: no extra model discovery round,
   no double tool-budget charge and no second execution identity for the wrapper.
8. Preserve logical tool names, arguments, call IDs, result pairing and
   diagnostics. Private provider envelopes remain in native replay history. Do
   not rewrite the provider's historical call into a different wire operation.
   Server-owned calls remain absent from standard caller-local function-call
   items/events; their logical calls remain in Session journals/exports. Actual
   caller-local tool calls and continuations retain their public SDK contract.

Changing instructions/tool authority does not implicitly authorize moving model
credentials, reallocating an existing workspace or changing a user-owned history
boundary. Legacy serialized pin fields may remain compatibility/provenance
records without remaining the active Agent-policy authority.

## Existing implementation mechanisms worth reusing

- [`tool_discovery.rs`](../crates/agents-worker/src/tool_discovery.rs) already has
  `INVOKE_ALIAS`, a generic `{tool, arguments}` provider envelope,
  `restore_item` for public name/namespace/inner-argument restoration, and
  `restore_event` buffering for incomplete provider envelopes. The
  `invoker_restores_inner_arguments_without_exposing_private_envelope` check
  records the contract. Reuse the pattern, not its response-scoped loading state
  if that would obscure Session ownership.
- Its current `SEARCH_ALIAS` description embeds an `Inventory` JSON list. Reusing
  that declaration unchanged would still change the first system prompt on a
  catalog edit. Its conditional `has_invoker()` exposure likewise must not define
  the new always-present dispatcher interface.
- [`session_configuration.rs`](../crates/agents-worker/src/session_configuration.rs)
  already supplies safe-boundary adoption and issued-effect recovery.
  [`tool_rendering.rs`](../crates/agents-worker/src/tool_rendering.rs) centralizes
  private alias rendering/choice policy. Preserve caller-local tools and hosted,
  MCP and programmatic ownership; do not accidentally turn a client callback into
  a server effect when logical names collide.

## Trade-off and discriminating acceptance

The benefit is a stable provider tool prompt while the real catalog evolves in
history. Costs are weaker vendor-side per-tool constrained decoding, some extra
envelope tokens, and potentially changed model tool-selection accuracy. These
are not solved by a prompt-cache percentage. Backend validation is mandatory;
selection/argument quality needs an actual new-tool invocation, not label-only
output under `tool_choice:none`.

The useful new acceptance is a warmed approximately 110K context, registered
tool addition and real invocation through the dispatcher, same-name schema
replacement, removal/stale-call correction and compact/cold recovery. Compare
the changed request to its adjacent warm control under `auto`; inspect both
actual provider cache reads and logical execution receipts. Do not replay the
already accepted startup or old product suites.
