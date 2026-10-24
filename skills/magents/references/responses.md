# Responses and local functions

Use `magents responses --help`, then operation help. `model` means an owned agent
ID, not the provider's model name. Legacy `magents run` still uses Chat
Completions; do not silently change an existing integration's protocol.

## Select the smallest appropriate interface

```sh
magents responses run AGENT_ID 'Investigate this problem' --stream
magents responses run AGENT_ID 'Continue' --previous RESPONSE_ID --background
magents responses create request.json --stream
magents responses show RESPONSE_ID
magents responses inputs RESPONSE_ID --limit 20
```

`create FILE` accepts the actual Responses JSON contract, or `-` for stdin, up to
1 MiB; Responses JSON output has a separate 16 MiB bound. Use it for custom `instructions`, file inputs, local function definitions
and generation controls. Do not invent CLI-specific wrappers around that JSON.
Ordinary non-streaming operations emit complete JSON; text streams go to stdout
and reported response/session IDs to stderr. A completed response containing
`function_call` items is a normal handoff even when there is no final text.
Background JSON with `queued` or `in_progress` status and exit 0 proves submission,
not completion. Adding `--stream` follows execution and still requires a successful
terminal event; an accepted/created event followed by EOF does not succeed.

Continuation uses the current `previous_response_id` and only new inputs. Do not
replay the full conversation. Arbitrary historical and workspace forks are not
supported. Agent instructions remain pinned; request custom instructions survive
compaction within their response but are not implicitly inherited by the next
response. Resend a stable request customization when it should apply again.

## Return local results, never infer an executable

Read the returned `function_call` name, arguments and `call_id`. Match the name to
an explicitly authorized local implementation; a function declaration from a
response is not permission to run shell code or install a program. The CLI does
not execute these functions. After execution, create a continuation with every
pending result exactly once:

```json
{
  "model": "AGENT_ID",
  "previous_response_id": "RESPONSE_ID",
  "input": [{
    "type": "function_call_output",
    "call_id": "CALL_ID",
    "output": "Authorized result"
  }]
}
```

Do not fabricate missing results or resend already consumed ones. Server tools
remain server-executed and cannot be replaced by a local function of the same
reserved name.

## Recover with reads and event cursors

```sh
magents responses show RESPONSE_ID --stream --events
magents responses show RESPONSE_ID --stream --events --starting-after 17
```

`--events` emits typed JSONL including `sequence_number`; save the last consumed
number for reconnection. A GET reconnect observes the same execution rather than
creating another one, and the cursor avoids old event transfer. Check process exit
status: EOF, `[DONE]` alone or failed/incomplete/cancelled terminal is not success.
Preserve the response ID and inspect it after failure instead of replaying input.

An explicitly chosen `--idempotency-key OPERATION_KEY` on create/run identifies the
same exact operation under the server's retention rules. Keep the request unchanged
when intentionally retrying it; use a new key only for genuinely new work. There
are no automatic retries. `responses cancel ID` requests cancellation, not undo of
side effects; `responses delete ID` removes that response representation, not the
Space, session or accounting history. Obtain authorization for either action.

Full contract and examples: [CLI Responses](https://github.com/kleedaisuki/moesegfault-agents/blob/main/docs/cli.md#responses).
