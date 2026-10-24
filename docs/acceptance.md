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

## Released production state

The initial public implementation is `8caff37333cf148a62c804e75cc9189203e8f167`:
[full CI](https://github.com/kleedaisuki/moesegfault-agents/actions/runs/40067168672),
[all fourteen staging SDK journeys](https://github.com/kleedaisuki/moesegfault-agents/actions/runs/40067574002),
and [successful production deployment](https://github.com/kleedaisuki/moesegfault-agents/actions/runs/40068971874).
Both public production hosts returned the exact build. A new, independently owned
production Free fixture completed public email verification/signup, confidential
BFF and published native PKCE login, same-account checks, time, actual Bash write,
R2 archive/hash verification, explicit resume and actual Bash readback. Its one
Free grant was `1000000` microUSD; six provider calls consumed model/resource
credit totaling `858` microUSD, with no remaining holds. Test keys/configurations
and the isolated single-recipient Rust mail receiver were cleaned up. No existing
production user, password, paid allocation or payment collection was changed.

| Original capability | Evidence and boundary |
| --- | --- |
| Rust Worker/DO harness; multiple configurations and per-session ownership | Published crates and migrations; real config/session/owner-isolation SDK cases; distinct serving namespaces verified in the control plane |
| Harness time / sandboxed Bash, batched compressed R2 state | Actual time/tool results; production write/archive/hash/restore/readback; sixteen Docker journeys including 120 independent exit-race executions; original detached-writer and corrupted-archive assertions retained |
| Stable server-managed instructions/tools/skills and cache behavior | Real pin-preservation and percentage-compaction cases; skill registry/version SQL tests; deterministic prefix tests; measured warm cache hits and bounded cold journal reads in performance notes |
| Steering and input-only inactivity, including long work | Real busy steering; passive unattended checkpoint at 603.880 seconds; 785.227-second task and all three successful long Bash results, with no input-clock advance |
| Long streamed API transport | A single real 150-second Bash task completed in 161.785 seconds with a 45-second SDK read timeout, ten standard keepalives, normal `stop` and `[DONE]`; heartbeat branches and delayed-provider actual-Wasm regression also pass |
| Exact credits, model/resource billing, tiers and quotas | Production Free/usage/settlement; real thirty-day Pro activation and single replacement to 180 USD-equivalent credit; sixty SQL tests and 108 independent conservation sequences; atomic Free quota case |
| BYOK | The original published Wasm executed two time-tool rounds with the exact user-owned mock provider key, rejected another owner's reference, charged zero model cost while charging resources, and released holds. This is compiled-Rust integration with a mocked upstream, not a claim of an external customer's key being used |
| Moe account login, refresh/logout/key lifecycle | Actual staged and production BFF/native sign-in and same subject; real staged refresh/independence/revocation cases, production revoked-key 401; no human profile scraping, forged JWT or account seeding for these journeys |
| Adash i18n, appearance/mobile; Rust CLI discovery | Dashboard interaction/build and six Chrome responsive/theme/language journeys; real authenticated model chooser/capability readback; actual published Windows CLI login and Linux/Windows native release builds |
| Content-free telemetry, tracing/provenance, consent and storage classes | Live time and passive workspace event joins; actual capture settings and private/isolated buckets; real consent false/true/unknown-field diagnostics results 400/202/400 with one dedicated seven-day report; financial/user data excluded from telemetry GC |
| Actions delivery and public CLI artifacts | Exact-source CI/deploy gates; registry publication digests match; native release recovery 40072066883 verified the existing tag without moving it; both public archives' SHA-256, LF sidecars and `BUILD-INFO` were independently checked |

The release is [v0.1.0](https://github.com/kleedaisuki/moesegfault-agents/releases/tag/v0.1.0),
with Linux x86_64 and Windows x86_64 archives. Billing access uses authorized
administrator activation grants; there is no card checkout or automatic debit.
Current model metadata documents the supported providers and text-only Chat
Completions subset. Native tracing capture/sampling and application correlation
were verified; no dashboard screenshot is substituted for an unobserved native
trace waterfall. Unknown historical staging spending remains an explicit hold,
not a forged zero charge or a reason to destroy user data.

### Process-exit race regression (2026-10-24)

[CI run 40065762512](https://github.com/kleedaisuki/moesegfault-agents/actions/runs/40065762512),
source `7641204`, completed fourteen real daemon cases but returned HTTP 500 for
the single detached-`setsid` cleanup journey. The supervisor remained running and
subsequent cases passed; this is distinct from the earlier stale Docker port.
A successful prior fifteen-case run does not rule out this intermittent failure.

The suite now retains that original assertion and adds 120 distinct executions
across six shell/short-lived-child/`setsid` exit patterns. Every individual POST
must return 200 with normal completion; requests are not retried to manufacture
a pass. Each delayed writer targets a unique file, and a single final one-second
wait verifies none survived. Command and HTTP timeouts are three and ten seconds
respectively. Local collection and portable harness success alone did not
establish that the underlying cleanup defect was fixed.

The corrected source passed [full CI 40067168672](https://github.com/kleedaisuki/moesegfault-agents/actions/runs/40067168672)
at exact main revision `8caff37333cf148a62c804e75cc9189203e8f167`:

- [Native Rust job 120792787036](https://github.com/kleedaisuki/moesegfault-agents/actions/runs/40067168672/job/120792787036)
  passed all **33 daemon tests**, including the deterministic real-Linux sequence
  that opens a child's status handle, kills/reaps the child, and observes `ESRCH`
  rather than `ErrorKind::NotFound`. The same cleanup classifier accepts that
  disappeared-task outcome while rejecting permission/resource/unknown errors.
- [Container job 120792786991](https://github.com/kleedaisuki/moesegfault-agents/actions/runs/40067168672/job/120792786991)
  passed **16 real HTTP journeys in 4.73 seconds**, including both the original
  detached writer case and all 120 distinct stress executions with the no-writer
  assertion. Restart again observed loopback port `32768 -> 32769` and a new
  process boot identity.
- All other full-CI jobs succeeded. No failed command was automatically replayed
  and the original safety assertion was not weakened or skipped.

The source correction is grounded in a deterministic kernel-lifetime regression,
not merely repeating the old suite until green. The old failed CI logs lacked an
underlying errno, so they cannot retrospectively identify its exact syscall.
New daemon cleanup events are content-free static-stage/numeric-errno logs; this
CI fixture's sanitizer does not yet export those new event fields, and this run
does not claim to validate that diagnostic-export path. These results establish
the cleanup correction for the tested revision, not a substitute for the separate
deployed Identity/Billing and lifecycle acceptance gates.

## Server authentication security update

Both production hosts were deployed at
`a2059d550c8b0651df740428554e93fcb2bf04dd`. The bounded server-only update moves
confidential-client private ES256 arithmetic to native Workers Web Crypto without
changing registered keys, JWT semantics, external APIs, billing or session lifecycle.
There is no software private-signing fallback; the scoped RustSec assessment is in
[the independent review](review-staging.md).

- [Exact-source full CI 40076378439](https://github.com/kleedaisuki/moesegfault-agents/actions/runs/40076378439)
  passed every job. Eighteen actual-Worker/transport cases include independently
  verified callback, refresh and revocation assertions and invalid-scalar rejection
  before exchange or authenticated-session creation.
- [Staging deployment 40076671584](https://github.com/kleedaisuki/moesegfault-agents/actions/runs/40076671584)
  passed all fourteen real SDK journeys in 583.71 seconds. Owned-account BFF
  expired refresh, fresh callback and logout also passed, with separately observed
  upstream Identity refresh, authorization exchange and revocation returning 200.
- [Production deployment 40077421671](https://github.com/kleedaisuki/moesegfault-agents/actions/runs/40077421671)
  succeeded; both public health responses reported this exact build. The existing
  owned Free/live account passed expired BFF refresh, fresh callback, same-principal
  validation and logout followed by 401. A separate bounded global tail observed
  all three upstream Identity phases returning 200 in that owned test window;
  those phase events are not claimed to contain build or subject fields.
  No account, mailbox, API key, model request
  or allocation grant was created for this narrow production regression.

The public `v0.1.0` CLI archives and their immutable source tag remain unchanged:
native PKCE clients do not hold or use the BFF's private signing key. Prior real
workspace, unattended-clock, long-stream and production model acceptance remains
applicable to the unchanged runtime paths; those expensive probes were not replayed
merely for an isolated authentication implementation update.

## v0.2 portable agents and protocol BYOK: real staging acceptance

After [full CI 40090727036](https://github.com/kleedaisuki/moesegfault-agents/actions/runs/40090727036),
public staging health matched exact source
`43cf770020fd86b4b91cb29ff4ddc78ad1c1ca5e` before this independent journey.
The actual clean, locked, source-built Windows `magents 0.2.0` candidate had SHA-256
`f26fe42a5db46f86f29f6278b8f11e876b6a6ba6b6c26a5c9dafa3a92b328150`.
This is source-candidate acceptance, not verification of a later public archive.

- Actual anonymous deployed Chrome showed the separate public entry without a
  dashboard/sidebar flash or private agent/session/skill/key queries. English/light
  desktop and Chinese/dark mobile at 390 x 844 passed, without horizontal overflow;
  language and appearance survived reload. The ordinary owned Pro/test BFF login
  then succeeded. Its actual Access form exposed exactly two protocols, accepted
  only base URL and hidden API key with the optional name blank, returned 201 and
  cleared the secret input.
- Two fresh owner credentials exercised **real DeepSeek Flash** through both
  `https://api.deepseek.com/v1` and `https://api.deepseek.com/anthropic`.
  The latter verifies Anthropic-compatible Messages against DeepSeek, **not** an
  Anthropic commercial model. Both completed actual time-tool loops; the OpenAI
  route additionally used real Bash to write a unique `proof.txt` marker.
- The actual CLI performed TOML init/validation, create publication, same-agent
  update with increased revision, download and semantic round-trip using standard
  `tomllib`. Downloaded configuration omitted server identity/revision and real
  credentials. JSON, JSONL and Markdown exports preserved the complete source
  journals and tool calls/results: six OpenAI-route messages and four Anthropic-route
  messages. Pending lists matched the status snapshot; export did not advance the
  input clock or revision. This does not claim a nonempty live steering queue was
  exercised by this particular export journey.
- The active workspace download returned 409 and the disabled workspace 404.
  After normal close/checkpoint, the native private R2 stream downloaded as a
  **294-byte tar.zst**, SHA-256
  `6b0af38df7ee8bb0b6f78b6f5fa0781000bf2ac35e403f6c30fd37753eb641a5`.
  Individual-file download matched the exact Bash-written bytes. Existing output
  was never clobbered, missing files published no destination, parent traversal was
  rejected, and downloads neither resumed the session nor advanced user activity.
- Read-only financial inspection was restricted to the two new owned sessions:
  **103 ledger events, five model calls, zero platform model charge**, all model
  prices `byok`; resource/export charges totaled **530 microUSD**, with **zero
  unresolved reservations**. Historical fixture holds were neither changed nor
  represented as zero. Both new sessions closed, both agents were soft-deleted,
  both credentials deleted, and the short-lived API key revoked through ordinary
  APIs. The BFF logged out with CSRF, its browser closed, and protected recovery
  state was removed only after complete cleanup.

The safe receipt and public-entry screenshots are under
`.temp/v2-acceptance-20261024/f8ac315a-01bd-4896-bde9-720b6d7d24a8/`; the isolated
runner is `.temp/v2-acceptance-20261024/journey.mjs`. It requires explicit staging
build and same-source binary gates, never prints credentials, and does not retry
accepted execution. Two earlier harness-only errors (Chinese line-break matching
and using a hash jump instead of clicking Access) were retained and reconciled
before the successful journey; no model submission was replayed. Public native
archive verification and production compatibility/promotion remain separate
delivery-owner gates, not implied by this staged result.

## Accepted v0.2.0 release

The immutable `v0.2.0` implementation and serving production build are
`265e0ea8bd557d540077afced3bb20b735bbfdfc`.

| Gate | Authoritative result |
| --- | --- |
| [Full CI 40100503604](https://github.com/kleedaisuki/moesegfault-agents/actions/runs/40100503604) | All jobs passed, including 25 actual-Wasm/transport cases and the final projection-failure regression |
| [Staging 40100946077](https://github.com/kleedaisuki/moesegfault-agents/actions/runs/40100946077) | All fourteen real SDK journeys passed in 593.70 seconds, including long-context streaming compaction, steering and workspace restoration |
| [Production 40102985955](https://github.com/kleedaisuki/moesegfault-agents/actions/runs/40102985955) | Attempt 2 succeeded from the identical tested artifacts; both production health endpoints report the exact build |
| [Native packaging 40102140342](https://github.com/kleedaisuki/moesegfault-agents/actions/runs/40102140342) | Linux and Windows native tests/builds and attachment of all four public assets succeeded |

Production attempt 1 partially applied the release: migration 0010 and the Worker
upload completed, then Cloudflare's container application update returned an
internal error. The same-run failed-job retry reconciled that declaration and
completed secret synchronization, diagnostic lifecycle and public smoke checks.
No schema rollback, source replacement, key generation or allowance adjustment
was used. Production now has no pending migrations. Both publication logs report
container manifest digest
`sha256:9114bc4855b4227dd5f2116cbde291c8cc874b184d49e7634bc1c97336bce206`;
this is registry publication evidence, not an invented resolved-digest App field.

The deterministic old-Wasm fault was corrected without weakening its oracle:
while a fixture denies only the final D1 status projection, canonical journal and
three settled reservations remain readable, SSE still emits `stop` and `[DONE]`,
and releasing the fault lets an alarm repair D1 using database-only observation.
No model/tool replay occurs. Run-scoped transports and opaque acknowledgement
generations also cover late-input ownership and older-writer rollback aliases.
This fixes a proven lifecycle defect; it does not retroactively identify every
earlier cloud 500 or early EOF. The final real staging compaction additionally
had terminal/run correlation captured by a bounded content-free live collector.

The public Windows binary, rather than a debug candidate, then completed normal
production BFF/native identity and a fresh TOML publish/download roundtrip. Its
export target was the original v0.1-era Closed session: all twelve messages,
the 378-byte compressed snapshot hash and the known file bytes were unchanged.
Reads did not advance user input or create model/container usage; the one new
configuration was deleted and BFF logout returned subsequent 401. Full evidence
is in [Identity acceptance](identity-billing.md).

Both public archives have LF-only checksum sidecars, exactly the expected binary,
license and BUILD-INFO members, and source `265e0ea` / `refs/tags/v0.2.0`:

| Archive | SHA-256 |
| --- | --- |
| Windows x86_64 ZIP | `dc063fcedd902ed66d969031fd36f54897b68d4a3da33ca3fd7c5359c3b327d2` |
| Linux x86_64 tar.gz | `f5261d7bf2c46d91f9b03c75bdc7fca92d6fa39b1e6f3794db302e81152b1877` |

The public Windows executable hash is
`1b2f2fed96b9ff4dc6c41512031cb8408a35be526e876a4d4179111f1c0b93f4`;
its real `--version`, offline template/validation and production journeys passed.
The release is stable, not a draft or prerelease. Existing `v0.1.0` tags/assets
were not moved or replaced. Publication remains account-private, and subscription
management does not silently enable card collection or automatic renewal.
