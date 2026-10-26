import assert from 'node:assert/strict';
import test from 'node:test';
import { deploymentRuntime, renderDeploymentConfig, requireDistinctEnvironmentDatabases } from './deploy-config.mjs';
import { readFile } from 'node:fs/promises';

const sha = '0123456789abcdef0123456789abcdef01234567';
const account = '0123456789abcdef0123456789abcdef';
const target = () => ({ vars: { EXISTING: 'retained', BUILD_ID: 'stale' }, d1_databases: [{ migrations_dir: 'migrations' }], containers: [{ image: 'container/Dockerfile', image_build_context: '.' }] });
const input = () => ({ main: 'build/worker/shim.mjs', assets: { directory: 'apps/dashboard/dist' }, build: { command: 'unused' }, env: { staging: target(), production: target() } });

test('image staging adds the pinned inline Seedream endpoint without removing accepted runtimes', async () => {
  const source = JSON.parse(await readFile(new URL('../wrangler.jsonc', import.meta.url), 'utf8'));
  const env = { DEPLOY_ENVIRONMENT: 'staging', CLOUDFLARE_ACCOUNT_ID: account, GITHUB_SHA: sha,
    RUNTIME_FEATURES: 'web_search,file_search,code_interpreter,image_generation' };
  const rendered = renderDeploymentConfig(source, env), stage = rendered.env.staging;
  assert.equal(deploymentRuntime(env).imageGeneration, true);
  assert.equal(stage.vars.HOSTED_TOOL_RUNTIMES, env.RUNTIME_FEATURES);
  const capabilities = JSON.parse(stage.vars.HOSTED_TOOL_CAPABILITIES_JSON);
  assert.equal(capabilities.platform_images, true); assert.equal(capabilities.platform_search, true); assert.equal(capabilities.file_search, true);
  assert.deepEqual(capabilities.available_container_profiles, ['1g', '4g']);
  const registry = JSON.parse(stage.vars.IMAGE_GENERATION_PROFILE_JSON);
  assert.equal(registry.profiles.length, 1);
  const profile = registry.profiles[0];
  assert.equal(profile.model, 'bytedance-seed/seedream-5-0-flash');
  assert.equal(profile.base_url, 'https://openrouter.ai/api/v1');
  assert.equal(profile.reservation_microusd, 18000);
  assert.equal(profile.price_revision, 'seedream-catalog-2026-10-26');
  assert.deepEqual(profile.asset_hosts, []);
  assert.deepEqual(profile.router_endpoint, { provider_slug: 'seed', provider_tag: 'seed', passthrough_parameters: [] });
  assert.equal(profile.capabilities.streaming, false);
  assert.equal(profile.capabilities.omit_default_output_format, true);
  assert.equal(profile.capabilities.parameters.output_format, undefined);
  assert.deepEqual(profile.capabilities.parameters.size, { type: 'boolean' });
  assert.equal(profile.capabilities.parameters.response_format, undefined);
  assert.equal(source.env.staging.vars.IMAGE_GENERATION_PROFILE_JSON, undefined);
  assert.deepEqual(rendered.env.production, source.env.production);
  assert.throws(() => renderDeploymentConfig(source, { ...env, DEPLOY_ENVIRONMENT: 'production' }), /staging-only/);
});

test('Code staging explicitly retains both real profile apps and the accepted search services', async () => {
  const source = JSON.parse(await readFile(new URL('../wrangler.jsonc', import.meta.url), 'utf8'));
  const env = { DEPLOY_ENVIRONMENT: 'staging', CLOUDFLARE_ACCOUNT_ID: account, GITHUB_SHA: sha,
    CONTAINER_SOURCE_SHA: 'd'.repeat(40), ACCEPTANCE_PROFILE: 'none', RUNTIME_FEATURES: 'web_search,file_search,code_interpreter' };
  const rendered = renderDeploymentConfig(source, env), stage = rendered.env.staging;
  assert.equal(deploymentRuntime(env).codeInterpreter, true);
  assert.equal(stage.vars.HOSTED_TOOL_RUNTIMES, env.RUNTIME_FEATURES);
  assert.deepEqual(stage.containers.map(app => [app.class_name, app.instance_type]), [['Session', 'basic'], ['Session4G', 'standard-1']]);
  assert.ok(stage.containers.every(app => app.image.endsWith(`:${env.CONTAINER_SOURCE_SHA}`)));
  assert.deepEqual(JSON.parse(stage.vars.HOSTED_TOOL_CAPABILITIES_JSON), { version: 1, platform_images: false,
    platform_search: true, file_search: true, container_memory: null, available_container_profiles: ['1g', '4g'],
    code_network_disabled: true, code_network_allowlist: false });
  assert.deepEqual(rendered.env.production, source.env.production);
  source.env.staging.containers.pop();
  assert.throws(() => renderDeploymentConfig(source, env), /explicit Code executor profile app/);
});

