# Staging application onboarding

## Environment contract

| Surface | Registration |
| --- | --- |
| Fixed issuer | `https://identity-staging.moesegfault.dev` |
| Dashboard BFF | `agents-dashboard-staging`, confidential `private_key_jwt` (ES256) |
| Dashboard callback | `https://adash-staging.moesegfault.dev/auth/callback` (exact) |
| Dashboard logout return | `https://adash-staging.moesegfault.dev/` (exact) |
| Native CLI | `magents-staging`, public native, no client secret |
| Native callback | `http://127.0.0.1/callback`, registered `native_loopback_any_port` |
| Scopes | `openid profile offline_access` |
| Shared Billing subject sector | `subscribe-staging.moesegfault.dev`, salt revision 1 |
| Public signing-key ID | `agents-dashboard-staging-2026-10` |

Public manifests and the public JWK are in [`deploy/identity`](../deploy/identity/). The private JWK is deployment secret material, not a browser asset or tracked file. Native clients never receive a client secret. Map accounts by the fixed `(issuer, sub)` pair; the shared sector intentionally makes the same human address the same Billing account through Subscribe, adash, and magents. Never change an existing client's sector without an explicit account migration. Production remains separate and untouched.

## Registration and ownership decision

Upstream Identity branch `agents-staging-onboarding`, commit `a374fd9`, adds only create-only staging overlay migrations 0012 (magents) and 0013 (dashboard), plus public manifests and review rationale:

- Repository: <https://github.com/kleedaisuki/moesegfault-indentity/tree/agents-staging-onboarding>
- The official `scripts/generate-oauth-client-migration.mjs` generated the native SQL directly.
- The generator deliberately requires a confidential redirect hostname to equal its sector because it cannot establish ownership of another host. User authorization on 2026-10-23 explicitly covers both applications and the shared Billing sector; both hosts belong to the same deployment owner. For this reviewed dashboard registration only, all fields were validated by the unchanged generator using the dashboard host sector, then the single sector literal in create-only SQL was reviewed and changed to the authorized Subscribe staging sector. The final public manifest records the actual shared sector.
- The generic generator guard and runtime authentication code were not weakened or changed. Runtime redirect matching is independent of registered sector derivation.
- `scripts/prepare-migrations.mjs staging` materialized the staging overlay. Remote migration listing showed exactly these two new migrations, then `wrangler d1 migrations apply moesegfault-identity-staging --remote --config wrangler.identity.jsonc` applied both successfully.
- Remote D1 readback confirmed both enabled client rows, exact/native redirect modes, shared sector revision 1, and the dashboard ES256 key ID.

The upstream OAuth provisioning suite passed all 9 tests after installing locked dependencies, including isolated D1 migration application, foreign-key checks, audit outbox creation, and staging/production overlay isolation. Staging authorization smoke requests for both registered clients returned HTTP 302 to the staging Login origin. These checks prove registration and authorization-start routing, not a completed human login/refresh/logout journey.

## Billing audience deployment

Upstream Billing branch `agents-staging-onboarding` adds exact audiences `agents-dashboard-staging` and `magents-staging` alongside existing `subscribe-staging` in **staging** `wrangler.billing.jsonc` only. Exact audience contract assertions are updated, not relaxed; local focused tests passed 8/8. Production configuration is unchanged.

- Repository: <https://github.com/kleedaisuki/moesegfault-subscriptions/tree/agents-staging-onboarding>
- Revision: `03e0e2f` (onboarding configuration `9777aa9`, separate development-tool security patch `1bf0b8b`, separate staging plan catalog `03e0e2f`).
- Existing GitHub Actions workflow dispatched with `delivery=staging-only`: <https://github.com/kleedaisuki/moesegfault-subscriptions/actions/runs/39969956731>.
- Initial candidate run failed the two old exact-audience fixture expectations; corrected expectations preserve an exact allowlist. No failed candidate was deployed.

