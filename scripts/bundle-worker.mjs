import { mkdirSync, writeFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { spawnSync } from 'node:child_process';

mkdirSync('.cache', { recursive: true });
const config = {
  name: 'agents-bundle-check',
  main: resolve('crates/agents-worker/build/worker/shim.mjs'),
  compatibility_date: '2026-10-23',
};
writeFileSync('.cache/bundle-check.json', JSON.stringify(config));
const result = spawnSync('npx', ['wrangler', 'deploy', '--dry-run', '--config', '.cache/bundle-check.json', '--outdir', '.cache/worker-bundle'], {
  stdio: 'inherit', shell: process.platform === 'win32',
});
process.exit(result.status ?? 1);
