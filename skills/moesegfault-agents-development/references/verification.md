# Locate the change and verify its user journey

Start in the user's intended checkout. These paths are relative to that repository,
not to the installed skill folder.

| Boundary | Implementation | Focused evidence |
| --- | --- | --- |
| Domain/context contracts | `crates/agents-core/src/` | Native tests and property tests |
| Identity, BFF and credential envelopes | `crates/agents-auth/src/`, Worker `auth*.rs` / `provider_keys.rs` | Token/envelope tests; real registered login when affected |
| HTTP, DO harness, pricing, projections | `crates/agents-worker/src/` | Native logic plus compiled-Wasm `tests/worker/` journeys |
| Atomic admission and migrations | `migrations/`, production SQL in Worker modules | `tests/test_*sql.py` |
| Bash, checkpoint, restore | `crates/workspace-daemon/`, `container/` | Linux native and actual Docker journeys |
| CLI and portable configurations | `crates/magents/`, `examples/` | Actual-binary `crates/magents/tests/` and offline TOML roundtrip |
| Dashboard | `apps/dashboard/` | Interaction/build and real browser `e2e/` cases |
| Packaging and deployment | `scripts/`, `.github/workflows/`, `wrangler.jsonc` | Contract tests and immutable artifact gates |

Read the matching document opening and concrete code before proposing an edit.
Do not apply historical acceptance receipts as commands to redeploy an old SHA.
Full Responses decisions supersede the old linear-only boundary: branches,
Conversations, rich native items, lanes/steering, auxiliary routes and real hosted
execution are named capabilities, not parser-completeness claims.

## Select the smallest informative checks

Read `docs/incremental-acceptance.md` and the committed selector plan. Seal/reuse
passing baselines by original component/source/dependency fingerprint unless a
new failure justifies a regression. Execute new/changed behavior and actual failed
cases, not broad old suites or paid trajectories just to reconstruct a green
aggregate. Reused/skipped checks are not new passes at this SHA. A newly executed
selector needs a positive current result; per-check reuse retains the matching
old/current input fingerprints and actual historical result even when unrelated
component inputs changed. A failed historical job stays failed, and diagnostic-only
components cannot become promotion evidence.

Read `rust-toolchain.toml` and use the lockfile. Prefer Actions and its Cargo/npm/
BuildKit caches for changed Rust/Wasm, browser and container components. Inspect
available caches before local compilation; do not cold-build the workspace to
validate a Markdown or UI-text change. A targeted local check is useful when its
cost and effect fit the current machine.

Choose the specific new property/SQL/SDK/browser case from the increment. Compile
changed artifacts when necessary; do not rerun old full package/suite commands
merely because another capability was added.

Inspect the actual planned commands before dispatch: several journey labels can
still point at the same whole test file. Use the existing case selector rather
than execute that harness repeatedly. A fixture or result-counter failure is not
evidence that an already accepted model/tool effect must run again; retain the
original artifact/state and repair only the failed observation where possible.

`npm run test:worker` requires an already built Worker artifact. A native unit
test or wasm type check does not prove `worker-build` packaging or workerd HTTP
behavior. Do not run the root workspace daemon directly with elevated privileges
on a developer host: its UID-wide cleanup is designed for a dedicated container.

Use a property test where an invariant spans values or interleavings: Unicode and
item boundaries, pinned-prefix stability, exact credit conservation, immutable
branches, lane scheduling, opaque generations or archive roundtrips. Add a
regression that fails for the demonstrated defect,
not a wording check or a weakened assertion.

For an affected user flow, exercise the actual binary/SDK/browser and its real
protocol boundary. Mock transport first; use an authorized isolated account for
necessary hosted acceptance. Retain a legacy SDK gate and use a current SDK for
new typed fields, Conversations, lanes and auxiliary operations. Observe actual
headers: OpenAI Node 7.43.0's convenience `idempotencyKey` has been seen to
send none; use explicit `Idempotency-Key` plus saved intent when identity matters.
Paid LLM checks use direct DeepSeek, not OpenRouter; authorized image checks may
use the approved image route. Observe accepted work before retrying; retain an
unknown outcome rather than deleting state to make cleanup look successful.
Report implemented, registered, newly executed, reused and pending evidence
separately. Exact terminal/text/source-history/owner/ledger assertions must not be
weakened to make a new capability pass.

OpenAI Node adds `output_text` and sometimes `parsed` convenience properties to
parsed response objects. Compare raw SSE snapshots with
`await client.responses.retrieve(id).asResponse().then(response => response.json())`,
not the decorated SDK object. Never delete/filter payload fields to force equality.

Full map: [architecture](https://github.com/kleedaisuki/moesegfault-agents/blob/main/docs/architecture.md)
and [acceptance](https://github.com/kleedaisuki/moesegfault-agents/blob/main/docs/acceptance.md).
