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

A legacy `magents run` calls `POST /v1/chat/completions` with the agent ID as `model` and optionally `x-agents-session`. Its response session header goes to stderr. The maintained `eventsource-stream` parser handles fragmented UTF-8 and SSE framing. Missing `[DONE]` is an error. After interruption, inspect the session rather than blindly replaying prompts. Steering appends new input without replacing pinned instructions. Automatic retries are not performed. The additive `responses` command family uses the separate Responses contract below; existing `run` behavior is not silently migrated.

## Responses

Start with a text prompt, or use the complete JSON interface when attaching files,
declaring local functions, supplying custom instructions or setting generation
controls:

```sh
magents responses run AGENT_ID 'Investigate the failing tests' --stream
magents responses run AGENT_ID 'Continue with the fix' --previous RESPONSE_ID --background
magents responses show RESPONSE_ID
magents responses inputs RESPONSE_ID --limit 20
magents responses cancel RESPONSE_ID
magents responses create request.json --stream
```

`responses create FILE` accepts a Responses API JSON object, bounded to 1 MiB;
`-` reads stdin. `--stream` sets its `stream` field to true. Otherwise a JSON
`stream:true` also selects streaming. The CLI forwards supported request controls
without inventing a separate tool or generation-options configuration format.
Responses JSON output is separately bounded to 16 MiB, allowing multi-round output
larger than an agent configuration; portable agent downloads keep their 1 MiB cap.
`model` is always the owned **agent ID**, including when BYOK selects an upstream
model. A `previous_response_id` continues only the current linear head. Send only
new input, not the entire existing conversation. Arbitrary historical or workspace
forks are not supported.

`--background` returns the accepted response without waiting for its final answer;
exit 0 with `queued` or `in_progress` JSON means successful submission, not
successful execution. It can also be combined with `--stream`, which follows the
response and requires its successful terminal event. Non-streaming `responses run`
and `create` print the full bounded JSON result, including its recovery ID, but exit
nonzero for `failed`, `incomplete` or `cancelled` status even when HTTP is 200.
Completed local-function handoffs remain successful. Inspection commands such as
`show`, `cancel`, `delete` and `inputs` retain exit 0 for valid successful HTTP
objects; reading a failed response is not another execution attempt.
A disconnected observer never implies
cancellation. Read the saved response, or resume its event stream instead of
resubmitting the original input:

```sh
magents responses show RESPONSE_ID --stream --events
magents responses show RESPONSE_ID --stream --events --starting-after 17
```

Streamed text is stdout; response/session identities go to stderr. `--events`
emits full typed events as JSONL, including `sequence_number`, function argument
events and final usage. Save the last successfully consumed sequence number and
use `--starting-after` to avoid retransferring earlier events. A completed event
with a consistent `completed` response status is required for success: an EOF,
`[DONE]` alone, failed/incomplete/cancelled terminal or malformed event exits
nonzero. JSONL consumers must also check the process exit status rather than
treating partial text as a finished response. There is no automatic replay.

An optional `--idempotency-key OPERATION_KEY` on `responses run` or `create` sends
an owner-scoped operation key. Reuse it only for an explicit retry of the exact
same request under the server's retained-key contract; use a new key for genuinely
new work. Changed input under the same key must conflict, not execute. The CLI
does not infer a retry merely because transport failed. Keys contain 1–255
printable non-space ASCII bytes. Keep the original request and response ID when
recovering an operation.

### Local functions and instructions

[`examples/response-local-tools.json`](../examples/response-local-tools.json)
shows a local function declaration. Replace its agent placeholder, then submit it
with `responses create`. The response may complete with `function_call` output
items instead of final assistant text. This is a valid handoff, not an error.
Inspect every name and argument, execute only an explicitly authorized local
implementation, and submit the matching `call_id` results through a new create
request. [`examples/response-local-results.json`](../examples/response-local-results.json)
shows that continuation shape. Include all pending calls exactly once. The CLI
never interprets function names as executable paths, launches a local shell,
downloads tool code or guesses an implementation.

