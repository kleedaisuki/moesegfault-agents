import { test } from 'node:test';
import assert from 'node:assert/strict';
import { affected } from './ci-changes.mjs';

test('documentation does not rebuild the platform', () => {
  assert.ok(Object.values(affected(['docs/architecture.md', 'README.md'])).every((x) => !x));
});
test('dashboard does not rebuild Rust or containers', () => {
  assert.deepEqual(affected(['apps/dashboard/src/App.tsx']), { rust: false, worker: false, dashboard: true, container: false, tooling: false });
});
test('shared contracts verify native and wasm without container rebuild', () => {
  const result = affected(['crates/agents-core/src/session.rs']);
  assert.ok(result.rust && result.worker && !result.container);
});

test('schema changes exercise SQL and real Worker migrations without rebuilding containers', () => {
  assert.deepEqual(affected(['migrations/0011_spaces.sql']), { rust: true, worker: true, dashboard: false, container: false, tooling: false });
});
test('lock changes exercise all Rust dependency consumers', () => {
  const result = affected(['Cargo.lock']);
  assert.ok(result.rust && result.worker && result.container);
});
test('first push and manual dispatch verify everything', () => {
  assert.ok(Object.values(affected([], true)).every(Boolean));
});
test('SDK observer contracts run tooling without rebuilding Rust', () => {
  for (const file of ['test_sdk_contract.py', 'test_responses_contract.py', 'test_responses.py', 'test_space.py', 'test_deployed.py', 'stream_usage.py', 'responses-sdk.mjs']) {
    assert.deepEqual(affected([`tests/e2e/${file}`]), { rust: false, worker: false, dashboard: false, container: false, tooling: true });
  }
});
test('shared Python fixture and locked requirements verify their real consumers', () => {
  assert.deepEqual(affected(['tests/e2e/conftest.py']), { rust: false, worker: false, dashboard: false, container: true, tooling: true });
  for (const file of ['requirements.txt', 'requirements.lock']) {
    assert.deepEqual(affected([`tests/e2e/${file}`]), { rust: false, worker: true, dashboard: false, container: true, tooling: true });
  }
});

test('actual Worker SDK-schema validators verify both Wasm output and fixture contracts', () => {
  for (const file of ['response_checks.py', 'validate_responses_wire.py']) {
    assert.deepEqual(affected([`tests/e2e/${file}`]), { rust: false, worker: true, dashboard: false, container: false, tooling: true });
  }
});

test('container artifact policy changes exercise the real image promotion path', () => {
  for (const file of ['container-artifact.mjs', 'container-artifact.test.mjs']) {
    assert.deepEqual(affected([`scripts/${file}`]), { rust: false, worker: false, dashboard: false, container: true, tooling: true });
  }
});
