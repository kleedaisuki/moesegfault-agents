# Locate the change and verify its user journey

Start in the user's intended checkout. These paths are relative to that repository,
not to the installed skill folder.

| Boundary | Implementation | Focused evidence |
| --- | --- | --- |
| Domain/context contracts | `crates/agents-core/src/` | Native tests and property tests |
| Identity, BFF and credential envelopes | `crates/agents-auth/src/`, Worker `auth*.rs` / `provider_keys.rs` | Token/envelope tests; real registered login when affected |
| HTTP, DO harness, pricing, projections | `crates/agents-worker/src/` | Native logic plus compiled-Wasm `tests/worker/` journeys |
| Atomic admission and migrations | `migrations/`, production SQL in Worker modules | `tests/test_*sql.py` |
| Bash, checkpoint, restore | `crates/workspace-daemon/`, `container/` | Linux native and actual Docker journeys |
| CLI and portable configurations | `crates/magents/`, `examples/` | Actual-binary `crates/magents/tests/` and offline TOML roundtrip |
| Dashboard | `apps/dashboard/` | Interaction/build and real browser `e2e/` cases |
| Packaging and deployment | `scripts/`, `.github/workflows/`, `wrangler.jsonc` | Contract tests and immutable artifact gates |

Read the matching document opening and concrete code before proposing an edit.
Do not apply historical acceptance receipts as commands to redeploy an old SHA.

## Select the smallest informative checks

Read `rust-toolchain.toml` and use the lockfile. Prefer Actions and its Cargo/npm/
BuildKit caches for the full Rust/Wasm, browser and container matrices. Inspect
available caches before local compilation; do not cold-build the workspace to
validate a Markdown or UI-text change. A targeted local check is useful when its
cost and effect fit the current machine.

Examples, from the checkout, when relevant:

```sh
cargo test --locked -p agents-core
cargo test --locked -p magents
python -m unittest discover -s tests -p 'test_*sql.py' -q
npm run test:tooling
```

`npm run test:worker` requires an already built Worker artifact. A native unit
test or wasm type check does not prove `worker-build` packaging or workerd HTTP
behavior. Do not run the root workspace daemon directly with elevated privileges
on a developer host: its UID-wide cleanup is designed for a dedicated container.

Use a property test where an invariant spans values or interleavings: pinned
prefix stability, exact credit conservation, opaque-generation acknowledgement,
or archive roundtrips. Add a regression that fails for the demonstrated defect,
not a wording check or a weakened assertion.

For an affected user flow, exercise the actual binary/SDK/browser and its real
protocol boundary. Mock transport first; use an authorized isolated account for
necessary hosted acceptance. Observe accepted work before retrying; retain an
unknown outcome rather than deleting state to make cleanup look successful.

Full map: [architecture](https://github.com/kleedaisuki/moesegfault-agents/blob/main/docs/architecture.md)
and [acceptance](https://github.com/kleedaisuki/moesegfault-agents/blob/main/docs/acceptance.md).
