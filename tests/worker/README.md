# Compiled Rust Worker integration

Run after `npm ci` and `npm run build:worker`:

```sh
node --test tests/worker/*.test.mjs
```

To reuse a downloaded GitHub Actions `worker` artifact instead of building locally:

```sh
WORKER_BUILD_DIR=.cache/integration/worker-fixed node --test tests/worker/*.test.mjs
```

The selected directory must contain `worker/shim.mjs`, `index.js`, and
`index_bg.wasm`. Node and Python are required; migration parsing uses the
standard-library SQLite module.
Override `PYTHON` if its executable is not named `python`.

Responses wire checks also require the locked Python SDK environment:

```sh
python3 -m venv .cache/e2e-venv
.cache/e2e-venv/bin/pip install -r tests/e2e/requirements.lock
MAGENTS_SCHEMA_PYTHON=.cache/e2e-venv/bin/python node --test tests/worker/*.test.mjs
```

For a focused shared-fixture diagnosis, set `MAGENTS_WORKER_CASE` to a subjourney
name substring and select `http.test.mjs`. This does not replace a complete gate;
CI leaves that filter unset. The independent hard-crash test forcibly terminates
only its owned Node/workerd process group and restores persistent storage in a
new process. Idle DO eviction cannot simulate a crash with active I/O references.

This runs the actual compiled Rust Worker inside workerd through Miniflare, with
real D1, SQLite-backed Session/AuthSession Durable Objects, R2, and rate limit
bindings. All repository migrations are applied in order; SQLite itself identifies
complete migration statements, including triggers. Identity metadata is synthetic
and outbound requests outside the fixture are rejected. No model calls or deployed
resources are used. Runtime files stay under `.temp/worker-integration-*`.

Coverage includes API-key authentication and revocation, first-use integer writes
across wasm-bindgen/D1, browser-session crypto/AuthSession authentication, key and
BYOK management, OIDC transaction creation/denial, logout, agent CRUD, session
prefix pinning after agent edits, DO status/close HTTP responses, and a complete
non-streaming SDK-style inference -> harness time tool -> inference journey.
The provider fixture emits real SSE over the Worker outbound fetch interface;
the harness, tool execution, alarms, reservations, and settlement are not mocked.

## Regression evidence

On 2026-10-23, the `worker` artifact from CI run `39978313937` returned HTTP 500
for first use of a valid API key whose `last_used_at` was NULL. The same migrated
fixture against CI run `39980574485` returned 200 and persisted SQLite `integer`.
The latter also passed actual AuthSession-based API-key creation/revocation and
BYOK creation/update. Broader requests exposed immutable-header mutation in
DO forwarding and the OIDC login redirect; these must remain failing assertions
until the production fixes are compiled, not be changed into expected errors.

An added continuation check reproduced the staging hang on the latter artifact:
after eight seconds, provider calls, reservations, and ledger entries were all
zero. Root-cause source inspection found that worker-rs `set_alarm(i64)` accepts
an offset, while the application supplied epoch milliseconds. The scheduled
alarm was therefore decades in the future. The test retains its bounded
eight-second deadline and two-provider-call/settled-model-ledger assertions to
verify the absolute-date scheduling repair end to end. The browser fixture also
arms a real three-second AuthSession expiry and checks physical D1 deletion within
five seconds; merely returning an expired-session error is insufficient.

CI run `39984214018` confirmed the first-use ABI, browser auth, actual AuthSession
expiry alarm, OIDC redirect, agent CRUD, and DO status/close checks pass. Model
execution advanced beyond admission but returned 500. The next fixture revision
includes OpenRouter's required numeric `usage.cost` and emits only allowlisted
session/accounting diagnostics on failure. Every mocked model round reports
$0.00001 and the complete two-run journey asserts four ledger entries of exactly
10 micro-USD each. After non-streaming and streaming runs, the test evicts the
actual Session DO and checks that reconstruction preserves the message journal.

The compiled artifact from CI `39985741803` isolated the next failure: workerd
rejects constructing a Request with `redirect: "error"`; only `follow` and `manual`
are supported. A diagnostic-only Request wrapper changing that option to `manual`
made the complete happy path, streaming continuation, exact charges, and real DO
eviction/reconstruction pass locally. This counterfactual is not acceptance of a
new Rust build. The artifact was restored afterward. A real 302 provider response
is now also tested: one trusted-endpoint call, zero redirect-target calls, and a
typed 502 error. The callback's Node Request `redirect` property cannot substitute
for that behavioral assertion because Miniflare reconstructs it as `follow` at
the host-service boundary.

## Native transport platform contract

`node --test tests/worker/transport.test.mjs` does not require a Rust build. It
uses a test-only JavaScript Worker and a real workerd service binding to verify
that a native Request's AbortSignal cancels both stalled response headers and
stalled body consumption. Successful bodies remain usable. A negative control
drops the signal and demonstrates that the 40 ms deadline no longer interrupts
the 1,200 ms body stall. This validates the platform mechanism, not the compiled
Rust workspace transport, Bash process cancellation, or dispatch recovery.

## Resume wire-format and retry contract

The compiled-Worker journey archives an unfunded, workspace-disabled session,
resumes it, and repeats the same public resume request. The pinned prefix remains
unchanged; only the first resume advances input revision/time. Public session
listing must still deserialize, and D1 JSON types for revision/time must be
`integer`, not `real`. During mocked model execution, a resume request must return
409 without changing D1's admission/snapshot state. CI `39989070343`'s artifact
reproduces the original first-resume HTTP 500 with this fixture; acceptance of the
repair requires a newly compiled artifact.
