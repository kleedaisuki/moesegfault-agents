import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

const workflow = readFileSync(new URL('../.github/workflows/deploy.yml', import.meta.url), 'utf8');
const before = (first, second) => assert.ok(workflow.indexOf(first) >= 0 && workflow.indexOf(first) < workflow.indexOf(second), `${first} must precede ${second}`);

test('deployment requires recomputed incremental provenance before infrastructure changes', () => {
  before('node scripts/deploy-provenance.mjs', 'npx wrangler d1 migrations apply');
  assert.ok(workflow.includes('name: incremental-plan'));
  assert.ok(workflow.includes('fetch-depth: 0'));
  assert.equal(workflow.includes("for (const name of ['changes', 'rust', 'worker', 'dashboard', 'container', 'tooling'])"), false);
  for (const component of ['worker', 'dashboard', 'container']) assert.ok(workflow.includes(`run-id: \${{ steps.provenance.outputs.${component}_artifact_run }}`));
});

test('known legacy paid journeys are not automatically replayed by deployment', () => {
  assert.ok(workflow.includes('options: [none, direct-deepseek-usage-probe, responses-increment]'));
  assert.ok(/acceptance_profile:[\s\S]*?default: none/.test(workflow));
  for (const prior of ['tests/e2e/test_deployed.py', 'tests/e2e/test_responses.py', 'tests/e2e/test_space.py', 'node tests/e2e/responses-sdk.mjs']) assert.equal(workflow.includes(prior), false);
  assert.ok(workflow.includes("?'not_selected'"));
});

test('paid stage profiles reject Actions reruns before deployment or model work', () => {
  before('Never replay paid stage work through an Actions rerun', 'npx wrangler d1 migrations apply');
  assert.ok(workflow.includes('test "$GITHUB_RUN_ATTEMPT" = 1'));
  assert.ok(workflow.includes("process.env.DEPLOY_ENVIRONMENT !== 'staging' && process.env.ACCEPTANCE_PROFILE !== 'none'"));
});

test('operator observation persists a global intent artifact before the single POST', () => {
  before('Reject any previously sealed operator-probe intent', 'npx wrangler d1 migrations apply');
  before('name: v030-direct-deepseek-usage-probe-intent', 'run: python scripts/deepseek-usage-probe.py');
  assert.ok(workflow.includes('artifacts?name=v030-direct-deepseek-usage-probe-intent'));
  assert.ok(workflow.includes("?'operator_observation_recorded'"));
  assert.ok(workflow.includes('MAGENTS_DEEPSEEK_PROBE_CLEARED_SOURCE: ${{ github.sha }}'));
});

test('both apps load and publish the artifact original immutable source without rebuilding', () => {
  assert.ok(workflow.includes('GITHUB_SHA: ${{ steps.provenance.outputs.container_artifact_sha }}'));
  assert.ok(workflow.includes('CONTAINER_SOURCE_SHA: ${{ steps.provenance.outputs.container_artifact_sha }}'));
  assert.equal((workflow.match(/node scripts\/container-artifact.mjs load/g) ?? []).length, 1);
  assert.equal(workflow.includes('docker build'), false);
  assert.equal(workflow.includes('worker-build --release'), false);
});

test('incremental receipts upload only explicit safe paths, never raw workspace data', () => {
  assert.ok(workflow.includes('.temp/deploy-verification/deployment-provenance.json'));
  assert.ok(workflow.includes('.temp/v0.3-full-acceptance/${{ github.sha }}/${{ github.run_id }}-responses-current-python.json'));
  assert.equal(workflow.includes('.temp/**'), false);
  assert.equal(workflow.includes('browser-profile'), false);
  assert.equal(workflow.includes('.secrets'), false);
});

test('a profile cannot activate an older incompatible runner after infrastructure changes', () => {
  before('Selected native increment runner is not ready at this source', 'npx wrangler d1 migrations apply');
  assert.ok(workflow.includes("typeof runner.assertExecutionWorkspace !== 'function'"));
  assert.ok(workflow.includes('runner.requireLiveDeploymentProvenance('));
});
