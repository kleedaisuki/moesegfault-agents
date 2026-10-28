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
- A Session keeps its original `pinned` creation snapshot for historical
  provenance and existing reference compatibility, not as a live prompt invariant.
  Its separate `configuration`
  overlay records the latest applied `agent_revision`, full Agent instructions,
  resolved available registered tools and any `unavailable_tools`.
- Existing idle Sessions apply accepted edits on their next use. A busy Session
  pulls changes at safe model/tool boundaries, not by rewriting an in-flight
  provider request. There is no global synchronous fan-out or unsolicited model
  run. Configuration edits do not reset the user-input inactivity clock or reopen
  closed Sessions; explicit resume restores history/workspace and then catches up.
- Dynamic native context separates necessary Agent instruction guidance from the
  ordinary, versioned `agents_tool_catalog` reference. Tool descriptions, schemas
  and availability are not system instructions. A fixed real dispatcher carries
  logical name/arguments; its provider declaration does not contain the changing
  inventory and remains present for an empty catalog. Server-owned positions and
  decision provenance, not a textual marker or model-written version, identify
  the authoritative configuration.

The active `context_layout` is independent of the historical pin. Current native
Sessions adopt the dynamic layout at safe boundaries; older sealed Responses
retain their original rendering identity. Adopting a new layout can legitimately
rebuild a provider prefix once. The previous requirement that the live Session
must always render its original pin bytes is intentionally removed. See
[dynamic context and dispatch](dynamic-tool-context.md) for wire capabilities and
trade-offs; ordinary context never grants execution authority.

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

Dynamic registered-tool catalogs no longer repeat each changing schema in the
provider's top-level declarations. The fixed dispatcher remains a real function
tool; backend validation enforces the current inner schema and permission. A
stale or revoked selection receives a structured catalog-change/revocation result
for continuation, rather than an unexplained missing-tool failure. Caller-local
and native hosted tool declarations retain their existing public API ownership;
their upstream cache behavior remains provider-dependent. No unsupported inline
tool-loading API is invented and no revoked grant survives merely for caching.

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

### Accepted increment — 2026-10-28