The second candidate passed contract and SQL tests but encountered pre-existing npm audit advisories in pinned development tools. A separate atomic security commit pins Vite 8.3.5 and Vitest 4.1.12; npm audit now reports zero vulnerabilities and local typecheck, 26 frontend tests, and frontend build pass. No audit gate was bypassed. The latest run above contains this fix.

The staging-only workflow builds and verifies immutable artifacts, then deploys through the repository's existing release script. The linked run completed successfully on 2026-10-23 after retrying only its failed deployment job: a transient Cloudflare D1 API internal error 7429 interrupted the first attempt before Worker release. The retry reused the same verified immutable artifact; all deployment/smoke jobs succeeded and production promotion was skipped. Do not merge the Identity branch to main just to apply staging registrations: its main push workflow also promotes production code. The user has already authorized production client/audience/shared-sector onboarding after staging acceptance. Execute that existing authorization through separately reviewed production registrations, keys, audiences, callbacks, and environment acceptance gates; no redundant user approval is required.


## Synthetic staging Agents plans

The staged catalog registers `agents-go`, `agents-plus`, and `agents-pro` under product `agents`, each lasting 30 days and carrying `agents.access` plus exactly one of `agents.tier.go`, `agents.tier.plus`, and `agents.tier.pro`. Free requires no subscription grant. Descriptions clearly mark these as synthetic staging plans with no real charges; the catalog schema has no numeric price field. Proposed prices (USD 10/20/200) and API-equivalent allowances (USD 8/18/180) are descriptive only; Agents owns quota and allowance enforcement. No numeric allowance entitlements or payment-provider changes were invented.

`plans:sync`, `plans:check`, and all 54 deployment contract tests passed after installing the complete pinned upstream lockfile. Root approval covers this staging-only catalog; no production catalog or configuration was changed. Existing Billing supports one subscription per account/product and only extends the same plan while active; plan changes require expiry under its current contract.

## Live staging acceptance readback (2026-10-23)

- Billing serving Worker version: `2ceefef5-6a6c-4396-8637-75844e6cb9c3`.
- Subscribe serving Worker version: `adfcb102-5274-4ff3-a649-c50b6b67972d`.
- Existing release pipeline verified capture settings for these exact serving versions and passed Billing health, Subscribe HTML/CSP, guest-session, and protected-Billing smoke checks.
- Public `GET https://billing-staging.moesegfault.dev/v1/plans` returned HTTP 200 with all three expected Agents plans, exact product `agents`, 30-day periods, and exact access/tier entitlements.
- Staging OAuth registration authorization-start smoke returned HTTP 302 to staging Login for the dashboard's exact callback and magents' variable-port loopback callback.
- Completed authenticated human login, refresh, activation, and application quota/allowance journeys remain application-level acceptance work; this onboarding does not claim those have been exercised.

## Production readiness review — 2026-10-24 (read-only, not provisioning)

No production registration, migration, deployment, catalog change, secret creation, activation issuance, or payment-provider configuration was performed for this review. The table below distinguishes intended application configuration from registered production facts.

