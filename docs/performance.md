# User-path performance

The target is a responsive first usable result, not an isolated Rust function's
throughput. Keep model latency, orchestration latency, workspace startup and client
rendering separate. All live probes in this document target **staging**, never
production. A fixture pass establishes the probe's behavior, not a deployment SLO.

## Workloads and measurements

| Journey | Observe | Important control |
| --- | --- | --- |
| Open dashboard / `magents agents list` | authenticated request count, payload, total latency | same account, region, history size |
| New SDK session | response headers, first event, first text, total time | same registered provider/model, short fixed answer |
| Continue session | same metrics plus cached/prompt tokens | pinned instructions/tools/skills; append only new input |
| Harness time tool | total latency across two model calls and dispatch | verify actual tool result in acceptance tests |
| First / second bash | container readiness, restore, execution, checkpoint bytes/time | same image/workspace; distinguish warm container from warm model cache |
| Idle / background browser tab | requests per minute, session discovery delay | running vs idle vs closed; visibility and error state |
| CLI startup | `--help` and authenticated command wall time | release binary, same machine; OS credential store vs API key |

`first_event_ms` can be only a role/status event; **first_text_ms** is the user-visible
time to first token (TTFT). Headers do not prove inference started. Total includes
durable terminal handling and stream completion. Compare distributions and failure
rates, not only a favorable request. Three samples locate large stalls; their p95
is a nearest-rank maximum, not a statistically reliable tail estimate.

### Reproduce bounded staging probes

Use the existing pinned acceptance dependencies. On Windows replace `bin/python`
with `Scripts/python.exe`. Keep API keys out of command arguments and artifacts.

```sh
python3 -m venv .cache/e2e-venv
.cache/e2e-venv/bin/pip install -r tests/e2e/requirements.lock
export MAGENTS_TEST_API_ORIGIN=https://agents-staging.moesegfault.dev
# Supply MAGENTS_TEST_API_KEY through the environment / secret store.
.cache/e2e-venv/bin/python scripts/perf/measure.py --samples 3
.cache/e2e-venv/bin/python scripts/perf/measure.py --samples 3 --inference \
  --provider openrouter --model deepseek/deepseek-v4.1-flash-1017 \
  --output .cache/perf/after.json
# Optional workspace lifecycle adds three more billed bash turns per sample.
.cache/e2e-venv/bin/python scripts/perf/measure.py --samples 1 --inference --workspace \
  --output .cache/perf/workspace.json
# Optional: use the same release artifact distributed to users.
.cache/e2e-venv/bin/python scripts/perf/measure.py --samples 3 \
  --cli target/release/magents --output .cache/perf/cli.json
```

`--inference` explicitly enables **three billed turns per sample**: new session,
continuation and time-tool request. The probe creates its own agent and closes
sessions/deletes that agent. Cleanup failure makes the command fail and appears
in its artifact; a run still executing may need later operator cleanup. Use a
dedicated funded staging account, not a user's real workspace. SDK retries are
disabled so failures and retry delays are not concealed. No prompt, output, key,
session ID, JWT or response body is persisted. The artifact records package,
platform and repository revisions; preserve the deployed commit alongside it.
With no model flags the server default is used and its normalized model/provider
are recorded from the created configuration. The 2026-10-23 staging default is
OpenRouter's `deepseek/deepseek-v4.1-flash-1017`, with registered input/cache/output
prices of $0.30/$0.006/$1.20 per million tokens (`openrouter-2026-10-23`). The direct
DeepSeek registry entry is not by itself evidence of configured platform-funded
pricing or a provisioned direct-provider secret; do not benchmark it by assumption.

For comparable before/after results alternate revisions or use repeated A/B runs
close together from the same client. Neither a fresh session nor a fresh HTTP
client guarantees a cold Worker/container/provider cache. The first management
request is recorded separately and excluded from that endpoint's warm sample.
The pooled HTTP client and the OpenAI SDK use separate pools, as real clients do.
Cache construction can take seconds: add an explicit recorded inter-turn pause
when studying provider cache persistence rather than ordinary immediate turns.
The optional workspace path records cold/warm bash, archive, resume and restored
bash independently; it requires an actual new tool result containing the random
file marker after warm execution and restore, not a model's remembered answer.

## Implemented critical-path changes

### Authoritative authentication without write amplification

`crates/agents-worker/src/auth.rs` keeps every API-key revocation check in D1.
Only the informational `last_used_at` field is now coarsened to five minutes.
The read returns both the owner and last-use timestamp; most requests skip the
second awaited write. The conditional update rechecks revocation and timestamp,
so concurrent old reads cannot repeatedly overwrite a newer timestamp. No
positive token cache delays revocation, and quotas/balances are never cached.

An existing native OIDC principal and wallet now resolve with one indexed join,
instead of two conflict-ignored inserts around a lookup on every request. Missing
accounts/wallets use one atomic D1 batch, then resolve the winning principal.
The join deliberately misses an incomplete legacy account, allowing safe repair.
Billing refresh and allowance synchronization remain separate authoritative work.

