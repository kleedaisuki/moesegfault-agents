# Portable agent configuration

Publication creates a callable configuration in the signed-in account, not a public marketplace entry. Obtain authorization for creation or replacement; use the intended API origin consistently.

## Draft and check offline

```sh
magents agents init agent.toml
magents agents validate agent.toml
```

Both commands work without login or network. Use the installed template, not a separately maintained duplicate. Edit it for the requested instructions, model, tools, and skills. Validation confirms local schema/domain constraints, not remote model availability, credential ownership, registered skill existence, funding, or plan admission. `magents models` discovers current server model information when a live read is appropriate.

The established version-1 fields are `schema_version = 1`, `name`, `instructions`, `tools`, `skills`, `compaction_percent`, `workspace`, and `[model]` fields `provider`, `model`, `context_window`, `max_output_tokens`, optionally `credential_id`. Candidate-capable builds additionally accept `model.protocol` and top-level `tool_services`; do not assume an older published binary or deployment accepts them. Name and the complete model table are required. Unknown fields are rejected. Instructions/skills default empty, tools to `["time"]`, workspace false, compaction 80. `bash` requires workspace enabled. Compaction is an integer percentage 1–99 of the selected context budget, not a token count.

Platform-funded routes are `openrouter` and `deepseek`; server registry metadata governs their limits. BYOK uses `provider = "byok"` with an owned credential ID, an endpoint-supported model ID, and explicit supported context/output budgets. Output must be positive and less than context. These budgets do not discover endpoint capabilities. Never put raw API keys in TOML. See [account and BYOK](account.md) for credential creation.

## Native transport and independent tool services — candidate config

Credential authentication and model wire transport are separate. Create an
OpenAI-family credential with `byok add openai`; selecting `protocol = "responses"`
in `[model]` uses its same immutable origin/credential for native Responses when
supported. It is not `byok add responses`, a new auth family or a change to the
public Agents API. Model transport values are `openai`, `responses`, `anthropic`;
omitting the selector preserves existing route/pin behavior. Do not apply
Responses transport to an Anthropic-family credential to evade compatibility.

Illustrative schema-v1 TOML for a candidate build, not a production capability
claim; replace IDs/model with owned, provisioned values:

```toml
schema_version = 1
name = "Native helper"
workspace = false
compaction_percent = 80
tools = ["time"]

[model]
provider = "byok"
credential_id = "cred_outer"
protocol = "responses"
model = "endpoint-supported-model"
context_window = 131072
max_output_tokens = 8192

[tool_services.image_generation]
provider = "platform"

[tool_services.web_search]
provider = "platform_deepseek"
max_output_tokens = 1024
max_tool_calls = 4
```

`tool_services` routes paid image/search services independently from the outer
language model. Its current fields are `image_generation` and `web_search`, not
arbitrary tool names. Platform service entries use operator routes, not a user
credential. BYOK image providers are `openai_images` or `openrouter_images` with
an owned `credential_id` and optional supported `model`; search providers are
`deepseek_search` or `openai_responses` with their matching owned route/options.
Never put an API key in these tables, a tool definition or program environment.
Code Interpreter and first-party Space retrieval have separate capability/runtime
interfaces; do not invent `tool_services.code_interpreter` or `file_search` fields.

Default platform images target the configured Seedream 5 variant; platform web
search may use DeepSeek's genuine Search/Fetch. Registry/function `tool_search`
is not external web search. Configuring routes neither enables missing hosted
dispatch nor proves model, resource, network or billing availability. Validate
locally, then inspect the selected realm/catalog and accepted feature increment
before an authorized publish/run. BYOK outer-model funding does not make a paid
tool service or Container/storage free.

## Publish or replace

```sh
magents agents publish agent.toml
magents agents publish agent.toml --agent AGENT_ID
magents agents show AGENT_ID
```

Capture the returned ID. Without `--agent`, publish creates a new configuration; with it, publish replaces the selected owned configuration, not a partial merge. Existing session pins never change, but instruction/tool edits append a separate durable configuration revision at safe execution boundaries. Create a new session for a changed model, workspace authority or pinned skill content. See [configuration lifetimes](https://github.com/kleedaisuki/moesegfault-agents/blob/main/docs/session-config-updates.md). Legacy `agents create FILE` accepts JSON; `agents update ID FILE` retains JSON PATCH semantics. Do not silently substitute a replacement for a requested partial update. JSON/TOML inputs accept `-` for stdin and are bounded to 1 MiB.

## Download or transfer

```sh
magents agents download AGENT_ID --output downloaded.toml
magents agents validate downloaded.toml
```

The output is editable configuration, excluding owner IDs, revision, timestamps, raw keys, and encrypted secrets. Credential references remain account-local: another account must choose its own credential. Tools and skills remain server-resolved names, not bundled executable definitions or registry instructions. Review private instructions before sharing. Named output refuses overwrite; choose a new path. Use `--output -` only for an intentional stdout pipeline.

Full contract: [CLI versioned TOML](https://github.com/kleedaisuki/moesegfault-agents/blob/main/docs/cli.md).