| Item | Intended production contract | Read-only finding / required action |
| --- | --- | --- |
| Issuer | `https://identity.moesegfault.dev` | Live discovery returned 200 with the exact issuer. |
| Dashboard client | `agents-dashboard`, confidential `private_key_jwt`, fresh production-only ES256 key | Application config contains the ID and proposed key ID `agents-dashboard-production-2026-10`; live production Identity D1 contains **no such client**. Public manifest, production key custody, reviewed create-only overlay, apply, and readback remain required. Never reuse the staging key. |
| Dashboard redirects | `https://adash.moesegfault.dev/auth/callback`; logout `https://adash.moesegfault.dev/` | Intended exact registration, not provisioned. |
| CLI client | `magents`, native `none`; `http://127.0.0.1/callback` with `native_loopback_any_port` | Application config contains the ID; live production Identity D1 contains **no such client**. A production overlay must register it without any client secret. |
| Shared sector | `subscribe.moesegfault.dev`, revision 1 | Live `subscribe` client is enabled and already uses this sector. New Agents clients must join deliberately; do not change existing Subscribe subjects. The confidential dashboard needs the same explicit cross-host ownership/privacy review used in staging, without relaxing the generic generator guard. |
| Billing audiences | Exact `subscribe`, `agents-dashboard`, `magents` | Inspected upstream production configuration currently contains only `subscribe`; no audience update has been staged or applied. This is a configuration inspection, not a live Worker binding readback. |
| Plans | `agents-go`, `agents-plus`, `agents-pro`, product `agents`, 30 days, `agents.access` + exact tier entitlement | Live production `GET /v1/plans` returned 200 with platform/amail plans only; **no Agents plans**. Free remains implicit. A separate reviewed production catalog and synchronized Worker configuration are required. |
| Product policy | Free USD 0 / credit 1; Go 10 / 8; Plus 20 / 18; Pro 200 / 180 | Approved local product policy; no successful monetary purchase or production charge is established by these numbers. Billing's catalog has no price/payment field. |
| Return journey | Subscribe → adash account refresh; CLI rereads trusted Billing grants | Check exact production continuation allowlists and session-preserving navigation; a `return_to` query is not registration or proof of activation. Test actual production-paired authenticated flows after the staging acceptance gate. |
| Application billing mode | Current `wrangler.jsonc` production value is `live` | A prefilled mode flag is not a payment integration or readiness signal. Keep production promotion gated; do not represent synthetic grants as collected money. |

The live production client query selected only public configuration columns for the three exact IDs (`agents-dashboard`, `magents`, `subscribe`); it returned only enabled `subscribe`. Public catalog readback and issuer discovery did not authenticate a human, create a login transaction, or modify data.

### What Subscribe actually supports

The inspected upstream contract (`docs/api-contract.md`, `docs/subscribe-ui.md`, `crates/subscribe/src/lib.rs`) supports server-side OIDC login, a plan catalog, administrator-issued activation codes delivered privately, idempotent code redemption, and subscription/profile views. `Subscription.activation_source` is `activation_code`. There is **no demonstrated money-collection checkout, payment-provider integration, recurring subscription mandate, webhook payment confirmation, or automatic debit** in this integration. An existing activation code is a grant capability, not proof that USD 10/20/200 was collected. Do not invent a provider, merchant account, checkout URL, paid receipt, webhook, or refund capability.

A 30-day activation period is also not a calendar-month recurring purchase. Upstream permits extending the current plan but returns `409 plan_conflict` for a different plan while a product subscription remains active. Therefore the immediate-upgrade/prorated-difference policy in [pricing](pricing.md) is a product target, not an upstream checkout capability. Keep the UI truthful about manual activation and plan-change constraints until a separately designed transition contract exists.

### Minimal release gates

1. **Staging acceptance first:** accept the actual deployed WASM/model/exact-cost run plus real shared-account dashboard/CLI authentication, refresh/logout, synthetic activation, quota/allowance deduplication, and workspace recovery journeys. A successful authorization redirect or mocked CI token is not this gate.
2. **Production registration environment gate:** review separate public manifests and keys, create only the two new clients under the production overlay, verify exact callbacks/scopes/key metadata/sector, apply to production D1 only, and read back. No generic auth or generator relaxation is needed.
3. **Production Billing environment gate:** register the three exact production audiences and a production Agents catalog with deliberately approved copy; preserve existing plans and sectors. Verify serving configuration and `/v1/plans` after the reviewed environment-gated release. Do not copy staging descriptions that say synthetic into a paid offer, nor remove that label and imply payment collection by configuration alone.
4. **Choose an honest launch boundary:** an invitation/Free/administrator-issued-grant launch can be supported by existing primitives after integration acceptance, with bounded exposure and no claim of self-service purchase or automatic renewal. A self-service paid checkout is **not ready** on the current evidence and requires a separate explicit product/provider decision and implementation. No new provider should be configured just to make the release appear complete.
5. **If paid checkout is later approved:** establish the actual merchant/provider contract, authoritative price/currency/period mapping, authenticated account binding, verified and idempotent payment confirmation, replay/refund/revocation/failed-renewal policy, and cancellation/support disclosures. Only trusted settlement/grant records may change entitlements; browser success URLs and displayed plan prices cannot. Exercise the complete staging purchase/failure/retry/cancellation journey before any separately authorized production charging.
6. **Promote immutable accepted artifacts only:** isolated production issuer/accounts/secrets, explicit workflow approval, migration review, rollback/recovery plan, serving-version readback, and bounded smoke. Billing audience/catalog registration does not itself settle model or Cloudflare resource invoices.

