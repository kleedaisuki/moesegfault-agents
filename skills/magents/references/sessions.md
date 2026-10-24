# Durable session work

For a long task, create the session first so its ID survives even if the execution observer disconnects. CLI `run` streams responses. Use the intended API origin for every command; initiating a run can incur model and resource charges.

```sh
magents sessions start AGENT_ID
magents run AGENT_ID 'Investigate the failing tests' --session SESSION_ID
```

Capture returned JSON from session creation and session identifiers on stderr. Use the agent ID, not an upstream model ID, for `run`. A session owns its history: subsequent runs append only genuinely new user input. Never paste the full conversation back into it.

## Choose the operation from current state

| Intent/state | Operation | Effect |
| --- | --- | --- |
| Inspect progress or uncertain outcome | `magents sessions show SESSION_ID` | Read current status/journal without user activity |
| Continue an idle session | `magents run AGENT_ID 'New input' --session SESSION_ID` | Append and execute |
| Add direction to a running task | `magents sessions steer SESSION_ID 'Focus on persistence'` | Queue durable input for a safe model/tool boundary |
| Independent parallel work | `magents sessions start AGENT_ID` | Separate session, subject to account limits |
| Finish and checkpoint | `magents sessions close SESSION_ID` | Request safe close/persistence, not durable-data deletion |
| Continue a closed session | `magents sessions resume SESSION_ID`, then new input | Explicit readmission preserving the old pin and workspace |

One session permits one active run. Steering does not launch a second loop or instantly cancel an in-flight external command. Check status rather than assuming an immediate effect. Resume is subject to current plan/resource admission; it does not load the agent's newest configuration.

## Keep the two clocks and two histories distinct

The idle deadline is ten minutes after the last accepted user append, including steering. Model output, tool activity, status reads, and exports do not refresh it. A running task may continue past the deadline; once it finishes, an already-expired session closes and releases its container. Explicit resume counts as user activity and starts a new ten-minute period. Do not send dummy input to simulate observation.

Sessions pin agent revision, instructions, resolved tools/skills, model budgets, and workspace setting. Configuration or registry edits affect new sessions only. Compaction changes the derived model context, not the original append-only journal. Pending steering may remain separate until consumed.

## Recover without duplicate work

The CLI performs no automatic run retries. Missing terminal `[DONE]`, a transport error, or a disconnected terminal is not proof that the server stopped or rejected input. Preserve the ID; inspect status and journal, including pending steering, before choosing any new action. Never blindly replay the prompt or assume an arbitrary idempotency header guarantees exactly-once tool side effects. If acceptance remains unknown, report that uncertainty and ask before resubmitting potentially destructive work.

For retrieving results after closure, read [exports](exports.md).

Full contracts: [CLI](https://github.com/kleedaisuki/moesegfault-agents/blob/main/docs/cli.md), [session lifecycle and append semantics](https://github.com/kleedaisuki/moesegfault-agents/blob/main/docs/api.md).
