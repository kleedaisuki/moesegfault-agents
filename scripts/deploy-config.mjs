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

export function deploymentRuntime(env) {
  const features = env.RUNTIME_FEATURES ?? 'configured';
  if (!['configured', 'none', 'web_search', 'file_search', 'web_search,file_search', 'web_search,file_search,code_interpreter', 'web_search,file_search,code_interpreter,image_generation', 'web_search,file_search,image_generation'].includes(features)) throw new Error('Invalid runtime feature allowlist');
  if (features !== 'configured' && env.DEPLOY_ENVIRONMENT !== 'staging') throw new Error('Runtime feature overrides are staging-only');
  const limited = env.DEPLOY_ENVIRONMENT === 'staging' && (features !== 'configured' || env.ACCEPTANCE_PROFILE === 'responses-increment');
  const workerFeatures = limited && features !== 'none' && features !== 'configured' ? features.split(',') : [];
  return { features, limited, workerFeatures, legacyContainerCompatible: limited && !workerFeatures.some(feature => feature === 'code_interpreter' || feature === 'image_generation'), webSearch: workerFeatures.includes('web_search'), fileSearch: workerFeatures.includes('file_search'), codeInterpreter: workerFeatures.includes('code_interpreter'), imageGeneration: workerFeatures.includes('image_generation') };
}

// Seed endpoint discovery, 2026-10-26: /api/v1/images/models/bytedance-seed/seedream-5-0-flash/endpoints.
// Inline output is the API contract; no speculative CDN allowlist or format selector.
export const seedreamProfile = {
  version: 1, profiles: [{ api: 'openrouter', base_url: 'https://openrouter.ai/api/v1',
    model: 'bytedance-seed/seedream-5-0-flash', public_model: null,
    capabilities: { streaming: false, omit_default_output_format: true, parameters: {
      resolution: { type: 'enum', values: ['1K', '2K'] },
      aspect_ratio: { type: 'enum', values: ['1:1', '1:2', '2:1', '2:3', '3:2', '3:4', '4:3', '4:5', '5:4', '9:16', '16:9', '9:19.5', '19.5:9', '9:20', '20:9', '9:21', '21:9', 'auto'] },
      n: { type: 'range', min: 1, max: 1 }, input_references: { type: 'range', min: 0, max: 14 }, seed: { type: 'boolean' },
      size: { type: 'boolean' },
    }, max_references: 14, mask: false },
    router_endpoint: { provider_slug: 'seed', provider_tag: 'seed', passthrough_parameters: [] },
    asset_hosts: [], price_revision: 'seedream-catalog-2026-10-26', reservation_microusd: 18000, platform_funded: true }],
};

function requireFileSearchBindings(target) {
  const profile = JSON.parse(target.vars.FILE_SEARCH_PROFILE_JSON ?? 'null');
  const vector = target.vectorize?.find(binding => binding.binding === 'VECTORIZE');
  if (target.ai?.binding !== 'AI' || vector?.index_name !== 'moesegfault-agents-file-search-staging-v1'
    || profile?.ai_binding !== 'AI' || profile.vector_binding !== 'VECTORIZE' || profile.namespace !== 'agents-staging'
    || profile.embedding?.model !== '@cf/baai/bge-m3' || profile.embedding.dimensions !== 1024
    || profile.embedding.revision !== 'workers-ai-bge-m3-2026-10-26' || profile.embedding.input_byte_limit !== 8192) throw new Error('Missing explicit staging FileSearch profile/bindings');
  const pricing = JSON.parse(target.vars.RESOURCE_PRICING_JSON ?? 'null');
  for (const unit of ['embedding_input_bytes', 'queried_dimensions', 'written_dimensions', 'allocated_vector_dimension_ms']) {
    const tariffs = pricing?.tariffs?.filter(tariff => tariff.resource === 'worker' && tariff.unit === unit) ?? [];
    if (tariffs.length !== 1 || tariffs[0].price_version !== pricing.version || [tariffs[0].microusd_numerator, tariffs[0].quantity_denominator].some(value => !Number.isInteger(value) || value <= 0)) throw new Error('Missing explicit FileSearch allocation tariff');
  }
}

export function renderDeploymentConfig(input, env) {
  const environment = env.DEPLOY_ENVIRONMENT;
  if (!['staging', 'production'].includes(environment)) throw new Error('Invalid deployment environment');
  const runtime = deploymentRuntime(env);
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
  target.vars = { ...target.vars, BUILD_ID: workerSource, DEPLOYMENT_ASSEMBLY_SHA: sha, DAEMON_IMAGE_SOURCE: containerSource };
  if (runtime.limited) {
    target.vars.HOSTED_TOOL_RUNTIMES = runtime.workerFeatures.join(',');
    if (runtime.legacyContainerCompatible) target.containers = target.containers.filter(container => container.class_name !== 'Session4G');
    target.vars.HOSTED_TOOL_CAPABILITIES_JSON = JSON.stringify({
      version: 1, platform_images: runtime.imageGeneration, platform_search: runtime.webSearch, file_search: runtime.fileSearch,
      container_memory: null, available_container_profiles: runtime.codeInterpreter ? ['1g', '4g'] : [],
      code_network_disabled: runtime.codeInterpreter, code_network_allowlist: false,
    });
    if (!runtime.fileSearch) {
      delete target.ai;
      delete target.vectorize;
      delete target.vars.FILE_SEARCH_PROFILE_JSON;
    }
  }
  if (runtime.codeInterpreter || runtime.imageGeneration) {
    for (const [className, instanceType] of [['Session', 'basic'], ['Session4G', 'standard-1']]) {
      const apps = target.containers.filter(container => container.class_name === className);
      if (apps.length !== 1 || apps[0].instance_type !== instanceType) throw new Error('Missing explicit Code executor profile app');
    }
  }
  if (runtime.fileSearch) requireFileSearchBindings(target);
  if (runtime.imageGeneration) target.vars.IMAGE_GENERATION_PROFILE_JSON = JSON.stringify(seedreamProfile);
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
