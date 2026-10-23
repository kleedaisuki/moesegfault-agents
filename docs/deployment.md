# Deployment and release discipline

## Environment boundaries

Staging and production have separate Workers, D1 databases, R2 buckets, Identity clients, signing keys, encryption keys, Billing issuer/audience configuration and hostnames. `wrangler.jsonc` contains public resource identifiers only. Production registrations and secrets must exist and staging acceptance must pass before production delivery.

Initial resources were provisioned in the Cloudflare account belonging to this repository's operator. Production resources remain unused until release approval. Re-running deployment must not recreate databases or overwrite encryption keys. Changing `DATA_ENCRYPTION_KEY` without re-encrypting existing data makes BYOK and OAuth token envelopes unreadable.

## GitHub Actions

`CI` computes affected areas from the actual Git diff. Documentation-only changes do not rebuild the platform; dashboard-only changes do not build containers. Native Rust checks, wasm/Worker builds, dashboard interaction tests and isolated container journeys run independently. Cargo, npm and Docker BuildKit caches are keyed by their real inputs. Actions are pinned by commit; Dependabot reviews updates.

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
