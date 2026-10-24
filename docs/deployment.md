# Deployment and release discipline

## Environment boundaries

Staging and production have separate Workers, D1 databases, R2 buckets, Identity clients, signing keys, encryption keys, Billing issuer/audience configuration and hostnames. `wrangler.jsonc` contains public resource identifiers only. Production registrations and secrets must exist and staging acceptance must pass before production delivery.

Initial resources were provisioned in the Cloudflare account belonging to this repository's operator. Production is activated only after release approval; the verified initial release is recorded below. Re-running deployment must not recreate databases or overwrite encryption keys. Changing `DATA_ENCRYPTION_KEY` without re-encrypting existing data makes BYOK and OAuth token envelopes unreadable.

## GitHub Actions

`CI` computes affected areas from the actual Git diff. Documentation-only changes do not rebuild the platform; dashboard-only changes do not build containers. Native Rust checks, wasm/Worker builds, dashboard interaction tests and isolated container journeys run independently. Cargo, npm and Docker BuildKit caches are keyed by their real inputs. Actions are pinned by commit; Dependabot reviews updates.

Dashboard browser journeys use the Ubuntu 24.04 runner's installed Google Chrome through the existing Playwright executable-path option. The step logs the actual browser version and fails if it is absent; all interaction tests remain mandatory. This avoids re-downloading system fonts through the runner's slow APT mirror (over eleven minutes for 25.9 MB in run `39985741803`; the six browser journeys took 12.2 seconds). This tests the maintained Chrome channel, not an exactly pinned browser build. See the [versioned runner image manifest](https://github.com/actions/runner-images/blob/ubuntu24/20261012.334/images/ubuntu/Ubuntu2404-Readme.md) and [Playwright's Chrome support](https://playwright.dev/docs/browsers#google-chrome--microsoft-edge).

The initial deployment uses a manual **CI** dispatch, which forces all areas and produces `worker` and `dashboard` artifacts. Then dispatch **Deploy** on the same commit with `environment=staging` and that successful `source_run`. Deploy verifies exact source SHA, successful CI, workflow identity and non-PR origin. Missing environment-paired secrets stop delivery before infrastructure changes. Container images are published before the Worker changes because Cloudflare Worker/image rollout is not transactional. The deployment points at a commit-tagged Cloudflare registry image and reuses the verified Worker/UI artifacts.

```sh
gh workflow run ci.yml --ref main
gh run list --workflow ci.yml
gh workflow run deploy.yml --ref main -f environment=staging -f source_run=RUN_ID
```

The deploy environment serializes releases and never cancels an in-flight rollout. Configure protected reviewers for the GitHub `production` environment. Do not deploy an unreviewed arbitrary branch to production.

### Required GitHub secrets

| Secret | Purpose |
| --- | --- |
| `CLOUDFLARE_API_TOKEN` | Least-privilege Worker, D1, R2, routes and Containers delivery |
| `CLOUDFLARE_ACCOUNT_ID` | Target Cloudflare account |
| `STAGING_IDENTITY_CLIENT_PRIVATE_JWK` | Private ES256 client-assertion signing key for the registered staging BFF |
| `STAGING_DATA_ENCRYPTION_KEY` | Random 32-byte base64url key for staging envelopes |
| `PRODUCTION_IDENTITY_CLIENT_PRIVATE_JWK` | Independently provisioned production BFF key |
| `PRODUCTION_DATA_ENCRYPTION_KEY` | Independently generated production envelope key |
| `OPENROUTER_API_KEY` | Platform-funded OpenRouter calls |
| `DEEPSEEK_API_KEY` | Optional platform-funded DeepSeek calls |
| `STAGING_SMOKE_API_KEY` | Dedicated capped staging acceptance account, not a personal credential |
| `STAGING_SECONDARY_API_KEY` | Separate synthetic staging tenant for ownership-isolation tests |

The S3 credentials already present in repository secrets are not injected into bash or the Worker: R2 access uses platform bindings. Private JWKs are never committed. Public client registration records live under `deploy/identity`.

Two explicitly synthetic staging principals were provisioned for automated acceptance, not presented as completed human Identity logins. Their API keys are in GitHub secrets and an operator-only ACL-protected local task file. Wallet funding still uses the ordinary Free grant and admission paths; no unlimited-balance bypass exists. The deployment smoke explicitly enables billed DeepSeek Flash calls through OpenRouter, workspace execution and quota saturation only against these dedicated test tenants. Browser/native login acceptance is a separate gate.

## Retention and recovery

- Conversation archives and current workspace checkpoints are user data; no blanket age-deletion lifecycle is applied.
- Opt-in diagnostics are in their own bucket. `deploy/r2-diagnostics-lifecycle.json` deletes them after seven days and aborts incomplete multipart uploads after one day. The staging rule is applied during provisioning and reasserted by delivery.
- Cloudflare native logs/traces have separate observability retention and sampling. They are not the billing ledger, and must not contain raw prompts, bash output, request bodies, tokens, codes or keys. Query-string redaction is enabled because login callbacks carry authorization codes.
- Schema migrations are forward-only and applied to the selected environment. Deploy does not delete databases, user buckets or session state.
- After a rollback, verify container protocol compatibility and session schema handling as well as Worker health. A healthy HTTP endpoint alone does not prove resumed workspaces or active runs recovered.

## Acceptance before promotion

Use `tests/e2e` and the concrete checklist in `docs/acceptance.md`. Required evidence includes real account login/logout/refresh, account isolation, SDK time and bash calls, streaming, live steering, pinned-prefix behavior, percentage compaction, archive/resume/R2 restoration, concurrent quota admission and model/resource ledger reconciliation. Absent credentials or skipped tests are not production acceptance.

Cloudflare references: [Containers deployment](https://developers.cloudflare.com/containers/guides/deploy/), [R2 object lifecycles](https://developers.cloudflare.com/r2/buckets/object-lifecycles/), [Workers tracing](https://developers.cloudflare.com/observability/traces/workers-tracing/).

## Build boundary learned from the first CI run

`cargo check --target wasm32-unknown-unknown` is not a substitute for the full `worker-build` binding pass. With wasm-bindgen 0.2.129, `strip=true` removes the externref table required for catch wrappers and the release binding generator fails. Keep `strip="debuginfo"`, which preserves those symbols; see [workers-rs issue 1014](https://github.com/cloudflare/workers-rs/issues/1014). Native checks and browser journeys passed the first CI run while this packaging boundary failed, so the Worker artifact remains an explicit release gate.

Worker-build 0.8.7's `build/worker/shim.mjs` also exports `../index.js`. Preserve the **entire `build` directory** when uploading/downloading the verified artifact, not only `build/worker`. CI runs a Wrangler dry-run bundle check after worker-build so transitive JavaScript/Wasm dependencies are validated before an image is pushed or a Worker is changed.

CI also runs the compiled Wasm inside workerd with actual D1 and Durable Objects, not a JavaScript rewrite of the harness. This exposed an important Fetch difference: workerd rejects `redirect: "error"` at Request construction. Provider requests use `manual` and reject non-2xx responses, so a redirect cannot forward the bearer credential. The fixture checks both successful tool continuation and rejection of a cross-host redirect. Built artifacts are retained even after an integration failure for inexpensive local diagnosis; failed CI artifacts remain ineligible for deployment.

## Request abuse controls

Native Workers Rate Limiting bindings protect ingress before database authentication, then bound per-account requests and mutations. Staging and production use distinct namespaces. Initial per-60-second limits are 2,400 requests per hashed ingress IP, 1,200 per authenticated account, and 120 account mutations. Responses include `429 request_rate_exceeded` and `Retry-After`. These are inexpensive, approximate per-Cloudflare-location abuse controls, **not** globally exact financial or concurrency admission; D1's atomic constraints remain authoritative for those. Normal management-plane operations are included in the bounded service surface rather than generating a recursive ledger event for every management/ledger query.

## Verified initial release readback — 2026-10-24

Read-only Cloudflare API inspection at 06:22–06:24 UTC verified both **100% serving
versions**, rather than relying only on local Wrangler configuration. Their
version-specific bindings carry `BUILD_ID=8caff37333cf148a62c804e75cc9189203e8f167`.
The serving deployment stayed unchanged across each readback.

| Realm | Worker version | D1 database | Session namespace | AuthSession namespace |
| --- | --- | --- | --- | --- |
| Production | `532518a8-a04d-41e6-8453-8426c3717eda` | `4b7789d6-538f-40d1-a658-138d1688214c` | `6ed66cca67e448879b33379b7d51cbc2` | `326d03a6f9e24ff1b338965acbea26ec` |
| Staging | `d08b2222-db22-4001-9cf0-161a96bd20f6` | `f52d24b2-90eb-4156-8048-33c1da11b181` | `00b9db0fcc03436eb146b61fbc7811d8` | `3a97c96b34ad4554a19b4ea675c0c8d5` |

The exact scripts are `moesegfault-agents-production` and
`moesegfault-agents-staging`. Production's Identity issuer, client IDs,
callback/dashboard origins and Billing origin match its public configuration;
staging uses its separate `-staging` registrations and origins. Billing mode is
`live` in production and `test` in staging. Secret binding presence was checked
by name only; secret values were neither retrieved nor compared.

The six bound buckets follow
`moesegfault-agents-{workspaces,conversations,diagnostics}-{production,staging}`.
All six have managed public access disabled and zero custom domains. Both
diagnostics buckets have the enabled seven-day object deletion rule and one-day
incomplete multipart abort rule. No user object contents or session records were
read during this audit.

The active Worker resource and script settings agree: persisted invocation logs
are enabled at sampling `1`, persisted traces at `0.1`, query-string redaction is
on, Logpush is off, and there are no tail consumers. These are verified capture
settings, not a claim that every request is trace-sampled. The sanitized operator
receipt is `.cache/deployment-runtime-audit-safe.json`; it contains only public
resource references, fixed public settings and secret names.

Container App metadata independently matches those Session namespaces:
production App `a0334311-659f-45d3-92a0-16cc378d14c5`, active version `1` at
100% with maximum 20 instances; staging App
`a03bce76-eb9d-4db6-a9a1-e486fab9fe60`, active version `7` at 100% with maximum
5 instances. Both App and active Version configurations reference the exact
`magents-workspace:8caff37333cf148a62c804e75cc9189203e8f167` commit tag.
**The App/Version APIs did not expose a resolved image SHA256 digest.** This audit
therefore verifies the configured tag, not byte-identical resolved images; neither
the App version nor deployment configuration ID is a digest. Registry credential
creation was outside this read-only audit. The sanitized image metadata receipt
is `.cache/deployment-image-audit-safe.json`.

Separate publication-time evidence is available in the official deployment logs:
the final Docker registry push in both [staging 40067574002](https://github.com/kleedaisuki/moesegfault-agents/actions/runs/40067574002)
and [production 40068971874](https://github.com/kleedaisuki/moesegfault-agents/actions/runs/40068971874)
reports the same `8caff37333cf148a62c804e75cc9189203e8f167` tag with manifest digest
`sha256:58feb5ef6d58bce367f44cc6e388d3cd7dcb1a8a5a2ab5be4c85fe256bb4d3e0`.
This is the image publication digest, not the Buildx helper-image digest or an
Actions download checksum; it does not add a resolved-digest field to the App API.

## Maintained deployment-tool security

The deployment/test Node toolchain pins Miniflare's Undici to patched `7.30.1`
without changing Wrangler `4.152.0` or Miniflare `5.20261019.2-beta`. A fresh audit
reported zero vulnerabilities, and the published Rust/Wasm artifact still passed
all sixteen HTTP/transport cases under the patched toolchain. Targeted tooling CI
also runs `npm audit --audit-level=high`. These are operator development dependencies;
the deployed Rust services and distributed Rust CLI archives do not run Undici.
The immutable `v0.1.0` implementation tag is not moved to rewrite build history.

## Native-signer server promotion

The server-only security update at
`a2059d550c8b0651df740428554e93fcb2bf04dd` supersedes the initial serving build
above. Full [CI 40076378439](https://github.com/kleedaisuki/moesegfault-agents/actions/runs/40076378439)
passed before [staging 40076671584](https://github.com/kleedaisuki/moesegfault-agents/actions/runs/40076671584)
and then [production 40077421671](https://github.com/kleedaisuki/moesegfault-agents/actions/runs/40077421671)
were promoted from its exact artifacts. Staging's fourteen deployed SDK cases
passed; both production host health responses reported the exact new commit.
Production Wrangler reported Worker version `fec10e58-928d-453e-81a3-72d065f46bb6`.

The existing client registrations and signing/envelope keys were retained. The
change isolates private ES256 arithmetic in native Web Crypto, with no software
fallback, and does not alter the data schema or container daemon. Both deployment
logs report the new commit's container tag with the unchanged publication digest
`sha256:58feb5ef6d58bce367f44cc6e388d3cd7dcb1a8a5a2ab5be4c85fe256bb4d3e0`.
This remains publication evidence, not a resolved-digest field in the App API.
The independently verified CLI `v0.1.0` tag and four public assets were not moved
or replaced. See [acceptance](acceptance.md) and [Identity](identity-billing.md)
for the real owned-account BFF regressions after this promotion.

## v0.2.0 production readback

Runtime `265e0ea8bd557d540077afced3bb20b735bbfdfc` passed full CI 40100503604
and staging 40100946077 before production 40102985955. Attempt 1 applied 0010
and uploaded the Worker, but failed at Cloudflare's Container application update;
the same-run attempt 2 reconciled the unchanged declaration and passed all steps.
Production has no pending D1 migrations. No reverse migration or user-data reset
was used; migration 0010 preserves legacy credential identity and encryption.

The final secret-reasserted serving Worker is
`d198f098-1278-44f8-a2dc-90d62de3a9bc` at 100%, not the intermediate code-upload
version. Both production host health responses report `265e0ea`. The production
Container App is version 3 at 100% with maximum 20 instances; staging App version
12 uses the same accepted commit tag. Their registry publication digest is
`sha256:9114bc4855b4227dd5f2116cbde291c8cc874b184d49e7634bc1c97336bce206`.
Namespace, bucket and identity registrations remain environment-paired.

Subscribe/Billing production promotion 40102195561 preceded Agents. The
[cross-service receipt](https://github.com/kleedaisuki/moesegfault-subscriptions/blob/af4060467997cc57eb68b2910b9b19e0a807480a/docs/deployment/production.md#completed-production-release-receipt)
records the authoritative bindings/versions and actual production return journey.
CLI `v0.2.0` is the immutable `265e0ea` tag with four verified public Linux/Windows
assets; production export acceptance includes the original v0.1 user snapshot.
