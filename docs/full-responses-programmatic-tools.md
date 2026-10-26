# Programmatic tool execution

## Contract and implementation boundary

The stable Responses feature runs JavaScript, not Python. Its public causal
items are `program`, nested tool calls with `caller.type = "program"`, and
`program_output`. The latter retains the parent program's call ID. Function and
custom results must return that same caller relationship. Program tools have no
direct network, Node environment, general filesystem or subprocess authority.
Deferred tools become eligible only after authenticated tool discovery.

Source: [official programmatic tool guide](https://developers.openai.com/api/docs/guides/tools-programmatic-tool-calling).

The platform executor is Cloudflare Dynamic Workers. A unique opaque
identity binds owner, Session, admitted program and code; it is not shared between
programs or accounts. `get()` selects an opaque cached definition/instance, not a resumable JavaScript
request. It does not guarantee that an isolate remains alive. `globalOutbound: null`, an empty
environment, explicit Node-disable flags and resource limits are mandatory.
The dynamic isolate receives no provider key, management token, R2, D1, namespace
or raw Session stub. Program JavaScript can only propose tools to its Rust actor.

Sources: [Worker Loader API](https://developers.cloudflare.com/dynamic-workers/api-reference/),
[resource limits](https://developers.cloudflare.com/dynamic-workers/usage/limits/).

Actual workerd probing found that plain-string modules require a `.js`
extension, not `.mjs`. Current compatibility dates enable Node by default:
both `no_nodejs_compat` and `no_nodejs_compat_v2` are explicitly required.
Production preserves the default cross-request Promise protection. Returning a
paused JSON response ended the original context; merely leaving its response
stream open also failed with a hung-request error. Instead, the Rust actor keeps
one full-duplex NDJSON request open. The program awaits real input I/O while
the actor reads proposals and feeds durable results through that same request.
An actual workerd test executes two sequential awaits on this original context.
The experimental JSON/replay profiles remain disabled, not fallbacks.

Source is inserted as a JSON string literal, not unquoted module statements.
A trusted startup `AsyncFunction` compilation gives it no module lexical scope;
execution begins only after the funded request. The child explicitly enables
`allow_eval_during_startup`. This prevents a closing brace in user source from
escaping into startup code or accessing the trusted wrapper's active channel.

Source: [compatibility flags](https://developers.cloudflare.com/workers/configuration/compatibility-flags/).

## One actor and actual continuation

The Session remains the only harness. It admits an immutable `ProgramPolicy`,
persists `ProgramState` before runtime invocation, and independently authorizes
every nested request. A JavaScript-supplied nonempty `caller_id` is not authority.
Only a validated actor journal mints `AuthorizedInvocation`, which deliberately
cannot be deserialized from a client payload. Server time is available only when
the current actor has pinned `time` and explicitly includes it in this program's
grant. No corresponding grant to Bash or MCP is implied.

The JavaScript wrapper retains a live async stack within one source request.
It returns bounded proposed calls when awaiting `tools.*`. The Rust actor validates
the complete batch, persists it, and dispatches an existing server
tool. After a known result is durable,
the actor resumes the original JavaScript promise. This avoids reverse HTTP
callbacks and actor reentrancy while a program is waiting for a tool.

Each nested call has an immutable sequence, target, argument digest and stable
execution identity. The actor records dispatch before any effect. An ambiguous
dispatch cannot mint another execution authority. Client outputs must match both
the exact pending call and its authenticated program parent; output schemas are
checked before a program receives structured results. Invalid strict arguments
or results fail canonically after known model usage, without hidden paid repair.

## Runtime loss is not replay permission

After a `start` intent has been committed, a re-entered actor without the original
channel produces an incomplete program,
retaining any uncertain financial/effect records. It never silently reruns source.
Normal execution preserves native Date and Math behavior and actual promise
settlement order. The wrapper contains an explicitly disabled future replay
profile with recorded entropy; it is not a production fallback or an arbitrary
V8 snapshot. An opaque fingerprint currently identifies authenticated retained
actor state, not a promise that Cloudflare can persist an arbitrary JavaScript
heap across isolate eviction.

Existing public Responses and store/connection retention rules still apply.
Fingerprint round-tripping does not grant another owner access, extend a lease,
reintroduce removed capabilities or justify indefinitely retaining intermediate
results. Known settlement is allowed after expiry, while new effects are not.

## Limits and observability

The initial profile bounds code at 64 KiB, policy at 256 KiB, each argument/result
at 1 MiB, the nested-call journal at 4 MiB and total calls at 256. A runtime turn
proposes at most 64 calls. These are platform limits, not claims about an upstream
provider's maximums. Rust performs authoritative byte/schema/permission checks;
the untrusted JavaScript wrapper's limits are defense in depth.

Console and `reportError` are unavailable to the program. Raw source, values,
exceptions and images are user data, not telemetry. `tails: []` prevents a user
isolate from being attached to the platform log pipeline. Dynamic Worker logs
are not automatically inherited by loader Worker Logs; without an attached tail,
the documented default discards them. Rust emits only fixed outcome codes and
trusted correlated identifiers. Module-load failures are also normalized without
reflecting exception text. Actual staging tests must verify all these channels,
including unhandled rejection and malicious module-body injection.

Source: [Dynamic Workers observability](https://developers.cloudflare.com/dynamic-workers/usage/observability/).

## Resource accounting

Root approved the staging-only public-rate allocation
`dynamic_worker_creation_day = 2000 micro-USD`. Charge it at most once for a real
distinct opaque owner/operation/code identity in the UTC day. The upstream bill
has included allowances; this allocation is not a claim about an account's final
Cloudflare invoice. `.load()` creates a new identity per invocation; stable
`.get()` does not charge creation again merely because a program awaits a tool.

Requests use the explicitly configured `dynamic_worker_request` tariff. The
approved named capacity allocation `dynamic_worker_cpu_slot_100ms = 2 micro-USD`
has revision `2026-10-25-dynamic-allocation-v1`. Its one slot is the enforced
100-ms budget of the original source request, not 100 ms per await and not an
observation of actual CPU consumption. Only the native channel can mint the
non-deserializable budget proof after a valid execution frame. The financial
helper pins this tariff before invocation and settles the same accepted program
identity once; an unproven outcome retains its bounded hold and explicit gap.
Early return does not refund a published capacity allocation. Startup allocation
belongs to the creation charge, not an invented CPU measurement. Waiting does
not allocate another slot. Root Worker charges remain distinct. The allocation
profile revision is separate from the shared deployment tariff version; all
RESOURCE_PRICING_JSON entries retain the existing common-version invariant.
Staging binding/deployment is separately authorized; production is unchanged.

Source: [Dynamic Workers pricing](https://developers.cloudflare.com/dynamic-workers/pricing/).

## Integrated Session path

`programmatic_tools.rs` owns authority/journaling, `programmatic_runtime.rs`
owns the native Loader boundary, and `session_programmatic.rs` is an actual
Session subroutine. It is registered at public request admission, shared native
rendering/token counting, provider choice validation, dispatch and canonical
publication. Missing Loader binding or tariffs reject declared programmatic
execution before inference; ordinary API calls acquire no new requirement.

The supported verticals are pinned server time and owner-bound MCP capabilities.
MCP permission is not inferred from a name: `AuthorizedInvocation` is minted
after the write-ahead journal, the existing loaded registry validates the
programmatic caller, the shared allowance admits that execution identity once,
and MCP rechecks owner/caller/schema at its own durable effect boundary.
`tools.<server_label>.<tool>` receives MCP `structuredContent` when available,
otherwise model-visible text content. Transport metadata, credentials and
user-only text are not passed to the JavaScript program. Binary MCP results
without a structured representation remain explicitly unsupported here; direct
MCP retains its typed rich-content path.

The public sequence is `program`, causal nested function calls and outputs,
then `program_output`; nested calls and outputs carry the exact program caller
relationship. A private provider alias never enters that sequence. Original SSE
and raw GET are compared without filtering SDK convenience fields, and the
current strict Python SDK validates the unmodified wire objects. `program_output`
is terminal-only: its added event carries completed/incomplete already. It uses
the existing canonical publisher directly, not the hosted tools' synthetic
in_progress projection.

A human-approval-required MCP call produces an incomplete program without
remote execution. Client-owned function/custom handoff and program image
publication are not enabled: a lost original request is not transparently
replayed, and no second JavaScript agent harness exists. Independent accepted
operations get fresh opaque contexts. Cancellation interrupts the pending native
read without allocating another request or granting a later nested effect.

## Verification boundary

The earlier workerd/Miniflare component proof passed eight isolation/duplex checks;
it is retained, not rerun by this integration. Actual Wasm compilation and all
21 newly registered programmatic Rust contracts passed in
[GHA 40339977394](https://github.com/kleedaisuki/moesegfault-agents/actions/runs/40339977394).
That run stopped only because its broad test filter also selected an obsolete
MCP-unavailable assertion, which is now a positive scoped-permission contract.

`tests/worker/programmatic.test.mjs` is the new compiled-Worker journey: actual
SDK request → Rust Session → real Loader → two sequential pinned time calls,
plus two dependent program-only MCP calls through that same originating request.
It checks raw canonical SDK wire, strict nested argument denial, immutable
same-key replay and exactly one creation/request/CPU allocation per program.
A separate same-harness admission check removes Loader and pricing independently
and requires zero model calls while ordinary APIs remain available.
Those two admission cases passed against unchanged compiled artifact 12659282454
from GHA 40340822807 (source `cac7d54`): both returned 422, model calls and dynamic
charges remained zero, and ordinary agent-list requests returned 200. The source
run itself failed its positive journey; this selective negative evidence does
not promote the artifact or imply an overall-green run.

The first full positive journey with the native-JSON and terminal-only event
fixes is pending GHA; no deployment or final Cloudflare invoice verification is
claimed by the component or Rust results. A temporary local diagnostic shim
normalized only the old artifact's platform profile to locate the next boundary:
two time calls and two dependent MCP calls actually completed, and the remaining
strict SDK failure was the old terminal item's synthetic running phase. That
instrumented probe is mechanism evidence, not acceptance of the repaired binary.
