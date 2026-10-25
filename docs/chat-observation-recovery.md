# Chat observation recovery without execution replay

Design decision for the `23eaed0` staging incident; not an implementation or
acceptance receipt. The original prefill completed once and settled, while its
HTTP stream ended without the terminal stop. A native inactive-DO-connection
signature was observed. Neither a deployment cause nor an executor bug is proven.

## Decision

Make the completed Chat run addressable inside its existing Session DO. On loss
of an internal live stream, the edge reads that **same run**, verifies the already
forwarded text prefix, and sends only the missing suffix and original terminal.
It never repeats `/run`, provider inference, tool dispatch, or financial mutation.

This is observation repair, not an exactly-once inference claim. Responses keeps
its existing per-ID receipts and event replay; do not build another harness or a
new global routing/index service for Chat.

## State and ownership

| State | Owner | Lifetime / invariant |
| --- | --- | --- |
| Active run and external-operation fences | Existing Session DO `RunState` | Unchanged execution authority |
| `source_begin: Option<usize>` | New defaulted RunState field | Captured from the fully hydrated live `journal.len()` when a new run starts; `None` for old persisted runs |
| Terminal receipt under `chat:terminal:<run_id>` | Same DO, chunk helpers | Immutable after canonical run completion; user-data lifetime with the session |
| Source span `[begin, end)` and final assistant index | Receipt references immutable journal | Fixed before admitting the next run; compaction cannot change these indices |
| Forwarded text byte count and SHA-256 state | Edge response stream only | O(1) state; discarded when caller disconnects |

The receipt is small metadata, not a second source journal. Store the exact
canonical completion object **with its single final `choices[0].message` factored
out into a journal index**, plus the fixed source span. This preserves original
ID, `created`, model, exact usage and optional billing fields without maintaining
a second hand-written completion schema or copying output text into every receipt.
Validate the final indexed Message equals the removed message when preparing the
receipt. A failure receipt stores the bounded original terminal error code instead.
Also retain the exact small terminal chunk produced by the existing
`session_observer::terminal_frame` builder. Its usage/created/error representation
must not be reconstructed differently by the relay; this adds fixed metadata,
not a token/frame log or a second text copy.

`load_state` currently hydrates every persisted source entry before exposing the
live Session. `append` extends that complete vector before persistence. Conversely,
`save_state` updates `journal_count` on its serialized clone, so the live count
field can lag. Capture the live vector length rather than that stale field; if
future lazy hydration changes this invariant, introduce an explicit logical source
offset before changing receipt creation. Old runs without an offset finish
normally but cannot offer guessed observation repair.

On a rare recovery read, reinsert the immutable indexed Message into the retained
completion metadata. Concatenate string content of assistant entries in the fixed
span to obtain the run's public text. Current Chat emits role/text deltas only;
server tool calls/results and compaction summaries are not public Chat deltas.
The prefix check below fails closed if a future provider violates that relation.

Do not put an unbounded receipt map into `PersistedSession`: its frequent clones
and rewrites would scale with historical runs. Use the existing `read_chunks` /
`write_chunks` primitives for one independently addressed receipt per Chat run.
Do not duplicate all stream frames or write once per token. No D1 migration or
public endpoint is needed. Receipt content is user data, not operational telemetry
or arbitrarily expiring diagnostics. Reuse the existing user/session retention
boundary; no new TTL/GC subsystem is required for this fix.

## Commit barrier

Under the existing persistence mutex:

1. Prepare/write the run's terminal metadata and journal references.
2. Commit the canonical completed Session state and journal as today.
3. Clear the live run in memory and publish its original observer outcome.

The private read endpoint must acquire the same mutex. A receipt cannot be
exposed while its matching live run remains active: return pending until the
canonical completion commit succeeds. Writing a prepared receipt first is safe
because the read barrier prevents premature success; recovery may rewrite an
unpublished prepared receipt, but never a committed one. A write/commit failure
leaves the original terminal run busy and recoverable rather than fabricating a
successful transport result.

