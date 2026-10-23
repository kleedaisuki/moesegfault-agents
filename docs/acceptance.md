# End-to-end acceptance

The acceptance boundary is the real user API, an unmodified OpenAI Python SDK,
and the Linux workspace supervisor reached over HTTP. Unit tests complement
these journeys; neither compilation nor skipped tests establish deployment readiness.

## Workspace supervisor

Run the supervisor inside a disposable Docker container, not directly as root on
a shared runner. It owns and quiesces UID 1000; running it directly could kill
unrelated processes owned by that UID. The test fixture owns only its randomly
named container, publishes port 8080 on an ephemeral loopback port, and removes
that container even after failure. No host workspace directory is mounted.

```sh
python -m venv .cache/e2e-venv
.cache/e2e-venv/bin/python -m pip install -r tests/e2e/requirements.lock
docker build -f container/Dockerfile -t magents-workspace:acceptance .
MAGENTS_DAEMON_IMAGE=magents-workspace:acceptance \
  .cache/e2e-venv/bin/python -m pytest tests/e2e/test_workspace_daemon.py -ra \
  -o cache_dir=.cache/pytest --basetemp=.temp/pytest
```

The suite checks:

- authenticated HTTP and bash running as UID 1000 without the supervisor token;
- invalid request boundaries, output caps, timeouts, cancellation, execution ID
  lookup and duplicate admission, and refusal to checkpoint a busy workspace;
- actual filesystem write, archive download and independent SHA-256 verification,
  mutation, corrupted archive rejection without changing the current generation,
  restore, mode preservation, and supervisor process restart;
- rejection of links escaping the workspace without losing the valid state.
- cleanup of detached `setsid` descendants before a delayed write can occur;
- a checkpointed 64 MiB sparse file followed by twelve read-only commands must
  keep `dirty_bytes` at zero; a same-size write, empty-file creation and deletion
  must mark the tree dirty;
- UID 1000-owned hardlinks restore as independently owned regular files with
  equal contents, while root-owned linked files fail checkpointing with an
  actionable ownership error.

This validates the daemon protocol, not Cloudflare Container lifecycle, R2 storage,
cross-tenant isolation, or crash durability of the deployed Workers.

## Deployed platform

Use API keys created through the normal account flow. There is no test bearer
bypass, no fake account claim, and no automatic production fallback origin.
Credentials must be supplied through environment secrets, never command arguments
or committed fixtures. Error assertions deliberately omit response bodies.

```sh
export MAGENTS_TEST_API_ORIGIN=https://agents.moesegfault.dev
export MAGENTS_TEST_API_KEY='...'
.cache/e2e-venv/bin/python -m pytest tests/e2e/test_deployed.py -ra \
  -o cache_dir=.cache/pytest --basetemp=.temp/pytest
```

| Setting | Required capability / effect |
| --- | --- |
| `MAGENTS_TEST_API_ORIGIN`, `MAGENTS_TEST_API_KEY` | Authenticated deployed management API; otherwise deployed tests skip explicitly |
| `MAGENTS_TEST_INFERENCE=1` | Opt in to billable real provider calls; provider credentials and balance must exist |
| `MAGENTS_TEST_OTHER_API_KEY` | A second valid account key for ownership checks; same-account credentials correctly fail this check |
| `MAGENTS_TEST_WORKSPACE=1` | Opt in to real workspace tool calls and busy-session steering |
| `MAGENTS_TEST_COMPACTION=1` | Opt in to bounded real-provider compaction using the same dedicated funded staging key; only `agents-staging.moesegfault.dev` is accepted |
| `MAGENTS_TEST_QUOTA=1` | Dedicated, empty Free account; concurrently create three configs and expect exactly two admissions |
| `MAGENTS_DAEMON_IMAGE` | Existing local Linux Docker image for daemon tests |

