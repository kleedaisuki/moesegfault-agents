import assert from 'node:assert/strict';
import { test } from 'node:test';
import { selections } from './incremental-checks.mjs';

test('execution leases select their real actor journey without replaying the auxiliary baseline', () => {
  const source = 'a'.repeat(40), base = 'b'.repeat(40);
  const selected = selections(source, base, [
    'tests/worker/execution-lease-journey.mjs',
    'tests/worker/auxiliary-journey.mjs',
  ]);
  assert.deepEqual(selected.worker.map(check => check.env.MAGENTS_WORKER_CASE), [
    'ModelOperation execution lease cold loss and terminal admission rejection',
  ]);
  assert.equal(selections(source, base, ['tests/worker/auxiliary-journey.mjs']).worker[0].env.MAGENTS_WORKER_CASE,
    'Responses auxiliary compact and count');
});
