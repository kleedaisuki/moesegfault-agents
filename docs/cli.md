# magents CLI

`magents` is the native Rust client. Start with `magents --help`; every subgroup and operation has its own help. Resource responses are JSON on stdout, assistant text streams to stdout, and session identifiers and failures go to stderr. Failures exit nonzero.

## First journey

```sh
magents auth login
magents models
magents agents create agent.json
magents sessions start AGENT_ID
magents run AGENT_ID 'Investigate the failing tests' --session SESSION_ID
# In another terminal during execution:
magents sessions steer SESSION_ID 'Focus on persistence'
magents sessions show SESSION_ID
magents sessions close SESSION_ID
magents usage
```

Agent create/update accept API configuration JSON from a file or `-` for stdin. The server owns validation and defaults, including context percentage and plan limits. Inspect a configuration with `agents show` before updating. Session close requests server-side close/persistence, not deletion of durable user data.

A run calls `POST /v1/chat/completions` with the agent ID as `model` and optionally `x-agents-session`. The response session header goes to stderr. The maintained `eventsource-stream` parser handles fragmented UTF-8 and SSE framing. Missing `[DONE]` is an error. After interruption, inspect the session rather than blindly replaying prompts. Steering appends new input without replacing pinned instructions. Automatic retries are not performed.

## Native identity boundary

Deployment must provision a reviewed public/native moeSegFault Identity client. The CLI obtains reviewed native registration metadata from the configured API origin at `/auth/config`. To override it for deployment testing, set all three non-secret OIDC environment values together:

- `MAGENTS_OIDC_ISSUER`: exact HTTPS issuer.
- `MAGENTS_OIDC_CLIENT_ID`: actual registered native client ID, token authentication method `none`.
- `MAGENTS_OIDC_REDIRECT_URI`: exact registered HTTP URL on `127.0.0.1` or `[::1]` with a callback path. A portless registration uses the deployment-reviewed native loopback-any-port mode and an ephemeral local listener; an explicit port is used as-is. No localhost, wildcards, credentials, query, or fragment.
- `MAGENTS_API_URL`: optional API origin, default `https://agents.moesegfault.dev`. HTTP is permitted only for literal loopback development origins.

There are no guessed client IDs, dynamic registration, embedded secrets, or cross-environment fallbacks. The API must explicitly accept the registered native audience: Identity currently sets access-token `aud` to `client_id`, not a separate API audience. Coordinate pairwise-subject sectors between dashboard and CLI registrations.

The client uses `openidconnect` discovery and Authorization Code with S256 PKCE in the system browser. It generates fresh state/nonce and checks exact discovery and authorization-response issuers, callback path/state, RS256 ID-token signature, issuer/audience/expiry/nonce, and access-token hash when present. The loopback listener expires after three minutes. The static non-cacheable browser completion page reports success only after protected persistence and reflects no callback data.

`magents auth login --no-browser` prints the authorization URL on stdout instead of launching the system browser. Navigate to it manually in a browser you control; the same loopback listener, PKCE exchange, issuer validation, and three-minute timeout still apply. The URL contains the normal authorization transaction parameters, not the PKCE verifier, authorization code, or token, and is never uploaded as diagnostics. On SSH, the browser must be able to reach the CLI's exact loopback redirect (for example through an appropriate port-forward); the flag does not introduce a remote callback or device-code grant. Default login behavior is unchanged. Discover the flag with `magents auth login --help`.

