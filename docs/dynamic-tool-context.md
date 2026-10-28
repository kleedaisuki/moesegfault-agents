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

## Incremental execution checkpoints

Normal CI [40647641984](https://github.com/kleedaisuki/moesegfault-agents/actions/runs/40647641984),
source `63e5360a04e7e084fac5cc1d03c991e85b8b465f`, remains **failed overall**.
Its Core all-targets clippy, Worker Wasm compilation/bundle, three new Rust guards
and eight new selector assertions passed. Worker artifact `12798234412` and
verification receipt `12798980204` preserve the exact successful build/phases;
native/tooling receipts are `12798490871` / `12798535727`.

All three new controlled Worker journeys failed at initial context preparation,
before their controlled provider boundary. They are explicit unresolved checks,
not accepted runtime behavior. No old product/cache journey was replayed and no
real model was invoked. Source review and successful compilation are not a
substitute for this gate. Passing phases are retained rather than rerun merely
because the aggregate failed.

## Source review checkpoint

An independent, source-only review covered the new DynamicV2 dispatcher and its
Session/native/count/layout integration on 2026-10-28. This did not run a build,
business test, paid request or production operation. Runtime acceptance is a
separate gate.

Three concrete defects were reported and repaired in the reviewed source:

1. `session.rs` originally parsed the fallible logical dispatcher envelope before
   recording a known, paid `pending_completion`. A malformed envelope could lose
   settlement of already reported usage. The raw completion is now settled first;
   failed projection accumulates usage, clears the known pending result, persists
   its precise diagnostic and then terminates without issuing an effect.
2. A newly sampled bare registered-tool call could bypass wrapper-only
   catalog/schema validation. `registered_tools::completion` now assigns the same
   server-observed catalog digest to fresh known registered names, except actual
   caller-local declarations. Already-journaled legacy pending calls are not
   reclassified or replayed. Non-null wrapper namespaces are rejected rather than
   treated as plain registered authority.
3. An incomplete internal wrapper was removed from the logical call list but
   initially retained as an unanswered native function call, poisoning a later
   user continuation. `effective_output` now omits unissued incomplete private
   calls from effective native replay; the raw provider receipt remains intact.
   Only completed internal invocations become executable logical calls.

The reviewed routes validate named and allowed-tool restrictions against the
logical server-owned call, not merely the shared outer alias. Caller-local
names cannot be converted into server execution through the wrapper. Native
stream/output projection keeps internal calls private while the tool journal
retains logical names and arguments. The sampled catalog digest is server-owned;
current permission/schema checks and original issued-effect recovery remain
separate. Active configuration, skills and response-scoped context positions are
retained through compaction and required-position auto-window selection, and the
count renderer follows the same transition representation.

No remaining P1/P2 defect was found in this bounded reviewed scope after those
repairs. This is a source-review conclusion, not a claim that the new artifact
has compiled or that its real-provider cache behavior has passed.

### Controlled-fixture container capability checkpoint

Normal CI `40647641984` and `40650889172` compiled the new Rust/Wasm artifact
and passed the selected pure guards, but their three new controlled Worker
journeys failed before provider dispatch. This was a missing fixture capability,
not evidence that catalog input or provider schemas were rejected: all three
new journeys enable workspace authority in advance, while ordinary Miniflare
DO state has no native Container binding. Existing `Workspace::ensure_compute_lease`
checks the binding before native preparation, and cold restoration checks its
inactivity fallback before entering the Responses route. Both correctly reject
the missing binding with `Session container binding is missing`.

The controlled fixture must supply a stopped (`running:false`) Container proxy,
with start/connect attempts throwing, so catalog-only and invalid-argument cases
cannot silently start compute. Production container bindings and funding checks
must not be weakened. The exact CI2 artifact `12799584371` can be reused unchanged
for the repaired fixture; no new production build is needed for this correction.
The failed aggregate runs remain failures. One approved bounded local probe used
the already compiled artifact and no provider network calls; its temporary shim
was restored and no debug payload was committed. A precise standalone error code
for an actually missing production Container binding remains a separate
observability improvement, not a prerequisite for this fixture-only correction.

### Known malformed output and maintenance funding scope

After the Container fixture repair, CI `40653880540` passed the new lifecycle and
compact/cold/fork journeys. The remaining busy/revocation journey reached its
known-malformed-output accounting tail: the model completion was settled with
its reported usage, the Session had no active run, and there were no pending
resource observations. Its assertion incorrectly expected every active-Session
reservation to disappear.

One approved exact-artifact local probe identified the sole remaining row as
`maintenance:{session}:0`, with the same idempotency key, an 18 micro-USD reserve,
no ledger entries and no observations. This is the existing `cleanup::ensure`
backstop funded before inference for eventual conversation export, not an
unknown model outcome or leaked run hold. The assertion must validate that
specific owner/Session/archive-generation funding identity separately and require
the actual completion/run holds to be settled. Do not release the backstop or
weaken cleanup funding to satisfy a test. The exact Worker artifact remains
unchanged; only the new assertion's financial scope requires correction.

### Mixed incomplete-output handle mapping

Omitting an unissued private call from effective history also changes physical
entry offsets. Public item IDs must keep their original provider-output ordinal,
but the corresponding `ItemRef.index` must count only retained preceding output
items. Using `history.end - raw_output_count + raw_ordinal` after filtering can
bind a visible message/reasoning handle to the wrong stored entry when it appears
before an omitted call.

The history writer and handle registrar now share `effective_output_indices`,
which consumes the same registered-output selection. Registration uses the raw
ordinal only for the public ID and the retained ordinal for physical addressing.
A new bounded property guard covers mixed retained/omitted output, all-kept
compatibility, and invalid end/count rejection. The new incomplete-flow fixture
adds a public message before the partial wrapper and resolves its owned handle
through count-only input preparation. This preserves raw provider receipts and
canonical streaming identity without creating a false tool-output obligation.

### Real direct-DeepSeek 110K staging checkpoint

[Staging 40660068028](https://github.com/kleedaisuki/moesegfault-agents/actions/runs/40660068028)
deployed exact source `4e5e9ab0204880da08ab53140884b4db81719001` from
successful normal CI `40658272878`, retaining all six hosted feature flags and
unchanged dashboard/container artifacts. Production has not received this increment.

The authenticated OpenAI Node SDK 7.43.0 experiment continued an owned, sealed
approximately 110K history through a new child Session. It used direct
`deepseek-flash`, `tool_choice:auto`, real server-owned Bash once, and separate
server-side catalog/instruction revisions. Percentages below use actual provider
cached input / total input, not estimated count or startup-only probes.

| Phase | Input tokens | Cached tokens | Hit rate |
| --- | ---: | ---: | ---: |
| Warm control | 110,519 | 110,336 | 99.8344% |
| Instructions updated | 110,622 | 110,464 | 99.8572% |
| Tools changed, one real Bash effect | 222,134 | 221,568 | 99.7452% |
| Following turn | 111,360 | 111,104 | 99.7701% |
| Tools removed, instructions updated | 111,571 | 111,232 | 99.6962% |

The Bash row aggregates two model rounds; it is not a 222K single input. The
single journaled command wrote/read a fresh random file, returned exit 0 and the
expected content. Public Responses/SSE did not expose the private dispatcher as
a caller-owned function. The unchanged approximately 110K source consists of
65 imported reference messages, not 65 paid historical turns. The old active
schema experiment's zero-hit mutation remains historical evidence, not this run.

The seventh response compacted but answered the preceding label-only question
instead of its new user request. A single follow-up, originally planned memory
check recalled both the exact random filename and contents correctly, with no
new tool call. Thus factual summary memory survived; replacing working history
with only platform+summary lost the exact active user question. This is an actual
failed semantic boundary, not a successful compact acceptance. The original
receipt remains failed and all known provider outcomes are retained.

The targeted repair persists the exact current user input outside the summary,
including native multipart content, and strengthens only the compact controlled
leaf with a request-only sentinel intentionally omitted from its summary. Source
review also identified that restoring an unbounded since-user tool transcript
could prevent compaction from shrinking a long autonomous task and repeat paid
summaries without progress; the repair must bound retained execution history
and fence repeat compaction at the same journal boundary for every mode.
Previously successful lifecycle/busy checks and paid Bash/configuration calls
must not be replayed. Resume the same child Session for the failed compact tail.