The per-ID key and fixed journal span survive a later run, queued steering,
archive, isolate replacement and a stale D1 projection. Never substitute the
latest assistant or current run's usage for the requested run.

## Private read interface

Add `GET /chat-runs/<run_id>` to the Session's internal fetch surface only:

- **200**: committed success/error receipt and its reconstructed canonical data.
- **202**: that exact run is still active/finalizing; no re-admission or new work.
- **404**: no such retained receipt and no matching active run; stop recovery.
- **429 / overloaded**: stop rather than amplifying overload.

The edge has already authenticated and resolved the owned Session. Keep the
namespace identity `owner:session`, validate the run-ID shape, and never expose
this route through the public router. Reading does not append input, move the
input clock, modify the pin, resume a closed session, or acquire execution capacity.
Ordinary existing constructor recovery may finish the original fenced run; it
must retain the current unknown-provider-result semantics, not replay inference.

For old RunStates with `source_begin=None`, do not guess a span from timestamps
or inspect the newest assistant. Return explicit observation-unavailable when an
exact stream cannot be reconstructed. Existing data/download APIs stay unchanged.

## Edge stream relay

Install the relay only for a successful Chat SSE response carrying the original
`X-Agents-Run`. Capture owned Env/session/run/trace values, not a borrowed request
context. Keep the current response headers and immediate role/text behavior.

Use `eventsource-stream` to frame complete upstream SSE events. Forward each
original data string as `data: <original-data>\n\n`; inspect its JSON without
rewriting its payload. The current DO's data frames already have this form. Use
the existing 15-second comment keepalive around the relay so parser-discarded
internal comments do not create client read timeouts.

For every forwarded content delta, update only `sent_utf8_bytes` and an incremental
SHA-256 digest. Also track role, terminal/error and `[DONE]` state. Validate frame
identity against the known run before forwarding. A normal terminal plus DONE
finishes immediately and performs **zero recovery reads**. EOF after a terminal
needs only DONE, not execution or another result lookup. DONE without a terminal
is a truncated observation, not success.

On source error/EOF before a terminal:

1. Create a fresh stub and READ the exact run. Never resubmit the creation POST.
2. For pending, use bounded native-delay backoff: 250 ms, 500 ms, 1 s, 2 s, then
   4 s; stop after 32 reads or 120 seconds, whichever occurs first. No overload
   retry. Every new attempt gets a fresh stub and child trace context.
3. For a successful receipt, require the recovered text length to cover sent
   bytes, the prefix boundary to be valid UTF-8, and its prefix SHA-256 to equal
   the accumulated digest. Then emit only the missing text suffix, the original
   terminal stop/usage derived from the retained canonical result, and DONE.
4. For a failed/unknown-external-result receipt, send the existing explicit SSE
   error plus DONE, never a normal stop or invented usage.
5. For unavailable/mismatched/exhausted recovery, send a bounded transport error
   naming the existing session/run recovery handles, then DONE. Do not silently
   close or splice a different run into the stream.

Dropping the public response stream must drop the relay, pending read and timer.
Do not spawn `waitUntil` recovery or keep repairing an observer after the caller
has left. The alarm-owned execution continues independently, as before.

The incoming `Request.signal` is explicitly enabled with `enable_request_signal`
and is bound only to the observer's scoped `AbortHandle`, never to execution or
its provider/tool requests. A live pending stream pull is not reliably disposed
just because its outer body was cancelled. Native quiet-stream probes also show
that transport cancellation may not be reported until another write. Therefore
only a pending recovery emits a compact `:\n\n` comment every 100 ms, retaining
the same recovery future/read budget across ticks. This bounds its additional
payload to 3,600 bytes over the 120-second recovery window (excluding HTTP/TLS
overhead); ordinary streams retain their existing 15-second heartbeat. The abort
listener is unregistered before its Rust Closure is released, and abort quietly
drops the in-flight observation rather than sending an error after the client left.

