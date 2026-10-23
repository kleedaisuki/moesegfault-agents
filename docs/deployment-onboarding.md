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

The staging-only workflow builds and verifies immutable artifacts, then deploys through the repository's existing release script. The linked run completed successfully on 2026-10-23 after retrying only its failed deployment job: a transient Cloudflare D1 API internal error 7429 interrupted the first attempt before Worker release. The retry reused the same verified immutable artifact; all deployment/smoke jobs succeeded and production promotion was skipped. Do not merge the Identity branch to main just to apply staging registrations: its main push workflow also promotes production code. A future production onboarding requires separately reviewed production registrations, keys, audiences, callbacks, and acceptance tests.


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
