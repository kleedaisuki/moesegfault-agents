import assert from 'node:assert/strict';
import test from 'node:test';
import { selections } from './incremental-checks.mjs';

test('new Code context and write journeys use the actual container job', () => {
  const selected = selections('a'.repeat(40), 'b'.repeat(40), [
    'tests/e2e/test_workspace_code_contexts.py',
    'tests/e2e/test_workspace_code_writes.py',
  ]);
  assert.equal(selected.container.length, 2);
  assert.equal(selected.tooling.length, 0);
  assert.ok(selected.container.every(check => check.argv.includes('pytest')));
});
