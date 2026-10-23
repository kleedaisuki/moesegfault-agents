import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { resolve } from 'node:path';

const environment = process.env.DEPLOY_ENVIRONMENT;
if (!['staging', 'production'].includes(environment)) throw new Error('Invalid deployment environment');
const account = process.env.CLOUDFLARE_ACCOUNT_ID;
const sha = process.env.GITHUB_SHA;
if (!/^[a-f0-9]{32}$/.test(account ?? '') || !/^[a-f0-9]{40}$/.test(sha ?? '')) throw new Error('Invalid immutable image identity');
const config = JSON.parse(readFileSync('wrangler.jsonc', 'utf8'));
config.main = resolve(config.main);
config.assets.directory = resolve(config.assets.directory);
delete config.build;
const target = config.env[environment];
for (const db of target.d1_databases) db.migrations_dir = resolve(db.migrations_dir);
for (const container of target.containers) {
  container.image = `registry.cloudflare.com/${account}/magents-workspace:${sha}`;
  delete container.image_build_context;
}
mkdirSync('.temp', { recursive: true });
writeFileSync('.temp/deploy.json', `${JSON.stringify(config, null, 2)}\n`);