Normal CI [40617982649](https://github.com/kleedaisuki/moesegfault-agents/actions/runs/40617982649)
completed successfully at `4b29c77ca782e6b27eb18f96ef10fcb0439e3379`.
It ran only the remaining lifecycle journey (4.98 seconds) and reused the exact
Worker artifact `12756000291`, produced by `bf0fd027ff685ae501593295a8a77304c14d8455`
in run `40550908238`. The successful lifecycle receipt is artifact `12785862611`.
Previously passing checks were retained, not replayed:

| Scope | Actual positive receipt |
| --- | --- |
| Six new Core/Worker state guards, historical fork | `40546096194` |
| Busy revocation, issued-effect unknown recovery, new error-identity guard, native-output decoder | `40550908238` |
| Lifecycle, compact and cold recovery | `40617982649` |

The first two aggregate runs failed and remain recorded as failures. Repairs
addressed native text-part decoding, issued-recovery error classification, and
a fixture that incorrectly expected executable configuration events in the
summary request. Summary input deliberately excludes those events; the strict
continuation/cold checks prove restoration from authoritative configuration.
No accepted cache or old product journey was rerun.

Staging deployment [40618287856](https://github.com/kleedaisuki/moesegfault-agents/actions/runs/40618287856)
installed migration `0031` and the verified Worker with all six existing runtime
features retained. Its deployment evidence is artifact `12786401193`.

Production deployment [40619870820](https://github.com/kleedaisuki/moesegfault-agents/actions/runs/40619870820)
then published the **same** tested Worker and migration, retaining the configured
six-feature allowlist and reusing unchanged daemon/dashboard artifacts. Evidence
artifact `12786532941` records the assembly and component source identities.
Both public health endpoints reported Worker `bf0fd027ff685ae501593295a8a77304c14d8455`.
Production deployment did not replay paid acceptance or old regression journeys.

The real-provider journey used the direct platform DeepSeek Flash route and the
OpenAI Node SDK. Session `d5039ba9-52f4-4bc1-9425-eb2036d7e6e8` was created on
the **previous** Worker `3799809521a64a287aa10a037e017760228e9422`, then explicitly
resumed after inactivity on the new deployment. No replacement Session or
response-scoped instruction workaround was used.

| Same-Session step | Response | Observed outcome |
| --- | --- | --- |
| Original Agent policy | `resp_211564a0339c4ba0ad8607a2bad73992` | Original policy label |
| PATCH instructions/tools, compact | `resp_9b1c3e60b9b442e2a5c5c03409011d9d` | One actual compaction item, new label, effective revision 2 |
| Use newly enabled Bash after compact | `resp_6db2596f134242d2a7c1b590e38e337d` | One actual command wrote/read the unique file marker; exit 0; new label retained |
| PATCH final instructions and remove tools | `resp_271da02bd5e54366a41b4f1721ca730b` | Final label, effective revision 3, empty tool set |

The actual Bash execution ID was
`fdb3db9d5161282646288495bcc0158083be5c1731c8a5042c6f0b9e24d2776b`.
The Session journal contained the matching call/output, not merely model prose.
The original pin and original SDK-retrieved Response were unchanged.

A final invalid Responses execution request forcing the removed Bash returned
`400 tool_not_enabled`, without admitting a Response, appending input, launching
compute or invoking a model. The first live fixture incorrectly used the token
count endpoint as an authorization probe; its `200` was an estimate, not an
execution grant. Only that final probe was corrected and executed. The four
successful Responses submissions and their side effects were **not replayed**.

The owner-scoped ledger recorded exactly six model operations (four final
answers, one summary and one Bash decision), all with DeepSeek provider request
IDs and settled reservations: 1,439 micro-USD in model charges. Resource charges
at the closure observation were 6,323 micro-USD; total 7,762 micro-USD. No open
reservations or pending resource observations remained. The Session was closed,
conversation and workspace saved to R2, and the test Agent deleted. Retained
storage can accrue subsequent storage charges normally. Private receipts remain
under the project's protected `.temp/session-config-live/`; no credentials or
real user conversation content are committed.

## Controlled incremental checkpoints

Normal CI `40546096194`, source `3707c88f776ef2947919b5ecaef1c1b1b41fd560`,
retains its failed aggregate. Its Worker build (`12753008991`), six new Rust
guards and historical configuration-fork journey passed; native and tooling
jobs also passed. The lifecycle and busy-revocation fixtures incorrectly parsed
native tool-result content parts as a JSON string. The issued-operation fixture
correctly preserved uncertainty and the original authority, but revealed that
its precise unknown-outcome code was collapsed to `run_failed`.

Normal CI `40550908238`, source `bf0fd027ff685ae501593295a8a77304c14d8455`,
also remains failed overall. Actual downloaded receipts confirm the new pure
unknown-error guard, busy revocation and issued-operation recovery all passed;
the isolated tool-result decoder checks passed in its tooling job. Its successful
build produced Worker artifact `12756000291`; `verification-worker` is
`12755671681`. Neither the accepted historical fork nor the earlier six Rust
guards were replayed.

The remaining lifecycle failure was another fixture expectation: native
compaction deliberately strips trusted runtime/configuration updates from the
summarizer input and seals them independently. Its summary request therefore
contains the fixed summary guide and real conversation source, not an executable
Agent-configuration update. The repaired assertion checks that boundary while
keeping the subsequent latest-configuration, same-Session, exact-count and cold
continuation assertions intact. At that checkpoint it was the sole unresolved
controlled journey. It subsequently passed in `40617982649`; the separate real
provider and deployment outcomes are recorded in the accepted increment above.