**Current decision:** prepare production onboarding, but do not deploy production until staging acceptance passes or advertise a functioning paid recurring checkout. Existing user authorization already covers production OIDC client, audience, and shared-sector registration after that gate; this is not waiting for another registration permission. The concrete missing items are the two real production registrations and production key provisioning, the exact audience/catalog release, authenticated application acceptance, and an actual explicitly chosen payment/upgrade contract if self-service money collection is required.


## Staging Pro grant acceptance attempt — 2026-10-24

The user authorized a high-allowance synthetic staging test account. A single immutable administrator intent (`9b7a01d4-3106-4619-8379-2ad1b379cc5d`, plan `agents-pro`) was issued through the official Billing administrator `issue()` function, using the existing managed staging key only in script memory. No key was created or rotated; no D1 grant, claim, or wallet bypass was used; no actual payment occurred.

The test used the existing exact fixture allowlist and official route manager. Route creation was followed by the required 60-second data-plane settle and a second enabled-state audit. The private inbox Worker accepts only its exact recipient allowlist and bounded MIME, not only Identity senders. A purpose-specific reader checked Subscribe sender, exact activation subject, plan, independent issuance time window, and a unique new UUID-keyed object; it did not reuse OTP transaction parsing. The baseline and final private-bucket inventory were both empty.

The administrator API returned `sent`, but bounded private-inbox polling did not receive a capability. Cleanup succeeded: the formal managed `ADMIN_EMAIL` was restored using the unchanged authoritative private source, the exact test route was removed and audited absent, and no private inbox objects remained. No activation capability or key was printed, and no amail-indexed mailbox path was used.

A read-only, tightly time-bounded Cloudflare GraphQL metadata probe subsequently found exactly one sending event at **2026-10-23 17:17:42 UTC** (**2026-10-24 01:17:42 Asia/Singapore**), status `delivered`, categorically addressed to the **formal** administrator recipient; fixture inbound-event count was zero. Raw recipient, mail content, and provider identifiers were neither logged nor recorded in this document. This selects a prior-effective-recipient/configuration-convergence explanation rather than a fixture MIME-reader mismatch. `wrangler secret put` completion did not by itself establish that the next issuance request observed the new recipient. The adjacent upstream `docs/deployment/production-mail-diagnostic-2026-10-05.md` describes the same convergence risk, although that earlier incident lacked a selecting provider event.

The current intent must not be replaced or automatically resent: replay returns its existing receipt. Recovery requires the **existing delivered formal-mailbox message** through an authorized non-amail-indexed channel, followed by ordinary Subscribe redemption for the same staging fixture principal. Pro allocation/shared-sector/redemption-idempotency acceptance remains pending that recovery, not completed by `sent` or `delivered` alone.

