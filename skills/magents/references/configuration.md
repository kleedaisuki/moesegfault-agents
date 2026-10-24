# Portable agent configuration

Publication creates a callable configuration in the signed-in account, not a public marketplace entry. Obtain authorization for creation or replacement; use the intended API origin consistently.

## Draft and check offline

```sh
magents agents init agent.toml
magents agents validate agent.toml
```

Both commands work without login or network. Use the installed template, not a separately maintained duplicate. Edit it for the requested instructions, model, tools, and skills. Validation confirms local schema/domain constraints, not remote model availability, credential ownership, registered skill existence, funding, or plan admission. `magents models` discovers current server model information when a live read is appropriate.

Version 1 accepts `schema_version = 1`, `name`, `instructions`, `tools`, `skills`, `compaction_percent`, `workspace`, and `[model]` fields `provider`, `model`, `context_window`, `max_output_tokens`, optionally `credential_id`. Name and the complete model table are required. Unknown fields are rejected. Instructions/skills default empty, tools to `["time"]`, workspace false, compaction 80. `bash` requires workspace enabled. Compaction is an integer percentage 1–99 of the selected context budget, not a token count.

Platform-funded routes are `openrouter` and `deepseek`; server registry metadata governs their limits. BYOK uses `provider = "byok"` with an owned credential ID, an endpoint-supported model ID, and explicit supported context/output budgets. Output must be positive and less than context. These budgets do not discover endpoint capabilities. Never put raw API keys in TOML. See [account and BYOK](account.md) for credential creation.

## Publish or replace

```sh
magents agents publish agent.toml
magents agents publish agent.toml --agent AGENT_ID
magents agents show AGENT_ID
```

Capture the returned ID. Without `--agent`, publish creates a new configuration; with it, publish replaces the selected owned configuration, not a partial merge. Existing session pins never change. Create a new session to adopt the updated instructions, model, tools, or skill content. Legacy `agents create FILE` accepts JSON; `agents update ID FILE` retains JSON PATCH semantics. Do not silently substitute a replacement for a requested partial update. JSON/TOML inputs accept `-` for stdin and are bounded to 1 MiB.

## Download or transfer

```sh
magents agents download AGENT_ID --output downloaded.toml
magents agents validate downloaded.toml
```

The output is editable configuration, excluding owner IDs, revision, timestamps, raw keys, and encrypted secrets. Credential references remain account-local: another account must choose its own credential. Tools and skills remain server-resolved names, not bundled executable definitions or registry instructions. Review private instructions before sharing. Named output refuses overwrite; choose a new path. Use `--output -` only for an intentional stdout pipeline.

Full contract: [CLI versioned TOML](https://github.com/kleedaisuki/moesegfault-agents/blob/main/docs/cli.md).
