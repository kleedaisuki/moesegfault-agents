# Runtime ownership and compatibility

## Identify the authority before changing a transition

| Concern | Authority |
| --- | --- |
| Account, agent configuration, quotas, credit reservation and ledger | Owner-scoped D1 records and atomic constraints |
| Execution, immutable Response boundaries, pending input and pin | Existing Session DO; one mutable execution/workspace context per session |
| Model-independent Conversation items/revision | Owned Conversation DO, not an upstream model store |
| Workspace/retained artifact bytes | Private R2 manifests/blobs with owner-scoped strong references |
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

## Full Responses state and transports

Historical conversation and workspace branches are required; the old linear-only
exclusion is superseded. Preserve fixed source boundaries, pending calls, rich
native items and sealed workspace anchors. Fork into an independent Session,
never rewind the source. Same-agent children retain the original pin; a different
owned agent gets its target pin. Conversation revision and genuinely new input
are separate: terminal append must not duplicate already-prepended history or
mutate an older Response.

WebSocket lanes schedule the same harness. Lane identity, parent lineage and
owner/private connection leases are distinct. Steering is durable user intent,
applied once at safe boundaries with canonical successors or pending tool input;
publish accepted events only after source/queue commit. Disconnect releases
observation/cache grants, not admitted work. GET recovery observes the original
run/response and never resubmits input, models or tools. A transport reconnect
budget must not impose an arbitrary lifetime on healthy long tasks.

Compact/count resolve the same owned context, tools and protected instructions as
generation. Counting must not create a fake Session/inference or start a workspace.
Compact returns an authenticated complete next-input window, not just a summary.
Missing required usage stays an explicit capability gap; opaque reasoning remains
bound to its owner/provider/model route rather than an incompatible JSON target.

## Providers, workspaces and money

Platform funding, credential authentication family and model wire transport are
separate. Native `responses` reuses OpenAI-family authentication, not a fabricated
credential family. Credentials own an immutable auth-family/base URL; key rotation
must not silently switch a pinned session to another route. The adapter normalizes wire protocols
for one harness rather than adding separate execution loops. Do not dispatch
incomplete streamed tool arguments or forward credentials through redirects.

Hosted service bindings are secret-free and independent of the outer model. A
tool declaration/alias needs real authorized dispatch, separate funds/capacity,
native public output and retained artifact references before it is delivered.
Interpreter memory/network policies must correspond to physical runtime controls,
not configuration labels. The container gets workspace-scoped execution authority,
not provider keys or R2 credentials. Planned release follows checkpoint publication, not just successful
compression. The R2 manifest is the restoration commit point. Reads and exports
must not start a container, update activity or silently checkpoint a live tree.

Use integer micro-USD and existing idempotent reservation/observation/settlement
mechanisms. BYOK model charge is zero, not zero resource use. An unknown outcome
is not free usage; sampled telemetry cannot justify a charge or refund.

Deeper source contracts: [runtime](https://github.com/kleedaisuki/moesegfault-agents/blob/main/docs/session-runtime.md),
[workspace](https://github.com/kleedaisuki/moesegfault-agents/blob/main/docs/workspace.md),
[billing](https://github.com/kleedaisuki/moesegfault-agents/blob/main/docs/billing.md),
[data lifecycle](https://github.com/kleedaisuki/moesegfault-agents/blob/main/docs/data-lifecycle.md).