Agent-configuration instructions and server-managed behavioral/tool/skill content
remain pinned. The request's `instructions` supplies a separate custom layer,
preserved verbatim through compaction during that response. Top-level request
instructions are not inherited through `previous_response_id`; resend that
customization when needed on a later response. Changing it never grants additional
server tool, filesystem, credential or account permissions.

`responses delete RESPONSE_ID` deletes that stored response representation, not
the session, Space, workspace versions or financial journal. Cancellation requests
are not undo operations for completed model, shell or local-function side effects.
See [the Responses contract](responses-spaces-v0.3.md) for storage, cancellation,
continuation and unsupported capabilities.

### Exact counting and standalone compaction

```sh
magents responses count count-request.json
cat count-request.json | magents responses count -
magents responses compact compact-request.json --idempotency-key compact-operation-1
```

Both commands forward the JSON object unchanged to the native
`/v1/responses/input_tokens` or `/v1/responses/compact` endpoint. Files and stdin
share the existing 1 MiB request limit; replies use the 16 MiB Responses limit.
`model` remains your owned agent ID, not an upstream model name. The server
resolves owned context, pinned instructions, tools and attachments; the CLI never
guesses a tokenizer or converts the request into Chat messages. Unsupported exact
counts fail rather than returning an estimate. Neither command automatically
retries a transport or HTTP failure.

Compaction is a paid model operation. Choose an `--idempotency-key` before
dispatch and retain it with the exact request for explicit recovery. Reusing it
with different input must conflict. Omitting the key lets the server create a
new operation; submitting again without the original key is not recovery.
`compact` prints the **entire** returned object, including every retained user
item and the opaque compaction item in `output`. Use the complete `output` array
as the next Responses `input`; do not extract only text or discard roles/opaque
state. Its `instructions` field is guidance for the compaction operation, not a
replacement for pinned agent instructions or a new ongoing custom layer. See
[the auxiliary contract](full-responses-auxiliary.md).

## Durable Conversations and code containers

```sh
magents conversations create conversation.json --idempotency-key conversation-operation-1
magents conversations show CONVERSATION_ID
magents conversations update CONVERSATION_ID metadata.json
magents conversations items CONVERSATION_ID create items.json
magents conversations items CONVERSATION_ID list --limit 20 --order asc
magents conversations items CONVERSATION_ID show ITEM_ID
magents conversations items CONVERSATION_ID delete ITEM_ID
magents conversations delete CONVERSATION_ID
# Only after deletion and completion of active responses:
magents conversations purge CONVERSATION_ID
magents containers show CONTAINER_ID
magents containers delete CONTAINER_ID
```

Conversation create accepts `{"metadata":{},"items":[]}` (either field optional),
update accepts `{"metadata":{...}}`, and item create accepts `{"items":[...]}`.
Each file argument also accepts `-` for stdin. Requests and typed JSON replies
are forwarded without lossy projections; the server validates supported items,
ownership, mutations and capacity. There is no conversation-list endpoint.
Item listing defaults to descending order, follows `--after LAST_ID` when
`has_more` is true, and supports explicit ascending order. Attach the returned
Conversation ID through the `conversation` field of a `responses create` request;
Conversations are not aliases for session IDs. Conversation create's optional
idempotency key identifies only the exact create; appends are not implicitly
replayed or deduplicated by the CLI.

Conversation delete changes API visibility, not physical retention. `purge` is
a separate irreversible operation for retained conversation items and requires
prior deletion and no active response/capacity hold; it does not erase separately
stored response snapshots, workspace versions or financial records. Container
commands use the owned opaque ID returned by code execution. They do not create
a container, resume the creator's model harness, or erase its session/history.
Successful empty deletion responses produce no stdout. All operations use normal
authentication, server-side admission and metering, with no automatic retries.

## Space and files

Each account has one Space, shared across its dashboard, CLI and API callers.
Uploaded files are user data, distinct from immutable workspace versions:

