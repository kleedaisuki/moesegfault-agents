# Native Responses context ownership

This is the native text/function continuation foundation. It is not acceptance
of every rich public item, tool binding, or provider capability.

## Authoritative representation

`session_native.rs` stores native items in an independently addressed, chunked,
append-only log. `PersistedSession.native_context` contains only a generation,
published item boundary, provider binding, and active-message projection cursor.
It is not a map of historical prompts or another execution harness. Native
reasoning, annotations and opaque vendor continuations never enter
`Message.extra` or ordinary public message serialization.

Each input, known provider turn, and context replacement has an operation key
derived from the existing run/round identity. Item chunks are written before the
operation receipt and generation publication. A retry checks the same content
digest and finishes publication; it does not append the turn again. A compaction
starts a new generation and retains older references for historical branches.
Native log reads use the existing bounded multi-key chunk reader.

The provider binding covers the owned account, provider, effective transport,
endpoint, model and immutable credential identity. API keys are neither serialized
nor logged. The same-provider path reuses raw native items; an incompatible
provider can use the existing visible Message projection only when the native
items are explicitly portable. Non-portable transfers fail rather than dropping
public tool/context data or forwarding foreign opaque state.

## Prefix and current request

The system/pinned prefix is rendered first for every request, outside the native
history log. Current top-level custom instructions therefore survive compaction
through the existing durable request, but are not inherited by a later explicit
request. Historical system/developer input items are native history; they are not
also duplicated into a formatted system prefix.

`NativeInputBatch` records resolved native input and the corresponding old/new
Message boundaries. Newly accepted rich items are appended once; later server
tool results and steering use the provider's shared Message-to-item renderer.
The known native output is persisted verbatim before the next provider call.
Failed/cancelled native outcomes are handled after authoritative usage recording,
not mistaken for an empty successful answer.

For native compaction, the summarizer receives the actual native context, not a
lossy Chat projection. Its replacement retains protected historical role
instructions and the summary while the original source and previous native
generations remain intact.

## Integration interfaces

- `validate_native_generation` prepares provider controls without mutating a
  Session or starting inference.
- `prepare_native_generation` synchronizes the private log and gives generation
  the exact `PreparedRequest` representation also usable by input counting.
- `accept_native_turn` consumes only a provider result already durably fenced by
  the existing run pipeline.
- `native_branch_ref`, `native_export`, and `native_import` transfer exact
  immutable boundaries. Import returns a reference without changing live state;
  the branch owner publishes it with the imported Session snapshot.

New serialized fields default to absent and skip absent serialization, preserving
old Session/Chat state. A legacy response without a native reference is not
silently assigned the latest native context.

## Remaining full-contract integration

The semantic projection and generalized caller continuations described below are
implemented. Hosted execution items still require their service-owned bindings;
unexpected unbound execution items fail after known usage rather than becoming
a false completed response. Public item
references and caller-provided opaque reasoning/compaction items also require
owner/provider-bound resolution before entering a platform-funded provider
request; raw vendor IDs are not authorization.

Formatting and diff checks are inexpensive local checks. New native compilation,
actual Worker context/branch/compaction fixtures and live provider acceptance
belong in the captain's Actions gate. No live model call is claimed by this note.

## Native semantic output and caller continuations

`session_native_output.rs` maps private upstream output ordinals to deterministic
platform item IDs and contiguous public indexes. The provider's typed semantic
callback runs while upstream inference is still active; it never forwards vendor
response lifecycle IDs or sequence numbers. Harness-owned function aliases are
not exposed as caller function calls. Final known-usage processing verifies each
streamed item reached its terminal boundary and persists the canonical output
without replaying its deltas. The existing run actor remains the only harness.

Message parts retain phase, annotations, log probabilities and refusals. Public
reasoning contains summaries only; raw reasoning and unsealed provider ciphertext
remain in the private continuation log. Include filtering runs on public event
copies and public response copies, not on private native history.

`GeneralPendingCall` preserves the function/custom distinction, optional
namespace, exact call ID and input. New fields default to empty so legacy
function-only sessions keep their stored representation. A continuation must
supply every outstanding result once, with the corresponding function or custom
output type; a client cannot switch a custom tool return into a function return.
Historical branch points retain this pending-call set alongside native context.
Chat admission and idle processing use one combined pending-client predicate,
preventing an outstanding custom call from accidentally restarting the harness.

