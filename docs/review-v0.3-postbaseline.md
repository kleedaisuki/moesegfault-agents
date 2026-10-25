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

## Exact-candidate CI review receipt (2026-10-25)

Independently verified the clean worktree at
`944ac9bde7a9e59e97d15d2d79fdf899a63cd8ea` and queried GitHub run metadata/logs:

- [Full CI 40163182420](https://github.com/kleedaisuki/moesegfault-agents/actions/runs/40163182420)
  completed successfully at this exact SHA. All six required jobs (`changes`,
  `rust`, `worker`, `dashboard`, `container`, `tooling`) succeeded; only the
  intentionally separate diagnostic job was skipped.
- The Worker job compiled the Wasm target and release Worker, passed **239/239**
  library tests and **42/42** compiled-Worker tests. The logs explicitly include
  local-media SDK continuation, mixed server/local tools, exact repeated
  compaction instructions, linear concurrent children, no-store/failure/output
  limits, cancellation, idempotency, seeded attachment pins, hard-process-crash
  sequence recovery, strict function/structured output policy, conditional file
  reads, and quiet/burst coalescing without changing Chat.
- [Native candidate run 40163184570](https://github.com/kleedaisuki/moesegfault-agents/actions/runs/40163184570)
  succeeded at the same SHA on Linux and Windows, including real CLI journeys
  and packaged distributable metadata. Release attachment was correctly skipped
  for this candidate run.
- The tooling log reports **44 passing tests**, including **17** Responses SDK
  contract cases. Dashboard test/build/browser steps and container test/export
  steps succeeded. These are candidate CI checks, not a live-production claim.

The diff from `6f432484` to this candidate contains only verification documentation
and test/checker corrections, not production source changes. Independently
inspected corrections retain meaningful assertions: the image fixture now has
32,768 context tokens rather than accidentally demanding compaction; ledger
lookup uses exact prefix matching rather than a provider-limited long LIKE
pattern; the built-in clock is checked in the canonical journal and must not
leak as a caller-local function event; schema checker reserialization uses SDK
wire aliases and still rejects missing original-wire schemas. Terminal batching
latency now measures the text tail rather than unrelated durable terminal
publication. These changes do not bypass the behavior under test.

Inspected `performance-v3` receipts and corresponding Worker logs. In this
actual-Wasm synthetic-provider run, the first fragment added 1.90 ms, quiet
pending text added 52.11 ms and arrived 2,948.92 ms before the next upstream
fragment. The ready-stream burst preserved all 73,729 UTF-8 bytes in 73 delta
frames from 145 upstream fragments, with both padding policies and successful
terminal delivery. Terminal text-tail delay was 0.27 ms, distinct from 144.51 ms
terminal publication. Chat's first/second delays remained 0.92/0.50 ms. These are
fixture observations, not provider/network service-level guarantees or proof of
lower total memory/storage cost. File receipts show 327,697-byte full content
and a 1,008-byte selected range each requiring one R2 read; 304 and nine invalid
ranges required zero R2 reads. Source assertions additionally cover ownership,
whole-object SHA headers, range bounds, and accounting.

The original signature blocker is now resolved by both source inspection and
this exact-source build. No remaining substantive source/CI blocker was found
in this review. Staging deployment
[40163500012](https://github.com/kleedaisuki/moesegfault-agents/actions/runs/40163500012)
was still in progress at this receipt. The deployment workflow independently
checks the exact successful source run and required job set, downloads its
Worker/dashboard/container artifacts, and verifies/loads the tested container;
it does not justify claiming deployment success before that run completes.

**Decision: exact-candidate source and CI gate accepted; staging and final
promotion review remain pending.** No production code or infrastructure was
changed by this reviewer.
