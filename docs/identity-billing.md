# Identity boundary

The dashboard is a confidential OIDC Backend for Frontend (BFF), not a browser OAuth client. The native `magents` client uses a separately registered public client and the system browser. Product users are keyed exclusively by `(issuer, sub)`; no email, username, internal Identity principal or global session claim participates in account mapping.

## Environment pairing

| Setting | Staging | Production |
| --- | --- | --- |
| Issuer | `https://identity-staging.moesegfault.dev` | `https://identity.moesegfault.dev` |
| Dashboard callback | `https://adash-staging.moesegfault.dev/auth/callback` | `https://adash.moesegfault.dev/auth/callback` |
| Dashboard client registration request | `agents-dashboard-staging` | `agents-dashboard` |
| Native client registration request | `magents-staging` | `magents` |
| Native redirect | `http://127.0.0.1/callback`, `native_loopback_any_port` | Independently registered same loopback policy |
| Required Billing sector | `subscribe-staging.moesegfault.dev` | `subscribe.moesegfault.dev` |

The staging clients were provisioned and read back through the reviewed upstream deployment path; `deploy/identity/` contains their non-secret manifests and registration SQL copies. See [deployment onboarding](deployment-onboarding.md) for exact verification evidence and the reviewed shared-sector ownership policy. Production entries remain registration requests, not deployed capability. Never switch to an unrelated sector or join users by username. Production follows staging acceptance and explicit promotion; no staging secret is reused.

