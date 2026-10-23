import { spawnSync } from 'node:child_process';

const environment = process.argv[2];
if (!['staging', 'production'].includes(environment)) throw new Error('Choose staging or production');
const secrets = Object.fromEntries(
  ['IDENTITY_CLIENT_PRIVATE_JWK', 'DATA_ENCRYPTION_KEY', 'OPENROUTER_API_KEY', 'DEEPSEEK_API_KEY']
    .filter((name) => process.env[name])
    .map((name) => [name, process.env[name]]),
);
for (const required of ['IDENTITY_CLIENT_PRIVATE_JWK', 'DATA_ENCRYPTION_KEY']) {
  if (!secrets[required]) throw new Error(`Missing ${required}; refusing an unusable deployment`);
}
const config = process.env.DEPLOY_CONFIG ?? 'wrangler.jsonc';
const result = spawnSync('npx', ['wrangler', 'secret', 'bulk', '--env', environment, '--config', config], {
  input: JSON.stringify(secrets), encoding: 'utf8', shell: process.platform === 'win32',
});
if (result.stdout) process.stdout.write(result.stdout);
if (result.stderr) process.stderr.write(result.stderr);
process.exit(result.status ?? 1);
