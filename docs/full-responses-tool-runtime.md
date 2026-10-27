# Hosted runtime integration

This note distinguishes the compiled foundation from the next execution slice.
The foundation has shared secret-free tool bindings, native-provider rendering,
read-only owner admission, persistent Jupyter supervision, actual Container memory
and network controls, Workspace transport, independent API-fee holds and immutable
image storage helpers. Those pieces alone do not make the four hosted families
available through the public Responses execution loop.

## One execution loop

Code Interpreter has an independently compilable vertical in
`session_hosted_publication.rs` and `session_hosted_code.rs`. The shared publisher
commits native items/events without importing image, search or MCP execution.
Other service dispatch remains staged in `session_hosted_dispatch.rs` and its
service modules; enabling Code does not imply those families are accepted.
`HOSTED_TOOL_RUNTIMES` selects each admitted runtime explicitly, including BYOK.
Actual compiled and Container journey receipts are still required before rollout.

`HostedDispatch::Ready(Value)` supplies the existing harness's model tool result.
`Pending` means the same operation has more recovery/publication work; it is not
an empty successful result and must not append a tool-result message. The single
Session driver remains responsible for deadlines, steering, cancellation,
response-wide tool allowance and subsequent model turns. It must not turn a
recoverable publication delay into another paid model or tool invocation.

The shared dispatch wrapper first enforces the existing response-wide allowance.
Declared hosted aliases resolve only through that Response's retained binding;
ordinary pinned tools continue through the existing time/Bash/persistence path.
Caller-local function/custom calls remain client handoffs. Direct and
programmatic invocation permissions are independent of name lookup.

## Durable operation boundaries

1. Persist the operation identity, exact binding/argument digest and original
   admission timestamp.
2. Resolve credentials in Worker memory and acquire independent funds/capacity.
3. Persist the dispatch fence before invoking the one external operation.
4. Persist its actual known result and receipt before any settlement or artifact
   publication. Never derive unknown token counts or provider charges from gaps.
5. Publish artifacts using their immutable identities; persist each public
   reference before acknowledging the supervisor spool or releasing capacity.
6. Commit canonical items/events before transport publication, then return the
   ordinary harness result. A retry of the same identity cannot append another
   item/event or repeat the external operation.

Search uses chunked `SearchOperation` state. A dispatched operation without a
retained result becomes a fixed unknown outcome, never another HTTP request.
Known settlement and canonical-publication failures retry the retained receipt.
Code stores its large known outcome once, separately from the small per-artifact
publication manifest. All artifact publications precede any spool acknowledgments;
the aggregate Workspace publication fence is removed only after every ACK is
retained, including the known empty-artifact case.

## Outstanding integration constraints

- The current fee helper validates an admission window before attempting hold
  reconstruction. A crash after D1 reservation but before DO persistence, followed
  by recovery after the window, needs a separate deterministic existing-hold
  restore path. Expiration must prohibit a new provider dispatch, not prohibit
  recovery and release of a proven never-dispatched hold. Do not change the old
  admission timestamp or manufacture a new reservation to bypass this boundary.
- Code's declared input files need Response retention holds before Container
  import. Read-only ownership validation alone does not protect against a
  concurrent user deletion while the accepted operation still needs bytes.
- Fresh requests referring to an existing logical code-container ID need an
  owner-bound context-to-Session/profile catalog. The opaque `cntr_` ID does not
  reveal its Session. Unknown or unowned IDs must fail before funds/compute;
  they must never silently create a new kernel.
- Network domain secrets remain in the private request/gateway boundary, not
  the secret-free binding. The initial binding rejects domain-secret execution
  until private-policy resolution is explicitly connected. The daemon already
  uses a four-field, secret-free Python IPC request; no policy values enter it.
- Image generation still needs operation-state integration, canonical BlobRef
  storage and asynchronous public base64 hydration, including replay after Files
  deletion. Do not raise the old inline-output limit or retain repeated base64
  journals as a shortcut.
- File search still needs its allocated production migration, token-aware ingest,
  HTTP management/search routes, configured real embeddings/index and background
  lifecycle. The lexical/semantic modules are not a public ingestion acceptance.

Known operation manifests and derived retrieval indexes are recoverable
intermediate state. Canonical response/context, owned artifacts and required
source files are user data. Financial receipts and bounded operational identifiers
belong to accounting/telemetry; queries, code, output images and logs do not.
After publication is committed and the model-result message is durable, obsolete
operation payloads need bounded cleanup without removing required unknown holds,
unacknowledged spool fences, retained canonical artifacts or user conversation.

See [tool contracts](full-responses-tools.md),
[API-fee accounting](full-responses-tool-billing.md),
[tool allowance](full-responses-tool-budget.md),
[interpreter transport](full-responses-interpreter.md), and
[network review](reviews/full-responses-interpreter-network-security.md).
