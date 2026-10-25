import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

export function renderDeploymentConfig(input, env) {
  const environment = env.DEPLOY_ENVIRONMENT;
  if (!['staging', 'production'].includes(environment)) throw new Error('Invalid deployment environment');
  const account = env.CLOUDFLARE_ACCOUNT_ID;
  const sha = env.GITHUB_SHA;
  const containerSource = env.CONTAINER_SOURCE_SHA ?? sha;
  if (!/^[a-f0-9]{32}$/.test(account ?? '') || !/^[a-f0-9]{40}$/.test(sha ?? '') || !/^[a-f0-9]{40}$/.test(containerSource ?? '')) throw new Error('Invalid immutable image identity');
  const config = structuredClone(input);
  config.main = resolve(config.main);
  config.assets.directory = resolve(config.assets.directory);
  delete config.build;
  const target = config.env[environment];
  target.vars = { ...target.vars, BUILD_ID: sha };
  for (const db of target.d1_databases) db.migrations_dir = resolve(db.migrations_dir);
  for (const container of target.containers) {
    container.image = `registry.cloudflare.com/${account}/magents-workspace:${containerSource}`;
    delete container.image_build_context;
  }
  return config;
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  const config = renderDeploymentConfig(JSON.parse(readFileSync('wrangler.jsonc', 'utf8')), process.env);
  mkdirSync('.temp', { recursive: true });
  writeFileSync('.temp/deploy.json', `${JSON.stringify(config, null, 2)}\n`);
}
