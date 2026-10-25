# Responses, branches and local tools

Use `magents responses --help` and operation help from the installed binary.
`model` is an owned agent ID, not the upstream model name. `magents run` retains
Chat Completions; do not silently change an existing integration's protocol.

## Select the smallest interface

```sh
magents responses run AGENT_ID 'Investigate this problem' --stream
magents responses run AGENT_ID 'Continue' --previous RESPONSE_ID --background
magents responses create request.json --stream
magents responses show RESPONSE_ID
magents responses inputs RESPONSE_ID --limit 20
```

`create FILE` accepts actual Responses JSON, or `-` for stdin, rather than a
CLI-specific wrapper. Use it for typed input, request instructions, local tools
and generation controls. JSON output, event JSONL and text output are different
modes; save disclosed response/session IDs before consuming the body. A queued
background response proves acceptance, not successful completion. A completed
client-tool handoff need not contain final text.

Check the selected release and server capability before using expanded fields.
The full contract includes historical conversation/workspace branches,
Conversations, rich tools, WebSocket lanes/steering and compact/count; source
registration or a newer CLI parser is not proof that the selected realm has
activated or accepted them. Do not call a narrow baseline the full release.

## Continue or branch deliberately

Send only new input or pending results with `previous_response_id`; do not resend
source history. The expanded implementation uses a warm same-agent head when
eligible; a historical, concurrent or different-owned-agent continuation may
create an independent Session from the immutable parent boundary. Same-agent
children keep the original pin; a different target agent receives its own pin,
not the parent's tool authority. Workspace inheritance must come from a retained
sealed boundary, never today's mutable source workspace. Older unsealed parents
can report unavailable history rather than inventing a workspace snapshot.

A branch is new authorized work, not observation recovery. Save a new intent/key
for it. An omitted model resolves from the parent; choosing another owned agent
is deliberate branching, not a way to bypass quotas. Conversation-bound request
semantics are separate; do not invent a `magents conversations`, `ws`, `compact`
or `count` command unless the installed subgroup help actually exposes it. Use
an accepted SDK/HTTP interface when the CLI has no dedicated operation.

Server/agent instructions stay pinned. Request `instructions` survives compaction
inside its response but is not inherited as a later top-level override. Resend
that customization deliberately; neither instructions nor historical context
grant server-tool or cross-owner permissions.

## Return local results, never infer an executable

Match a returned call's declared type, name, arguments and `call_id` to an
explicitly authorized local implementation. A function/custom/namespace
declaration is not permission to run arbitrary shell code, install a program or
send credentials to a model-selected endpoint. The CLI does not execute it.
For a completed function handoff, send all outstanding results exactly once:

```json
{
  "model": "AGENT_ID",
  "previous_response_id": "RESPONSE_ID",
  "input": [{"type": "function_call_output", "call_id": "CALL_ID", "output": "Authorized result"}]
}
```

Use each tool's actual pending/result shape for richer types; do not coerce MCP
approval, custom, computer or shell results into fictitious function outputs.
Never fabricate missing results or re-run already recorded side effects. Hosted
service calls execute in the harness, not as caller-local aliases; configuring
`tool_services` does not authorize arbitrary local tool execution.

## Recover observation with GET only

```sh
magents responses show RESPONSE_ID --stream --events
magents responses show RESPONSE_ID --stream --events --starting-after 17
```

`--events` emits typed JSONL with `sequence_number`; checkpoint only processed
events. With no processed event, omit the cursor, never send `-1`. Reconnect to
the same response ID, append only authentic newer events and require a real
terminal. EOF, `[DONE]` alone, failed/incomplete/cancelled status or an invented
snapshot is not successful stream completion. Preserve the existing observation
prefix; do not resend POST, prompts, tool outputs or shell commands to repair it.

`--idempotency-key OPERATION_KEY` identifies one exact create/run request under
server retention rules; save it before dispatch. A changed body/binding needs a
new intended operation, not a replacement key used to hide uncertainty. There
are no automatic execution retries. Cancellation is not undo; deletion removes
that response representation, not its Space, source journal or financial history.
Obtain authorization for either mutation.

Canonical interfaces: [Responses and SDK recovery](https://github.com/kleedaisuki/moesegfault-agents/blob/main/docs/api.md),
[expanded contract](https://github.com/kleedaisuki/moesegfault-agents/blob/main/docs/full-responses-contract.md),
[CLI](https://github.com/kleedaisuki/moesegfault-agents/blob/main/docs/cli.md#responses).
