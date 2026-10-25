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

Rich native public item/event projection, generalized pending client calls and
hosted tool bindings must precede claiming those paths complete. Until that
projection is connected, an unexpected native execution item fails explicitly
after known usage rather than becoming a false completed response. Public item
references and caller-provided opaque reasoning/compaction items also require
owner/provider-bound resolution before entering a platform-funded provider
request; raw vendor IDs are not authorization.

Formatting and diff checks are inexpensive local checks. New native compilation,
actual Worker context/branch/compaction fixtures and live provider acceptance
belong in the captain's Actions gate. No live model call is claimed by this note.
