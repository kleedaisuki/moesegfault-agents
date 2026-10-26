# Request-scoped context management

Runtime slice after the public compact/count checkpoint. `context_management.rs`
is now registered and `session_context_management.rs` adapts the existing Session
manager. Compiled Worker acceptance remains an Actions gate, not a local claim.

## Public contract

The current [OpenAI compaction guide](https://developers.openai.com/api/docs/guides/compaction)
defines server-side compaction as part of Responses generation: rendered input
crosses `context_management[].compact_threshold`, compaction runs without a
separate compact endpoint call, a compaction item appears in the current response
stream, and inference continues on the reduced window. Stateless callers carry
the output item forward; callers using `previous_response_id` append only new
input. This is more than echoing a policy field in the returned object.

The current SDK schema describes each entry as `type: compaction` with optional
positive absolute token threshold. This is distinct from the product's pinned
Agent compaction percentage, which remains the user-facing configuration and
is converted using that pinned model's known context window. An omitted/null
threshold can use that existing model-derived platform default; it must not
invent a context size based on an upstream model name guess.

## Shared manager integration

1. Resolve one policy for the admitted Response. Absent/null/empty API policy
   preserves existing internal Agent compaction behavior. Explicit entries use
   the requested thresholds and request standard output-item publication.
2. Evaluate explicit thresholds only against exact rendered native input,
   including fixed prefix, effective current instructions, historical protected
   roles, tool bindings and model-control input effects. Reuse the same prepared
   request and official counter/provider count capability. A working-context
   estimate remains valid only for the pre-existing implicit product policy.
3. Call the existing `SessionInner::compact_if_needed` manager and its durable
   `compaction_in_flight` / `pending_compaction` phases. Do not add another paid
   summary loop or invoke the standalone account actor inside a Session run.
4. Journal the known summary before settlement, replace the native working
   window through the existing native-context replacement, and retain immutable
   source boundaries for old Responses. Do not alter the source conversation or
   user-input inactivity clock.
5. For an explicit policy, publish a service-sealed compaction output item in
   the current canonical response/SSE stream and continue the same run. Item
   sequence/index reservation and typed added/done events must use the existing
   output machinery. Protect fixed prefix, historical roles and effective
   response-scoped instructions; compact-only guidance never becomes ongoing
   instructions.

Repeated entries represent independent triggers. The proposed normalized policy
uses the smallest threshold so a single compaction satisfies every currently
crossed entry without a series of paid summaries. This is a platform resolution
decision, not a claim that the official guide defines duplicate-entry ordering.

## Discriminating acceptance

- An explicit threshold just above/below an independently counted input causes
  zero/one compaction, not an estimate-driven early or missed compaction.
- Text/tools/native controls use identical prepared bytes for counting and
  generation. Unsupported exact renderer/media capabilities fail explicitly
  before paid compaction; no guessed token counts or all-true control effects.
- A compaction item is emitted and the continuation uses the replaced native
  window, while a historical retained Response still exports its original
  source boundary.
- Provider interruption/cold recovery cannot issue the same paid summary twice;
  known usage settles once, unknown outcome remains funded and diagnosable.
- Old Chat and old pinned product policies retain their established rendering
  and configuration semantics; percentage configuration is not replaced by an
  absolute-number dashboard field.

## Implemented runtime boundary

Implicit product compaction retains its old position and working-size estimate.
For an explicit request, the Session prepares its actual native generation
request (including bound tools and controls), counts that exact representation,
and passes the measurement to the same compaction manager. Count-only work runs
inside the existing admitted Session/resource observations; it creates no second
account inference operation. A legacy/non-native or unsupported exact renderer
returns `context_management_exact_count_unavailable`, never an estimated success.

The durable known-summary application builds a service-sealed item, replaces the
native working window through the existing native-context log, appends canonical
added/done events and continues the same run. The original source journal and
historical Response boundaries are untouched. Private `compaction_source_end`
records the source boundary that was compacted. An unchanged source cannot buy
another summary merely because a deliberately tiny threshold is still below the
protected prefix or summary size; genuinely new source can trigger again.

The new compiled journey `Responses request context management emits sealed item
and continues` uses a native provider fixture reporting 42 rendered tokens. A
threshold of 43 buys no summary; 41 buys exactly one summary and one continuation.
It checks typed item events, native-window replacement, reusable encrypted state
and retained prior input identities. Pure tests cover exact/estimated distinction,
boundary idempotency and independent threshold ordering. These tests are wired
but not reported as compiled/passed before the next Actions run.

## Compiled shared-lifecycle checkpoint

Actions `40339011992`, source `192078d`, built Worker artifact `12657473101`
and passed the `context-capacity` lifecycle through the actual Rust Worker and
current Node SDK. Only one summary and one answer generation executed against
the controlled native provider; no external provider was called. The fixture
observed reserved output escrow inside both provider callbacks, terminal
`converted` state with zero remaining reservation, refund of unused capacity,
and exactly two settled known-usage BYOK model rows.

The same lifecycle preserved fixed server, current custom and historical
developer instructions after native replacement; emitted one sealed item with
canonical added/done events; retained Conversation items and logical bytes on
standard metadata DELETE; physically purged body/admission/chunk storage before
refunding body allocations; retained header/identity charges; and cold-read the
independent retained Response/input items plus its exact count after purge.
Actions `40338678726` failed compilation on an accidentally retained independent
workspace-import caller, before any generation. The subsequent pure repair
removed that unused hook; no accepted generations or nine SQL cases were
replayed. This is compiled controlled-provider proof, not real-provider staging
or a claim that an integrated Code/media artifact has identical source inputs.
