import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

export function requireDistinctEnvironmentDatabases(input) {
  const ids = ['staging', 'production'].map(environment => {
    const matches = input.env?.[environment]?.d1_databases?.filter(database => database.binding === 'DB') ?? [];
    if (matches.length !== 1 || !/^[a-f0-9]{8}(?:-[a-f0-9]{4}){3}-[a-f0-9]{12}$/i.test(matches[0].database_id ?? '')) throw new Error('Missing exact environment database identity');
    return matches[0].database_id.toLowerCase();
  });
  if (ids[0] === ids[1]) throw new Error('Staging and production database identities overlap');
}

export function renderDeploymentConfig(input, env) {
  const environment = env.DEPLOY_ENVIRONMENT;
  if (!['staging', 'production'].includes(environment)) throw new Error('Invalid deployment environment');
  const account = env.CLOUDFLARE_ACCOUNT_ID;
  const sha = env.GITHUB_SHA;
  const workerSource = env.WORKER_SOURCE_SHA ?? sha;
  const containerSource = env.CONTAINER_SOURCE_SHA ?? sha;
  if (!/^[a-f0-9]{32}$/.test(account ?? '') || !/^[a-f0-9]{40}$/.test(sha ?? '') || !/^[a-f0-9]{40}$/.test(workerSource ?? '') || !/^[a-f0-9]{40}$/.test(containerSource ?? '')) throw new Error('Invalid immutable image identity');
  const config = structuredClone(input);
  config.main = resolve(config.main);
  config.assets.directory = resolve(config.assets.directory);
  delete config.build;
  const target = config.env[environment];
  target.vars = { ...target.vars, BUILD_ID: workerSource, DEPLOYMENT_ASSEMBLY_SHA: sha };
  if (environment === 'staging' && env.ACCEPTANCE_PROFILE === 'responses-increment') {
    target.vars.HOSTED_TOOL_RUNTIMES = '';
    target.containers = target.containers.filter(container => container.class_name !== 'Session4G');
    target.vars.HOSTED_TOOL_CAPABILITIES_JSON = JSON.stringify({
      version: 1, platform_images: false, platform_search: false, file_search: false,
      container_memory: null, available_container_profiles: [],
      code_network_disabled: false, code_network_allowlist: false,
    });
  }
  for (const db of target.d1_databases) db.migrations_dir = resolve(db.migrations_dir);
  for (const container of target.containers) {
    container.image = `registry.cloudflare.com/${account}/magents-workspace:${containerSource}`;
    delete container.image_build_context;
  }
  return config;
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  const input = JSON.parse(readFileSync('wrangler.jsonc', 'utf8'));
  requireDistinctEnvironmentDatabases(input);
  const config = renderDeploymentConfig(input, process.env);
  mkdirSync('.temp', { recursive: true });
  writeFileSync('.temp/deploy.json', `${JSON.stringify(config, null, 2)}\n`);
}
