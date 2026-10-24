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
test('lock changes exercise all Rust dependency consumers', () => {
  const result = affected(['Cargo.lock']);
  assert.ok(result.rust && result.worker && result.container);
});
test('first push and manual dispatch verify everything', () => {
  assert.ok(Object.values(affected([], true)).every(Boolean));
});
test('SDK observer contracts run tooling without rebuilding Rust', () => {
  for (const file of ['test_sdk_contract.py', 'test_deployed.py', 'stream_usage.py']) {
    assert.deepEqual(affected([`tests/e2e/${file}`]), { rust: false, worker: false, dashboard: false, container: false, tooling: true });
  }
});
test('shared Python fixture and locked requirements verify their real consumers', () => {
  for (const file of ['conftest.py', 'requirements.lock']) {
    assert.deepEqual(affected([`tests/e2e/${file}`]), { rust: false, worker: false, dashboard: false, container: true, tooling: true });
  }
});