The same immutable receipt can later repair a detached private JSON observation,
but that must reuse the read primitive, not add another admission/retry mechanism.
Do not register a replacement in-memory Observer as the recovery primitive:
finalization captures its observer before yielding, which creates a late-attach
race. Pure pending/result reads avoid that race and new observer-group machinery.

## Resource and compatibility constraints

- Commit one small metadata record per Chat run; reconstruct text only on the
  uncommon recovery path. Normal streaming adds parsing/hash work, not D1 reads
  or token-level durable writes. Measure first-fragment latency against the
  existing immediate-Chat fixture.
- The current published DO tariff is wall-time allocation, not a separate KV
  byte-meter. Receipt preparation/read work must use the existing resource
  accounting envelope. Do not append observations to a settled model reservation;
  resource-only recovery operations must use the established account operation
  meter if separately charged. Bound their count and retain uncertain resource
  measurements normally. No model charge is created by observing a receipt.
- Default new serialized fields so old sessions load; no public JSON shape,
  full-history endpoint, source append clock, OpenAI IDs or binary contract changes.
- Recovery provides one original result, not a historical/workspace fork and
  not proof that arbitrary external side effects executed exactly once.

## Verification and implementation slices

1. Receipt metadata/span and commit/read guards; pure tests plus actual DO
   retrieval after canonical completion, archive and a later run.
2. Relay codec/digest state machine; controlled source EOF and native-style error
   before output, after a Unicode-split prefix, and after terminal-before-DONE.
   Use the already locked property-test library for arbitrary Unicode delta
   segmentation, prefix mismatch and invalid journal ranges where available.
3. Actual Worker integration: sever the **internal observation**, not the public
   client, while the one real harness run continues. Use a test-only binding/proxy
   fault or the relay's injectable reader seam; no production fault flag/backdoor.
   Assert exact public text, original terminal/usage, one provider/tool execution,
   one settlement and unchanged user clock. Include pending-then-complete recovery,
   prefix mismatch, error/unknown result, overload and caller-abort cancellation.
4. Full relevant CI and deployed SDK journey. Keep the original stream oracle;
   do not replace terminal stop/usage assertions with a loose source-history check.

Existing projection-fault and Responses process-crash tests remain required. A
receipt-read fallback must not turn an uncertain provider operation into a known
success, erase a reservation, or hide a second model call behind an idempotent bill.

## Implementation handoff

The implementation uses `session_chat_receipts.rs` for the independently chunked
metadata record/private read barrier, and `chat_relay.rs` for framing, prefix
verification and bounded observation-only reads. The ordinary Chat path performs
no recovery reads. A rare `observer.recovery` event contains only run/session/trace
IDs, attempt count, elapsed time and a fixed reconstruction outcome; it reports
canonical-tail reconstruction, not network delivery. Text and prefix digests never
enter telemetry. Each private read uses a separate session-bound maintenance
reservation and observed DO wall time, never an already-settled model reservation.

The test-only `chat-recovery-shim.mjs` wraps the real namespace/stub in Miniflare;
it never becomes part of the production bundle. Its old-artifact negative control
cut one internal stream and failed the actual SDK terminal oracle after exactly
one execution POST/provider call. The unmarked existing JSON journey still passed
through that shim. New-source positive checks include pending completion, Unicode
and tool-round prefixes, immutable archived/later-run identity, native overload,
provider uncertainty and caller-abort cancellation. Formatting, JavaScript syntax
and test sensitivity are established locally; compiled new-source and deployed
acceptance remain required.

## Grounding

Cloudflare's current [error handling](https://developers.cloudflare.com/durable-objects/best-practices/error-handling/)
and [stub](https://developers.cloudflare.com/durable-objects/api/stub/) documentation
requires a fresh stub after infrastructure exceptions and limits retries to
idempotent operations. This design retries observation only. The completion-record
and request-identity separation is also the relevant lesson from
[RIFL, SOSP 2015](https://sigops.org/s/conferences/sosp/2015/archive/abstracts.html):
result durability and retry rendezvous are separate from an RPC's connection.
We reuse that principle, not RIFL's broader exactly-once machinery or claims for
external model APIs.
