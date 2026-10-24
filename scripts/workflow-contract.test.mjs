import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

const workflow = (name) => readFile(new URL(`../.github/workflows/${name}.yml`, import.meta.url), 'utf8');

test('staging verifies the source build and every selected user journey without skip-success', async () => {
  const deploy = await workflow('deploy');
  const gate = deploy.indexOf('name: Require staging acceptance credentials before changing infrastructure');
  assert.ok(gate >= 0 && gate < deploy.indexOf('name: Publish tested container'));
  assert.match(deploy, /\.status == "ok" and \.build_id == \$build/);
  assert.match(deploy, /MAGENTS_TEST_REQUIRE_ALL: '1'/);
  assert.match(deploy, /pytest tests\/e2e\/test_deployed\.py tests\/e2e\/test_responses\.py tests\/e2e\/test_space\.py/);
  assert.match(deploy, /node tests\/e2e\/responses-sdk\.mjs/);
  assert.doesNotMatch(deploy, /exit 0/);
});

test('CI and deployment cache locked Python packages, never the user-journey environment', async () => {
  for (const name of ['ci', 'deploy']) {
    const source = await workflow(name);
    assert.match(source, /actions\/setup-python@[a-f0-9]{40}/);
    assert.match(source, /python-version: '3\.13'\n\s+cache: pip\n\s+cache-dependency-path: tests\/e2e\/requirements\.lock/);
    assert.doesNotMatch(source, /path: .*\.cache\/e2e-venv/);
  }
});

test('deployment continues promoting only successful exact-source non-PR CI artifacts', async () => {
  const deploy = await workflow('deploy');
  assert.match(deploy, /r\.head_sha!==process\.env\.GITHUB_SHA/);
  assert.match(deploy, /r\.conclusion!=="success"/);
  assert.match(deploy, /r\.event==="pull_request"/);
  assert.equal((deploy.match(/run-id: \$\{\{ inputs\.source_run \}\}/g) ?? []).length, 2);
  assert.match(deploy, /cancel-in-progress: false/);
});

test('compiled Worker cases use strict SDK validation and keep performance artifacts out of release builds', async () => {
  const ci = await workflow('ci');
  assert.match(ci, /run: npm run test:worker\n\s+env:\n\s+MAGENTS_SCHEMA_PYTHON: \.cache\/e2e-venv\/bin\/python/);
  assert.match(ci, /name: performance-v3\n\s+path: \.cache\/perf\/v3\/\*\.json\n\s+if-no-files-found: warn/);
  assert.match(ci, /name: worker\n\s+path: crates\/agents-worker\/build\n/);
  assert.match(ci, /node --test scripts\/perf\/v3-dataflow\.test\.mjs/);
});
