# magents CLI

`magents` is the native Rust client. Start with `magents --help`; every subgroup and operation has its own help. Resource responses are JSON on stdout, assistant text streams to stdout, and session identifiers and failures go to stderr. Failures exit nonzero.

## First journey

```sh
magents auth login
magents models
magents agents init
magents agents validate agent.toml
magents agents publish agent.toml
magents sessions start AGENT_ID
magents run AGENT_ID 'Investigate the failing tests' --session SESSION_ID
# In another terminal during execution:
magents sessions steer SESSION_ID 'Focus on persistence'
magents sessions show SESSION_ID
magents sessions close SESSION_ID
magents usage
```

Publication creates a callable agent in **your own account**, not a public marketplace entry. The returned JSON contains its ID. `agents publish agent.toml --agent AGENT_ID` replaces that owned configuration; it does not mutate existing session pins. Legacy `agents create agent.json` and `agents update AGENT_ID update.json` retain their JSON and PATCH semantics. Both JSON and TOML input accept `-` for stdin and are bounded to 1 MiB. The server remains authoritative for registered tools/skills, model capabilities, owned credentials, prices and plan limits. Session close requests checkpoint/persistence, not deletion of durable user data.

## Portable agent TOML

`agents init [FILE]` writes a complete template offline (`agent.toml` by default). `agents validate FILE` is also offline and checks the version, fields, types and local domain constraints; it cannot establish that a remote credential or personalized skill exists. It returns `{"valid":true,"schema_version":1}` on stdout. Neither command requires login or contacts a server.

```toml
schema_version = 1
name = "Research"
instructions = "Investigate carefully and present a concise result."
tools = ["time", "bash"]
skills = []
compaction_percent = 80
workspace = true

[model]
provider = "openrouter"
model = "deepseek/deepseek-v4.1-flash-1017"
context_window = 1048576
max_output_tokens = 8192
```

The version-1 document has exactly the fields above, plus optional `model.credential_id`. `name` and the complete `[model]` table are required. Omitted instructions/skills default empty, tools default `["time"]`, workspace defaults false and compaction defaults 80. Unknown fields, including accidentally pasted API keys, are rejected rather than silently discarded; parse errors do not echo TOML source or values. A BYOK model uses `provider = "byok"`, the owned credential ID and the endpoint's model identifier. Set context/output budgets to values supported by that custom model; these are execution budgets, not provider capability discovery. Compaction is still a percentage of the selected context budget, not an independent token threshold.

```sh
magents agents download AGENT_ID --output downloaded.toml
magents agents validate downloaded.toml
magents agents publish downloaded.toml --agent AGENT_ID
# Explicit stdout is useful for a pipeline:
magents agents download AGENT_ID --output -
```

