# Delivery and operational diagnosis

Use the checked-out `.github/workflows/ci.yml`, `deploy.yml` and
`release-magents.yml` as the executable contract. Read their inputs before acting;
the skill is not authorization to publish or change infrastructure.

## Select new work and reuse evidence

Read `docs/incremental-acceptance.md` and the committed selector plan. The user
requires reuse of already passing functionality unless there is concrete failure
evidence. Preserve each baseline's original component/source/dependency identity;
never relabel a reused or skipped check as newly passed at this SHA. Build changed
components as needed and select new capabilities or actual failed regressions,
not old Chat/file/UI/CLI or large-prefill trajectories merely to aggregate green.

```sh
gh workflow view ci.yml
gh workflow view deploy.yml
```

These read workflow contracts, not authorization to dispatch. An authorized
deployment uses the observed normal CI run for the exact candidate and its
source-bound plan/results. `scripts/verification-baselines.json`, component
fingerprints and `scripts/deploy-provenance.mjs` govern reuse; do not impose the
old all-six-jobs-rerun rule. A historical failed aggregate remains failed even
when a named passing phase/selector is reusable. Plans, diagnostic-only runs and
`no_selected_tests` cannot become successful new execution evidence.
Per-check proof can also be reused when its declared old/current input
fingerprints and actual historical result match, even if another part of the
component changed. Diagnostic-only component evidence cannot promote an artifact.

Worker, dashboard and daemon may reuse different accepted source artifacts only
when relevant source/dependency fingerprints and original run/artifact identities
match. The manifest states actual provenance; `BUILD_ID` labels the candidate
configuration, not a claim every reused binary was rebuilt. Preserve the complete
Worker build directory. Load/publish the accepted immutable container archive,
not a promotion rebuild. Basic and 4 GiB profiles use the same tested image;
require actual SKU/daemon capacity proof before memory-dependent paid work.

Read Deploy's current inputs. `acceptance_profile=none` verifies activation and
records no selected paid acceptance, not a successful user journey. The bounded
usage probe is an observation, not full compact/tool acceptance. A Responses
increment can precede hosted-tool completion without closing the full objective.
Never rerun an uncertain paid profile to evade its saved intent.

Check the source and live state before retrying a failed workflow. Worker upload,
container update and secret reassertion are not one transaction: a red job may
have applied a migration and activated code. A health response alone does not
prove the container rollout or remaining delivery steps completed. Reconcile the
same accepted declaration when appropriate; do not dispatch a competing release.

All paid language-model acceptance uses direct DeepSeek, never OpenRouter.
Authorized image acceptance may use the configured OpenRouter Seedream 5 route.
Missing cache-write/reasoning detail is unknown, not zero. Preserve accepted IDs;
never repeat model/tool work merely to repair observation or obtain a receipt.

Final production/public release requires root clearance and aggregate named
capability evidence: branches, Conversations, rich fields, WebSocket/steering,
count/compact, hosted execution/artifacts, accounting, performance and clients.
Old narrow baseline receipts do not close that objective. The integration captain
owns the shared index; ReleaseOps owns evidence/activation wording. Keep
environment-paired configuration and existing databases, namespaces,
buckets, Identity registration and encryption/signing keys. Do not generate keys,
change subject sectors, issue grants or rotate secrets as routine deployment
repair. An app's Subscribe return requires the exact corresponding producer
allowlist; coordinate that release before enabling the consumer's production hop.

## Migration and rollback

Inspect pending migrations in the selected environment before applying them.
Preserve IDs, ciphertext/AAD, legacy conflict targets and already issued grant
snapshots. An older binary may read legacy workloads without understanding a new
feature's rows. Application rollback is not an instruction to reverse financial
events, erase sessions or drop canonical R2 data. Review the predecessor's actual
schema compatibility instead of assuming a green old build is a safe rollback.

## Release or diagnose only what is requested

The CLI packager validates tag/version/source, tests native binaries and writes
checksum sidecars and BUILD-INFO. Preserve existing tags and assets; never move a
tag or overwrite a public binary to hide a failed release. Verify the distributed
archive itself, not just an unrelated debug executable.

Use bounded, content-free telemetry tied to the affected run/session/build.
Trace IDs correlate events; they do not authenticate owners. Do not export raw
tokens, callback queries, prompts, Bash output or private paths as diagnostics.
Distinguish captured evidence from gaps, and channel acceptance from client receipt.

Canonical procedures: [deployment](https://github.com/kleedaisuki/moesegfault-agents/blob/main/docs/deployment.md),
[incremental acceptance](https://github.com/kleedaisuki/moesegfault-agents/blob/main/docs/incremental-acceptance.md),
[onboarding](https://github.com/kleedaisuki/moesegfault-agents/blob/main/docs/deployment-onboarding.md),
[observability](https://github.com/kleedaisuki/moesegfault-agents/blob/main/docs/observability.md).
