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

## Installed Worker build-tool cache

The normal Worker job caches `worker-build` 0.8.7 separately from compiled
application artifacts. In the PDF repair run `40446113827`, installing this same
locked tool cost several minutes before the release build; this was tool setup,
not a repeat of accepted user journeys.

`scripts/worker-build-cache.mjs` binds the cache to its schema/tool version,
runner OS, architecture, image family and complete actual `rustc -vV` output.
Application `Cargo.lock` changes do not invalidate this unrelated tool's own
`cargo install --locked` dependency graph. There are no prefix restore keys.
After restoration, the helper checks the saved identity, binary SHA-256 and
the executable's exact `--version`; a missing/corrupt/incompatible entry falls
back to the same locked installation with `--force`. The checksum establishes
cache consistency, not independent package authenticity. The pinned official
[actions/cache](https://github.com/actions/cache/tree/caa296126883cff596d87d8935842f9db880ef25)
provides repository/branch-scoped storage; normal dependency trust remains Cargo's.

Only `.cache/toolchain/worker-build` is cached, never a user environment or the
Worker output. Existing exact-source artifact and incremental-acceptance gates
are unchanged. Cache validation still runs on an exact cache hit. The new four
focused Node contracts cover identities, first installation, validated reuse,
repair and workflow wiring without installing/building Rust locally. Actual
hosted warm-cache installation-time savings require the next normal Worker run;
no before/after speedup is claimed from these contracts.

## Local checks

```sh
node --test scripts/incremental-checks.test.mjs
node scripts/incremental-checks.mjs plan
```

The second command only inspects Git/source; it does not run a model, deploy, or
execute the planned tests. Inspect `required_jobs`, `checks`, fingerprints and
`required_remediations` before dispatching the next increment.

## Bounded startup diagnostics

Manual dispatch may set `container_scope=startup-1g` to run exactly the known
failing 1g allocation/startup case while investigating the common interpreter
startup failure. This is not a capability acceptance run: the plan marks the
container `validation_scope=diagnostic_startup_only`, sets
`diagnostic_only_components=["container"]`, and records the deferred 4g,
unlimited and new network checks explicitly. Deployment must reject that
component until its remaining required checks have been executed or validly
sealed. Default dispatch remains `incremental`.

Independent selected Node/Python checks collect their own results even if an
unrelated selected check fails; the job fails after collection. A compiler
failure stops immediately rather than recompiling the same broken crate for
each Rust test filter. Current SDK checks remain independent of a failed legacy
offline check. Pytest temporary data is always beneath the project's `.temp`,
including operator-probe test receipts; its production safety guard is not
relaxed to accommodate system temporary directories.

## Check-level seals inside partial jobs

A reviewed catalog entry can carry `verified_checks`, each binding its ID,
`passed_current` result, positive executed count, historical receipt path and
`input_fingerprint`. The entry also pins source/run and the verification artifact
name/ID. `checkFingerprint(source, check)` hashes the exact argv/environment,
required leaf titles and a finite dependency group: its implementation, shared
fixtures, applicable package/toolchain locks and runtime contracts. Known offline
SDK/payer/probe/bridge/tooling checks have explicit groups. Rust groups bind their
source module and shared protocol/provider/state dependencies; unknown contracts
fall back to conservative component inputs, never the test file alone.

Plans move an unchanged, positively proven check to `reused_checks` with status
`reused_verified`, retaining the historical source/run/artifact/receipt and actual
count. That label is not `passed_current`. New receipts include the fingerprint;
legacy structured receipts can be recomputed against their original exact plan.
Deployment independently verifies the historical result and both old/current
input fingerprints, including recipe equality. An old failed or zero-test
receipt cannot close a remediation. This permits a passed check in an otherwise
failed job to remain sealed while only its failing or changed neighbors rerun.

Compilation-only Worker checks run with Actions' inherited stdio before filtered
unit execution, so a long warning stream cannot hide the compiler's actual error.
Failures also retain bounded output tails in structured receipts and print their
critical tails synchronously before termination.

## Native v0.3.0 release packaging

`release-magents.yml` uses `scripts/magents-release-checks.mjs` to select only
the four new native-context actual-binary journeys and affected progressive-help
case. It reuses the normal Rust argv builder and positive-execution receipt
runner, with `--release`, the matrix's explicit native `--target`, and `--exact`.
Both Linux and Windows must execute their own release binary; a Linux debug
receipt is not Windows or release-profile evidence. Missing cases, zero-test
success, a mismatched host platform, or an undefined package version fail closed.
The finite selection is for `0.3.0`; later releases must declare their increment
rather than silently replay every historical CLI test or silently select none.

Each target retains its source/version/target/host/profile plan and exact argv,
executed counts and results under `.temp/magents-release-checks/TARGET/` in a
separate `native-checks-VERSION-TARGET` Actions artifact. This is a narrow
platform-specific receipt, not a new global proof catalog. Its artifact name
cannot match the public `magents-*` download pattern. Existing tag/source checks,
actual distributable `--version`, archive metadata, byte-exact SHA-256 sidecars,
two-platform package dependency and no-clobber attachment remain unchanged.
