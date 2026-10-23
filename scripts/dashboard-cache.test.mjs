import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

test('immutable cache rule covers only fingerprinted assets, never HTML/login/private API', () => {
  const source = readFileSync(new URL('../apps/dashboard/public/_headers', import.meta.url), 'utf8');
  const lines = source.split(/\r?\n/).filter(line => line.trim());
  assert.deepEqual(lines.filter(line => !/^\s/.test(line)), ['/assets/*']);
  assert.deepEqual(lines.filter(line => /^\s/.test(line)).map(line => line.trim()), [
    'Cache-Control: public, max-age=31536000, immutable',
  ]);
});
