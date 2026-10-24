# Agent selection, credentials, and permissions

## Three identities that must stay separate

| Value | Meaning | Where used |
|---|---|---|
| Platform API key | Application's owner-scoped service authentication | `Authorization: Bearer` to the Agents origin; SDK `api_key` |
| Agent ID | Executable account-owned configuration | SDK/completion `model` |
| Provider credential ID | Reference to encrypted upstream secret, protocol, and endpoint | Agent's BYOK `model.credential_id` |

`GET /v1/models` returns owned agent IDs as ordinary OpenAI model records in `data`. Its `available_models` extension is a server configuration catalog, not the set of IDs to pass to Chat Completions. For platform-funded configuration choices, use `available_platform_models`: it contains registered non-BYOK routes with valid configured pricing. Empty/missing catalog support is not permission to guess funded routes. Older deployments may omit this additive field. Catalog presence does not guarantee provider health, available credentials, credit, or admission capacity.

## Configure an agent

Use backend HTTP requests against the origin plus `/v1/agents`; do not append management paths to an SDK base already ending in `/v1`.

```http
POST /v1/agents
Authorization: Bearer <platform-api-key>
Content-Type: application/json

{"name":"Application helper","model":{"provider":"byok","credential_id":"<owned-credential-id>","model":"<endpoint-supported-model>","context_window":131072,"max_output_tokens":8192},"instructions":"Report concise results.","tools":["time"],"skills":[],"workspace":false,"compaction_percent":80}
```

For funded routes replace the entire model object with a supported catalog choice, such as `{"provider":"openrouter","model":"<registered-model-id>"}`; server registry limits control those budgets. `bash` requires `workspace:true`; account skills are instruction text, not tool definitions or extra authority. Agent PUT replaces editable configuration; PATCH merges top-level fields. Each new session resolves and pins instructions, tool/skill definitions, budgets, and revision; later edits do not alter existing sessions.

For arbitrary BYOK model IDs, context/output values are caller-selected working budgets, **not inferred upstream limits**. Confirm endpoint support and choose output positive and smaller than context. Omitted generic BYOK budgets default to 131,072 context and 8,192 output tokens. BYOK is not limited to the catalog. Legacy registered-model credentials can retain earlier registered limits; existing session pins remain unchanged.

## Human control plane

Provider credentials, account skills registry, platform-key issuance/revocation, and account management require a signed-in human context. Use the `magents` CLI with its OIDC/PKCE login, or the `adash` dashboard's browser login. A platform API key cannot manage this control plane. Do not fabricate a universal SDK administration API, collect a human's password, or put a bearer key in localStorage/sessionStorage. Browser service authentication uses secure HttpOnly cookies; mutations require CSRF protection and the configured dashboard origin. Third-party frontend integrations should retain keys on their backend, not rely on arbitrary credentialed CORS.

The human creates a credential using this JSON shape at `POST /v1/provider-keys` under **human authentication**, not the platform-key header above:

```json
{"name":"My endpoint","protocol":"anthropic","base_url":"https://api.anthropic.com","api_key":"<upstream-secret>"}
```

`protocol` is `openai` or `anthropic`, not a vendor name. `openai` retains a supplied versioned base path and appends `/chat/completions`; include `/v1` where required. `anthropic` normalizes a trailing `/v1` and appends `/messages`, including supported gateway prefixes. The app still uses OpenAI Chat Completions against Agents in both cases.

Routes must be public HTTPS without embedded credentials, query, fragment, IP literals, or local/reserved names; redirects are not followed. Returned metadata includes an ID, never the plaintext key. Protocol and normalized URL are immutable per credential ID: rotate its secret with PUT; for a route change create another credential and update the agent. Changing the agent does not repin existing sessions. BYOK avoids duplicate upstream model charges, not separately metered workspace/persistence costs or owner-wide capacity limits.

Canonical details: [API](https://github.com/kleedaisuki/moesegfault-agents/blob/main/docs/api.md), [BYOK adapters](https://github.com/kleedaisuki/moesegfault-agents/blob/main/docs/byok-protocols.md), [account skills](https://github.com/kleedaisuki/moesegfault-agents/blob/main/docs/skills.md).
