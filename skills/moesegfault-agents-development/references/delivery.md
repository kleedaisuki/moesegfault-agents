# Delivery and operational diagnosis

Use the checked-out `.github/workflows/ci.yml`, `deploy.yml` and
`release-magents.yml` as the executable contract. Read their inputs before acting;
the skill is not authorization to publish or change infrastructure.

## Promote tested artifacts

For an authorized staging delivery, dispatch CI on the intended ref, identify its
actual run and wait for its conclusion. Manual CI selects all components and
provides the paired `worker` and `dashboard` artifacts required by Deploy.

```sh
gh workflow run ci.yml --ref REF
gh run list --workflow ci.yml --branch REF
gh workflow run deploy.yml --ref REF -f environment=staging -f source_run=CI_RUN_ID
```

Replace the placeholders with observed values. The selected CI must be successful,
from the exact deployment SHA, from the CI workflow and not a pull-request run.
Never bypass that check or substitute artifacts from another revision. Preserve
the complete Worker build directory, including the JavaScript beside `worker/`.

Check the source and live state before retrying a failed workflow. Worker upload,
container update and secret reassertion are not one transaction: a red job may
have applied a migration and activated code. A health response alone does not
prove the container rollout or remaining delivery steps completed. Reconcile the
same accepted declaration when appropriate; do not dispatch a competing release.

Production requires its own release authority, complete relevant staging journey
and environment-paired configuration. Keep existing databases, namespaces,
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
[onboarding](https://github.com/kleedaisuki/moesegfault-agents/blob/main/docs/deployment-onboarding.md),
[observability](https://github.com/kleedaisuki/moesegfault-agents/blob/main/docs/observability.md).
