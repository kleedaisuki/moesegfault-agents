# Independent post-baseline v0.3 source review

Scope: release worktree `codex/responses-spaces-v0-3`, accepted baseline
`588baaf`, subsequent commits `9e683ae` / `6a6e886`, and the concurrent local
media/UI/actual-SDK fixtures inspected on 2026-10-25. This note supplements
`review-v0.3.md` and `review-v0.3-responses.md`; it does not replace their gates.

## Confirmed finding

### P1: generation-policy validator does not compile

At inspection, `session_responses.rs::validate_response_output` declared only
`completion` and `request`, although its body referenced `first_round` and both
production caller and tests supplied a third boolean argument. This is present
in `9e683ae`, not merely a partly written test. The Worker cannot build, so none
of the new runtime behavior can be accepted from the older green CI receipt.

Correction: declare the intended `first_round: bool` parameter. Reported directly
to the root and runtime owner before the new CI gate. Resolution must be checked
against the final source and its build receipt. Source correction independently
reinspected: the function now declares `first_round: bool`, matching production
and test call sites. The confirmed source blocker is resolved; new CI remains pending.

## Inspected behavior without an additional confirmed defect

- OpenAI Chat media projection operates on serialized copies, preserves the full
  contiguous correlated tool-result group, and appends image context only after
  that group. Text-only legacy projection remains unchanged. Anthropic maps
  typed content into the correlated native `tool_result` rather than separate
  unassociated user images.
- Stateless imported-call history and complete pending continuation-call sets
  are checked before attachment storage I/O. Owned file lookup precedes use;
  image/file media share bounded request admission and owner-scoped resolution.
- The transient Responses buffer emits first text immediately, retains a fixed
  deadline for quiet pending text, flushes at non-text/item/terminal boundaries,
  and yields after buffered emissions. Chat retains the immediate path. Sequence
  identity still belongs to the existing durable reservation mechanism, not the
  transient buffer.
- File validators/ranges are planned only after owner/lifecycle lookup. A 304 or
  416 avoids the R2 read; partial/full reads meter the dispatched operation and
  preserve whole-object SHA identity. Native R2 ranges avoid full-body slicing.
- Structured Responses suppress intermediate tool-round narration from public
  text, retain it in canonical source history, and validate final JSON before
  publishing it. Known provider usage is recorded before policy rejection.
- Dashboard message rendering uses escaped React text/JSON and raster-only data
  images or same-origin authenticated file references, not raw HTML or remote
  preview URLs. Declared MIME remains metadata, not a claim of decoder validity.
- CLI synchronous create now reports failed/incomplete/cancelled response state
  with nonzero exit while preserving JSON; background acceptance/local function
  handoff and read/cancel behavior are not reclassified as execution failure.

## Lightweight verification

`node --check` passed for `responses-coalescing-journey.mjs`,
`responses-policy-journey.mjs`, `responses-journey.mjs`, and `space-journey.mjs`.
`git diff --check` passed (only existing CRLF normalization warnings). No Cargo
build, Docker, live model call, cloud mutation, or production source modification
was performed by this reviewer.

## Remaining gate

Reinspect the correction and exact committed tree after source freeze. The new
CI must exercise actual Wasm timer/ready-stream behavior, actual SDK typed local
results and strict/structured failures, file ranges/accounting, and existing
recovery/legacy journeys. Staging evidence and promotion of those tested bytes
remain required; baseline CI is not evidence for the post-baseline change.
