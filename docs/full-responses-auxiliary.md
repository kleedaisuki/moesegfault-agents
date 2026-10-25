# Responses compaction and input-token counting

Implementation slice owned by the full Responses workstream. This note records
the public contract and integration boundary; it is not endpoint acceptance.

## Contract grounded in primary sources

- [Standalone compaction](https://developers.openai.com/api/docs/guides/compaction)
  accepts a complete window or `previous_response_id` and returns
  `object: response.compaction`, an opaque ID, `created_at`, `output` and usage.
  Its output is the complete next input window, not merely a generated assistant
  reply. Callers must be able to pass that window back into Responses unchanged.
- [Compact reference](https://developers.openai.com/api/reference/resources/responses/methods/compact)
  also defines custom instructions, cache key/options/retention and service tier.
- [Token counting](https://developers.openai.com/api/docs/guides/token-counting)
  counts the actual rendered model input, including message boundaries, tools,
  images and files. Counting JSON bytes or dividing characters by four cannot
  implement this endpoint.
- [DeepSeek Responses](https://api-docs.deepseek.com/guides/responses_api/)
  now documents a native Responses endpoint, custom tools, tool search, reasoning
  items and Flash images. Its public guide does **not** document a standalone
  compact or input-token-count endpoint.
- [DeepSeek token usage](https://api-docs.deepseek.com/quick_start/token_usage/)
  publishes an offline tokenizer package, but does not establish exact rendered
  image/tool counts. Its image calculation is expressly an estimate. Do not
  claim that an offline text tokenizer closes the full default-model count gap.
- Anthropic's [count endpoint](https://platform.claude.com/docs/en/api/messages/count_tokens)
  provides vendor-reported counts without generation. Its Messages rendering must
  be the same rendering used by the actual generation adapter.

## First module and explicit interfaces

`crates/agents-worker/src/responses_auxiliary.rs` contains:

- `CompactRequest` with current standalone controls and bounded validation.
- `CountRequest`, keeping full tool/policy values for the shared Responses codec
  rather than duplicating the old function-only schema.
- `CompactionState`, a service-owned `mcmp1.` AES-GCM envelope using existing
  `agents_auth::seal/unseal`. AAD binds owner and item ID; payload validation also
  binds the agent/model identity and pinned-prefix hash. Protected instructions
  and compacted context survive exactly. This is not an OpenAI reasoning blob.
- `compacted_response`, which only emits standard numeric usage when reported
  totals and reasoning details are known. It never invents zero reasoning counts.
- `CountProtocol::{Responses, Anthropic}` and `count_native`, a bounded 30-second,
  redirect-denying, bounded-reply POST to the provider's count-only endpoint.
  Errors never include upstream response text or credentials. Provider calls
  inherit a fresh child trace and native `provider.count` span.

The count caller must first resolve owned conversation/response references,
registered tools/skills, fixed prefix, custom instructions and files. It must pass
the **same prepared context and codec** used by generation, replacing only the
platform agent ID with its upstream model ID. Passing platform IDs to the vendor
or counting native Responses while generating a different Chat rendering is not
an exact count. The module rejects unresolved platform references. It does not
reserve inference capacity or run the model; the route must use existing
resource-only accounting.

`count_prepared` accepts the native adapter's `PreparedRequest`, so counting and
generation share that renderer rather than separately serializing messages/tools.
It uses the same account-only resource reservation as `count_account`; no model
reservation, synthetic Session or execution slot is created by counting.

The parent owns the shared route/codec hook and must add the module to `lib.rs`.
The standalone Rust file is intentionally not wired to an unbilled edge handler.
Pure tests cover controls, token response validation, authenticated-envelope
owner/item/model/prefix separation, and unknown usage rejection. New-source
compilation and actual provider-fixture endpoint tests belong in Actions.

The existing Node SDK 7.43.0 auxiliary-route contract fixture passed two offline
cases (`tests/worker/responses-auxiliary-contract.test.mjs`): count uses only
`POST /responses/input_tokens`, and compact returns a complete window whose
opaque compaction item can be passed unchanged into the next create's `input`.
These use injected fetch and establish SDK compatibility only, not Rust endpoint
acceptance or a successful compaction/model operation.

## Durable compaction integration

Use one defaulted `RunPurpose` discriminator (`Generate` / `Compact`) in the
existing Session DO run, not a parallel harness or several optional task flags.

1. Resolve the exact owned input window and pin before admission. Preserve
   server/agent instructions and this call's custom instruction separately from
   the summary prompt. Validate pending tool groups without executing them.
2. Persist admission identity and purpose before acknowledging or calling a
   provider. `Idempotency-Key` selects this compact operation, not a fresh normal
   response. The caller does not gain an arbitrary workspace or history fork.
3. Reserve/bill the one compaction model call through the existing run/completion
   ledger. Persist the pending summary before settlement or constructing output.
   An uncertain external outcome is not permission to repeat the model call.
4. Return retained input items plus an authenticated compaction item representing
   the compacted active window. Do not reveal private reasoning or fabricate a
   provider's encrypted item. Reusing the returned window must restore the same
   protected instruction context, checked against its pin/model binding.
5. Persist an immutable `CompactedResponse` receipt before successful transport.
   Retrying/observing it does not rerun compaction or alter the original source
   journal, workspace, user-append clock, or previously returned responses.

This requires the parent state integration hook; the current module does not
claim that adding wire types alone implements `/responses/compact`.

## Account model work without fake Sessions

The D1 model reservation/ledger schema already permits an absent Session. New
`store_auxiliary.rs` shares the existing admission/reserve/record implementation
between optional Session scopes. The established `Store::admit_run`,
`reserve_completion` and `record_completion` signatures remain wrappers with
`Some(session)`; their retry hash remains byte-for-byte unchanged. New
`admit_operation`, `reserve_operation_completion` and
`record_operation_completion` use `None` and a separately tagged admission hash.

Both scopes use the same account-plan concurrent execution INSERT, allowance
synchronization, monetary reservation vintage, model pricing snapshot, known
provider usage validation, immutable ledger append and settlement. Account work
does not acquire a workspace/archive cleanup lease or create a `sessions` row.
Null-safe scope checks use `session_id IS ?`, including owner and exact run ID.
This is one financial contract, not an account path that bypasses concurrency.

The four focused SQLite tests execute production SQL against all real migrations:
account/Session work shares a quota, other owners/scopes do not match, known model
charges settle once, and ending execution leaves uncertain model money reserved.
All four pass; the complete SQL suite passed 105 cases. Final compiled integration
remains an Actions gate.

## Remaining exact-count dependency

For an explicitly configured native Responses or Anthropic count capability,
the provider's reported count can be normalized directly. A 404/405/501 is an
explicit capability absence, not a successful zero count. Default DeepSeek and
OpenRouter require either a documented native counting capability or a matching
model-specific renderer/tokenizer with a justified treatment of tools and media.
This remains a completion gap until that contract is implemented and tested.
Never probe it by secretly buying a normal completion, return an estimate under
an exact-count claim, or weaken the financial rule that actual provider usage
remains the billing authority.

## ModelOperation durable implementation

`model_operation.rs` implements the approved `MODEL_OPERATIONS` binding and
exported `ModelOperation` SQLite Durable Object. It is a single paid summary
operation, not a second agent harness: no tool execution, workspace, Session row,
conversation mutation, or user-input clock changes. The edge resolves the same
owned model, pinned prefix, protected system/developer messages and source input
as generation, then constructs `PreparedCompact`. The `instructions` field in
that private structure is the ongoing response-scoped protected instruction;
`guidance` is compact-only guidance and never replaces it.

`auxiliary_routes::compact` accepts this resolved input, hashes the owner and
validated retry key into a deterministic operation identity, and dispatches a
private terminal-only stream. A missing retry key gets a new one, returned as
`Idempotency-Key`, along with `X-Agents-Operation`. A detached stream must retry
the original compact call with that same key, never retrieve a normal Response.
The operation checks the immutable request hash before accepting followers.
Native streaming is reused for the actual provider call; native Responses
models use the shared `PreparedRequest` codec and all provider secrets are
resolved afresh via `provider::resolve_config`, never persisted in actor state.

The paid-call fence is `accepted -> in_flight -> known -> complete`. Only an
accepted operation without a durable provider result can issue inference. The
provider result and measured wall duration are chunked and published before
billing. Recovery finds this result even if the phase write was interrupted.
An in-flight or known operation missing its result becomes failed/unknown and
cannot call the model again. Known model usage uses the same Store ledger and
price vintage as Session inference. Wall observations are declared before I/O,
recorded idempotently from the durable measurement, and settled through the
resource meter, not through unconditional reservation release. A cold I/O loss
with unknown duration deliberately leaves pending funded observations rather
than inventing a zero measurement.

Private input, provider result and output have a maximum 24-hour recoverable
retention. The alarm first records an expired tombstone, then erases all chunk
content, retaining only owner/operation identity, request hash, timestamps,
phase and non-content resource receipt. Expired keys cannot execute again.
Content erasure precedes final resource settlement so accounting failures do
not indefinitely retain private context. Completed output retains canonical
user items followed by an owner/item/model/prefix-bound service-sealed
compaction item containing protected messages and the summary.

`expand_compaction_items` authenticates and expands a service item using the
current owner/agent/prefix. It preserves ordinary items and returns protected
ongoing instructions separately for the common codec to merge exactly once.
Multiple compaction items in the same window are rejected. Provider-native
encrypted reasoning is a separate item type and is not claimed or forged by
this service token.

Current official required compaction usage details include both cache-write and
reasoning token integers. A missing provider detail is not known zero. The actor
settles authoritative known model usage, then returns
`provider_unsupported_usage_detail` if it cannot construct this required shape.
This capability gap is explicit, including for the default provider; it must not
be described as a successfully completed full native compact implementation.

Actions must compile and test the actor and actual route integration. Local
verification is limited to rustfmt, diff checks, the real SQL suite, and SDK
contract tests. The public source-resolution hooks are owned by the Responses
integration lead; adding this actor alone is not public endpoint acceptance.