test('native FileSearch runtime binds the staging 1024 profile and retains explicit existing Search only', async () => {
  const source = JSON.parse(await readFile(new URL('../wrangler.jsonc', import.meta.url), 'utf8'));
  const env = { DEPLOY_ENVIRONMENT: 'staging', CLOUDFLARE_ACCOUNT_ID: account, GITHUB_SHA: sha, ACCEPTANCE_PROFILE: 'none', RUNTIME_FEATURES: 'web_search,file_search' };
  const rendered = renderDeploymentConfig(source, env), stage = rendered.env.staging;
  assert.equal(stage.vars.HOSTED_TOOL_RUNTIMES, 'web_search,file_search');
  assert.deepEqual(stage.ai, { binding: 'AI' });
  assert.deepEqual(stage.vectorize, [{ binding: 'VECTORIZE', index_name: 'moesegfault-agents-file-search-staging-v1' }]);
  assert.equal(stage.containers.some(container => container.class_name === 'Session4G'), false);
  const capabilities = JSON.parse(stage.vars.HOSTED_TOOL_CAPABILITIES_JSON);
  assert.equal(capabilities.file_search, true); assert.equal(capabilities.platform_search, true);
  assert.equal(capabilities.platform_images, false); assert.deepEqual(capabilities.available_container_profiles, []);
  const profile = JSON.parse(stage.vars.FILE_SEARCH_PROFILE_JSON);
  assert.deepEqual(profile.embedding, { model: '@cf/baai/bge-m3', revision: 'workers-ai-bge-m3-2026-10-26', dimensions: 1024, input_byte_limit: 8192 });
  assert.equal(profile.namespace, 'agents-staging');
  assert.deepEqual(rendered.env.production, source.env.production);
});

test('native FileSearch tariffs are explicit byte/dimension allocations and unavailable modes drop their bindings', async () => {
  const source = JSON.parse(await readFile(new URL('../wrangler.jsonc', import.meta.url), 'utf8'));
  const env = { DEPLOY_ENVIRONMENT: 'staging', CLOUDFLARE_ACCOUNT_ID: account, GITHUB_SHA: sha, ACCEPTANCE_PROFILE: 'none', RUNTIME_FEATURES: 'file_search' };
  const stage = renderDeploymentConfig(source, env).env.staging;
  assert.equal(JSON.parse(stage.vars.HOSTED_TOOL_CAPABILITIES_JSON).platform_search, false);
  const pricing = JSON.parse(stage.vars.RESOURCE_PRICING_JSON);
  for (const [unit, numerator, denominator] of [['embedding_input_bytes', 12, 1000], ['queried_dimensions', 1, 100], ['written_dimensions', 1, 100], ['allocated_vector_dimension_ms', 50000, 259200000000000000]]) {
    assert.deepEqual(pricing.tariffs.find(tariff => tariff.unit === unit), { resource: 'worker', unit, microusd_numerator: numerator, quantity_denominator: denominator, price_version: pricing.version });
  }
  assert.ok(pricing.tariffs.every(tariff => tariff.price_version === pricing.version));
  const off = renderDeploymentConfig(source, { ...env, RUNTIME_FEATURES: 'web_search' }).env.staging;
  assert.equal(off.ai, undefined); assert.equal(off.vectorize, undefined); assert.equal(off.vars.FILE_SEARCH_PROFILE_JSON, undefined);
  assert.throws(() => renderDeploymentConfig(source, { ...env, DEPLOY_ENVIRONMENT: 'production' }), /staging-only/);
  delete source.env.staging.ai;
  assert.throws(() => renderDeploymentConfig(source, env), /explicit staging FileSearch/);
});

