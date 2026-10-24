# Identity boundary

The dashboard is a confidential OIDC Backend for Frontend (BFF), not a browser OAuth client. The native `magents` client uses a separately registered public client and the system browser. Product users are keyed exclusively by `(issuer, sub)`; no email, username, internal Identity principal or global session claim participates in account mapping.

## Environment pairing

| Setting | Staging | Production |
| --- | --- | --- |
| Issuer | `https://identity-staging.moesegfault.dev` | `https://identity.moesegfault.dev` |
| Dashboard callback | `https://adash-staging.moesegfault.dev/auth/callback` | `https://adash.moesegfault.dev/auth/callback` |
| Dashboard client | `agents-dashboard-staging` | `agents-dashboard` |
| Native client | `magents-staging` | `magents` |
| Native redirect | `http://127.0.0.1/callback`, `native_loopback_any_port` | Independently registered same loopback policy |
| Required Billing sector | `subscribe-staging.moesegfault.dev` | `subscribe.moesegfault.dev` |

Both environments' clients were provisioned through the reviewed upstream deployment path; `deploy/identity/` contains non-secret manifests and registration SQL copies. See [deployment onboarding](deployment-onboarding.md) for verification evidence and the reviewed shared-sector ownership policy, and the production protocol acceptance below for the real BFF/native journey. Never switch to an unrelated sector or join users by username. Production followed staging acceptance and explicit promotion; no staging secret is reused.

