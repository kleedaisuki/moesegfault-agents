# Agent selection, credentials, and permissions

## Three identities that must stay separate

| Value | Meaning | Where used |
|---|---|---|
| Platform API key | Application's owner-scoped service authentication | `Authorization: Bearer` to the Agents origin; SDK `api_key` |
| Agent ID | Executable account-owned configuration | Chat Completions/Responses `model` |
| Provider credential ID | Reference to encrypted upstream secret, authentication family, and immutable endpoint | Agent's BYOK `model.credential_id` |

`GET /v1/models` returns owned agent IDs as ordinary OpenAI model records in `data`. Its `available_models` extension is a server configuration catalog, not the set of IDs to pass to Chat Completions or Responses. For platform-funded configuration choices, use `available_platform_models`: it contains registered non-BYOK routes with valid configured pricing. Empty/missing catalog support is not permission to guess funded routes. Older deployments may omit this additive field. Catalog presence does not guarantee provider health, available credentials, credit, or admission capacity.

## Configure an agent

Use backend HTTP requests against the origin plus `/v1/agents`; do not append management paths to an SDK base already ending in `/v1`.

```http
POST /v1/agents
Authorization: Bearer <platform-api-key>
Content-Type: application/json

{"name":"Application helper","model":{"provider":"byok","credential_id":"<owned-credential-id>","model":"<endpoint-supported-model>","context_window":131072,"max_output_tokens":8192},"instructions":"Report concise results.","tools":["time"],"skills":[],"workspace":false,"compaction_percent":80}
```

For funded routes replace the entire model object with a supported catalog choice, such as `{"provider":"openrouter","model":"<registered-model-id>"}`; server registry limits control those budgets. `bash` requires `workspace:true`; account skills are instruction text, not tool definitions or extra authority. Agent PUT replaces editable configuration; PATCH merges top-level fields. Sessions preserve the original resolved pin. Later instruction/tool edits append a durable configuration revision at safe boundaries, including after compact; incompatible workspace tools remain unavailable rather than escalating authority. Model/workspace and pinned skill changes require a new session. Responses request instructions remain a separate, response-only layer. See [configuration lifetimes](https://github.com/kleedaisuki/moesegfault-agents/blob/main/docs/session-config-updates.md) and [Responses](responses.md).

For arbitrary BYOK model IDs, context/output values are caller-selected working budgets, **not inferred upstream limits**. Confirm endpoint support and choose output positive and smaller than context. Omitted generic BYOK budgets default to 131,072 context and 8,192 output tokens. BYOK is not limited to the catalog. Legacy registered-model credentials can retain earlier registered limits; existing session pins remain unchanged.

## Native model transport and tool services — candidate additions

The credential record's `protocol` remains the authentication family (`openai`
or `anthropic`). Agent `model.protocol` is a different field: `openai`,
`responses` or `anthropic` chooses wire rendering. A native Responses model uses
an existing OpenAI-family credential/origin, not a fabricated `responses`
credential family. An omitted selector preserves saved-route/pin behavior;
do not silently upgrade an old session or forward opaque context to a different
provider/model route. Public application requests still use the Agents API.

For a compatible candidate realm, the BYOK model object can add
`"protocol":"responses"`. Confirm both the credential family and actual model
capability before publication; budgets are not capability discovery.

An agent's top-level `tool_services` independently selects image/search services:

```json
{
  "tool_services": {
    "image_generation": {"provider": "platform"},
    "web_search": {"provider": "platform_deepseek", "max_output_tokens": 1024, "max_tool_calls": 4}
  }
}
```

This is an editable-config fragment, not a complete create request or a claim
those services are already deployed. It does not change `model`, grant a tool
permission or add arbitrary service fields. BYOK image routes use `openai_images`
or `openrouter_images` plus an owned credential ID; BYOK search uses
`deepseek_search` or `openai_responses` with the corresponding model/options.
No raw credentials appear in config or public tool declarations. The platform
image route is the configured Seedream 5 variant; platform web search may use
DeepSeek Search/Fetch. First-party Space file retrieval and supervised Code
Interpreter have their own runtime interfaces, not invented extra table keys.

Service binding, per-response declaration, funded capacity and executed public
tool items are distinct gates. A parser accepting this fragment does not prove
hosted dispatch or environment resources exist. Outer-model BYOK does not make
a separately paid service or infrastructure free. Check the selected release's
feature evidence before invoking an expanded capability.

## Human control plane

Provider credentials, account skills registry, platform-key issuance/revocation, and account management require a signed-in human context. Use the `magents` CLI with its OIDC/PKCE login, or the `adash` dashboard's browser login. A platform API key cannot manage this control plane. Do not fabricate a universal SDK administration API, collect a human's password, or put a bearer key in localStorage/sessionStorage. Browser service authentication uses secure HttpOnly cookies; mutations require CSRF protection and the configured dashboard origin. Third-party frontend integrations should retain keys on their backend, not rely on arbitrary credentialed CORS.

The human creates a credential using this JSON shape at `POST /v1/provider-keys` under **human authentication**, not the platform-key header above:

```json
{"name":"My endpoint","protocol":"anthropic","base_url":"https://api.anthropic.com","api_key":"<upstream-secret>"}
```

`protocol` is `openai` or `anthropic`, not a vendor name. `openai` retains a supplied versioned base path; the legacy Chat transport appends `/chat/completions`, while an explicit native model transport appends `/responses`. Include `/v1` where required. `anthropic` normalizes a trailing `/v1` and appends `/messages`, including supported gateway prefixes. The application uses the Agents OpenAI-compatible public API in both cases; upstream protocol selection does not make the public service an Anthropic Messages endpoint.

Routes must be public HTTPS without embedded credentials, query, fragment, IP literals, or local/reserved names; redirects are not followed. Returned metadata includes an ID, never the plaintext key. Protocol and normalized URL are immutable per credential ID: rotate its secret with PUT; for a route change create another credential and update the agent. Changing the agent does not repin existing sessions. BYOK avoids duplicate upstream model charges, not separately metered workspace/persistence costs or owner-wide capacity limits.

Canonical details: [API](https://github.com/kleedaisuki/moesegfault-agents/blob/main/docs/api.md), [BYOK adapters](https://github.com/kleedaisuki/moesegfault-agents/blob/main/docs/byok-protocols.md), [account skills](https://github.com/kleedaisuki/moesegfault-agents/blob/main/docs/skills.md).
