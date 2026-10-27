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
an increment, its component owner records the immutable source/run and the
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

The Worker job first compiles `cfg(test)` when its plan selects Rust contracts,
then runs only those selected contracts, before installing the build tool or
producing release Wasm. This catches test-only namespace/type errors before the
expensive release build. No library tests are executed by `--no-run`, and an
empty Rust selection does not trigger test compilation. Default step success
gates stop tool setup/release work after either early failure. A valid reused
artifact can still run its selected actual journeys independently of a unit
assertion failure; existing artifact receipts and failure conclusions remain
separate and are never rewritten as an aggregate pass.

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


## Cheap checks before a compiled-Worker journey

Use the actual native target: Worker library tests use `--lib`, not an implicit
all-target command that also compiles unrelated benchmark harnesses. Selected
`cfg(test)` compilation and execution precede build-tool installation and release
Wasm compilation in normal CI; they do not execute historical tests.

When interposing Miniflare bindings for latency or crash injection, preserve
`constructor` unchanged and bind native methods to their original target.
worker-rs checks native binding constructor identities; wrapping the constructor
as an ordinary function changes its name and can reject a valid D1/AI binding
before the intended fault. Reuse the established Conversation/native-binding
wrappers rather than synthesizing plain objects. Verify that property cheaply
before waiting for an application build. Active actors cannot always be evicted
with `unsafeEvict`; a supported runtime configuration restart with persistent
D1/R2/DO storage can test the cold boundary without editing production state.
A corrected fixture must reuse the retained exact Wasm and select only failed or
unexecuted cases, not replay already successful effect journeys.

### Promote a prebuilt native release without replaying checks

The artifact-only package run executes the five v0.3 cases on both actual native
platforms once. After the root release gate approves an immutable tag and creates
its draft release, dispatch `Package magents` with that `release_tag` and the
successful artifact-only `reuse_run`. This is promotion, not another build.

The protected upload path reads both successful platform jobs, both original
native plans/results, package metadata and LF checksum sidecars. All five exact
release recipes must have positive counts. The archived compiler must match the
pinned toolchain, and the complete native dependency fingerprint from the
existing incremental verifier must match the tag. Documentation or operator-only
commits may therefore follow the binary build; native/dependency changes cannot.
The archives and `BUILD-INFO.json` remain byte-for-byte original, including their
**actual** binary source commit, source ref and producer run. They are never
relabeled as compiled from the later tag.

Promotion uploads only absent assets and never uses `--clobber`. A partial upload
can resume only with the explicit original run and byte-equal existing assets.
After publication, the ordinary release event sees the complete assets, validates
them against their same successful producer and skips both the native build matrix
and uploads. If no assets or reuse run exist, the original build-and-attach flow
remains. No helper creates, publishes, edits or moves a release/tag; those actions
remain the root gate.

This uses GitHub's [cross-run artifact downloads](https://docs.github.com/en/actions/how-tos/manage-workflow-runs/download-workflow-artifacts)
and [existing-release uploads](https://cli.github.com/manual/gh_release_upload),
not another proof catalog or a custom signing format. Six new offline promotion
contracts exercise archive/checksum safety, exact native receipts, source identity,
failed producer rejection, partial uploads and publication idempotence. The real
artifact-only run `40457558376` (source `bb18283f80cdc1a9cf87a020de174eb828fc0a8c`)
was also downloaded and validated read-only: both archives, sidecars and ten
already executed native-case receipts passed. No native executable or old test
was rerun, and no tag/release/production mutation was performed for this check.

Draft visibility is a planning permission, not a publication shortcut. GitHub's
[release API contract](https://docs.github.com/en/rest/releases/releases#list-releases)
returns drafts only to callers with repository push access. The `prepare` job
therefore has job-local `contents: write` plus `actions: read`; the workflow's
build defaults remain read-only. Its planning command still only reads the
existing draft and selects build/reuse/existing. The first real promotion run
`40468621818` failed at this read with `release not found`, before native jobs or
uploads; the owner separately confirmed the draft exists with zero assets.
Dispatch the corrected workflow definition from `main` while checking out the
unchanged immutable release tag for source/fingerprint validation. Do not move
the tag or publish an empty draft to work around visibility.