Credentials use service `dev.moesegfault.agents` and the exact API origin in Windows Credential Manager, macOS Keychain, or Linux Secret Service (`keyring` v4's `v1` API). There is no plaintext fallback and no ordinary token/config file. Login requests `openid profile offline_access`. The access/refresh pair is replaced in one credential-store write. A cross-process advisory lock in the platform application-data directory serializes read, refresh, and replacement across CLI invocations; this lock file contains no secrets. The predecessor is removed before dispatching a refresh, so an ambiguous exchange or failed persistence cannot later reuse it and revoke the refresh family. Such failures require `auth login`. A missing or unchanged rotated refresh token is rejected. Older saved logins without refresh tokens remain readable and request login at expiry.

`auth logout` removes the local login and refresh credential only, not Identity SSO or the provider grant. `auth status` checks local expiry, not fresh server authorization. Deployments without `offline_access` grants can still log in but must reauthenticate at access-token expiry.

Headless automation can supply `MAGENTS_API_KEY` from a secret manager. It is never a command argument or written to config. It overrides interactive credentials; logout does not mutate the environment.

## Management and privacy

### Personalized skills

```sh
magents auth login
magents skills list
magents skills show review-rust
magents skills create skill.json
magents skills update review-rust update.json
magents skills delete review-rust
```

Creation JSON is `{"name":"review-rust","instructions":"Check ownership and compatibility."}`; update JSON is `{"instructions":"Also inspect session pinning."}`. Both commands accept `-` instead of a filename to read JSON from stdin. `magents skills create --help` and `update --help` reveal their payloads without login or network requests.

Names contain 1–64 ASCII lowercase letters, digits, `_`, or `-`; instructions contain 1–49152 UTF-8 bytes and cannot be whitespace-only. The server validates these limits and computes the version digest; do not submit `version`, tool definitions, or extra fields. Listing merges deployment defaults with account overrides; each item reports `source`. To customize a deployment-only skill, create an account skill with that name instead of updating the deployment entry. Deleting an override exposes the deployment default again when one exists.

Registry operations require the human OIDC login, just like provider-key management; API keys do not grant this authority. If `MAGENTS_API_KEY` is set, unset it before these commands so the CLI uses your protected account login. Select registered skills through the agent configuration's `skills` name array. New sessions resolve and pin full skill content and its digest on the server; editing or deleting a registry entry never rewrites existing session prefixes. Close and start a new session to use changed content.

### Account, keys, and diagnostics

```sh
magents account
magents keys list
magents keys create automation
magents keys delete KEY_ID
magents byok list
magents byok set deepseek
magents byok delete deepseek
magents diagnostics
magents diagnostics --upload --consent
```

Key creation intentionally prints the one-time server response; redirect only into protected storage. BYOK uses a hidden terminal prompt, never flags. API failures include HTTP status and `x-moesegfault-correlation-id`, never upstream bodies that might echo sensitive content. Diagnostics contains only consent, client name/version, and OS-architecture platform: no prompts, sessions, credentials, environment dumps, paths, or user identifiers. Upload requires both flags and authentication; preview is offline. Authorization, encryption, subscription limits, and retention remain server responsibilities.

## Verification

`cargo test -p magents` exercises the real binary against a loopback HTTP fixture through create, start, streamed run, steer, and close, checking authorization and session headers. The personalized skill lifecycle covers stdin creation, list/show, file-based update, and deletion; help is checked without authentication. Tests also cover offline diagnostics, upload consent, sensitive error suppression, truncated streams, origin boundaries, callback confusion/duplicate fields, and Clap command consistency. A registered Identity deployment and OS credential store are needed for the final real-browser login check; tests do not replace it.

The opt-in `manual_login_real_binary_receives_a_validated_loopback_denial` fixture runs the actual CLI with `--no-browser`, fetches public deployment configuration/discovery, checks the emitted PKCE authorization URL, then sends a matching-state/issuer denial to the live loopback listener. It does not navigate the authorization URL, open a browser, obtain tokens, or change an account. Set `MAGENTS_TEST_NATIVE_LOGIN_API_URL` to an approved API origin and run `cargo test --locked -p magents --test native_login manual_login_real_binary_receives_a_validated_loopback_denial -- --ignored --exact`. Unit tests separately prove that manual presentation never invokes the browser opener and default presentation still does.

References: [openidconnect](https://docs.rs/openidconnect/latest/openidconnect/), [keyring](https://docs.rs/keyring/latest/keyring/v1/index.html), and [RFC 8252](https://www.rfc-editor.org/rfc/rfc8252).