Management journeys create uniquely named agents, check percentage validation,
pin the complete instructions/tools/model configuration per session, edit the
agent, and ensure only future sessions observe the edit. Inference journeys use
the official SDK, inspect `x-agents-session` on its raw response, verify an actual
time tool call/result in server-managed history, append a streamed turn using the
same session header, and check completion termination and unchanged pinned state.
The steering journey appends while a real workspace run is active and checks the
new instruction reaches the final answer rather than merely receiving HTTP 200.

The compaction journey reuses its single managed agent with the normal registered
Flash model and a 1% setting. A fixed English corpus is approximately 12–20k input
tokens, verified against the real provider's reported usage (10,500–20,000 accepted
to account for tokenization and prefix variation). It never fills the million-token
context window. The following user append invokes time and triggers compaction;
the test checks `compact_count`, an unchanged pinned configuration, exact retention
of the original source journal prefix and payload, and complete tool-call/result
groups without orphaned or repeated IDs. The test makes no client tokenizer or
token-budget configuration part of the user's API. A dedicated funded staging
account and explicit inference/compaction opt-ins are mandatory. This is intended
as a small real-provider probe, not a performance benchmark; actual cost follows
the deployment's current provider tariff and is not asserted from token counts.

Tests close their created sessions and delete their agents. A failed or still
running task can make cleanup fail; this is reported, not silently ignored.
Do not run quota saturation against a personal account. Model nondeterminism is
not hidden with retries: a missed required tool invocation is a failed journey
whose history should be inspected using normal authenticated management tools.

## Release evidence still required

Before calling a deployment production-ready, record real execution results for:

1. Account login, subscription activation, API key issuance, revocation and quota
   reconciliation using the actual Identity and Billing services.
2. Worker → Durable Object → Container execution → R2 checkpoint → container
   release → fresh-container restoration, including billed resource usage.
3. No new user append for ten minutes while a task remains active: the task must
   continue, then archive immediately after completion. Ordinary tool activity
   must not extend the user-input idle deadline.
4. Context compaction at the configured percentage with a provider-reported
   context budget, preserving instructions and pending steering messages.
5. Consent-gated diagnostic upload and a complete correlated operational trace
   without prompts, workspace contents, provider secrets, or user tokens in logs.

These require deployed capabilities and are not inferred from daemon success or
source inspection. Pending release evidence is not a passed acceptance check.

## Initial local verification (2026-10-23)

Windows / Python 3.14.6 / OpenAI SDK 2.6.1 / pytest 8.4.2:

```text
python -m pytest tests/e2e -ra -o cache_dir=.cache/pytest --basetemp=.temp/pytest
4 passed, 29 skipped
```

The four passes validate only the harness: canonical nonstream/SSE wire fixtures
for the official SDK, a simulated ephemeral Docker port change across restart,
and diagnostic redaction. They do not establish actual container success.
Fourteen deployed cases skipped because no test origin/API key was supplied;
fifteen daemon cases skipped because no Linux Docker image was supplied. Docker
is not installed on this development host. No deployed-platform or daemon
acceptance success is claimed by these results.

## First Linux container CI and restart harness correction

The initial Linux Docker job in [CI run 39974777922](https://github.com/kleedaisuki/moesegfault-agents/actions/runs/39974777922)
ran real daemon HTTP tests: ten passed, then the snapshot/restore/restart journey
failed to reconnect, followed by four connection-refused failures against the
shared fixture. Restore and mode checks had already succeeded before restart.
The logs did not preserve container state or the post-restart host-port mapping,
so they do not by themselves prove a daemon startup failure or a changed port.

The fixture previously cached Docker's ephemeral loopback port forever. It now
re-inspects `NetworkSettings.Ports` while waiting for startup/restart, bounds
health requests, and requires a changed daemon `boot_id` before asserting data
survived. A portable regression simulates port 32768 changing to 32769; a second
check proves diagnostic filtering omits tokens and user commands. On failure the
fixture reports only selected Docker state fields, observed loopback ports and
allowlisted structured daemon event metadata, never `Config.Env` or raw logs.
The next Linux CI run must establish whether this resolves the actual restart
failure; these harness tests alone are not a substitute for that run.
