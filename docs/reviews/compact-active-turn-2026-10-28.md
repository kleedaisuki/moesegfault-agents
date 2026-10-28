# Compaction active-turn source review

Reviewed source: `19e9bb83c628db1e65a1cfe909e61993da3cac9b` and controlled fixture `21f5fb928de37e69a7164a88c342b0b43cb07961`, 2026-10-28.

Scope: exact current-user preservation across summary replacement, native item ownership, sealed continuation compatibility, and bounded progress. This was source-only review; no tests, build, provider request, or production operation was executed.

## P1: retained long execution can immediately trigger another paid compaction

At the reviewed source, `session_context_management.rs::ActiveTurn::capture` (lines 16–22) retains every projected/native item from the last real user onward. `session.rs` restores that entire suffix after the summary (line 1385). A single long-running tool task can therefore retain the same threshold-sized working set the summarizer was intended to reduce.

`session_context_management.rs::triggered` fences an unchanged journal source only when `policy.emits_item()` is true (lines 46–51); `compact_if_needed` similarly records the source boundary only for explicit compaction (session.rs lines 2362–2363). For the ordinary percentage-triggered implicit policy, the next generation loop observes the same over-threshold retained tail, issues another summary, restores it again, and never reaches a new model/tool step. Paid summaries can repeat until funding or another guard terminates the run.

Mandatory correction: maintain an all-mode, persisted source-progress fence, including Chat runs which have no ResponseState. The same journal boundary must not start another paid summary. Genuine model/tool/steering journal progress may reopen the trigger. This should be covered in the new narrow guard without replaying older successful journeys.

The source fence removes the immediate financial loop but does not make retaining an entire long execution an effective compression strategy. The exact current user request must survive; older tool history should normally be summarized, keeping only a bounded complete recent native call/result group when necessary. Never split a matched call/result or rewrite opaque current native items through a lossy Chat projection. This remaining design consequence concerns the ordinary long-task workload rather than a hypothetical oversized-media case.

## Positive boundaries

- Tail capture happens before summary I/O and is persisted on the run.
- Native source filters storage-owned context positions before interpreting user roles; transport-role catalog/runtime frames are not treated as user turns.
- Existing sealed compaction states without the optional native field retain their legacy expansion path.
- The changed controlled compact fixture requires the exact multipart current request outside the summary; mere summary mention cannot satisfy it.

Status: P1 sent to the implementation owner and root before promotion. Follow-up source review must record the actual correction, not treat the suggested remedy as executed evidence.

## Correction reviewed: 829f8a48ac635494827ffcb024afc61687d9608e

The mandatory issue and the compression consequence are resolved in the follow-up source:

- `ActiveTurn::capture` retains only the exact latest real user message/native item, not all settled execution siblings. All previous tool and opaque history remains input to the summarizer. The compaction branch is reached after `next_tool` work has drained and the `general_pending_calls` terminal guard, so removing settled call/output siblings together does not leave an outstanding call obligation.
- `RunState.compaction_source_end` is optional for old serialized state, set before summary I/O and when applying a recovered summary. `triggered` checks this source boundary in both implicit and explicit modes, including Chat runs without a `ResponseState`.
- Only genuine journal progress reopens the paid summarization trigger. The newly added pure guard now includes 512 settled call/output pairs, exact multipart/media current-request retention, durable fence roundtrip, both compaction modes, and trigger reopening after journal append.

No remaining substantive P1/P2 issue was found in this bounded follow-up source review. This is not executed CI or real-provider acceptance. The original exact native user item remains byte-structurally preserved; resolved opaque/tool history is summarized rather than claimed to survive as raw tail.