test('Code capacity provenance injects the actual verified image source rather than the assembly label', () => {
  const containerSource = 'd'.repeat(40);
  const rendered = renderDeploymentConfig(input(), { DEPLOY_ENVIRONMENT: 'staging', CLOUDFLARE_ACCOUNT_ID: account, GITHUB_SHA: sha, CONTAINER_SOURCE_SHA: containerSource }).env.staging;
  assert.equal(rendered.vars.DAEMON_IMAGE_SOURCE, containerSource);
  assert.notEqual(rendered.vars.DAEMON_IMAGE_SOURCE, rendered.vars.DEPLOYMENT_ASSEMBLY_SHA);
});

test('runtime feature allowlist enables only Worker search independently of acceptance', () => {
  const source = input();
  source.env.staging.containers.push({ class_name: 'Session4G', image: 'unbuilt-4g' });
  const config = renderDeploymentConfig(source, { DEPLOY_ENVIRONMENT: 'staging', CLOUDFLARE_ACCOUNT_ID: account,
    GITHUB_SHA: sha, CONTAINER_SOURCE_SHA: 'b'.repeat(40), ACCEPTANCE_PROFILE: 'none', RUNTIME_FEATURES: 'web_search' });
  const stage = config.env.staging, capabilities = JSON.parse(stage.vars.HOSTED_TOOL_CAPABILITIES_JSON);
  assert.equal(stage.vars.HOSTED_TOOL_RUNTIMES, 'web_search');
  assert.deepEqual(capabilities, { version: 1, platform_images: false, platform_search: true, file_search: false,
    container_memory: null, available_container_profiles: [], code_network_disabled: false, code_network_allowlist: false });
  assert.equal(stage.containers.length, 1);
  assert.equal(stage.containers[0].image, `registry.cloudflare.com/${account}/magents-workspace:${'b'.repeat(40)}`);
  assert.equal(source.env.staging.vars.HOSTED_TOOL_RUNTIMES, undefined);
});

test('runtime feature overrides cannot enable unverified tools or change production defaults', () => {
  const base = { DEPLOY_ENVIRONMENT: 'staging', CLOUDFLARE_ACCOUNT_ID: account, GITHUB_SHA: sha, ACCEPTANCE_PROFILE: 'none' };
  for (const features of ['code_interpreter', 'image_generation', 'web_search,code_interpreter', '', 'all']) {
    assert.throws(() => renderDeploymentConfig(input(), { ...base, RUNTIME_FEATURES: features }), /allowlist/);
  }
  assert.throws(() => renderDeploymentConfig(input(), { ...base, DEPLOY_ENVIRONMENT: 'production', RUNTIME_FEATURES: 'web_search' }), /staging-only/);
  assert.equal(deploymentRuntime(base).limited, false);
  assert.equal(deploymentRuntime({ ...base, RUNTIME_FEATURES: 'none' }).limited, true);
  assert.equal(deploymentRuntime({ ...base, ACCEPTANCE_PROFILE: 'responses-increment' }).limited, true);
  assert.equal(deploymentRuntime({ ...base, DEPLOY_ENVIRONMENT: 'production' }).limited, false);
});

test('the fixed billing return reaches authenticated Worker code instead of SPA assets', async () => {
  const config = JSON.parse(await readFile(new URL('../wrangler.jsonc', import.meta.url), 'utf8'));
  assert.ok(config.assets.run_worker_first.includes('/billing/return'));
});

test('each rendered environment links the Worker and immutable container to the same Git SHA', () => {
  for (const environment of ['staging', 'production']) {
    const source = input();
    const config = renderDeploymentConfig(source, { DEPLOY_ENVIRONMENT: environment, CLOUDFLARE_ACCOUNT_ID: account, GITHUB_SHA: sha });
    assert.equal(config.env[environment].vars.BUILD_ID, sha);
    assert.equal(config.env[environment].vars.EXISTING, 'retained');
    assert.equal(config.env[environment].containers[0].image, `registry.cloudflare.com/${account}/magents-workspace:${sha}`);
    assert.equal(config.env[environment].containers[0].image_build_context, undefined);
    assert.equal(config.build, undefined);
    assert.equal(source.env[environment].vars.BUILD_ID, 'stale');
  }
});