Download projects only editable configuration into TOML: no account ID, server timestamps, revision, raw key or encrypted credential envelope. The credential **reference** is preserved, so a different account must choose one of its own credentials before publication. Tools and skills remain names resolved and pinned by the server, not exported executable registry definitions. Instructions themselves may be private user data; review a configuration before sharing it. The maintained [toml crate](https://docs.rs/toml/latest/toml/) handles serialization and Unicode/multiline escaping; the document is not assembled with string replacement.

## Conversations and workspace downloads

```sh
magents sessions export SESSION_ID --output conversation.json
magents sessions export SESSION_ID --format jsonl --output conversation.jsonl
magents sessions export SESSION_ID --format markdown --output conversation.md
magents sessions export SESSION_ID --format markdown --output -
magents sessions close SESSION_ID
magents sessions workspace download SESSION_ID --output workspace.tar.zst
magents sessions workspace file SESSION_ID src/main.rs --output main.rs
```

Conversation export uses the source journal, including original tool calls/results and pending steering, rather than only a compacted prompt or UI-visible summary. Reads do not append messages or advance the user inactivity clock. The requested format is streamed without JSON re-encoding, up to 64 MiB.

Workspace download requires a closed session with a clean committed checkpoint; active or checkpoint-pending sessions return an error rather than silently exporting stale files. It never starts a container or resumes execution. The archive is the existing `tar.zst` format (up to 256 MiB compressed), with workspace entries below `data/` and a separate internal manifest. The CLI streams to disk and verifies the full known length and compressed SHA-256 receipt before publishing any output. R2 export requests remain subject to resource metering and account credit admission.

Single-file download fetches that same verified archive and selects one exact normalized, UTF-8 relative regular-file path below `data/`; callers use `src/main.rs`, not `data/src/main.rs`. It never unpacks a directory tree, resolves symlinks/hardlinks or writes an archive-supplied destination. Parent components, absolute/drive paths and non-normalized paths are rejected. Non-UTF-8 Linux names remain preserved in the full archive but are not addressable through the string-based single-file selector. The native decoder bounds zstd window, expanded bytes, raw tar header counts and extension sizes before the tar parser can allocate GNU extension payloads; PAX/sparse input is outside this daemon format and rejected. It consumes the complete archive and requires a final version-1 manifest whose selected-file size, type and SHA-256 agree with the extracted bytes; missing, malformed, nonfinal or incompatible metadata never publishes the selected file. Download transfers have a five-minute total deadline and are not automatically retried.

All named output files refuse to overwrite existing files or links. Private temporary output is created beside its destination and only published after a successful transfer, checksum and applicable parsing; failure removes the temporary output and leaves no partial target. This uses maintained [tempfile no-clobber persistence](https://docs.rs/tempfile/latest/tempfile/struct.NamedTempFile.html#method.persist_noclobber), not an existence-check/overwrite race. `--output -` explicitly selects stdout; binary workspace output is refused on an interactive terminal. Existing local files are never replaced merely because a server supplies a filename.

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
magents byok add openai --base-url https://api.example.com/v1 --name research
magents byok add anthropic --base-url https://api.anthropic.com --name claude
magents byok set CREDENTIAL_ID
magents byok delete CREDENTIAL_ID
magents diagnostics
magents diagnostics --upload --consent
```

Key creation intentionally prints the one-time server response; redirect only into protected storage. BYOK offers exactly two generic wire protocols, `openai` (alias `openai-compatible`) and `anthropic`. `add` stores a named endpoint credential and returns its opaque ID. The API key is read with a hidden terminal prompt, never command arguments, environment dumps or TOML. `set ID` rotates its key; optional `--name`, paired `--protocol`/`--base-url` update compatible metadata. An existing credential's protocol/normalized endpoint are immutable: add a new credential to change its route, so pinned sessions cannot silently move to another host. Legacy `byok set openrouter`/`deepseek` and their delete aliases remain compatible but are not separate protocol choices.

Provider-key management requires the human OIDC login; unset `MAGENTS_API_KEY` to use the protected account login. API failures include HTTP status and `x-moesegfault-correlation-id`, never upstream bodies that might echo sensitive content. Diagnostics contains only consent, client name/version, and OS-architecture platform: no prompts, sessions, credentials, environment dumps, paths, or user identifiers. Upload requires both flags and authentication; preview is offline. Authorization, encryption, subscription limits, and retention remain server responsibilities.

## Verification

The version-0.2 portable/configuration changes passed focused Windows verification with `cargo test --locked -p magents` and `cargo clippy --locked -p magents --all-targets -- -D warnings`: **34 tests passed**, with the pre-existing opt-in real-browser-denial test left ignored. Eight new real-binary loopback journeys cover TOML init/validate/publish/download/PUT replacement, stdin and one-MiB limits, non-echoing private parse errors, all conversation formats, opaque archive and binary single-file download, failed hashes/HTTP admission, malformed final manifests, path rejection, no-clobber destinations and progressive help. Unit cases additionally cover a 36-combination provider/context/percentage editable-state roundtrip, Unicode/GNU long names, extension headers declaring large absent payloads, selected-file manifest mismatches and legacy BYOK aliases. These local results do not substitute for deployed owner/account and real Container/R2 acceptance.

`cargo test -p magents` exercises the real binary against a loopback HTTP fixture through create, start, streamed run, steer, and close, checking authorization and session headers. The personalized skill lifecycle covers stdin creation, list/show, file-based update, and deletion; help is checked without authentication. Tests also cover offline diagnostics, upload consent, sensitive error suppression, truncated streams, origin boundaries, callback confusion/duplicate fields, and Clap command consistency. A registered Identity deployment and OS credential store are needed for the final real-browser login check; tests do not replace it.

The opt-in `manual_login_real_binary_receives_a_validated_loopback_denial` fixture runs the actual CLI with `--no-browser`, fetches public deployment configuration/discovery, checks the emitted PKCE authorization URL, then sends a matching-state/issuer denial to the live loopback listener. It does not navigate the authorization URL, open a browser, obtain tokens, or change an account. Set `MAGENTS_TEST_NATIVE_LOGIN_API_URL` to an approved API origin and run `cargo test --locked -p magents --test native_login manual_login_real_binary_receives_a_validated_loopback_denial -- --ignored --exact`. Unit tests separately prove that manual presentation never invokes the browser opener and default presentation still does.

References: [openidconnect](https://docs.rs/openidconnect/latest/openidconnect/), [keyring](https://docs.rs/keyring/latest/keyring/v1/index.html), and [RFC 8252](https://www.rfc-editor.org/rfc/rfc8252).
