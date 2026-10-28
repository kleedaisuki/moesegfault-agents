import assert from 'node:assert/strict';
import test from 'node:test';
import { selections } from './incremental-checks.mjs';

test('Agent configuration increments select only their new current-Session journeys', () => {
  const selected = selections('a'.repeat(40), 'b'.repeat(40), ['tests/worker/session-config-journey.mjs']);
  assert.deepEqual(selected.worker.map(check => check.env.MAGENTS_WORKER_CASE), [
    'Agent revisions append to an existing Session and survive compaction and cold recovery',
    'Busy Agent revisions revoke queued tools only at safe execution boundaries',
    'Agent revision historical forks preserve sealed context before adopting new configuration',
    'Issued workspace effects remain unknown instead of revoked or replayed after Agent edits',
  ]);
  assert.ok(selected.worker.every(check => check.argv.at(-1) === 'tests/worker/http.test.mjs'));
  assert.equal(selected.worker.some(check => check.env.MAGENTS_WORKER_CASE.startsWith('Responses cache')), false,
    'Previously accepted prompt-cache cases are not replayed for this distinct increment');
  assert.equal(Object.entries(selected).filter(([name]) => name !== 'worker').flatMap(([, checks]) => checks).length, 0);
});
