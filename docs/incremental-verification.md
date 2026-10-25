# Incremental verification and artifact provenance

Already accepted behavior is not a reason to rerun an unrelated suite. Normal CI
now computes an incremental plan from sealed component evidence, builds changed
artifacts, and runs new or modified checks plus explicitly recorded failure
repairs. It does not run the historical workspace/Worker/UI/paid suites by default.
This changes the acceptance policy, not the implementation scope.

## Plan and results are different objects

`node scripts/incremental-checks.mjs plan` writes
`.cache/verification/plan.json`, uploaded as `incremental-plan` by the `changes`
job. The plan contains:

- Exact candidate SHA and required current jobs.
- Runtime/dependency fingerprints for Worker, native clients, container,
  dashboard and tooling. A fingerprint hashes Git's sorted tracked tree entries
  (mode, object ID and path), including applicable dependency locks and vendored
  code. It does not infer equivalence from a matching version label.
- The selected sealed baseline SHA/run/job, explicit verified phases, and any
  unresolved checks. Reuse requires matching fingerprints.
- The immutable artifact's own source SHA/run/optional pinned artifact ID. A
  retained image is loaded with that original source identity, not relabeled as
  a newly built candidate image.
- Exact selected argv/environment/reasons. These are planned checks, not passes.

Execution writes separate result receipts uploaded as `verification-<job>`.
`passed_current` requires successful execution and a positive actual test count.
Rust, unittest, pytest, TAP and Vitest have explicit count readers. A missing
filter, zero-test run or absent required journey title fails. `no_selected_tests`
is a distinct status and never means that old tests passed on this SHA.

The runner invokes argv directly, without constructing shell commands from
workflow inputs. Manual dispatch uses the same source-derived plan; there is no
implicit `FORCE_ALL` mode. The existing retained-Worker diagnostic workflow
remains explicitly non-promoting and independent.

## Sealing and reusing evidence

`scripts/verification-baselines.json` is the reviewed catalog. After accepting
an increment, the integration captain records its immutable source/run and the
components actually accepted. A source with a failed aggregate job is not
silently called successful. The b1 Worker baseline records successful Wasm
build, bundling and 363 library tests, the exact retained artifact, and three
remaining observed leaf failures. Those remediation IDs are mandatory current
checks until a newer accepted baseline supersedes them. The previously passing
library/leaf cases are not rerun merely because their aggregate job failed.

Deployment independently verifies trusted same-repository run metadata,
source/job/step conclusions, artifact identity, availability and digest. For a
partial baseline it checks the pinned successful build phases and requires
current passed receipts for every unresolved check. A plan alone cannot
approve a deployment. `scripts/deploy-provenance.mjs` and the deploy workflow
are owned by release operations, not the CI selector.

A dependency or component fingerprint change requires a new build. Test-only
changes can run against an identical retained Worker/container artifact. An
expired or missing artifact fails closed; it is not replaced by an unrelated
run or a fresh untracked build.

## Selection granularity

- Changed Rust unit/property-test functions are selected by their source diff;
  new modules include all of their declared tests. Changed native packages still
  receive compilation/lint checks. Old workspace suites are not substituted.
- Changed SQL/offline test modules are selected individually, rather than
  discovering every historical SQL file.
- New/changed standalone Worker contracts run individually. The shared HTTP
  harness receives an exact `MAGENTS_WORKER_CASE` selector, whose existence is
  checked before execution. Auxiliary/count, tokenizer, forks and immutable blob
  tests have explicit behavior mappings.
- Known nested Conversation repair checks use an explicit skip list for sealed
  unrelated leaf cases. New/modified leaf cases join that check. Merely passing
  Node's `--test-name-pattern` for a child is unsafe: a filtered-out parent can
  yield a successful file-level TAP result without executing the child. Required
  leaf titles must be present in actual TAP output.
- Changed current SDK checks use a separate locked Python environment. Offline
  current-schema fixtures never stand in for real deployed API acceptance.
- Real-container and UI test files are selected only when changed. Changed
  artifact typechecking/building remains required; an unchanged UI does not
  rerun its historical rendering/browser suite.

A genuinely new behavior without a corresponding journey remains a delivery
coverage gap; a green build does not invent that coverage. The owning domain
adds its focused standalone contract or registers the exact shared-harness case.
Release operations run only the new deployed journeys and documented failure
repairs, with direct DeepSeek preflight for all paid language-model calls.

## Local checks

```sh
node --test scripts/incremental-checks.test.mjs
node scripts/incremental-checks.mjs plan
```

The second command only inspects Git/source; it does not run a model, deploy, or
execute the planned tests. Inspect `required_jobs`, `checks`, fingerprints and
`required_remediations` before dispatching the next increment.