Comparable local SQL workload, 2026-10-23, Windows/Python 3.14, in-memory SQLite
3.50.4, one connection and one requester, migrated real schema:

| Workload | Before statements | After statements | Before actual key updates | After actual key updates |
| --- | ---: | ---: | ---: | ---: |
| 100 API-key requests, 3 seconds apart over 297 seconds | 200 | 101 | 100 | 1 |
| 100 existing native-principal resolutions | 300 | 100 | — | — |

These are executed SQL counts, **not** measured Cloudflare latency improvements.
The local run was respectively 0.292 → 0.150 ms and 0.414 → 0.144 ms for the
entire workloads; those tiny in-memory timings do not model D1 network cost.
The useful mechanism is 99 fewer key writes and 200 fewer principal round trips.

```sh
python scripts/perf/auth_workload.py
python -m unittest discover -s tests -p test_auth_sql.py -v
cargo check -p agents-worker --target wasm32-unknown-unknown
.cache/e2e-venv/bin/python -m pytest scripts/perf/test_measure.py -q \
  -o cache_dir=.cache/pytest --basetemp=.temp/perf-pytest
```

The SQL tests extract actual production statements, not lookalike fixtures.
They cover immediate revocation, the five-minute boundary, racing timestamp
updates, repeated bootstrap, issuer separation, partially missing wallets and
transaction rollback on injected wallet failure. Probe tests feed delayed SSE
chunks through an unmodified SDK and verify first text precedes completion.

### Selected-agent session discovery

`GET /v1/agents/{id}/sessions` previously loaded the first 1,000 **owner-wide**
snapshots, then filtered in Rust. Besides overfetch, another agent's history could
push all of the selected agent's sessions beyond that limit. The query now filters
both owner and agent before its unchanged 1,000-record limit. Migration `0005`
adds `(account_id, agent_id, created_at DESC, id)` so SQLite can satisfy filtering
and stable ordering without a temporary sort. The global list contract is unchanged.

The actual-SQL regression fixture creates 1 old selected-agent session and 1,001
newer unrelated sessions. The previous path fetches 1,000 rows and returns none;
the new path fetches and returns the one correct row. `EXPLAIN QUERY PLAN` confirms
the new index and no temporary B-tree. Cross-owner and tied-timestamp ordering
tests pass. This removes a concrete discovery bug rather than adding a cache.

```sh
python -m unittest discover -s tests -p test_session_sql.py -v
```

### Runtime, workspace and dashboard

The runtime owner is eliminating artificial one-alarm-per-state-transition gaps,
while journaling external results before the next side effect. Alarms remain the
durable recovery mechanism, not an intentional per-step sleep. Their at-least-once
contract means any latency change must retain tool IDs, billing idempotency and
steering interleavings. Workspace ownership is a funded warm lease: a successful
turn should not imply checkpoint/release/restore before its immediate follow-up.
Checkpoint dirty batches before intentional release and never destroy a container
when publishing that checkpoint fails. See [runtime-workspaces.md](runtime-workspaces.md).

The dashboard owner uses lazy Access/Usage panels, running/idle-aware polling,
backoff on transient failures and no background-tab interval polling. Initial
account authentication gates private panel fetches. Do not prefetch secret-key
management just to make a later click look fast. Keep language/theme changes
client-side; they must not invalidate account, agent or model query caches.

