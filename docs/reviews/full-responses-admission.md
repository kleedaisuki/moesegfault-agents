# Responses admission, context, and steering review

Independent source review of `b1edf97746d3902373dbb05e224799f03d9a3a10`
plus the actively integrated next-slice working tree on 2026-10-25. This is not
compiled or deployed acceptance. Production files were not modified by the
reviewer. Existing passing baseline suites were not rerun.

## Findings and closure

### P2: automatic successors re-expand the original prompt (source fix verified)

Locations: `session_response_steering.rs::advance_response_steering`,
`session_responses.rs::create_response`, `prompt_templates.rs::ResolvedPrompt::apply`.

The automatic successor clones its parent's raw request and clears only `input`.
If that request contains a prompt reference, ordinary admission resolves it again
and prepends the template's input messages. Those messages already belong to the
parent's accepted source/native history. Each steering application can therefore
duplicate template messages; a registry deployment can also change the inherited
version or make an already accepted chain fail because its template was removed.
The steering owner confirmed that no hidden reuse seam prevented this path.

Correction: transport the already accepted rendered prompt settings through a
trusted, queue-bound successor context. Preserve its resolved version/instructions
but do not reapply its previously accepted input. Ordinary external requests that
explicitly supply a prompt still resolve/apply it normally. Keep the raw public
request separate for idempotency and provenance.

The reviewer verified the complete source correction: `ExecutionContext.inherited_prompt` is accepted only after the deterministic child/parent queue proof, is cleared from the persisted transport context, and the actor supplies `ResolvedPrompt::for_successor`. That helper clears already-applied template input while retaining its original version/settings. The generated successor also inherits the actual accepted `effective_instructions`, covering mcmp-only guidance rather than only prompts. `ResolvedPrompt` Debug is redacted to identity metadata. Required
new journey: accept a templated parent and steering, change/remove the registry
version, then assert one copy of template input, the original inherited guidance,
one copy of queued input, and no additional execution on retry.

A related private-context omission was also found and source-fixed: the old private DTO selected only mcmp guidance or raw request instructions. A template's actual instructions were instead stored in
`resolved_prompt.instructions`. Compacting a templated response must preserve
that accepted guidance without making ordinary previous-response continuations
inherit a previous request override. Owners changed acceptance to persist the actual effective instructions from all sources; the reviewer verified that template and explicit request guidance now populate that field. The private DTO therefore preserves accepted template guidance. Compiled precedence/journey coverage remains pending.

### P2: source boundary coordinates straddle asynchronous I/O (source fix verified)

Location: `session_branch.rs::seal_branch_point`.

The original working tree captured `source_end`, awaited the workspace-token
read, and then captured unapplied steering ranges. Steering accepted during the
read appended a range whose end exceeded the earlier source end. Export truncated
source at that earlier end, so `project_selected_history` correctly rejected the
now-invalid exclusion range, breaking historical forks and count/compact views.

The state owner moved external I/O before one synchronous observation of source
end, inherited/unapplied ranges, pin and native reference. The reviewer verified
that no await now separates those source coordinates. Late steering naturally
lies outside the immutable prefix. This source-level correction is closed;
compiled targeted coverage remains required for the new boundary behavior.

### P2: compact item identity is shared by independent operations (source fix verified)

Location: `model_operation.rs::Inner::output`, the `cmpitem_` identity passed to
`CompactionState::seal`.

The item ID is derived from the public request hash. Two different idempotency
keys with an identical request are independent model operations and can produce
different summaries/ciphertexts, yet receive the same item ID. Conversation
append and item-provenance maps use one immutable body per ID; inserting both
results conflicts rather than representing the two actual artifacts.

The corrected item ID derives from the owner-bound durable operation ID: replay of
the same operation remains stable, while separate explicit operations cannot
alias. No cross-owner authentication bypass was found. The auxiliary owner raised the issue and, after captain approval, changed the ID to derive from the complete owner-bound operation identity. The reviewer verified the helper and output call site. Owner-added tests cover same-key replay, separate keys/owners, and appending two actual compact results to one Conversation; compiled execution remains pending.

## Invariants examined without an additional demonstrated defect

- Existing-response lookup precedes prompt resolution; raw request identity is
  retained separately from rendered prompt and authenticated mcmp expansion.
- Compaction envelopes bind owner/item through authenticated encryption and
  validate agent/pin context. Raw opaque hitchhikers are rejected before trusted
  expansion. Explicit current instructions override restored guidance.
- Conversation activation occurs before model admission. Its independent stable
  cleanup hold survives a later model-credit failure. Conversation append is
  prepared before the execution cutoff, and release has a durable outbox after
  canonical Session completion. Accounting acknowledgement failure is not a
  reason to regenerate an already committed model response.
- Accepted steering appends source immediately but does not enter the current
  execution projection. Branch points retain exclusion ranges. Successor
  acceptance reuses source positions and marks application once, rather than
  appending the same user message again. Original source chronology is retained.
- The volatile publication barrier holds synchronous native events while a
  steering source/queue commit is pending, with explicit bounded overflow.
  A later successful canonical persist flushes in sequence. Recovery now also
  rebinds an already accepted successor observer after a lost initial save ack.
- Disconnect removes ephemeral external parent grants, not accepted intent.

## Review disposition

All demonstrated findings above are source-fixed and independently rechecked.
No additional unresolved source-level blocker was found in this bounded review.
This does not replace the next targeted compiled acceptance.

## Acceptance still needed

The next compiled increment must exercise the owner-added prompt/steering,
source-boundary and compact-ID tests and the real Worker WebSocket/Conversation
journeys. Source review does not establish storage-failure injection, actual
SDK behavior or deployed financial recovery. Reuse the previously passing
baseline evidence explicitly; do not relabel skipped old tests as current passes.
