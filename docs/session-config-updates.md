# Live Agent instruction and tool revisions

Agent configuration edits and Responses request customization have different
lifetimes. Updating an Agent's `instructions` or registered `tools` through its
normal PUT/PATCH endpoint publishes a durable configuration revision for existing
Sessions. It does not require copying the instructions into each Responses call
or creating a replacement Session.

## Ownership and application

- D1 owns the Agent row and its append-only, owner-scoped configuration revision
  log. Exact revision comparison and log publication are atomic; concurrent
  edits cannot silently overwrite another edit from a stale merged payload.
- A Session keeps its original `pinned` prefix. Its separate `configuration`
  overlay records the latest applied `agent_revision`, full Agent instructions,
  resolved available registered tools and any `unavailable_tools`.
- Existing idle Sessions apply accepted edits on their next use. A busy Session
  pulls changes at safe model/tool boundaries, not by rewriting an in-flight
  provider request. There is no global synchronous fan-out or unsolicited model
  run. Configuration edits do not reset the user-input inactivity clock or reopen
  closed Sessions; explicit resume retains the original pin and then catches up.
- Native Responses appends a trusted `<agents_configuration_update>` system item
  to the existing private journal. Its complete replacement scope is **Agent
  instructions and registered tool availability**, not platform policy or caller
  role history. Storage-owned positions, not a matching string in user input,
  identify an authoritative internal revision.

The original cache-layout policy is not edited in place. The new configuration
event explains its own scope, so an already-existing legacy or append-only-v1
Session can receive the update without replacing its initial prefix. Providers
that cannot preserve inline system-message caching use compatible rendering;
correct configuration semantics take priority over a cache-hit claim.

## Scope and tool safety

`Responses.instructions` remains response-scoped: omission/null on a later
Response ends that override. It does **not** revert the durable Agent revision.
Agent instructions remain active until another Agent configuration edit.

Model/provider, physical runtime profile, workspace authority and pinned skills
are not silently hot-swapped. A tool that needs a workspace cannot acquire that
authority because an Agent edit also changed `workspace` for future Sessions.
Such tools are reported as unavailable for the old Session, while compatible
instruction/tool changes still apply. Start a new Session for a changed model,
workspace authority or pinned skill selection.

Actual execution uses the effective tool bindings, not merely the model's view
or old declarations left in history. A queued call revoked before execution
receives a `tool_revoked` result and does not start that effect. Already-issued
commands are not undone. Call/output relationships and known model usage remain
journaled. A caller-local tool is not reinterpreted as a harness tool when names
collide; existing binding identity and collision validation still apply.

Changing tool schemas can invalidate a provider's rendered cache prefix. The
configuration notification does not invent unsupported inline-tool APIs or keep
removed tools callable to preserve cache hits.

## Compaction, cold recovery and historical context

Compaction retains the latest Agent configuration independently of the current
response-only instruction snapshot. Old configurations are historical, not active
instructions to be revived by a summary. Internal updates are not executable
summary guidance. Cold recovery resumes the persisted revision cursor rather
than appending or executing the same transition twice.

Historical Responses retain the configuration at their sealed boundary. A new
execution fork may intentionally catch up with later Agent revisions **after**
importing that boundary; it must not silently import later user messages or
rewrite its source. Cross-Agent transfers must not promote the source Agent's
private configuration into the target Agent's active policy.

Read-only input counting previews pending configuration updates through the same
renderer as generation, without appending them to a live Session or admitting a
run. Current status exposes applied configuration separately from the base pin.
Operational logs carry revision IDs and tool/transition counts, not instruction
text. Configuration history is user data, not telemetry or disposable execution
state.

## Targeted acceptance

The selected new controlled Worker journeys cover normal management PATCH on an
existing Session, effective tool execution, compaction/cold recovery, queued-tool
revocation, workspace-authority boundaries and historical forks. This is a new
increment, not a rerun of the previously accepted prompt-cache suite.

`tests/e2e/session-config-live.mjs` provides a separate two-phase real DeepSeek
journey. `prepare` seeds an actual Session on the previous deployment; `complete`
continues that same Session after deployment, changes only Agent configuration,
compacts, executes newly enabled Bash against a unique file marker, removes the
tool and checks durable final policy. It never supplies the new policy through
response-scoped instructions, and never automatically retries paid POSTs.

Compilation, controlled journeys and real-provider acceptance are pending until
their actual results are recorded here. Prepared code is not a passing result.