Live production discovery was checked on 2026-10-23 and advertised Authorization Code, S256 PKCE, RS256 ID/access JWTs, `private_key_jwt`/`none`, refresh rotation, UserInfo and RP-initiated logout. The application loads discovery instead of hardcoding its endpoints and rejects issuer or endpoint-origin drift. See [Identity integration contract](https://github.com/kleedaisuki/moesegfault-indentity/blob/main/docs/integrating-app.md), [OAuth security BCP](https://www.rfc-editor.org/rfc/rfc9700.html), and [native app BCP](https://www.rfc-editor.org/rfc/rfc8252.html).

## Configuration and token custody

Required bindings are `DB` (application D1) and `AUTH_SESSIONS` (`AuthSession`, SQLite Durable Object). Secrets are `DATA_ENCRYPTION_KEY` (base64url unpadded 32 bytes) and `IDENTITY_CLIENT_PRIVATE_JWK` (registered P-256 ES256 private JWK with `kid`). Variables are `IDENTITY_ISSUER`, `IDENTITY_WEB_CLIENT_ID`, `IDENTITY_NATIVE_CLIENT_ID`, `IDENTITY_REDIRECT_URI`, `IDENTITY_NATIVE_REDIRECT_URI`, `APP_ORIGIN`, and optional `BILLING_API_ORIGIN`.

Only public client IDs, issuer and native redirect are served by `GET /auth/config`. Browser tokens never appear in responses, localStorage, sessionStorage or frontend bundles. D1 stores encrypted token envelopes and BYOK keys. AES-256-GCM includes owner/purpose authenticated data: a copied database value cannot be substituted for another account, provider or session. The envelope has a format byte, a fresh 96-bit nonce and authenticated ciphertext. Back up encryption keys in deployment secret custody; replacing the key without a migration invalidates existing ciphertext. `key_version` identifies BYOK credential replacement, not a supported master-key rotation workflow.

API keys are 256-bit random `mag_` secrets. D1 stores SHA-256 digests, never recoverable API key values. Creation discloses the plaintext once. API keys can use product APIs but cannot mint/revoke management keys, read BYOK secret values, change provider keys or send diagnostics; native human tokens and browser sessions own those management operations.

### Native confidential-client signing boundary

`auth_crypto.rs` performs private ES256 operations with native Worker Web Crypto:
import the existing P-256 JWK as non-extractable and sign-only, then use ECDSA/SHA-256.
`agents-auth::prepare_client_assertion` only validates metadata and frames the JWT;
assembly requires the 64-byte IEEE-P1363 `r || s` signature. The registered JWK,
ES256/kid, `iss=sub=client_id`, token-endpoint audience, two-minute expiry and fresh
JTI are unchanged. Import/sign failures fail closed, with no Rust private-scalar,
PKCS#8 or software-signing fallback. The original JWK remains a deployment secret;
this is a private-arithmetic boundary, not a claim of hardware key isolation.

This scoped mitigation follows the timing-variability advisory
[RUSTSEC-2026-0387](https://github.com/RustSec/advisory-db/blob/main/crates/crypto-bigint/RUSTSEC-2026-0387.md),
the [Worker Web Crypto API](https://developers.cloudflare.com/workers/runtime-apis/web-crypto/)
and [W3C ECDSA operations](https://www.w3.org/TR/webcrypto/#ecdsa-operations).
Direct `p256` is test-only; transitive `crypto-bigint` remains in public verification
dependencies. This does not claim an advisory-free lockfile or change issuer, client,
token purpose, registration or key rotation policy.

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

### Authoritative paid allowance correction: deployed acceptance, 2026-10-24

The same independently registered staging fixture subsequently redeemed an authorized
synthetic Agents Pro capability through Subscribe's normal UI. This was not a payment
or a direct database grant. The original calendar-pinned allocation exposed
48,744,159 microUSD rather than the full verified paid period's 180,000,000 microUSD.
That earlier observed balance is historical evidence, not the intended Pro allowance.

After public `/healthz` reported exact build
`a918c6730cf95fcb66e3ec360b4d68f64eb091f8`, the published Windows CLI from source
`928c533` was exercised again against staging, using its existing OS-keyring credential
and the same fixture. No registration, capability issuance, subscription redemption or
financial database mutation was performed during this correction acceptance.

| Boundary exercised | Observed result |
| --- | --- |
| Existing native credential lifecycle | `auth status` initially reported `Login expired`; normal protected `account` refreshed it, and a separate process then reported `Logged in` |
| Three consecutive normal account reads | Same privately compared account; Pro, test billing mode, 180,000,000 microUSD available each time |
| Pro admission policy | 200 configurations, 100 concurrent sessions, 20 concurrent runs |
| Owned-account, read-only wallet inspection | Budget 180,000,000; spent 0; reserved 0 microUSD, matching the API's available balance |
| Immutable replacement provenance | Exactly one replacement: previous allocation 48,744,159, delta 131,255,841, target 180,000,000 microUSD |
| Verified paid validity | Replacement paid anchor and wallet expiry matched the active product-scoped Pro snapshot: 2026-10-23 17:33:02 UTC through 2026-11-22 17:33:02 UTC, exactly 30 days |

The replacement preserves the existing funding-window identity and its spend/hold
provenance while replacing the legacy Free calendar anchor with the authoritative paid
period. It does not stack a second monthly allowance. Repeated protected account reads
and the ordinary native rotating-refresh path neither added another replacement nor
increased the balance. These observations used actual deployed Rust Worker/D1 and the
released CLI, not a mocked account response. Inspection was restricted to this owned
account's wallet, replacement journal and verified subscription snapshot; credential
material, principal identifiers and raw subscription records remain private.

### Real-clock inactivity acceptance, 2026-10-24

Build `a918c6730cf95fcb66e3ec360b4d68f64eb091f8` was exercised through the published
CLI and official OpenAI SDK, using the same isolated Pro fixture. The CLI issued a
task-specific API key; two temporary configurations and workspaces were created.
No clock, Durable Object state, credential or financial record was edited. Reads
sampled the real deployed sessions approximately every ten seconds. After the sole
accepted input, both `last_input_at_ms` and `input_revision` remained unchanged by
those GET requests.

**Polled idle workspace: passed.** One real bash call wrote a unique marker and returned
exit code 0 in 35 ms. No further input was appended. The stored close timestamp was
605.399 seconds after that input; the first polling observation of Closed was at
616.24 seconds. Thus the ten-minute deadline initiated archival, with checkpoint
and release finishing several seconds later, rather than GET traffic renewing it.
This exercised the application idle deadline under read traffic, **not fully
unattended persistence**: polling can keep the Durable Object active and mask the
platform's independent container inactivity-stop behavior. A follow-up must leave
the workspace entirely unrequested until after the deadline, with the native
container inactivity timeout explicitly configured longer than the session window.

The archived workspace had generation 1 and a 365-byte compressed archive. Its actual
private R2 manifest and archive were downloaded, and their generation, byte count and
SHA-256 matched the session snapshot. The conversation archive was also retrieved.
A read-only Cloudflare Containers query matched this exact owned Durable Object's
instance and reported `inactive`. All its reservations were settled or released, with
no held balance or pending resource observations. Its first container interval was
594,368 wall-ms, charged 5,944 microUSD; the initial model journal contained 1,373
tokens across two provider calls, charged 283 microUSD.

Normal explicit resume preserved the pin. A **new actual bash tool result**, not an
assistant recollection, read the restored marker with exit code 0 in 19 ms. The session
was then explicitly closed; its container again reported inactive. Including this
resume/restore cycle, the idle fixture's final recorded charge was 6,774 microUSD,
with all reservations finalized.

**Task continuing beyond ten minutes: not yet passed.** The other fixture requested
exactly three sequential bash calls sleeping 240, 240 and 150 seconds, each with
`timeout_ms=270000`; SDK timeout was 1,000 seconds and automatic retries were disabled.
The model emitted the correct first bash call. It was present in the journal by
12.85 seconds, but between the 25.22- and 37.06-second observations the run ended
with `workspace_unavailable`, `execution_status=unknown`, and no confirmed tool result.
The container record was inactive without evidence of a successful process start.
Consequently neither 630 seconds of command execution nor continued Running state
after the idle deadline was demonstrated. This is a workspace-start/transport incident
for diagnosis, not evidence that the model ignored the requested sequence or that
long-task inactivity semantics passed.

There was no retry, appended recovery command or forced close of that uncertain task.
It later reported logical Closed at input plus 601.187 seconds and persisted its
conversation, but had no workspace snapshot. Its flags still showed
`checkpoint_pending=true`, `workspace_checkpoint_pending`, and
`workspace_container_exit_before_checkpoint`: logical Closed alone is **not** proof
that archival or cleanup completed. Its unresolved container observation and
38,018-microUSD financial hold were deliberately left intact, not released by the
test. No DELETE was sent to that session. Read-only diagnostic evidence was handed
to the runtime owner. Both temporary configurations were subsequently soft-deleted
after observing closed, non-running sessions; the uncertain session's retained
checkpoint journal and hold still require runtime reconciliation. The
task-specific API key was revoked through the released CLI and an API request with it
then returned 401. Five model calls were recorded across both fixtures, totaling
3,925 tokens; total posted charges were 7,085 microUSD at inspection, plus the preserved
uncertain hold, within the authorized synthetic test budget. No production state or
existing smoke-test tenant was touched.

### Unattended idle and long-task acceptance, 2026-10-24

The follow-up used public build
`164d0aec3ea9404ef9b94f58847d775caacaa46e`, with the native container inactivity
timeout configured to 15 minutes while the application session deadline remained
10 minutes. A released-CLI task key and two new owned sessions were used; the failed
earlier command was not replayed. The short-workspace task **C** had to produce an
actual successful tool result before the independent long task **D** was started.

**C: unattended preservation passed.** Its initial write returned actual marker
stdout and exit code 0. An initial journal read fixed the accepted input timestamp
and revision. After that verification, no HTTP, CLI, Durable Object, container or
financial reads or writes targeted C until its first deadline read, at input plus
637.728 seconds. That response was Closed with a snapshot and no checkpoint or
billing pending flags. Independently, a teammate subscribed only to the global
Cloudflare tail and observed checkpoint at 05:05:21.262 UTC and archive at
05:05:22.707 UTC, the latter input plus **603.880 seconds**. Tail collection made no
session request and read no credential, so it did not keep the workspace active.

The generation-1, 364-byte workspace archive and its actual R2 manifest were fetched
and checked against the recorded SHA-256, generation and size; the conversation
archive was fetched too. The exact owned container instance was inactive. Normal
explicit resume preserved the pinned configuration, and a new actual bash result
read the saved marker with exit code 0 in 18 ms. Explicit close then succeeded with
no checkpoint/billing pending flags. Including restoration, all C reservations were
settled or released and its recorded total was 6,428 microUSD.

**D: work survives both the idle deadline and client disconnect.** A single user
turn requested three sequential bash calls with sleeps of 240, 240 and 150 seconds;
each actual call used `timeout_ms=270000`. There was no request to D between dispatch
and its first status read at accepted input plus **619.267 seconds**. It remained
Running with the original input timestamp and revision 1. Later reads, after that
deadline, inspected progress without appending input or restarting the task.

All three actual tool results were subsequently verified: exit codes 0, durations
**240,043 / 240,004 / 150,006 ms**, no timeout/cancellation, and the final tool's stdout
equal to the workspace marker. The global tail observed D's checkpoint at
05:08:40.551 UTC, followed by archive and successful `run.completed` together at
05:08:41.826 UTC, input plus **785.227 seconds**. Thus work was not cancelled at ten
minutes, and completion immediately archived the already-inactive session rather
than starting another ten-minute interval. The resulting generation-1, 366-byte R2
workspace archive and conversation archive were retrieved; the compressed digest,
generation and size matched. Final API flags had no checkpoint/billing pending
state, the exact container instance reported stopped, and all D reservations were
settled or released. D's recorded total was 9,629 microUSD, including container,
model, Worker, Durable Object, D1 and R2-operation entries. C plus D totaled 16,057
microUSD at inspection, without an unresolved hold in either new fixture.

There is a separate transport qualification: the non-streaming official SDK request
reported `APIConnectionError`, despite its configured 1,000-second timeout and zero
retries. Only the exception class was retained by this harness, not its occurrence
time, cause chain or errno; no proxy/server cause is inferred. The backend task
nevertheless completed its original intent without a second POST. This verifies
durable execution across a client disconnect, **not** a successful end-to-end
non-streaming response through every intermediary. A bounded streamed-tool journey
must separately exercise standard SSE keepalive and final SDK parsing.

The older never-ready session was also revisited through its normal public API under
this build: one explicit close returned 200, cleared checkpoint pending and its
checkpoint error, and retained `billing_pending=true` with
`resource_reconciliation_pending`. The original command was not replayed and no
uncertain financial hold was refunded or directly altered.

### Stream keepalive and final fixture cleanup, 2026-10-24

After public health identified exact build
`8caff37333cf148a62c804e75cc9189203e8f167`, one new streamed task was issued through
the official OpenAI SDK. Its sole requested bash call slept for 150 seconds and then
printed a unique marker. The HTTP read timeout was **45 seconds**, with
`max_retries=0`; thus a silent 150-second interval could not pass merely by using a
large client timeout. A private read-only byte-stream tee observed the same response
that the SDK parsed, without issuing a second request.

The request completed normally in **161.785 seconds**. Ten standard SSE comments
arrived at approximately 15-second intervals, the SDK parsed 20 chunks and a final
`finish_reason=stop`, and the wire contained `[DONE]`. The journal had exactly one
actual bash call and result: **150,020 ms**, exit code 0, no timeout, and stdout equal
to the marker. This separately closes the streamed-transport qualification above;
it does not retroactively attribute the older non-streaming disconnect to a specific
intermediary. The session was normally closed, and all its reservations finalized;
the recorded model/resource total was 2,107 microUSD.

The same task-owned account also completed a headless dashboard check without another
model call. Its authenticated API returned four `available_models` and two
`available_platform_models`, both OpenRouter. The actual New agent dialog offered
OpenRouter and BYOK as enabled providers and two platform model suggestions, rather
than advertising the unconfigured platform DeepSeek route. The isolated browser was
closed afterward.

C, D and the streamed session were confirmed closed; all three temporary agent
configurations were soft-deleted and absent from the normal owner listing. The
published CLI revoked the task-specific API key, and using that key afterward
returned 401. A native refresh that required reauthentication was not replayed:
the same fixture completed a fresh official PKCE login through its isolated SSO
profile before the final CLI revocation. No human browser profile was reused.
Read-only financial inspection found no held reservations or pending resource
observations in these three fixtures. Their combined recorded amount was 18,185
microUSD at the final inspection, including 21 microUSD of subsequently metered R2
retention. The older uncertain startup hold remains separate and untouched.

### Production protocol-acceptance preparation: read-only gate

Before any production application acceptance, exact-identifier existence queries were
restricted to the two explicitly managed test-inbox recipients. Both already existed
as verified email identifiers in production Identity. Only existence booleans were
read; neither principal data nor credentials were queried. No OTP was sent and no
production account, password, subscription, allowance or email route was changed.

This task has not established ownership of a dedicated production credential for
either existing identity. Its staging password and OAuth credentials must not be
tried against production, and an unrelated existing account must not be reset.
The current private test-inbox configuration and operational checks are explicitly
staging-only, so they are not automatically a production verification-mail contract.
A production journey remains gated on an established agent-owned production test
credential or a reviewed, exact-recipient production-appropriate mailbox for a new
independent Free account, followed by explicit application-live clearance. A new
production acceptance must use a distinct password, browser profile and issuer/client
registrations; it must not acquire Pro or alter any existing account's quota.

A subsequently authorized read-only check covered exactly the three clearly
agent/test-owned amail aliases already used by this task, not other addresses.
All three were absent from production Identity, so one existing alias was selected
for a new, independent Free acceptance account. A distinct production password was
prepared privately; no signup or OTP request has occurred. The proposed additional
mail receiver was cancelled before any resource was created.

The existing amail session needs attention before that signup: local status still
reported authenticated, but its access expiry was past, and the first ordinary
read-only address listing failed during refresh. It was not retried. Normal human
amail browser reauthorization is required before relying on it to retrieve the new
Identity OTP; neither a task-owned test credential nor a human browser profile can
be substituted for that mailbox owner's authorization. No address, budget, quota or
production account was changed while preparing this path.

### Production protocol acceptance: completed, 2026-10-24

After production app-live clearance and exact public health build
`8caff37333cf148a62c804e75cc9189203e8f167`, a **new independently owned Free account**
was created through the ordinary public Identity UI. The old amail authorization
remained unavailable; it was not refreshed repeatedly, impersonated, or used through
a human browser profile. Instead, the explicitly approved fallback created a new
production-purpose, exact-recipient mail receiver, completely separate from the
staging-only fixture and every existing amail alias.

That temporary receiver used the Rust Worker SDK, a new private R2 bucket and one
literal Email Routing rule on the already configured owner domain. It enforced the
exact random recipient, an advertised and streamed 64-KiB cap, complete-message
collection and atomic first-write-only storage at `verification.eml`. HTTP publication,
logs and traces were disabled; R2 public endpoints were disabled and a one-day expiry
was configured as a backstop. The actual compiled-WASM/R2 checks covered recipient,
oversize, truncated-stream and duplicate-write rejection before deployment. No
catch-all, MX, staging receiver or existing user route was changed.

The sole public email-transaction request returned 201. Actual inbound delivery
produced a 6,670-byte MIME object, retrieved privately using the official R2 CLI and
parsed with Python's standard MIME parser. Its exact recipient and expected sender
matched, and one verification code was extracted without printing it. The owned
route was removed before submitting the code. Public verification returned 200; the
remote raw-mail object was deleted, and public password registration returned 201.
No sender outbox/body, direct principal insert, synthetic JWT or administrator grant
substituted for delivery and authentication.

| Production user journey | Observed result |
| --- | --- |
| Normal BFF callback | Authenticated as the new account; Free, live mode, 1,000,000 microUSD; limits 2 configurations / 2 sessions / 1 run |
| Published native CLI `928c533` | Official production PKCE + loopback and OS credential store; separate-process account matched the BFF privately; credit still 1,000,000 |
| Model metadata | Four registered models; two platform-funded models, both OpenRouter |
| One agent / one session / three sequential user turns | Actual time tool; actual bash marker write; close; explicit resume; a new actual bash `cat` returned the saved marker with exit code 0 |
| Workspace provenance | Generation 1, 378-byte archive; actual R2 manifest, compressed archive and conversation archive downloaded; digest, generation and size matched |
| Final closure | No checkpoint or billing pending flags; exact owned container instance inactive; all reservations settled or released |
| Model/resource accounting | Six provider calls, 4,871 tokens, 370 microUSD model charge; container, Worker, Durable Object, D1 and R2-operation ledger entries present |
| Free funding invariant | Exactly one 1,000,000-microUSD grant; final inspected spend 858 microUSD, held 0, available 999,142 microUSD |
| Disposable API access | Agent soft-deleted; released CLI revoked the key; official OpenAI SDK then returned 401 |

The account is intentionally preserved for future operator acceptance. Its independent
production password, username, email, fixed issuer/client identifiers and purpose are
held only in the ignored `.secrets/production/acceptance-account.json`, whose ACL was
verified as limited to the current Windows user and SYSTEM. Temporary plaintext
password copies and local raw verification mail were removed; native OAuth credentials
remain in the OS credential store. No values from that secret record belong in docs,
Git, Docker build contexts, telemetry or agent messages.

Finally, the unique receiver Worker and private bucket were deleted and their absence
verified, along with the already removed exact route and fixed mail object. Only
ownership-receipted resources created for this one acceptance were removed. Existing
production accounts, passwords, subscriptions, quotas and amail authorization were
not changed; no Pro grant, payment or cross-user test was performed.

### Diagnostic consent: real authenticated HTTP acceptance

The existing owned staging account's normal BFF cookie and account-provided CSRF
token were used for exactly three diagnostic POSTs. Its headless browser profile
was task-owned, not a human profile. The submitted metadata was limited to synthetic
client/version/platform/error-code identifiers and a random request UUID; no model
call, new key, email operation, raw log or actual path was involved.

| Request | HTTP result | Owned-account report count |
| --- | --- | --- |
| Valid bounded metadata, `consent=false` | 400 `invalid_diagnostic` | Unchanged at 0 |
| Same metadata, `consent=true` | 202, `accepted=true`, `retention_days=7` | Exactly 1 |
| Metadata plus unsupported `raw_path` field, with no real path value | 400 `invalid_diagnostic` | Remained 1 |

Only the accepted owned report was inspected. Its D1 row contained the empty JSON
payload `{}` and a 604,800-second expiry interval. Its actual object was retrieved
from the dedicated DIAGNOSTICS bucket, not a workspace/conversation bucket. The
submission's complete key set and values matched the synthetic allowlisted request;
the remaining fields were only the expected server report/owner identifiers,
schema version and receipt/expiry timestamps. The object expiry interval was also
exactly seven days. JSON equality was checked semantically, without treating object
key order as payload meaning. No additional submission was made for that local
validation correction.

The synthetic report was left to normal retention, with no database deletion or
seven-day clock manipulation. This verifies explicit-consent admission, strict
field rejection and separate storage; physical expiry was not simulated. The
deployment owner's separate API-key probe returned 403
`human_authentication_required`, which remains the intended management boundary,
not a reason to grant API keys diagnostic-report privileges. The browser was closed
afterward and production account/billing state was not touched.

### Public release asset: final production account check

The final check used the anonymously downloaded **public `v0.1.0` Windows release**,
not the earlier `928c533` CI artifact. Its published BUILDINFO/digest were independently
verified by the delivery owner against source
`8caff37333cf148a62c804e75cc9189203e8f167` and `refs/tags/v0.1.0`.

`magents --version` reported 0.1.0. Using the existing production OS-keyring login,
offline status initially reported expired; one normal `account` request refreshed
successfully and returned the same owned principal, Free and live billing mode.
Read-only inspection of that owned account still found exactly one included-credit
grant totaling 1,000,000 microUSD. Current available credit was 999,126 microUSD,
consistent with ongoing retained-storage metering rather than a new login grant.
There was no registration, model/mail call, new key, password/token inspection,
deployment or second account request in this final asset check.

### Native-signer staging protocol regression

After public health matched server build
`a2059d550c8b0651df740428554e93fcb2bf04dd`, the existing owned staging profile exercised
expired-access refresh, a fresh public authorization callback and normal CSRF logout.
Refresh returned 200 for the same principal and increased the stored rotation version.
The callback returned 302 and a 200 Pro/test account response, with a newly recorded
BFF session. Logout returned 200, removed that local session, and a following account
read returned 401.

An independent, bounded global tail captured actual Identity upstream refresh,
authorization-code exchange and revocation **all returning 200**, with no failed
provider event. Thus remote revocation success was observed separately from the
application's best-effort local logout response. The test used no new account, key,
mailbox, model request or Billing activation, and closed its headless owned profile.
Production subsequently passed its separate deployment clearance and regression below.

### Native-signer production protocol regression

After independent promotion clearance and exact public health build
`a2059d550c8b0651df740428554e93fcb2bf04dd`, the existing owned production profile
completed expired-access refresh with HTTP 200 and an increased stored rotation
version. A fresh authorization callback returned 302 and a 200 account response for
the same Free/live principal, with a new BFF session recorded. Normal CSRF logout
returned 200, removed that local session, and the following account read returned 401.

The independent production global-tail collector observed Identity refresh,
authorization-code exchange and remote revocation each returning 200 during this
coordinated window; it stopped at 08:20:22 UTC. Safe receipts are in
`.cache/telemetry/a2059d55-production-identity-{events,watch}.json`. These observations
support this owned-account protocol regression: they are bounded phase/status
evidence, not a token inspection, Identity SSO logout claim or proof of all possible
authentication paths. In particular, remote revocation was observed independently,
not inferred from the application's best-effort local logout response.

The headless owned browser was closed. No account, password, registered JWK, issuer,
client, API key or Billing grant was created or rotated; no mail or model request
was made. This was a server-only signer replacement. The published immutable
`magents v0.1.0` client release was unchanged.