References: [Cloudflare email activity states](https://developers.cloudflare.com/email-service/observability/logs/) distinguish accepted/queued from recipient-server delivery; [Email Service GraphQL analytics](https://developers.cloudflare.com/email-service/observability/metrics-analytics/) documents zone-scoped sending/routing event metadata. Neither provider handoff nor an event recipient is a successful subscription redemption.

### Explicit second grant and successful staging redemption

After the first outcome was positively established as formal-mailbox delivery, the administrator explicitly authorized **one second independent synthetic staging grant**, not an uncertain retry or an automatic resend. Its separate immutable intent is `a1f2e730-a017-460f-a0c6-6933f8b51c39`. The first grant remains delivered to the formal administrator and was not redeemed by this journey; it was not revoked, destroyed, or silently replaced.

The second issuance preserved the exact fixture route and 60-second route settle. After temporarily changing only staging `ADMIN_EMAIL`, the helper additionally waited **180 seconds** and required the same single 100%-serving Billing Worker version before and after that interval (`6c85386e-88ab-47c0-af4b-3e9ea1299f88`). This is a propagation/stability safeguard, not a claim that a secret value is readable or that the recipient was independently observed before sending. The administrator API accepted this intent once. The first bounded reader attempt failed with one object now present; a subsequent **read of that same delivered object**, without another issuance or send, passed exact sender/recipient/subject/plan/time and uniqueness validation. The private R2 object was deleted after extraction; inventory returned to zero. The formal recipient and exact route were already restored before redemption. Private key, code, MIME, and recipient values were never printed or sent through an amail-indexed path.

The same real staging fixture then completed:

| Journey | Observed result |
| --- | --- |
| Subscribe's ordinary OIDC login | Authenticated same test user through the staging issuer |
| Visible activation form | HTTP success, `agents-pro`, initial `replayed=false` |
| Same code + same UI idempotency key replay | `replayed=true`, same subscription ID and period end; no extension |
| Portal reload | Same active Agents Pro subscription persisted |
| adash account projection | Same account previously observed as Free now resolves to Pro |
| Pro limits | 200 configurations, 100 live sessions, 20 concurrent runs |
| Test wallet after grant | `48744159` microUSD (USD 48.744159), not an asserted full USD 180 payment or grant |
| Repeated account refresh | Same wallet credit; no duplicate allocation |
| Capability cleanup | Local private activation-code file removed after successful redemption |

The observed credit follows the current-period prorated-upgrade path from the existing Free allowance; it is intentionally recorded as the actual wallet amount rather than advertised as a full-period Pro allocation. No currency was collected. The shared sector is now exercised end-to-end by Subscribe redemption becoming a Pro grant for the existing adash account; the native CLI's independently registered client is verified against the same account by the authentication owner. Production remains untouched, and these two staging administrator intents do not implement recurring checkout or paid tier-change support upstream.

## Production preparation refresh — 2026-10-24 (no deployment)

A repeat read-only check while Agents source `a918c6730cf95fcb66e3ec360b4d68f64eb091f8` awaited its latest staging rollout confirmed:

- Production discovery is HTTP 200 with issuer exactly `https://identity.moesegfault.dev`.
- Production Identity's exact-ID query still returns only enabled `subscribe`, sector `subscribe.moesegfault.dev`, salt revision 1. There are no `agents-dashboard`/`magents` client rows, no redirects for those clients, and no dashboard client key rows. The query reads only public registration metadata, not principals, tokens, sessions, or signing secrets.
- Production public `/v1/plans` remains HTTP 200 with **zero** product `agents` entries. Do not recreate a client or reinterpret a previously issued plan if either condition changes before execution: repeat the exact-ID/catalog check and reconcile its reviewed metadata instead.
- [`deploy/identity/magents-production.json`](../deploy/identity/magents-production.json) is prepared **public registration metadata only**. The unchanged official `renderClientMigration()` accepted it in memory; no production SQL was persisted or applied by this preparation.
- The confidential dashboard manifest is now prepared at [`deploy/identity/agents-dashboard-production.json`](../deploy/identity/agents-dashboard-production.json) using the deployment owner's **production-only public JWK** from [`production-public-jwk.json`](../deploy/identity/production-public-jwk.json). The public coordinates were checked distinct from staging and no private JWK fields were accepted. This remains prepared metadata, not a live registration. Public metadata: client `agents-dashboard`, `private_key_jwt`/ES256, configured key ID `agents-dashboard-production-2026-10`, exact callback `https://adash.moesegfault.dev/auth/callback`, exact post-logout `https://adash.moesegfault.dev/`, scopes `openid profile offline_access`, shared sector `subscribe.moesegfault.dev`, revision 1.

### Ready-to-execute sequence after the staging acceptance gate

The existing user authorization covers these client, audience, and shared-sector additions. The sequencing below is an environment/review gate, **not a request for another user authorization**. No step below was executed in this preparation phase.

1. **Refresh deployment source and numbering.** Inspect the current upstream branch/history and pending environment migrations, preserving other operators' work. The local staging preparation checkouts are currently Identity `a374fd9` and Billing `03e0e2f`; these historical snapshots are not an assertion that upstream main or serving Workers still match them. Obtain the independent production public JWK from the deployment owner and compare its public metadata with the application-configured key ID/algorithm.
2. **Generate only new client rows.** From the Identity checkout, run its existing `node scripts/generate-oauth-client-migration.mjs <public-manifest-path>` for the native manifest. For the confidential client, retain the already documented controlled shared-sector procedure: use the unchanged official generator to validate all dashboard fields with its own host sector, then review the single create-only client-sector literal changed to the explicitly authorized `subscribe.moesegfault.dev`. Keep the final public manifest truthful about the shared sector. The generic cross-host guard and authentication code stay unchanged. Assign fresh, collision-free migration filenames from current history; move reviewed SQL only into `migrations/environments/production/`, never the shared root.
3. **Compose, inspect, apply, read back.** `node scripts/prepare-migrations.mjs production` composes the production stream. Use the official pinned Wrangler with `--config wrangler.identity.jsonc --env production` to list remote `moesegfault-identity-production` migrations before any apply. Proceed only when every pending migration is understood and reviewed; the intended change is exactly the two new clients and their redirects/scopes/public key/audit rows. Then apply the reviewed stream and repeat exact client/redirect/scope/key metadata queries. Existing `subscribe`, existing pairwise subjects, and staging rows remain untouched. Registration is deployment-owned D1 data; it does not require an Identity authentication-code change or an unrelated Worker rollout. Do not casually merge Identity to main: its existing main workflow can also promote production code.
4. **Prepare activation-only Billing metadata.** In a separate reviewed change, preserve existing production audiences and add only exact `agents-dashboard` and `magents`; preserve all existing plans and add `agents-go`, `agents-plus`, `agents-pro`, product `agents`, 30 days, `agents.access` plus one exact tier entitlement. Use `npm run plans:sync:production` followed by `npm run plans:check:production` and deployment contract tests. Descriptions must say administrator activation with no automatic charge/renewal. Go USD 10 / credit 8, Plus 20 / 18, and Pro 200 / 180 describe product policy, not a payment receipt. Free remains the default without an upstream paid grant. No merchant/provider/payment-webhook integration is introduced.
5. **Use verified upstream delivery, not an untracked local rebuild.** Inspect the current official Billing workflow before execution. The reviewed snapshot accepts `delivery=staging-only` to validate a branch and requires a successful `Deploy staging` job plus unexpired checksummed artifact. Production promotion is an explicit `delivery=production-only`, `artifact_run_id=<accepted-staging-run>`, `--ref main` dispatch and reuses verified Rust/frontend artifacts. Its checked-out production configuration must contain the reviewed audience/catalog changes, and its existing unrelated secret-provisioning/release steps must be understood before dispatch. Do not select an old successful run merely because it has the right branch name, or overwrite unrelated administrator/OIDC/service secrets.
6. **Verify environment pairing and real use.** Read back production serving versions and public catalog. Test production dashboard/native login against exact issuer and audiences, the unchanged Subscribe account mapping, default Free policy, and manual activation only if a production issuance is intentionally in scope. Do not make an activation code visible to logs or use the amail-indexed path. The currently inspected Subscribe continuation allowlist contains only the first-party Account return; the dashboard currently links to Subscribe without a return hint, so automatic adash return is not an established capability. A future explicit adash return requires one exact reviewed allowlist entry and application URL pairing, not a wildcard.

This plan deliberately does not modify production runtime configuration, registrations, secrets, grants, or upstream deployment branches while the staging acceptance gate remains pending. The latest corrected full-period allowance/replay result belongs to that gate's owner; the earlier USD 48.744159 snapshot above remains historical evidence, not the current full-period entitlement claim.

### Prepared upstream production sources (still no production apply/deploy)

Before editing, both temporary upstream checkouts fetched their remotes and saved their tracked `origin/main` diff and exact source revisions under repository-local `.temp/onboarding/pre-production-review/`. Their existing staging onboarding branches were not merged to main and no other operator's changes were overwritten.

| Upstream branch `agents-staging-onboarding` | Reviewed preparation | Verification |
| --- | --- | --- |
| Identity `18e0cf8` | Independent public dashboard key + production public manifests + create-only production overlays `0014_oauth_client_magents.sql`, `0015_oauth_client_agents-dashboard.sql`; same explicitly reviewed shared-sector exception | Official provisioning suite **9/9**, including actual isolated local D1 application and environment overlay isolation |
| Billing `e7990ed` | Exact production audience additions plus activation-only Go/Plus/Pro catalog; existing audiences/plans preserved; no secret, payment provider, or auth changes | Catalog sync/check, existing deployment contracts **54/54**, plus new exact activation-only production catalog assertion (production-focused file **4/4**) |

The new Identity SQL copies in [`deploy/identity/production-migrations`](../deploy/identity/production-migrations/) are **planned, unapplied** SQL. The unchanged official generator produced the native SQL and all dashboard fields with its own sector; the only reviewed dashboard difference is the unique client-sector literal switched to the authorized shared Billing sector. The production JWK is public-only, ES256, key ID `agents-dashboard-production-2026-10`; private JWK and envelope encryption material were not copied to either upstream repository or read into an agent context.

Billing CI [run 40062408884](https://github.com/kleedaisuki/moesegfault-subscriptions/actions/runs/40062408884) was dispatched with **`delivery=verify-only`**. It is a preparation check, not a staging or production deployment and not an accepted production-promotable artifact by itself: the existing workflow requires an actually successful `Deploy staging` job before promotion. Root's explicit full staging-gate-clear remains required before any production registry/configuration/secret/deployment mutation. Existing user authorization already covers those mutations once that environment gate is satisfied.

The first verify-only preparation run `40062408884` passed Rust/WASM and contract/SQL validation but stopped at the unchanged npm audit gate for newly disclosed Undici advisories (GHSA-4qv8-2p9j-2685, GHSA-f72v-6w45-q472, GHSA-xjj6-f33q-929c). A separate atomic development-tooling commit, Billing **`5c65890`**, scopes a `miniflare -> undici` override to **7.30.1**, preserving Wrangler **4.147.0** and its D1 splitter/API contract. This uses the [upstream's patched 7.30.1 release boundary](https://github.com/nodejs/undici/security/advisories/GHSA-4qv8-2p9j-2685), not the audit tool's breaking downgrade suggestion. Locked installation now reports **zero vulnerabilities**; all **56 deployment tests** pass, including real workerd auth metadata behavior, D1 SQL splitting, and the added exact lockfile-security assertion. No audit threshold was relaxed.

Replacement preparation CI [run 40062552657](https://github.com/kleedaisuki/moesegfault-subscriptions/actions/runs/40062552657) is likewise `delivery=verify-only`: it may create validation artifacts but must not deploy either environment. Production source/audience/catalog preparation remains Billing `e7990ed`, with the independent security patch `5c65890` on top.

Preparation run **40062552657 completed successfully**: frontend/deployment contracts, Rust tests/WASM builds, audit, and immutable package all passed. Both **Deploy staging** and **Explicit production promotion** were skipped, as intended for `verify-only`. This verifies the prepared source without changing either environment; it does not clear the separate Agents runtime long-task/inactivity acceptance gate or satisfy the upstream requirement for an actually staged artifact before production promotion.

## Formal production Identity registration receipt — 2026-10-24

Root cleared the existing user-authorized Identity/Billing onboarding gate after the accepted Agents staging journeys (source `164d0ae`, SDK tests, real inactivity/archive/resume, 785-second long run, zero run holds, and corrected Pro allowance handoff). This clearance covers client/audience/catalog onboarding, **not deployment of the Agents production application**, production activation issuance, or money collection.

Immediately before mutation, the temporary Identity checkout fetched upstream, confirmed reviewed source `18e0cf80e0876406b1a6491faa19776bc7a28367`, recomposed the production stream with the official script, and listed the remote pending migrations. The list contained **exactly** `0014_oauth_client_magents.sql` and `0015_oauth_client_agents-dashboard.sql`. An exact-ID preflight returned only existing enabled `subscribe`; neither new client existed. No other migration or conflicting client was overwritten.

The official pinned Wrangler applied those two forward-only migrations to `moesegfault-identity-production` with explicit production configuration. Both succeeded. Remote public-metadata readback established:

| Client | Type / authentication | Sector / revision | State |
| --- | --- | --- | --- |
| `agents-dashboard` | confidential / `private_key_jwt` | `subscribe.moesegfault.dev` / 1 | enabled |
| `magents` | native / `none` | `subscribe.moesegfault.dev` / 1 | enabled |
| Existing `subscribe` | confidential / `private_key_jwt` | `subscribe.moesegfault.dev` / 1 | enabled, unchanged |

Readback also confirmed dashboard exact callback `https://adash.moesegfault.dev/auth/callback`; native `http://127.0.0.1/callback` with `native_loopback_any_port`; both clients' exact scopes `openid`, `profile`, `offline_access`; dashboard key ID `agents-dashboard-production-2026-10`, algorithm ES256; and both applied migration-history filenames. No Identity Worker code or secret was changed, no existing sector/subject mapping was mutated, and no production user session or activation was created.

### Billing promotion sequencing issue identified before mutation

The existing official Billing production job is restricted to `ref=main` and checks out that ref's production configuration. The verified immutable artifact includes WASM/frontend assets, **not** the branch's Wrangler production configuration. Consequently, simply dispatching main against branch artifact `5c65890` would omit the prepared production audiences/catalog. Dispatching a production job on the branch would be skipped by the existing gate; a local alternate upload would not satisfy the official promotion workflow. This was escalated before any production Billing mutation rather than weakening the gate or silently merging main. The production job also includes unrelated amail service-secret provisioning, which requires a deliberately scoped release decision.

Meanwhile [Billing staging run 40065305487](https://github.com/kleedaisuki/moesegfault-subscriptions/actions/runs/40065305487) was dispatched at the already reviewed branch source `5c65890` with `delivery=staging-only` to establish the required genuinely staged artifact, not merely a verify-only package. This does not itself apply production audiences/plans. No production administrator recipient/key, grant, or payment-provider configuration has been changed by this step.

Billing source `5c65890f43b6d4fdfc77dd7ff500610c8bef219d` now has a **successful actual staging deployment**, [run 40065305487](https://github.com/kleedaisuki/moesegfault-subscriptions/actions/runs/40065305487), with serving Billing version `b7719d91-0a53-4b1e-97a5-16e03159ee0e` and Subscribe version `295ad88c-8fed-4874-99d0-862c87619f7a`. Official serving capture readback and bounded post-release smoke passed. A separate process using the existing native keyring and real released CLI read the original owned fixture: same account, Pro, limits 200/100/20, available test credit `179938855` microUSD after earlier real task usage. No new grant, login, code issuance, or production account was created for this readback. Production Billing promotion remains pending the main/configuration binding decision above.
