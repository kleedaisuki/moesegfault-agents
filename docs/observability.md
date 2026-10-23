# Observability and diagnostic data

## Two trace planes, one investigation

Cloudflare Workers tracing owns platform span IDs and instruments fetch, Durable Object,
D1 and R2 operations. Enable `observability.traces.enabled` and structured Workers Logs
in Wrangler. Rust calls the platform custom-span API through the small `wasm-bindgen`
bridge in `crates/agents-worker/src/telemetry.rs`; this is not a second Worker or harness.
The current worker-rs 0.8.7 `Context` does not expose `tracing`.

`NativeSpan` ends its platform span on drop, including error returns. It uses
`startSpan`, so it measures an operation under the current native invocation without
pretending that later fetch operations are its children. Native automatic fetch spans
retain platform-determined ancestry. Cloudflare currently exposes neither
`spanContext()` nor manual parent wiring: application-generated IDs must **not** be
written as native trace/span IDs.

`TraceContext` separately supplies W3C application correlation across request,
session and trusted workspace hops. Invalid contexts start a fresh application trace;
valid contexts preserve the trace ID and sampled bit, generating a new operation ID.
Each outgoing internal operation uses `child()` and `inject()`; never reuse one span ID
for independent operations. The parser checks lowercase hex, nonzero IDs, delimiters,
version-00 exact length, forbidden version ff, and a 512-byte upper bound. It accepts
forward-compatible versions using the version-00 fields, ignoring extension semantics.
Unknown sampling flag bits are not propagated. No `tracestate` or baggage is forwarded.
ProviderConfig explicitly opts into validated `traceparent` propagation to the configured
provider; the session harness supplies a fresh child for each call. This shares only the
opaque application correlation IDs, not owner/session IDs, credentials or baggage. Other
arbitrary user URLs must not receive tracing context. Provider redirects are forbidden.

Record `app.trace_id` on native custom spans and `trace_id` in sanitized events. These
are correlation keys, not a claim that platform and application trace graphs are the
same graph. A platform trace and its attributed console events provide the join.

## Content-free events and provenance

Use `telemetry::emit`, never raw user/provider errors or arbitrary JSON in console logs.
The emitter drops unknown keys, nested structures, negative counters and free-form
strings. String fields are bounded machine identifiers. Event names have a fixed
allowlist, with unknown names becoming `operation`.

Useful event sequence:

```text
request.completed -> run.started -> provider.completed -> tool.completed
                                  -> session.checkpoint -> run.completed
                                                        -> session.archived
```

Failure paths use stable `error_code` values, not exception messages. Instrument each
retry with `attempt`, each turn with `turn` and monotonic session events with `sequence`.
Include `duration_ms`, input/output/cache token counters and whether usage was reported
when known; unknown usage must not become a zero-cost observation.

Provenance is a chain of machine references: `request_id`, `agent_id`, `session_id`,
`run_id`, `tool_call_id`, `snapshot_id` / `workspace_revision`, `registry_version`,
`prefix_hash`, `config_hash`, and `build_id`. Configuration and fixed-prefix hashes
identify immutable pinned session inputs without logging their contents. IDs and
hashes are not authorization: retrieval always checks the authenticated owner.
Never emit prompts, instructions, skill contents, file paths, bash commands, tool
output, full provider responses, BYOK credentials, cookies or authorization headers.
Avoid identity subjects in operational logs; ownership belongs in authorized records.

## Storage and retention boundaries

| Class | Storage | Lifetime and access |
| --- | --- | --- |
| User conversations, pinned configuration, workspace snapshots | Session DO, D1 metadata, workspace R2 | User-data policy; owner-authorized export/delete; not telemetry cleanup |
| Recovery/checkpoint/intermediate state | Explicit recovery namespace | Remove superseded/unreferenced generations after successful commit; preserve latest durable recovery state |
| Operational logs and platform traces | Cloudflare Observability | Sampling and provider retention configured by deployment; no content payloads |
| Consented client diagnostics | Separate `DIAGNOSTICS` R2 binding, `diagnostics/` prefix | Seven-day lifecycle deletion; support/operator access only, separate from workspace credentials |
| Billing/audit ledger | D1 billing records | Financial retention policy; never delete as temporary telemetry |

