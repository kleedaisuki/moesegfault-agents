# Full Responses delivery decisions

These user decisions supersede the earlier v0.3 linear-only scope. They are
requirements and implementation constraints, not an acceptance receipt.

## Product boundary

- Implement the complete Responses contract, retain Chat Completions, and include
  historical conversation branching and workspace branching. Do not retain the
  previous fork exclusions as an implicit acceptance boundary.
- Keep one canonical platform execution engine, owned conversation state,
  pinned server/agent instructions, explicit request instructions through every
  compaction, exact accounting, and owner-isolated Space data.
- Provide platform code interpretation and image generation, with BYOK support.
  Space file retrieval is a platform capability.
- Web search may use the platform DeepSeek API as well as BYOK. The earlier
  BYOK-only web-search decision was superseded. Function/tool discovery is not
  external web search and must not inherit that funding restriction.
- Platform language-model routes are OpenRouter and DeepSeek, offering the
  requested DeepSeek 4.1 Flash / DeepSeek Pro choices. Resolve and label actual
  vendor model IDs honestly; preserve existing saved routes and session pins.
- Platform image generation uses Seedream 5 through OpenRouter. Choose and expose
  the concrete supported variant rather than silently substituting another family.
- Paid **language-model acceptance calls must use DeepSeek directly**, never
  OpenRouter. OpenRouter image-generation acceptance is allowed. Synthetic
  transport fixtures and public model metadata reads are not paid LLM calls.

## Provider facts to verify in implementation

- [DeepSeek Responses](https://api-docs.deepseek.com/guides/responses_api/)
  and [Search/Fetch](https://api-docs.deepseek.com/guides/search/) are separate
  capabilities. Native Responses web search is model-dependent; the dedicated
  Search/Fetch API uses the same bearer authentication and can supply a separate
  platform search tool without pretending Flash natively supports every tool.
- Current DeepSeek documentation identifies `deepseek-flash` and
  `deepseek-v4-pro`; older retirement announcements conflict with later/current
  continued-Pro documentation. Use current catalog/pricing evidence, not a silent
  Pro-to-Flash substitution.
- OpenRouter lists `bytedance-seed/seedream-5-0-flash` as a Seedream 5 variant.
  Configuration, pricing, output formats and the eventual paid smoke test must
  agree with the actual selected model.

Provider limitations are adapter/capability facts, not permission to silently
ignore requested fields or call a narrow implementation complete. Preserve
credential authentication families independently from richer wire transports.

## Delegated delivery

Subsystem leads own implementation integration, independent review, validation
and failure resolution within their workstreams, including their child agents.
The Responses lead is the shared Git integration captain and owns coherent
commits, pushes and CI. Only one owner writes the shared Git index at a time;
finished work crosses teams as explicit interfaces and scoped patches, not a
pile of leaf changes for the root to assemble.

The root owns user decisions, global contract/dependency arbitration and the
final production/public-release decision. Existing staging evidence is a
baseline, not acceptance of the expanded full objective. Production remains on
its prior accepted release until the new objective's actual end-to-end gates pass.

## Incremental validation policy

The user explicitly requires that already tested, passing functionality is not
repeatedly regression-tested unless there is failure evidence. This supersedes
the earlier default requirement to rerun all normal suites or the complete paid
staging journey at every checkpoint.

- Seal and reuse passing baseline receipts. Record their source/component and
  dependency identity; do not claim old checks ran again on the current commit.
- Validate new capabilities and changed behavior with focused tests and real
  user journeys. Repeat a regression only for a documented failure and its fix.
- Build/type-check changed components as necessary, reuse valid unchanged
  artifacts and caches, and preserve exact deployment provenance. A skipped or
  reused test is not a new passing execution.
- Select CI and staging cases from the increment, rather than rerunning old
  Chat, file, UI, CLI or large-prefill trajectories by default. Deployment health
  and artifact activation checks verify the new deployment itself.
- Keep strict correctness, ownership, instruction, storage and accounting
  assertions in the selected tests. Narrowing redundant execution does not
  narrow the required final product or permit synthetic completion evidence.
- Paid calls remain purpose-bounded, preserve accepted IDs and use the user's
  prescribed providers. Observation recovery never repeats accepted execution.

The integration captain and release lead own the CI selectors, evidence ledger
and artifact/deployment gate changes needed to implement this policy.
