import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

test('native FileSearch driver cannot replay an existing intent before any network call', () => {
  const root = path.resolve(fileURLToPath(new URL('../../', import.meta.url)));
  fs.mkdirSync(path.join(root, '.temp'), { recursive: true });
  const directory = fs.mkdtempSync(path.join(root, '.temp/retrieval-intent-guard-'));
  const receipt = path.join(directory, 'original.json');
  const original = '{"original_unknown_operation":"preserved"}\n';
  fs.writeFileSync(receipt, original);
  try {
    const code = `let attempted=0;globalThis.fetch=()=>{attempted++;throw Error('network_forbidden')};try{await import(${JSON.stringify(new URL('./file-search-sdk.mjs', import.meta.url).href)});throw Error('guard_not_reached')}catch(error){if(error.code!=='EEXIST')throw error;process.stdout.write(JSON.stringify({attempted,code:error.code}))}`;
    const result = JSON.parse(execFileSync(process.execPath, ['--input-type=module', '-e', code], {
      cwd: root, encoding: 'utf8', env: { ...process.env, FILE_SEARCH_API_KEY: 'synthetic-never-dispatched',
        FILE_SEARCH_RECEIPT: receipt, FILE_SEARCH_TRACE_SEED: '123:1' }, stdio: ['ignore', 'pipe', 'pipe'], timeout: 10000,
    }));
    assert.deepEqual(result, { attempted: 0, code: 'EEXIST' });
    assert.equal(fs.readFileSync(receipt, 'utf8'), original);
  } finally {
    fs.unlinkSync(receipt);
    fs.rmdirSync(directory);
  }
});
