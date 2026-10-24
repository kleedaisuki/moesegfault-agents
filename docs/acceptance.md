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
The portable harness checks did not establish the fix by themselves. The real
Linux execution below closes that specific acceptance gap.

## Confirmed Linux Docker CI (2026-10-23)

[CI run 39977237486](https://github.com/kleedaisuki/moesegfault-agents/actions/runs/39977237486)
executed the real Docker daemon journeys on main commit
`100ee6303987d78c941207e81664f6b133fd01ab`. Its
[container job 120494989954](https://github.com/kleedaisuki/moesegfault-agents/actions/runs/39977237486/job/120494989954)
completed successfully at 23:21 Singapore time (15:21 UTC):

```text
Daemon restart: loopback port 32768 -> 32769; process boot changed
15 passed in 2.85s
```

This directly observes the ephemeral host-port change, rather than assuming it
from the earlier connection-refused trace. Re-resolving the endpoint reconnects
to a new supervisor process, after which the test verifies restored file contents
survived the restart. All fifteen real container cases pass, including the
previously unreachable escaping-link, detached-process, 64 MiB read-only dirty
tracking, and hardlink ownership journeys. The elapsed time is pytest execution
time, not image build time or a performance benchmark.

The same commit's native Rust, Worker release packaging, dashboard and tooling
jobs also succeeded. These results establish container protocol and build
acceptance for that exact revision. They do **not** establish deployed Identity
login, billing, Cloudflare Container/R2 lifecycle or production readiness; the
separate deployed release evidence listed above remains required.

## Live staging integration milestones (2026-10-24)

- [CI 39989070343](https://github.com/kleedaisuki/moesegfault-agents/actions/runs/39989070343), source `156d641`, passed all six jobs, including the actual compiled Rust/Wasm HTTP, D1, alarm, streaming, exact model-cost, DO reconstruction and redirect-refusal tests. Browser journeys use the runner's maintained Chrome rather than slow system-package downloads; no interaction case was skipped.
- [Native packaging 39993274170](https://github.com/kleedaisuki/moesegfault-agents/actions/runs/39993274170), source `928c533`, passed Linux and Windows native release builds and CLI tests. The downloaded Windows archive's SHA-256 and build metadata were independently checked; its actual binary completed staging PKCE login and a separate-process account request. See [Identity/Billing acceptance](identity-billing.md) for real signup, refresh, logout, API-key revocation and same-account checks.
- [Staging deployment 39993513683](https://github.com/kleedaisuki/moesegfault-agents/actions/runs/39993513683), source `928c533`, deployed successfully but its user-journey gate finished with **13 passed, 1 failed and 1 cleanup error**. Real Flash calls, time, SSE, live steering, pinned configuration, percentage compaction, isolation and configuration quota checks passed. A real Bash task exited successfully and its Container allocation and lease settled.
- The remaining journey failure was archive/resume: SQLite `json_set` inserted an integral timestamp as JSON REAL, which the typed session reader rejected. The malformed projection also prevented cleanup. Integer JSON writes, narrow legacy decoding and idempotent partial-resume reconciliation are fixed in subsequent source, but require new-artifact and deployed restore acceptance. No live D1 snapshot was reset to manufacture success.

These are intermediate milestones, not production promotion approval. The separately discovered Free-to-paid allocation-window issue is under correction and independent financial review; successful subscription redemption alone is not credit-conservation acceptance. Production has not been deployed or charged by these runs.

### Completed a918c67 integration gate

[Staging deployment 40062057785](https://github.com/kleedaisuki/moesegfault-agents/actions/runs/40062057785) deployed `a918c67` and passed **all fourteen real SDK journeys** in 671.59 seconds. This includes archive/resume/file restoration after the timestamp and partial-admission fixes. An older failed-resume fixture was recovered and closed through the public API, not a D1 reset.

The existing real Pro fixture also refreshed through the published CLI: its allocation became exactly `180000000` microUSD, with one immutable replacement of `48744159 + 131255841`, no duplicate grant on repeated reads, and the exact verified thirty-day term. Content-free live Cloudflare events independently joined a real time-tool run using build, pinned-prefix, registry, app trace, run and session references; see [identity](identity-billing.md) and [observability](observability.md).

An additional real-clock probe passed ten-minute **polled** idle archival, private R2 hash checks, Container stop, actual file restoration and full settlement. It did not prove unattended native lifetime management: periodic reads could keep the DO active. Its separate long-task probe failed during startup, before confirmed Bash dispatch, and correctly remained a diagnostic/financial hold rather than a claimed successful long run. Native inactivity configuration and never-ready-generation cleanup are fixed in subsequent source; truly unattended idle and greater-than-ten-minute execution acceptance remain promotion gates.

### Unattended lifecycle acceptance on 164d0ae

The native-lifecycle source passed its next full CI and was deployed as `164d0ae`. Two new, independently owned Pro sessions exercised real clocks and actual bash, not timestamp edits or a fake clock:

- **Unattended idle:** after one confirmed write, no session, DO, Container or financial probes were made until the first status read at 637.728 seconds. Passive Cloudflare tail had already observed checkpoint and automatic archive at **603.880 seconds** after the user append. The real R2 manifest/archive matched SHA-256, generation and size; a later explicit resume produced an actual successful `cat`, then normal close left no holds or pending state.
- **Long task:** one user turn requested three serial bash calls. At the first status read, **619.267 seconds** after append, execution was still running with the original input timestamp and revision. Actual results subsequently showed **240043 / 240004 / 150006 ms**, all exit zero and not timed out. With no appended input, the task completed and automatically archived at **785.227 seconds**. R2 data/hash checks, Container stop and full model/resource settlement passed; holds were zero.
- The older never-ready startup fixture closed through its ordinary API with `checkpoint_pending=false` while its unknown financial hold remained explicitly pending. No command was replayed and no live database was edited.

The long task's non-streaming SDK connection had detached, so this proves durable continuation and final persistence, **not** a successful non-streaming transport response. Subsequent source adds standard SSE comment keepalives; a shorter real streamed task with a stricter read timeout is its separate acceptance gate. Full private timing, storage, ledger and passive-provenance evidence is summarized in [Identity/Billing](identity-billing.md) and [Observability](observability.md).
