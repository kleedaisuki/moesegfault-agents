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
`index_bg.wasm`. Node and Python (standard-library SQLite only) are required.
Override `PYTHON` if its executable is not named `python`.

This runs the actual compiled Rust Worker inside workerd through Miniflare, with
real D1, SQLite-backed Session/AuthSession Durable Objects, R2, and rate limit
bindings. All repository migrations are applied in order; SQLite itself identifies
complete migration statements, including triggers. Identity metadata is synthetic
and outbound requests outside the fixture are rejected. No model calls or deployed
resources are used. Runtime files stay under `.temp/worker-integration-*`.

Coverage includes API-key authentication and revocation, first-use integer writes
across wasm-bindgen/D1, browser-session crypto/AuthSession authentication, key and
BYOK management, OIDC transaction creation/denial, logout, agent CRUD, session
prefix pinning after agent edits, and DO status/close HTTP responses.

## Regression evidence

On 2026-10-23, the `worker` artifact from CI run `39978313937` returned HTTP 500
for first use of a valid API key whose `last_used_at` was NULL. The same migrated
fixture against CI run `39980574485` returned 200 and persisted SQLite `integer`.
The latter also passed actual AuthSession-based API-key creation/revocation and
BYOK creation/update. Broader requests exposed immutable-header mutation in
DO forwarding and the OIDC login redirect; these must remain failing assertions
until the production fixes are compiled, not be changed into expected errors.