Dashboard production build measurements supplied by its owner and current emitted
assets verified locally (decimal kB; Vite's gzip estimates):

| Asset | Before i18n/theme work | After i18n/theme + lazy panels + font CSS narrowing |
| --- | ---: | ---: |
| Main JS, gzip | 86.08 kB | 110.54 kB |
| Main CSS, gzip | 6.77 kB | 4.19 kB |
| Skills JS, gzip, deferred | not present | 1.33 kB |
| Access JS, gzip, deferred | in main | 1.76 kB |
| Usage JS, gzip, deferred | in main | 1.20 kB |
| All emitted font files, uncompressed | 127.58 kB | 61.77 kB |

Internationalization/streaming/skills add about 24 kB compressed to the main JS: this is an
explicit feature trade-off, not a claim that every bundle shrank. Emitted fonts
are **not equivalent to transferred fonts**: CSS `unicode-range` can already defer
unused subsets, so the smaller output directory alone does not prove faster FCP.
Browser request traces and real network emulation are needed for that conclusion.
The owner reports six Chromium journeys across English/Chinese, light/dark and
320/360/1280 px viewports, with one account fetch through the full navigation flow.
Idle submissions now consume incremental SSE through `eventsource-parser`; partial
text renders before the completion marker and active polling pauses while attached.
Concurrent steering remains a queued content append, not a second execution. The
19 client unit tests include fragmented UTF-8 SSE, partial rendering, truncation
and errors. This establishes the streaming mechanism, not a quantified live TTFT
improvement over the previous terminal-snapshot-only display.

Selected-session polling is 2 seconds when running and 5/15/30 seconds as idle
input age increases, and stops on closed/401/403/404. For a visible session already
idle at least a minute, the steady-state upper bound falls from the former
3-second cadence's 20 requests/minute to 2 requests/minute (90% fewer). This is
calculated scheduling work, not measured backend throughput. Running cadence is
deliberately 50% more frequent for a shorter feedback delay. Hidden-window polling
remains disabled; that was already a TanStack behavior, not a newly fixed bug.

The CLI already owns one `reqwest::Client` per `Api`; retain that shared pool and
incremental SSE stdout flushing rather than recreate clients per request or
buffer the whole answer. Login/refresh discovery is not required for an unexpired
OS-stored token. Do not trade stream responsiveness for an unconditional whole-run
HTTP timeout or retry side-effecting prompt submissions automatically.

## Investigation order and guardrails

1. Find an observed multi-second stall: first headers, alarm dispatch, provider
   first text, tool startup or checkpoint publication. Correlate the request with
   native Workers tracing, not added console output containing user content.
2. Count serialized D1/R2/auth/billing calls on that exact path before introducing
   caching. Prefer removing redundant work and batching independent statements.
3. Compare cold session, warm turn and warm workspace separately. Always report
   prompt growth and cached-token ratio; a better cache-hit metric does not imply
   better total latency when the conversation grows or output gets longer.
4. Only add another cache when its consistency contract is clear. Deployment-owned
   registries and immutable pinned prefixes are safe reuse candidates. Revocation,
   wallet balances, admission and mutable cross-session state are not.
5. Watch the cost curve: polling, warm leases and observation writes can make an
   apparently faster UI substantially more expensive. Cost and tail latency are
   separate guardrails, not afterthoughts.

No production benchmark or staging end-to-end speedup is claimed by the local
SQL/probe-instrumentation results above. Record real staging runs here when the
matching deployed revision and dedicated credentials are available.

### Deployed public dashboard assets, 2026-10-24

A credential-free HTTP/1.1 probe at 00:25 Singapore time fetched only the staging
HTML, its JS/CSS, and fonts. It made no account/API requests and did not compete
with authenticated acceptance quotas. Actual compressed transfer sizes were
109,114 B for `index-Mqk5BhS8.js` (350,493 B decoded) and 4,142 B for
`index-DZ9Q8rn9.css` (16,167 B decoded); both Latin fonts totaled 61,768 B.

Every fingerprinted asset had `Cache-Control: public, max-age=0, must-revalidate`.
The first/second observed JS header times were 797.6/187.4 ms and CSS 286.2/92.6 ms,
with reported edge MISS/HIT respectively. These are single observations on one
pooled client, not a latency distribution or proof that cache state explains all
of the difference. Cloudflare notes its cache-status header is not infallible.

The fix is the narrow `public/_headers` rule: `/assets/*` receives
`public, max-age=31536000, immutable`. Those URLs are content-hashed, so a changed
deployment gets new URLs while repeat navigation can reuse unchanged bytes
without network revalidation. HTML, stable `appearance.js`, login, and private
API data are not included. Source scope and build copying are tested; recheck the
served staging header after the next deployment before claiming this live fix.
The original artifact is `.cache/perf/staging-public-assets.json`.

## Primary sources and implications

- [Cloudflare Durable Object rules](https://developers.cloudflare.com/durable-objects/best-practices/rules-of-durable-objects/)
  and [alarms](https://developers.cloudflare.com/durable-objects/api/alarms/):
  state ownership, storage gates, at-least-once wakeups and single-alarm scheduling.
- [DeepSeek context caching](https://api-docs.deepseek.com/guides/kv_cache/):
  stable complete prefixes, observed hit tokens and best-effort asynchronous cache
  construction. Pinning preserves opportunity; it cannot promise provider residency.
- [reqwest Client](https://docs.rs/reqwest/0.12/reqwest/struct.Client.html):
  connection-pool reuse is built into the mature client, not an application cache.
- [TanStack Query defaults](https://tanstack.com/query/latest/docs/framework/react/guides/important-defaults):
  set freshness, focus and polling deliberately instead of stacking custom caches.
- [SGLang / RadixAttention](https://arxiv.org/abs/2312.07104): reusable prefixes
  are a meaningful optimization for multi-call agent programs. Here inference is
  outsourced, so preserve deterministic prefix bytes rather than implement a KV
  cache or a GPU scheduler inside the Worker.
- [LLM Query Scheduling with Prefix Reuse and Latency Constraints, NeurIPS 2025](https://proceedings.neurips.cc/paper_files/paper/2025/hash/878bbdf6227315995c207561211ddb53-Abstract-Conference.html):
  prefix reuse and latency/fairness objectives can conflict. Measure user TTFT and
  tail latency as well as hit rate; do not queue independent sessions indefinitely
  merely to improve prefix matching.