Official semantic event and tool shapes:
https://developers.openai.com/api/reference/resources/responses/streaming-events
https://developers.openai.com/api/reference/resources/responses/methods/create

Local verification remains rustfmt and diff checks. Pure projector/policy tests
and the controlled actual-Worker native streaming/fork journey belong to the
incremental Actions gate; no live model execution is implied by this change.

## Owner-bound stateless native item handles

`session_native_items.rs` resolves an item reference or sealed reasoning handle
back to its exact immutable native-log entry. A handle binds the owner, agent,
pinned prefix, provider origin/transport/model/credential identity, public ID,
log generation/index and content digest. It contains no upstream reasoning or
provider ciphertext. AES-GCM sealing reuses the existing account-data key and
purpose-specific associated data; one ciphertext is persisted before canonical
publication so retries cannot reseal an item into different response/event bytes.

Creation, read-only count and Conversation imports use the same resolver. The
provider receives authoritative private native bytes, not caller-edited summaries
or a platform proxy token. Public Message projection remains separate. Bare
references require a retained owned Response; a valid encrypted capability can
resolve its source independently of Response retention, without turning a vendor
ID into authorization. Same-Session resolution stays local to avoid self-RPC.

This slice does not include native auto-window fitting, programmatic origin
rights, MCP or discovery. Existing future WIP for those boundaries is excluded.
The new controlled journey covers actual create/count/stateless continuation,
foreign/tampered rejection before dispatch, current SDK raw response/events and
byte-identical SSE retrieval. Source and syntax checks are not execution receipts;
compiled results are recorded only after the new Worker artifact runs.
### Compiled native-item checkpoint

Actions `40318548395`, source `9681a8d`, Worker artifact `12648367160`
(Wasm SHA-256 `6b4158a466de0e6cbce5b063618d8a1079ff609edfb682fb7f3aa6f574ad933a`)
passed both new capability/routing Rust contracts and the actual create/count/
stateless-reasoning/message-reference flows: three controlled native generations,
two authoritative counters, current Python SDK 3.37.0 validation of original JSON
and SSE, and byte-identical enriched SSE GET. No external provider was called.
The first invalid count was rejected before dispatch but returned 500 instead of
400; the aggregate job therefore correctly failed rather than sealing completion.

Only that failed tail was executed after its domain-error mapping fix. Actions
`40319817718`, source `144a6cb`, artifact `12648678145` (Wasm SHA-256
`1554ac67a56b97919a7bca7670dc731e073a4ec574b30deecedee5d48326eba6`)
passed all four negative count inputs with zero generation/counter dispatch.
The successful positive calls and two Rust contracts were not replayed. Owner,
public identity, pin and provider binding rejection are covered by the executed
capability contract; no live foreign-provider or production rollout is claimed.
Both topic-only CI selectors are explicitly non-promoting and are excluded from
the integrated source. The auto-window and explicit request compaction work
remain separate follow-ups.
## Effective native auto windows

Native `truncation=auto` selects a view of the immutable native log, rather than
removing only the legacy Message projection while silently sending the full log.
The view stores an absolute suffix boundary plus earlier protected instruction
positions. Absent views serialize exactly as older references. History exports,
continued native reads and branches consume the same selected positions; source
journals and older response boundaries remain unchanged.

The existing exact rendered counter chooses a verified fitting whole user/tool
group and reserves the configured output budget. Count and execution share the
same renderer, tools, pin and selected input; upstream truncation is disabled on
that already fitted private request while public policy stays `auto`. Count-only
preparation uses one funded resource operation and never mutates the source
Session. Warm-up preparation is funded without admitting inference. Existing
percentage compaction still operates on the selected active context; public
`context_management` item emission is a separate follow-up, not bundled here.

New pure/property checks cover protected instructions, whole pending groups,
absolute view indexes, old-reference bytes, source immutability and the actual
vendored DeepSeek renderer. Two new controlled Worker journeys cover historical
windows and pending tool pairs; their synthetic counter is explicitly not a
production tokenizer claim. Compilation and real execution are still pending at
this source checkpoint.