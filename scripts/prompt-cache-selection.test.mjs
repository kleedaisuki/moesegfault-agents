import assert from 'node:assert/strict';
import test from 'node:test';
import { selections } from './incremental-checks.mjs';

test('prompt-cache increments select only the four new compiled native journeys', () => {
  const selected = selections('a'.repeat(40), 'b'.repeat(40), ['tests/worker/prompt-cache-journey.mjs']);
  assert.deepEqual(selected.worker.map(check => check.env.MAGENTS_WORKER_CASE), [
    'Responses cache context appends instruction changes and reset across cold continuation and exact count',
    'Responses cache context tool revisions preserve historical forks without revoked aliases',
    'Responses cache context compaction retains the latest revision and resets without stale policy',
    'Responses cache explicit Session counting preserves legacy and versioned pins without admission',
  ]);
  assert.ok(selected.worker.every(check => check.argv.at(-1) === 'tests/worker/http.test.mjs'));
  assert.equal(Object.entries(selected).filter(([name]) => name !== 'worker').flatMap(([, checks]) => checks).length, 0);
});
