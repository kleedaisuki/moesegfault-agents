# Responses WebSocket transport

This design and implementation slice targets the current public OpenAI Responses
WebSocket contract. It is not a completed transport acceptance receipt. The
interoperable OpenResponses core currently documents sequential sockets; its
older limit must not replace the public OpenAI multiplexing contract.

Sources: [WebSocket mode](https://developers.openai.com/api/docs/guides/websocket-mode),
[connect](https://developers.openai.com/api/reference/resources/responses/methods/connect),
and [mid-turn steering](https://developers.openai.com/api/docs/guides/steering).

## Ownership and integration boundary

`response_websocket.rs` owns client-event framing, connection-local lane ordering,
cache rendezvous and limits. It does not generate a response ID, invoke a provider,
admit compute, mutate a workspace, assign response sequence numbers, or synthesize
a completed response. Those remain responsibilities of the existing Session DO
harness and its immutable response/branch machinery.

| State | Owner | Invariant |
| --- | --- | --- |
| Authenticated owner and initial trace | Existing HTTP authentication | Authenticate once before upgrade; retain no raw token in connection tasks |
| Lane queues and active dispatches | Socket coordinator | FIFO within one lane; at most sixteen active lanes; independent lanes may interleave |
| Response/source/workspace/usage | Existing Session DO | One mutable execution/workspace context per session; distinct branches get distinct sessions |
| Latest cached parent reference per lane | Socket coordinator | Small private, connection-leased reference; never a client-supplied authority or full journal clone |
| Canonical response and steering events | Existing harness | Original IDs, fields and sequence numbers; transport only supplies `stream_id` routing |

The connection supports 32 distinct named lane IDs plus the implicit default lane.
Named IDs remain counted after a lane becomes idle. Reusing a lane without a
`previous_response_id` starts a new independent response, not an implicit
continuation. A parent cache lookup happens when queued work actually starts:
advancing another lane can evict a `store=false` parent before its fork starts.

The current implementation slice bounds incoming messages to the existing 1 MiB
HTTP request envelope, queued requests to 64 and queued bytes to 4 MiB. The full
request-envelope owner must reconcile these operational bounds with any enlarged
inline-media limits; they are not an assertion that OpenAI itself has these exact
resource limits. Active requests and branch references are not charged against
the pending-byte counter. A cached reference is bounded to 4 KiB and must remain
a reference, not a serialized conversation.

## Execution interface required from the harness

The socket upgrade needs an owned `Env`, owner ID and `TraceContext`, rather than
a borrowed request/route context. A dispatcher consumes a queued `Create` and its
optional cached parent lease. It returns canonical events through a local future/
stream. A terminal publication can attach the newly committed immutable parent
lease for the lane cache. The generic connection coordinator must not interpret a
public response ID as sufficient access to a private `store=false` branch.

The gateway assigns an internal dispatch number only for scheduling. It is not a
financial run ID or a public response ID. A dispatch can emit an original response
and automatic steering successors; its lane is released only when that complete
execution stream finishes or reports required client input. The first
`response.incomplete(reason=steered)` must not prematurely release the lane while
the harness is producing its automatic successor.

The HTTP authentication owner must call the browser-upgrade origin guard. Cookie
authentication skips CSRF for ordinary GETs, but a WebSocket GET enables later
mutations. A cookie-authenticated upgrade therefore requires the exact configured
application origin. Native bearer/API-key SDK clients do not need a browser CSRF
token. Existing global origin policy remains in force.

Connection lifetime is sixty minutes. Register socket event listeners before
accepting it. Socket closure removes its observers and ephemeral cache leases;
it must not cancel or restart an already admitted durable run. Do not place
long-lived socket handling behind the HTTP post-response `waitUntil` allowance.
Cloudflare's native WebSocket lifetime and backpressure mechanisms own the
connection; queues still require explicit byte/count bounds.

## Client and error semantics

- `response.create` separates lane identity from response lineage. `generate:false`
  is a warm-up request and must be handled by the same harness without inference.
  `stream` and `background` are transport fields and must be omitted.
- `response.steer` accepts only `type`, `previous_response_id` and user `input`.
  It cannot choose a different `stream_id`; the target response owns that lane.
- The harness queues accepted steering, emits its authenticated acceptance and
  pending/failure events, and constructs automatic successors at safe tool/output
  boundaries. The gateway does not substitute the older plain session-steering
  endpoint for this richer contract.
- Errors associated with a valid named lane carry that lane's `stream_id`. An
  invalid lane or connection-wide error has no lane. The implicit default lane
  omits `stream_id`, not an empty string or a forged value from another event.
- A same-lane continuation receiving 4xx/5xx evicts its referenced lane-local
  parent. A failed cross-lane fork preserves the source lane's shared parent.
- Missing stored/cached previous state yields `previous_response_not_found`, not
  a fabricated new conversation or an automatic replay of the original input.

## Verification status

The source slice contains pure request parsing, lane scheduling, cache failure
rules, origin and deadline checks with ten native unit cases and a randomized
lane-operation property. It also provides
the Rust `WebSocketPair` gateway behind an `Executor` callback. Listener setup
completes before the HTTP upgrade returns. The socket loop fairly polls incoming
messages and independent execution futures; sixteen bounded pending control
RPCs cannot grow into an unbounded side queue. Slow-reader/native-frame bounds
close only observation, not admitted durable work. A prematurely ended executor
stream produces an explicit observation error, never a fabricated terminal.

Formatting and diff checks are local-only evidence. The gateway is not yet wired
to the authenticated HTTP route and canonical harness executor, so it does
**not** establish a working public WebSocket API. Remaining integration depends
on the shared branch lease, full request normalization, warm-up and steering
interfaces. The executor must emit each steering acceptance once: either through
its returned control events or its execution event stream, not both. Automatic
successors and branch leases belong to the execution stream, not control replies.

Final checks must use the current official SDK WebSocket clients, not only the
older Python 2.6 compatibility fixture. Cover concurrent lanes and same-lane FIFO,
the 16/32 limits, queued fork eviction, warm-up without model work, steering
successor/pending tool input, owner isolation, caller disconnect, bounded slow
readers and sixty-minute expiry. Model/tool calls and ledger counts must prove
that transport reconnects never replay execution.

## Public gateway integration checkpoint

`GET /v1/responses` now performs the authenticated Worker WebSocket upgrade.
The existing ingress and account gates still run before upgrade; each dispatched
create or steer additionally passes account/mutation rate gates and credential
validation. Browser-cookie upgrades require the exact application Origin. Native
API keys do not require a browser Origin. No raw credential survives upgrade:
API-key hashes are checked against non-revoked owner rows on every command,
browser-session hashes are revalidated through AuthSession (including token
refresh and local logout), and verified OIDC grants retain only their signed
expiry. Native access-token revocation follows the existing short-lived JWT
contract, not an invented introspection endpoint. Credential expiry does not
cancel an already admitted durable run.

The executor calls the shared owned Responses admission function directly. There
is no public HTTP loopback and no second model engine. Native Session event SSE
is decoded through the mature `eventsource-stream` parser; comments are ignored,
canonical event objects and sequence numbers are preserved, and each frame has a
32 MiB bound independent of task duration. Observation failures never retry
creation or synthesize terminal completion. Account-plan admission remains below
the protocol's sixteen concurrent execution lanes.

`tests/worker/responses-websocket.test.mjs` exercises actual compiled Worker
upgrade and socket frames in isolated Miniflare with deterministic providers,
including cookie Origin rejection, native API-key access, 32 named lanes,
post-upgrade revocation, warm-up/no-store continuation, Free-plan admission and
run survival after disconnect. These are integration acceptance tests, not an
offline fake WebSocket server. At this checkpoint only syntax/format/diff checks
have run locally; Rust compilation and these integration journeys await the
coordinated GitHub Actions build. Warm-up, canonical steering successors and
private connection-parent leases are being joined to the actor-owned execution
context; the public API must not be called complete before those journeys pass.

Private `ExecutionContext` now carries a server-generated connection lease and
warm-up flag separately from public request JSON. A cached parent becomes an
`OwnedParent` only through a token issued by the owning actor for the exact
connection, owner, response and expiry. Terminal observation captures this token
without exposing it on the wire. Failed lease capture does not rewrite the
original terminal result. Replaced cache parents are revoked only after no lane
cache and no in-flight fork still references them; a focused scheduler test
protects this ordering. Both the cleanup queue and cleanup concurrency are
bounded. Disconnect invalidates only connection grants in remembered actors;
actor alarms provide bounded eventual expiry if an isolate or close-time RPC is
lost. Admitted model/tool work is never cancelled by this cleanup.

The compiled journey suite also includes a held server `time` tool followed by
`response.incomplete(reason=steered)` and an automatic successor, plus a client
function that produces `response.steer.pending` and continues only after explicit
matching results. Assertions count real provider fixture calls and check accepted
input appears exactly once, rather than treating transport acknowledgements as
proof of model application.


The current official Node `ResponsesWS` client is also exercised against the
actual Miniflare HTTP listener (`mf.ready`), not a substituted socket factory.
Two same-lane requests use real network upgrades, canonical IDs and a parent
continuation. SDK reconnect/retry is disabled so observation failures cannot
silently create extra model requests. The accepted-steering disconnect test
asserts the project's explicit durable-intent policy: one automatic successor,
one source-journal occurrence and exactly two model-ledger settlements.