test('deployment build identity cannot be missing, truncated, free-form, or supplied with extra whitespace', () => {
  for (const invalid of [undefined, '', 'abc123', 'unknown', sha.toUpperCase(), `${sha}\n`, `${sha}/private`, 'a'.repeat(41)]) {
    assert.throws(() => renderDeploymentConfig(input(), { DEPLOY_ENVIRONMENT: 'production', CLOUDFLARE_ACCOUNT_ID: account, GITHUB_SHA: invalid }), /Invalid immutable image identity/);
  }
});

test('rendering permits an environment with no preexisting variables', () => {
  const source = input();
  delete source.env.staging.vars;
  assert.deepEqual(renderDeploymentConfig(source, { DEPLOY_ENVIRONMENT: 'staging', CLOUDFLARE_ACCOUNT_ID: account, GITHUB_SHA: sha }).env.staging.vars, { BUILD_ID: sha, DEPLOYMENT_ASSEMBLY_SHA: sha, DAEMON_IMAGE_SOURCE: sha });
});

test('reused container source remains immutable while the deployment identity advances', () => {
  const containerSource = 'a'.repeat(40);
  const rendered = renderDeploymentConfig(input(), { DEPLOY_ENVIRONMENT: 'staging', CLOUDFLARE_ACCOUNT_ID: account, GITHUB_SHA: sha, CONTAINER_SOURCE_SHA: containerSource });
  assert.equal(rendered.env.staging.vars.BUILD_ID, sha);
  assert.equal(rendered.env.staging.containers[0].image, `registry.cloudflare.com/${account}/magents-workspace:${containerSource}`);
  for (const invalid of ['', 'latest', `${containerSource}\n`, containerSource.toUpperCase()]) assert.throws(() => renderDeploymentConfig(input(), { DEPLOY_ENVIRONMENT: 'staging', CLOUDFLARE_ACCOUNT_ID: account, GITHUB_SHA: sha, CONTAINER_SOURCE_SHA: invalid }), /Invalid immutable image identity/);
});

test('two runtime profile apps share exactly the same verified daemon image without changing their SKUs', () => {
  const source = input();
  source.env.staging.containers = [
    { class_name: 'Session', instance_type: 'basic', image: 'unbuilt-basic', image_build_context: '.' },
    { class_name: 'Session4G', instance_type: 'standard-1', image: 'unbuilt-4g', image_build_context: '.' },
  ];
  const containerSource = 'b'.repeat(40);
  const apps = renderDeploymentConfig(source, { DEPLOY_ENVIRONMENT: 'staging', CLOUDFLARE_ACCOUNT_ID: account, GITHUB_SHA: sha, CONTAINER_SOURCE_SHA: containerSource }).env.staging.containers;
  assert.deepEqual(apps.map(app => [app.class_name, app.instance_type]), [['Session', 'basic'], ['Session4G', 'standard-1']]);
  assert.deepEqual(new Set(apps.map(app => app.image)), new Set([`registry.cloudflare.com/${account}/magents-workspace:${containerSource}`]));
  assert.ok(apps.every(app => !('image_build_context' in app)));
  assert.equal(source.env.staging.containers[1].image, 'unbuilt-4g');
});

test('mixed-source identity preserves the real Worker build separately from the assembly and daemon', () => {
  const workerSource = 'c'.repeat(40), containerSource = 'd'.repeat(40);
  const rendered = renderDeploymentConfig(input(), { DEPLOY_ENVIRONMENT: 'staging', CLOUDFLARE_ACCOUNT_ID: account, GITHUB_SHA: sha, WORKER_SOURCE_SHA: workerSource, CONTAINER_SOURCE_SHA: containerSource }).env.staging;
  assert.equal(rendered.vars.BUILD_ID, workerSource);
  assert.equal(rendered.vars.DEPLOYMENT_ASSEMBLY_SHA, sha);
  assert.ok(rendered.containers.every(app => app.image.endsWith(`:${containerSource}`)));
  assert.throws(() => renderDeploymentConfig(input(), { DEPLOY_ENVIRONMENT: 'staging', CLOUDFLARE_ACCOUNT_ID: account, GITHUB_SHA: sha, WORKER_SOURCE_SHA: 'latest' }), /Invalid immutable image identity/);
});

