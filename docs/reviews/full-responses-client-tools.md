# Caller-executed Responses tools: independent review

Reviewed the working-tree client-tool increment during the separate
`40196587758` compiler-fix run. That run does not establish acceptance of this
uncommitted increment. Review writes are confined to this artifact; no production
changes, builds, deployments, paid calls, or old baseline test reruns were made.

## Contract and ownership examined

Primary local contract: installed official OpenAI Node SDK 7.43.0,
`node_modules/openai/src/resources/responses/responses.ts`, including
`ResponseInputItem.LocalShellCallOutput`, `ComputerCallOutput`,
`ShellCallOutput`, `ApplyPatchCallOutput`, `McpApprovalResponse`, and the
`ResponseSteerPendingEvent` required-input variants.

The four tools are caller-executed continuations, not claims that this platform
has executed a desktop, patched a file, or run a caller's local shell. Hosted
shell environments are distinct from `shell.environment.type=local`.

## Concrete P2 findings

### Nullable safety acknowledgements (source fix verified)

Location: `client_tool_items.rs::{validate_output, validate_result}`.

The official input accepts `acknowledged_safety_checks: null`. The first version
sent explicit null to the array validator, rejecting a valid empty-safety call
while omission worked. The owner now filters null as absent; pending checks still
require actual acknowledgements. The related nested optional-field case is also corrected: nullable `code` and `message` are filtered before comparison, so matching check identity with explicit null metadata is equivalent to omission. The reviewer verified this final source path; non-null incorrect values still fail.

Targeted new cases: no pending checks plus null array succeeds; pending check plus
null array fails; pending check plus matching ID and null optional fields succeeds;
non-null incorrect code/message fails.

### Nullable MCP response identity (source fix verified)

Location: `mcp_items.rs::validate_result`.

The SDK permits `McpApprovalResponse.id: null`. The helper previously rejected it
although the wire parser already treated null as omitted. The owner changed the
helper to validate only non-null IDs and added an approval test with both nullable
`id` and `reason`. Source correction independently verified. The approval request
ID and boolean decision remain mandatory; caller-controlled arguments cannot
replace the approved request's saved arguments.

### Invalid known provider fields reached canonical public objects (source fix verified)

Location: `client_tool_items.rs::public_item`.

The original action allowlist copied known optional fields such as shell
`timeout_ms` and `max_output_length` without checking their types. A malformed
provider could therefore publish a string/object where the official SDK requires
a number/null, even though unrelated vendor fields were removed. The owner added
finite nonnegative numeric validation, optional local-shell text validation,
status/caller validation, and the shell output's required nullable fields. Source
correction independently verified; malformed-provider focused tests were added.

## Security/provenance boundaries verified in source

- Local shell results pair using `id=call_id`, exactly as the SDK specifies;
  other families use `call_id`. This is not a generic output-item ID rewrite.
- `GeneralPendingCall` retains the tool family, original pairing identity and
  limited private safety/caller metadata. Its Debug does not print details.
  Same-family return validation precedes paid model admission, and the historical
  branch path reuses the same validator.
- Computer screenshot file references use the existing owner-scoped file lookup
  and bounded byte reader before generation. They are frozen into the effective
  execution input, while original user input remains available for provenance.
  The implemented resolver supports owned files/data URLs; it explicitly rejects
  remote URLs rather than silently granting a network-fetch capability.
- Computer actions, drag points, safety checks, patch operations and caller fields
  use nested public allowlists. Unknown provider metadata is not copied to
  canonical output/SSE. Arbitrary local-shell environment variable names/values
  are part of its actual documented action, not hidden provider metadata.
- A model-supplied program `caller_id` is not a causal execution capability.
  Until a real program registry exists, client programmatic declarations fail
  before provider reservation and program-origin calls fail declaration matching.
  This is a safe capability boundary, **not** completion of programmatic execution.
- Shared required-input stubs retain computer/shell/patch/MCP approval families;
  no client tool is converted into a fabricated function result. Local-shell
  steering is explicitly unavailable where the SDK supplies no required-input
  variant; ordinary local-shell continuation remains distinct.

## Steering retry follow-up

The actor owner requested a bounded check of terminal notification persistence.
Terminal steering event construction is now pure: queue flags/exclusions are
staged in the canonical completion snapshot and copied to live state only after
successful storage acknowledgement. Failed publication cannot consume a pending
notification. The same staging applies to definitive successor rejection.

The revised pre-acceptance error classification preserves unknown outcomes when
an accepted live/durable successor exists, or when its durable read fails. Only a
successful negative read with no matching live run returns accepted intent as a
canonical failure. No inference dispatch before run acceptance was found in the
reviewed path. These are source conclusions, not storage-fault execution receipts.

## Review disposition

All three demonstrated P2 findings are source-fixed and independently rechecked.
Programmatic execution remains an explicit capability gap, not a completed feature;
the pre-admission guard is verified at the native provider tool-validation call site.
No additional unresolved source blocker was found within this client-tool scope.
The owner also added an actual foreign-owned PNG screenshot rejection before model
work to the new computer journey. These source checks do not assert compiled success.

## Acceptance boundary

The new per-family compiled Worker journey must still execute: exact return,
wrong-family and safety rejection before model work, idempotent continuation,
historical fork without original-task replay, public nested-metadata redaction,
and exact known model ledger actions. Owner-added lightweight pure tests are not
a substitute for these current-source journeys. Reuse sealed passing baseline
behavior; do not report skipped old tests as passing at this source.
