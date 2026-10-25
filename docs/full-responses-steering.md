# Canonical Responses steering and warmup

## Authority

`session_response_steering.rs` owns accepted user intent. The Session actor assigns
steering IDs, event sequence numbers and successor IDs. The WebSocket gateway only
routes committed events; its control RPC does not independently emit an accepted
acknowledgement. A regular HTTP per-ID event observer ends at that response's
terminal, while the original trusted WebSocket observer follows automatic
successors until the chain actually stops.

Acceptance appends user messages to the immutable source journal immediately,
updates the last-user-input clock, and persists a queue entry containing the exact
source range and application fence. The current execution projection does not
receive those messages. A successor consumes the queued input once, adds it to
its execution projection and references the existing source messages instead of
appending duplicates. Automatic continuation is not new user activity and does
not extend the inactivity clock. Branch projections exclude unapplied ranges;
the original acceptance-ordered journal is never reordered to repair provider
adjacency.

The queue is bounded to sixteen pending submissions and 256 KiB aggregate input.
Only user messages containing text, image or file input parts are accepted.
Rejected roles, tool outputs and user-supplied item IDs cannot manufacture an
assistant/tool result or affect another response's execution.

## Safe boundaries and required input

An admitted provider request or server-owned tool is not restarted on steering.
After a complete server-tool group, before another model generation, the actor
can finish the original response as `incomplete` with reason `steered`. A naturally
completed response can also be followed by an automatic successor. The successor
inherits request settings, uses a deterministic ID derived from the parent and
first accepted submission, and reserves its routing identity before ordinary
admission. Model, compute and account limits remain those of the existing harness.

Client-owned function/custom-tool calls stop the chain instead. Each accepted
submission receives one `response.steer.pending` after the parent's terminal,
with identifying `required_input` stubs, never fabricated results. One explicit
continuation supplies saved results; queued user input is prepended in submission
order while the explicit request retains its settings. No tool is rerun merely to
recover observation. Cancellation, execution failure and definitive successor
rejection produce `response.steer.failed` containing the uncommitted input.
Uncertain storage/transport outcomes retain the deterministic application fence;
they are not reported as proven rejection and do not authorize a second run.

Accepted intent survives observer disconnect. Closing a socket releases only its
external transient parent/cache grants. It does not erase source messages, discard
queued user intent or cancel an admitted chain. A private queue-bound successor
handoff must remain possible without restoring a released external `store=false`
parent capability. New external commands still require authentication and ordinary
ownership checks.

## Warmup

A trusted `ExecutionContext.warmup` represents WebSocket `generate:false`; it is
not an accepted public HTTP request-body field. It follows normal request parsing,
attachment ownership and source acceptance, synchronizes rich native input through
the same private native history codec, and finishes a canonical empty-output
response without admitting model execution or starting a Container. Its response
ID is therefore a genuine branch/continuation boundary, not a gateway placeholder.

## Verification boundaries

Pure tests exercise role/media validation, submission order, source-range reuse,
last-user-input clocks, and an interspersed-tool-result property. The WebSocket
owner adds actual compiled Worker journeys for safe server-tool interruption,
client-tool pending input, warmup and disconnect continuation. Formatting or source
readiness is not a successful compiled/deployed acceptance receipt. Full compiled
and current-SDK gates remain required before release.

Primary contracts: [mid-turn steering](https://developers.openai.com/api/docs/guides/steering),
[WebSocket mode](https://developers.openai.com/api/docs/guides/websocket-mode),
and the current checked-in official Node SDK `ResponseSteer*` interfaces.

## Publication fence

A DO storage acknowledgement can yield while native provider callbacks are ready.
A Session-local, bounded publication barrier holds the accepted/rejected steering
event and later live events until the source/queue snapshot is durably committed.
This prevents a higher-sequence text delta from overtaking its steering event.
The barrier is limited to 16 MiB and 65,536 frames; overflow detaches observation
and marks an explicit event-limit error rather than buffering without a bound.
A failed acknowledgement leaves the outcome unknown. The next successful Session
publication flushes the barrier and rebinds any original chain observer from the
durably accepted successor, without submitting a second model request.

Conversation-bound requests and requests using public automatic compaction do not
support mid-turn steering in the current official contract. Their rejection is an
actor-sequenced `response.steer.failed`, not a silent ignore. An empty compaction
configuration does not enable compaction and does not disable steering.
