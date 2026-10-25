import assert from 'node:assert/strict';
import test from 'node:test';
import { renderDeploymentConfig } from './deploy-config.mjs';
import { readFile } from 'node:fs/promises';

const sha = '0123456789abcdef0123456789abcdef01234567';
const account = '0123456789abcdef0123456789abcdef';
const target = () => ({ vars: { EXISTING: 'retained', BUILD_ID: 'stale' }, d1_databases: [{ migrations_dir: 'migrations' }], containers: [{ image: 'container/Dockerfile', image_build_context: '.' }] });
const input = () => ({ main: 'build/worker/shim.mjs', assets: { directory: 'apps/dashboard/dist' }, build: { command: 'unused' }, env: { staging: target(), production: target() } });

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
  assert.deepEqual(renderDeploymentConfig(source, { DEPLOY_ENVIRONMENT: 'staging', CLOUDFLARE_ACCOUNT_ID: account, GITHUB_SHA: sha }).env.staging.vars, { BUILD_ID: sha });
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