Do not place the diagnostics bucket under a public R2 URL. Apply an R2 lifecycle rule
to delete the `diagnostics/` prefix after seven days; `expires_at_ms` is metadata, not
an automatic deletion mechanism. Deployment must enforce the rule. Keep observability
retention and sampling independent from user data retention; disabling telemetry must
not erase conversations, and cleaning old snapshots must not delete the current one.

## Client diagnostics contract

`POST /v1/diagnostics` is authenticated and opt-in **per submission**. The HTTP handler
must reject a body over 16 KiB before JSON decoding (check Content-Length and enforce
a stream byte cap; Content-Length alone is not sufficient). Rate-limit it using the
authenticated account, not a client-supplied owner or request ID. Missing diagnostics
storage is an explicit unavailable response, never a fallback into workspace storage.
`receive_diagnostic` enforces five validated submissions per rolling hour with an
atomic D1 insert-and-count predicate. `diagnostic_reports` contains only an empty JSON
placeholder plus owner/time/ID references, never the client payload. These references
expire after seven days and require scheduled D1 cleanup. Failed R2 uploads retain
their rate reservation, so repeated backend failures cannot bypass the limit.

```json
{
  "consent": true,
  "client": "magents",
  "version": "0.1.0",
  "platform": "linux-x86_64",
  "error_code": "connection_failed",
  "request_id": "req_example"
}
```

Only `magents` and `adash` are accepted clients. Version/platform/error-code fields
are bounded machine tokens. Unknown fields are rejected, including logs, stack traces,
environment variables and attachments. A successful submission receives a server-generated
diagnostic ID. The stored record includes authenticated ownership, receipt/expiry time,
schema version and the exact validated consent decision. This is deliberately a narrow
support signal, not a general remote log upload endpoint. The CLI must show the exact
payload and obtain affirmative consent before making the request.

## Verification and operational checks

Module tests cover invalid/non-ASCII trace headers, zero IDs, future versions, child
correlation, unknown-field redaction, negative counters and diagnostic consent. A real
deployment smoke test should start a session, execute a workspace command, checkpoint,
archive and restore. Use its application trace ID to find the native Worker/DO/storage
operations. Check that a recognizable secret in prompt/command/output does not appear
in console event payloads or diagnostics. Verify lifecycle deletion with a short-lived
test prefix before enabling production retention. Never test by uploading real keys.

## Sources and engineering rationale

- [Cloudflare custom spans](https://developers.cloudflare.com/workers/observability/traces/custom-spans/):
  native async ancestry and current ID/manual-parent limitations; checked 2026-10-23.
- [worker-rs Context source](https://docs.rs/worker/latest/src/worker/context.rs.html):
  SDK binding capabilities; local downloaded 0.8.7 source also inspected.
- [W3C Trace Context](https://www.w3.org/TR/trace-context/): validation and propagation.
- [Workers Logs](https://developers.cloudflare.com/workers/observability/logs/workers-logs/):
  platform log ingestion and operational retention.
- [Dapper](https://research.google/pubs/dapper-a-large-scale-distributed-systems-tracing-infrastructure/):
  production research supports low-overhead sampling and shared-boundary instrumentation.
- [Uncertainty in Aggregate Estimates from Sampled Distributed Traces](https://research.google/pubs/uncertainty-in-aggregate-estimates-from-sampled-distributed-traces/):
  sampled trace aggregates have uncertainty. Billing must use the durable usage ledger,
  never extrapolation from sampled logs or traces.

The practical design principle is explicit correlation, not heuristic reconstruction:
carry a bounded context on trusted hops, use native tracing for platform operations,
and persist content only in its authorized data plane. This keeps tracing overhead
small and prevents a convenience logging path from becoming a second user-data store.
