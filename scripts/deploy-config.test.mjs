import assert from 'node:assert/strict';
import test from 'node:test';
import { renderDeploymentConfig } from './deploy-config.mjs';

const sha = '0123456789abcdef0123456789abcdef01234567';
const account = '0123456789abcdef0123456789abcdef';
const target = () => ({ vars: { EXISTING: 'retained', BUILD_ID: 'stale' }, d1_databases: [{ migrations_dir: 'migrations' }], containers: [{ image: 'container/Dockerfile', image_build_context: '.' }] });
const input = () => ({ main: 'build/worker/shim.mjs', assets: { directory: 'apps/dashboard/dist' }, build: { command: 'unused' }, env: { staging: target(), production: target() } });

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