Live production discovery was checked on 2026-10-23 and advertised Authorization Code, S256 PKCE, RS256 ID/access JWTs, `private_key_jwt`/`none`, refresh rotation, UserInfo and RP-initiated logout. The application loads discovery instead of hardcoding its endpoints and rejects issuer or endpoint-origin drift. See [Identity integration contract](https://github.com/kleedaisuki/moesegfault-indentity/blob/main/docs/integrating-app.md), [OAuth security BCP](https://www.rfc-editor.org/rfc/rfc9700.html), and [native app BCP](https://www.rfc-editor.org/rfc/rfc8252.html).

## Configuration and token custody

Required bindings are `DB` (application D1) and `AUTH_SESSIONS` (`AuthSession`, SQLite Durable Object). Secrets are `DATA_ENCRYPTION_KEY` (base64url unpadded 32 bytes) and `IDENTITY_CLIENT_PRIVATE_JWK` (registered P-256 ES256 private JWK with `kid`). Variables are `IDENTITY_ISSUER`, `IDENTITY_WEB_CLIENT_ID`, `IDENTITY_NATIVE_CLIENT_ID`, `IDENTITY_REDIRECT_URI`, `IDENTITY_NATIVE_REDIRECT_URI`, `APP_ORIGIN`, and optional `BILLING_API_ORIGIN`.

Only public client IDs, issuer and native redirect are served by `GET /auth/config`. Browser tokens never appear in responses, localStorage, sessionStorage or frontend bundles. D1 stores encrypted token envelopes and BYOK keys. AES-256-GCM includes owner/purpose authenticated data: a copied database value cannot be substituted for another account, provider or session. The envelope has a format byte, a fresh 96-bit nonce and authenticated ciphertext. Back up encryption keys in deployment secret custody; replacing the key without a migration invalidates existing ciphertext. `key_version` identifies BYOK credential replacement, not a supported master-key rotation workflow.

API keys are 256-bit random `mag_` secrets. D1 stores SHA-256 digests, never recoverable API key values. Creation discloses the plaintext once. API keys can use product APIs but cannot mint/revoke management keys, read BYOK secret values, change provider keys or send diagnostics; native human tokens and browser sessions own those management operations.

## Browser lifecycle

1. `GET /auth/login?return_to=/...` creates a five-minute, encrypted, single-use server transaction with independent state, nonce, verifier and browser-binding token. A separate host-only secure login cookie prevents login CSRF with an attacker-created transaction. Local return destinations cannot escape the origin.
2. `GET /auth/callback` atomically consumes the transaction (`DELETE RETURNING`), validates state, response issuer, browser binding and expiry, and exchanges the exact redirect/code/verifier using a fresh ES256 client assertion. Assertions expire in two minutes and have unguessable, single-use JTIs.
3. The ID token must have an RS256 signature from issuer-pinned JWKS, compatible RSA signing metadata, exact issuer and single registered audience, valid time bounds, `token_use=id`, and the original nonce. The app issues a `__Host-magents_session` cookie: Secure, HttpOnly, SameSite=Lax, path `/`, no Domain. The absolute application session lifetime is 14 days.
4. Every authenticated browser request enters the session's `AuthSession` object. `blockConcurrencyWhile` serializes refreshes across all Worker isolates. OAuth `offline_access` material stays encrypted in D1. Before exchanging a rotating refresh token the DO durably writes an in-flight marker; after validating the replacement access token and unchanged principal, it atomically replaces the encrypted token set, then removes the marker. A crash or unknown response never retries the predecessor: the next request ends that application session. This favors a recoverable login over accidental token-family revocation.
5. Browser mutations require the exact application Origin and `X-CSRF-Token`. The anti-CSRF token is returned by authenticated `GET /v1/account`; it is not an OAuth bearer credential.
6. `POST /auth/logout` checks CSRF without refreshing an expired token, terminates the local session and clears its cookie independently of best-effort refresh revocation. This operation does **not** claim Identity SSO logout or connected-app grant revocation. RP-initiated global SSO navigation is not exposed by this initial application UI.

Discovery/JWKS use a five-minute isolate cache; unknown signing keys permit one bounded refresh per 30 seconds, and the refreshed JWKS is authoritative: retiring keys are never carried forward by the consumer; the issuer must publish its intended overlap. Remote key URLs in JWT headers are rejected. Native bearer credentials are accepted only for the exact configured native client audience, `token_use=access`, and `openid` scope; a browser ID token or arbitrary other client's access token cannot authorize the API.

## Subscription and metering

The server calls Billing `GET /v1/me` with the client-bound access token and stores a verified product-specific subscription list and schedules human-token refreshes separately from grant validity. The legacy `valid_until` column is a refresh hint, not authorization expiry. A fresh lookup can be coalesced by the cached snapshot. Only `product_id=agents`, an active time interval and an `agents.tier.go`, `agents.tier.plus` or `agents.tier.pro` entitlement select the corresponding tier; overlapping grants select the strongest tier without adding allowances or limits. A plan display name does not. A failed lookup does not extend a grant beyond its verified period end, but also does not remove a still-active grant from an API-key-only user. Browser logout and snapshot refresh deadlines cannot downgrade paid SDK access. The current Billing contract has no subscription cancellation/revocation operation; any future revocation integration needs an explicit trusted feed. See [billing accounting and registration](billing.md) for canonical Free/Go/Plus/Pro limits, monthly included-credit funding, exact-money ledger invariants and upstream registration gates.

## User data and diagnostics

`accounts`, configurations, sessions, key metadata and Billing snapshots are user/product data. Login transactions, browser sessions and in-flight refresh markers are expiring security state, not conversation archives. Metering ledger entries are immutable accounting records. Diagnostics require explicit per-submission consent and strict, small metadata-only payloads; payloads are isolated in the `DIAGNOSTICS` R2 binding with seven-day retention, while D1 contains only bounded admission/expiry metadata. No tokens, cookies, instructions, prompts or provider errors are telemetry fields.

## Verification

### JavaScript/D1 integer boundary incident

The first real staging authenticated request exposed an ABI error which native Rust and
SQLite-only tests cannot detect. With wasm-bindgen 0.2.129, `i64.into()` and `u64.into()`
construct JavaScript **BigInt**, while Cloudflare D1 accepts JavaScript Number but not
BigInt. A fresh API key had `last_used_at=NULL`, so its first authenticated request took
the timestamp-update path and failed with HTTP 500 before normal product handling.
Invalid keys still correctly returned 401, which made unauthenticated smoke checks
insufficient to reveal the failure. Manually avoiding the timestamp update was only a
diagnostic experiment, not a fix.

The same real-WASM journey also caught a second platform boundary: Fetch's
`Response.redirect()` produces immutable response headers. Appending a BFF login or
callback cookie therefore fails at runtime even though Rust compilation succeeds.
Both BFF redirects now construct a normal mutable empty response with status 302 and
an explicit Location, then append the session/login cookies and security headers.
This follows the [Fetch response-header guard contract](https://fetch.spec.whatwg.org/#dom-response-redirect)
without buffering or changing the redirect's body semantics.

A third boundary is the Rust SDK's alarm argument, not JavaScript's `setAlarm` contract:
in worker-rs 0.8.7, `ScheduledTime::from(i64)` means a **relative offset** in milliseconds.
Passing an epoch timestamp silently adds it to the current epoch again. AuthSession now
constructs an explicit JavaScript Date and `ScheduledTime::new(date)` for its absolute
expiry; it does not infer units from the numeric magnitude. See the pinned
[`ScheduledTime` SDK implementation](https://docs.rs/worker/0.8.7/worker/struct.ScheduledTime.html).
The real-WASM auth journey creates a short-lived browser session, arms the actual DO
alarm through authentication, and observes physical D1 deletion within five seconds.
Request-time expiry rejection alone cannot make that regression pass.

All auth D1 timestamp bindings now use the checked `d1_integer` adapter, which rejects
values outside the exact JavaScript integer range and explicitly constructs `JsValue`
with `from_f64`. Refresh-version increments remain SQLite-side. Money continues to use
decimal-string bindings with SQLite INTEGER casts, never this timestamp adapter.
See [wasm-bindgen number mappings](https://rustwasm.github.io/docs/wasm-bindgen/reference/types/numbers.html)
and [the D1 type-conversion contract](https://developers.cloudflare.com/d1/worker-api/).

`tests/worker/http.test.mjs` runs the actual compiled Rust Worker in Miniflare/workerd
against the real migrations. Its fresh-key test forces `last_used_at=NULL`; the shared
`auth-journey.mjs` extends that boundary coverage to browser-session expiry, key creation
and revocation, BYOK creation/update, OIDC transaction insertion and single-use callback
claim, and local logout. These are WASM ABI tests, not a replacement for a deployed human
OIDC and rotating-refresh acceptance journey. No synthetic key or cookie is logged.

`cargo test -p agents-auth --lib` covers real RS256 signature verification, issuer/audience/time/purpose rejection, callback mix-up and browser-binding defenses, origin-safe return paths, ciphertext integrity and owner/purpose isolation, and exact-money accounting properties. `python -m unittest discover -s tests -p test_billing_sql.py` covers actual SQL transactions/triggers, concurrent budget admission, replay and large integer precision. Worker native/WASM checks verify platform integration. Environment acceptance must additionally demonstrate a human login, concurrent refresh, API-key/BYOK management, expired-session logout and shared-sector subscription recognition against the registered staging clients; unit tests do not substitute for that journey.
