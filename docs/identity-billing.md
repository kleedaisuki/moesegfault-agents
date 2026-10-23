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

### Real staging account preparation, 2026-10-24

The public Identity registration UI was exercised with a fresh, task-owned Chromium
profile, not an existing human browser session. Password registration is available;
Passkey enrollment is optional. Random test credentials and browser state remain in an
ACL-protected, ignored `.temp/identity-acceptance-*` directory, readable only by the
current Windows user and SYSTEM. No email address, OTP, password or browser token was
printed or included in this document.

Three existing, clearly agent/test-owned aliases were attempted, with no further alias
enumeration. For each, the public UI accepted the registration email request with 201,
the narrowly scoped amail retrieval delivered the intended verification message, and
the public UI verified its code with 200. Password registration then returned 409
`identifier_conflict`. A read-only staging query scoped solely to these attempted
identities returned `username_exists=0`, `email_any_exists=1`, and
`email_verified_exists=1` for all three. Thus these aliases already belong to verified
Identity accounts; the new random accounts were not created. This is not evidence of
a registration transaction defect. The handler's broad constraint-to-conflict mapping
was reviewed rather than assuming the machine error alone proved email ownership.

One separately authorized attempt to provision a dedicated new amail alias was rejected
with 409 `address_limit`. No retry with another new alias, allowance increase,
subscription activation, spending authorization, account reset, provider-control-plane
change or Identity database write was performed. Existing accounts were not impersonated.

Those initial alias attempts did not complete Agents sign-in and did not justify
resetting an existing account. The later dedicated staging-fixture journey below
resolved the mailbox constraint without increasing production amail limits or requiring
an unrelated account. Passwords were not requested in chat.

### Real staging BFF and native lifecycle acceptance, 2026-10-24

The existing, narrowly allowlisted `identity-test-inbox` operational fixture provided
an unused staging registration address. Its deployed recipient allowlist, private R2
binding and bounded verification-object retention were checked using its official
management scripts, copied only into this project's private `.temp` tree. No catch-all
was enabled and no production amail address quota, subscription or spending limit was
changed. Normal public Identity UI created the account after actual mail delivery and
code verification; no credential, principal or grant was inserted directly into D1.

The first controlled route attempt illustrated a documented upstream operational
constraint: Email Routing rule readback does not establish data-plane readiness.
The existing acceptance helper explicitly waits 60 seconds because immediate sends
can later fail as `routing_unknown_address`. After applying that settle interval and
auditing the exact route again, a fresh bounded UI verification attempt succeeded.
The exact route was removed before submitting the delivered code, and only the owned
verification MIME object was deleted after verification. A provider-accepted outbox
row alone was never treated as proof of delivery.

| Real deployed journey | Observed result |
| --- | --- |
| Public registration email request / mailbox code completion / password registration | 201 / actual private MIME received / 200 / 201 |
| Normal BFF authorization callback and authenticated account read | Dashboard reached through Identity SSO; account 200 |
| Actual initial entitlement | Free, test billing mode, 1,000,000 microUSD included credit; 2 configurations, 2 live sessions, 1 concurrent run |
| Free configuration admission | First two creations succeeded; third returned 429 `agent_limit`; temporary configurations were deleted |
| Management key custody | Creation disclosed a key once; metadata list contained neither plaintext key nor digest |
| OpenAI SDK using the new user's API key | `models.list()` succeeded before and after browser logout |
| Real BFF refresh window | Session `refresh_version` increased from the initial zero; included credit was not granted again |
| Real native refresh window | `auth status` first reported expired; protected account read refreshed successfully; another process reported logged in |
| Browser logout | Browser account read became 401; native client and separately issued API key remained usable |
| Explicit API-key revocation | Native management deletion succeeded; subsequent OpenAI SDK request returned 401 |

Native acceptance was first exercised with the new no-browser option in a debug build,
then repeated with the **published Windows CLI** from source `928c533` after root
verified its artifact digest and BUILDINFO (package run `39993274170`). The published
binary completed the real Authorization Code + S256 PKCE exchange using its normal
temporary loopback callback, persisted credentials through the OS credential store,
and passed `auth status` plus protected `account` from separate processes. Its account
ID was compared privately with the BFF account and matched, exercising the deliberate
shared subject-sector mapping across both registered clients.

The no-browser authorization URL was carried only through a private process pipe into
the task's isolated browser; it was not logged publicly, and no existing human browser
profile or amail OAuth token was reused as an Agents credential. The Identity session
belonged to the newly created staging fixture. Browser OAuth-token custody is enforced
by the BFF implementation; these results do not claim an independent browser-memory
forensics audit. Durable metadata and secret-bearing acceptance artifacts remain
separated: only the sanitized outcomes above belong in repository knowledge.