test('remote migration target uses distinct exact database IDs rather than staging name labels', () => {
  const pair = { env: { staging: { d1_databases: [{ binding: 'DB', database_name: 'agents-staging', database_id: '11111111-1111-1111-1111-111111111111' }] }, production: { d1_databases: [{ binding: 'DB', database_name: 'agents-production', database_id: '22222222-2222-2222-2222-222222222222' }] } } };
  requireDistinctEnvironmentDatabases(pair);
  pair.env.staging.d1_databases[0].database_id = pair.env.production.d1_databases[0].database_id;
  assert.throws(() => requireDistinctEnvironmentDatabases(pair), /overlap/);
  delete pair.env.staging.d1_databases[0].database_id;
  assert.throws(() => requireDistinctEnvironmentDatabases(pair), /exact environment database identity/);
});

test('core staging leaves unaccepted hosted tools disabled without changing other environments', () => {
  const base = { CLOUDFLARE_ACCOUNT_ID: account, GITHUB_SHA: sha, ACCEPTANCE_PROFILE: 'responses-increment' };
  const source = input();
  source.env.staging.containers.push({ class_name: 'Session4G', instance_type: 'standard-1', image: 'unaccepted' });
  const core = renderDeploymentConfig(source, { ...base, DEPLOY_ENVIRONMENT: 'staging' });
  const capabilities = JSON.parse(core.env.staging.vars.HOSTED_TOOL_CAPABILITIES_JSON);
  assert.equal(core.env.staging.vars.HOSTED_TOOL_RUNTIMES, '');
  assert.equal(core.env.staging.containers.some(container => container.class_name === 'Session4G'), false);
  assert.deepEqual(capabilities, { version: 1, platform_images: false, platform_search: false, file_search: false, container_memory: null, available_container_profiles: [], code_network_disabled: false, code_network_allowlist: false });
  assert.equal(source.env.staging.vars.HOSTED_TOOL_CAPABILITIES_JSON, undefined);
  assert.equal(renderDeploymentConfig(source, { ...base, DEPLOY_ENVIRONMENT: 'production' }).env.production.vars.HOSTED_TOOL_CAPABILITIES_JSON, undefined);
  assert.equal(renderDeploymentConfig(source, { ...base, DEPLOY_ENVIRONMENT: 'staging', ACCEPTANCE_PROFILE: 'none' }).env.staging.vars.HOSTED_TOOL_CAPABILITIES_JSON, undefined);
});

test('image-only staging preserves verified cleanup apps while Code rejects before paid admission', async () => {
  const source = JSON.parse(await readFile(new URL('../wrangler.jsonc', import.meta.url), 'utf8'));
  const env = { DEPLOY_ENVIRONMENT: 'staging', CLOUDFLARE_ACCOUNT_ID: account, GITHUB_SHA: sha,
    CONTAINER_SOURCE_SHA: 'd'.repeat(40), RUNTIME_FEATURES: 'web_search,file_search,image_generation' };
  const runtime = deploymentRuntime(env), stage = renderDeploymentConfig(source, env).env.staging;
  assert.equal(runtime.imageGeneration, true); assert.equal(runtime.codeInterpreter, false);
  assert.equal(runtime.legacyContainerCompatible, false);
  assert.deepEqual(stage.containers.map(app => [app.class_name, app.instance_type]), [['Session', 'basic'], ['Session4G', 'standard-1']]);
  assert.ok(stage.containers.every(app => app.image.endsWith(`:${env.CONTAINER_SOURCE_SHA}`)));
  const capabilities = JSON.parse(stage.vars.HOSTED_TOOL_CAPABILITIES_JSON);
  assert.equal(capabilities.platform_images, true); assert.equal(capabilities.platform_search, true); assert.equal(capabilities.file_search, true);
  assert.deepEqual(capabilities.available_container_profiles, []); assert.equal(capabilities.code_network_disabled, false);
  assert.equal(stage.vars.HOSTED_TOOL_RUNTIMES.includes('code_interpreter'), false);
  assert.equal(stage.vars.DAEMON_IMAGE_SOURCE, env.CONTAINER_SOURCE_SHA);
});
