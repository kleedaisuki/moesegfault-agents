# Runtime ownership and compatibility

## Identify the authority before changing a transition

| Concern | Authority |
| --- | --- |
| Account, agent configuration, quotas, credit reservation and ledger | Owner-scoped D1 records and atomic constraints |
| Accepted conversation, pending input, run phase and pinned prefix | One Rust Session Durable Object per session |
| Released workspace restoration | Private R2 checkpoint manifest pointing to an immutable tar.zst object |
| Session list/status index | D1 projection; not the conversation owner |
| Logs, tracing and diagnostics | Separate operational data; never the financial ledger |

Platform implementation lives in `crates/agents-worker`; pure contracts are in
`crates/agents-core`. Resolve owner identity before routing to a DO or R2 object.
An opaque ID, URL, trace ID or object key is not an authorization grant.

## Execution and context

An alarm owns durable execution; an HTTP observer does not. A disconnect must not
cancel the task or trigger a second execution. Bind an observer to its run, not a
mutable session-global slot. A completed authoritative journal and settled ledger
must not lose their terminal response merely because a D1 status projection
failed. Preserve pending repair and bounded backoff; do not fabricate a successful
terminal for an incomplete run. Generation acknowledgements must remain safe when
an older writer drops unknown metadata during rollback.

Only accepted user input advances the ten-minute inactivity clock. Model output,
tools, polling and stream comments do not. A long task continues; it archives on
completion if the deadline has already passed. Late steering must remain durable
and enter the prompt at a safe boundary, with ordinary admission and accounting.

Tools and personalized instructions are server-resolved and pinned with the model,
instructions and registry versions. Compaction changes the derived prompt, not
the append-only source journal. Keep a stable fixed prefix and preserve provider
continuation fields, tool-call IDs and matching results.

## Providers, workspaces and money

Platform funding routes and BYOK protocols are different abstractions. Generic
credentials own an immutable protocol/base URL; key rotation must not silently
switch a pinned session to another route. The adapter normalizes wire protocols
for one harness rather than adding separate execution loops. Do not dispatch
incomplete streamed tool arguments or forward credentials through redirects.

The container gets workspace-scoped execution authority, not provider keys or R2
credentials. Planned release follows checkpoint publication, not just successful
compression. The R2 manifest is the restoration commit point. Reads and exports
must not start a container, update activity or silently checkpoint a live tree.

Use integer micro-USD and existing idempotent reservation/observation/settlement
mechanisms. BYOK model charge is zero, not zero resource use. An unknown outcome
is not free usage; sampled telemetry cannot justify a charge or refund.

Deeper source contracts: [runtime](https://github.com/kleedaisuki/moesegfault-agents/blob/main/docs/session-runtime.md),
[workspace](https://github.com/kleedaisuki/moesegfault-agents/blob/main/docs/workspace.md),
[billing](https://github.com/kleedaisuki/moesegfault-agents/blob/main/docs/billing.md),
[data lifecycle](https://github.com/kleedaisuki/moesegfault-agents/blob/main/docs/data-lifecycle.md).