```sh
magents space show
magents space rename Research
magents space files upload ./brief.txt --content-type text/plain
magents space files upload ./screenshot.png --content-type image/png
magents space files upload ./notes.txt --name 'Project notes.txt'
magents space files list --limit 20
magents space files list --limit 20 --after FILE_ID
magents space files show FILE_ID
magents space files rename FILE_ID 'Updated notes.txt'
magents space files download FILE_ID --output notes.txt
magents space files delete FILE_ID
```

Listing returns one page; follow `last_id` when `has_more` is true. This avoids
fetching the entire Space just to discover recent files. Upload accepts a regular
file up to 64 MiB, also subject to the account's actual server limits and funding.
The client reads a bounded 64 KiB buffer, computes SHA-256, rewinds the same open
file, then streams its bytes once over the network with explicit length and
checksum. It uses the native raw upload endpoint, not base64 JSON or an in-memory
multipart copy. UTF-8 filenames travel in an encoded query, not a header or local
filesystem path. `--name` overrides only the stored name; content is not rewritten.
The server's returned byte count and digest must match the local receipt.

Raw upload defaults to `application/octet-stream`; the CLI does not infer media
type from the filename. Set `--content-type image/png` (or the actual supported
image type) before using an uploaded ID in an `input_image` part, including a
local-function result. Renaming a `.png` file does not change its declared type.

File download streams to private temporary output and requires both length and
SHA-256 receipts before no-clobber publication. `--output -` explicitly selects
binary stdout and is refused on an interactive terminal. Neither the server's
filename nor an archive entry selects the local destination. Failed downloads
leave no partial destination. Uploads and downloads have a five-minute deadline
and are not retried automatically. If an upload loses its reply, inspect the Space
for its metadata/digest before deciding to upload again.

To use an uploaded file, include its owned ID in a Responses request:

```json
{
  "model": "AGENT_ID",
  "input": [{
    "role": "user",
    "content": [
      {"type": "input_text", "text": "Read this brief and identify the next step."},
      {"type": "input_file", "file_id": "FILE_ID"}
    ]
  }]
}
```

This is an attachment reference, not permission to read another account's file.
Use the server's documented file types and size limits; upload support does not
imply every upstream model can interpret arbitrary media.

### Workspace persistence and versions

```sh
magents sessions workspace commit SESSION_ID --name 'Before migration'
magents space versions list --limit 20
magents space versions show VERSION_ID
magents space versions download VERSION_ID --output before-migration.tar.zst
magents sessions workspace restore SESSION_ID VERSION_ID
# Explicit recovery of the exact same action after an uncertain reply:
magents sessions workspace restore SESSION_ID VERSION_ID --operation-id OPERATION_ID
```

Both modifying commands generate a cryptographically random UUID once per
invocation. The operation ID is printed to stderr **before** dispatch and included
as `operation_id` in the successful JSON outcome. Preserve it with the exact
session, action, name or version ID if a connection fails. `--operation-id` reuses
that identity for an explicit identical-action retry; never reuse it for different
work or generate a new identity merely because a reply was lost. IDs contain
1–128 ASCII letters, digits, underscores or hyphens. The corresponding request
bodies are `{"operation_id":"...","name":"..."}` for commit (name optional),
and `{"operation_id":"...","version_id":"..."}` for restore. The CLI does not
automatically repeat either action.

Commit creates an immutable workspace version in the account Space. Versions are
discoverable across the user's sessions, never publicly shared by knowing an ID.
They have no user rename, overwrite or delete command. Download verifies the
archive's full length and compressed checksum without extracting it or starting a
workspace. Existing session workspace downloads retain their closed-session
contract; a Space version can be downloaded independently of that session's later
changes.

Commit and restore require an eligible idle workspace; they do not interrupt a
running task. Restore first preserves the current workspace as a safety version,
then restores the selected version into that same session. It is not a workspace
fork and does not replace the session's pinned instructions or conversation. These
operations can consume storage, transfer and compute resources under the normal
account limits. Agents can invoke the corresponding registered persistence,
version-discovery and restore tools when enabled in their configuration; CLI
management does not execute an extra model turn.

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
