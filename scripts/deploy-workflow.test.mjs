import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';

const workflow = readFileSync(new URL('../.github/workflows/deploy.yml', import.meta.url), 'utf8');
const before = (first, second) => assert.ok(workflow.indexOf(first) >= 0 && workflow.indexOf(first) < workflow.indexOf(second), `${first} must precede ${second}`);

test('native FileSearch workflow accepts an explicit additive Worker allowlist without selecting paid tests', () => {
  const code = workflow.match(/<<'REQUEST'\n([\s\S]*?)\n\s+REQUEST/)[1];
  const run = overrides => vm.runInNewContext(code, { process: { env: { DEPLOY_ENVIRONMENT: 'staging', ACCEPTANCE_PROFILE: 'none', RUNTIME_FEATURES: 'web_search,file_search', SOURCE_RUN: '123', GITHUB_RUN_ATTEMPT: '1', ...overrides } } });
  assert.doesNotThrow(() => run({}));
  assert.doesNotThrow(() => run({ RUNTIME_FEATURES: 'file_search' }));
  assert.throws(() => run({ RUNTIME_FEATURES: 'file_search,code_interpreter' }), /allowlist/);
  assert.throws(() => run({ DEPLOY_ENVIRONMENT: 'production' }), /staging-only/);
  assert.ok(workflow.includes('const compatible=deploymentRuntime(process.env).limited;'));
  assert.ok(workflow.includes("const accepted=profile==='none'?'not_selected'"));
  assert.equal(workflow.includes('node tests/e2e/file-search-sdk.mjs'), false);
});

test('runtime feature workflow flags do not select or replay an acceptance family', () => {
  const code = workflow.match(/<<'REQUEST'\n([\s\S]*?)\n\s+REQUEST/)[1];
  const run = overrides => vm.runInNewContext(code, { process: { env: { DEPLOY_ENVIRONMENT: 'staging', ACCEPTANCE_PROFILE: 'none',
    RUNTIME_FEATURES: 'web_search', SOURCE_RUN: '123', GITHUB_RUN_ATTEMPT: '1', ...overrides } } });
  assert.doesNotThrow(() => run({}));
  assert.throws(() => run({ RUNTIME_FEATURES: 'code_interpreter' }), /allowlist/);
  assert.throws(() => run({ DEPLOY_ENVIRONMENT: 'production' }), /staging-only/);
  assert.ok(workflow.includes("options: [configured, none, web_search, file_search, 'web_search,file_search']"));
  assert.ok(workflow.includes("const accepted=profile==='none'?'not_selected'"));
  assert.equal(workflow.includes('node tests/e2e/hosted-search-sdk.mjs'), false);
  before('Require the existing selected Worker search service credential', 'npx wrangler d1 migrations apply');
});

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
  assert.ok(workflow.includes('name: operator-usage-observation-evidence'));
  assert.ok(workflow.includes('MAGENTS_DEEPSEEK_PROBE_CLEARED_SOURCE: ${{ github.sha }}'));
});

test('both apps load and publish the artifact original immutable source without rebuilding', () => {
  assert.equal(workflow.includes('GITHUB_SHA: ${{ steps.provenance.outputs.container_artifact_sha }}'), false);
  assert.ok(workflow.includes('await loadImageArtifact(".temp/container-release", process.env.CONTAINER_SOURCE_SHA)'));
  assert.ok(workflow.includes('CONTAINER_SOURCE_SHA: ${{ steps.provenance.outputs.container_artifact_sha }}'));
  assert.equal((workflow.match(/await loadImageArtifact\(/g) ?? []).length, 1);
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

test('mixed-source health and API expectations preserve the real Worker source instead of relabeling it', () => {
  assert.ok(workflow.includes('WORKER_SOURCE_SHA: ${{ steps.provenance.outputs.worker_artifact_sha }}'));
  assert.ok(workflow.includes('--arg build "$WORKER_SOURCE_SHA"'));
  assert.ok(workflow.includes('MAGENTS_TEST_EXPECTED_BUILD: ${{ steps.provenance.outputs.worker_artifact_sha }}'));
  assert.ok(workflow.includes('MAGENTS_TEST_ASSEMBLY_SOURCE: ${{ github.sha }}'));
  assert.ok(workflow.includes('assembly_source_sha:process.env.GITHUB_SHA'));
});

const operatorJob = workflow.slice(workflow.indexOf('  operator_usage:'), workflow.indexOf('  deploy:'));
const deploymentJob = workflow.slice(workflow.indexOf('  deploy:'));

test('operator-only job has no app deployment, Cloudflare credential or user API path', () => {
  assert.ok(operatorJob.includes("if: inputs.environment == 'staging' && inputs.acceptance_profile == 'direct-deepseek-usage-probe'"));
  assert.ok(deploymentJob.includes("if: inputs.acceptance_profile != 'direct-deepseek-usage-probe'"));
  for (const forbidden of ['CLOUDFLARE_', 'wrangler', 'worker-secrets', 'STAGING_SMOKE_API_KEY', 'full-responses-sdk']) assert.equal(operatorJob.includes(forbidden), false);
  assert.equal(deploymentJob.includes('run: python scripts/deepseek-usage-probe.py'), false);
});

test('operator-only request validation rejects production scope and retries before secret access', () => {
  const code = workflow.match(/<<'REQUEST'\n([\s\S]*?)\n\s+REQUEST/)[1];
  const run = overrides => vm.runInNewContext(code, { process: { env: { DEPLOY_ENVIRONMENT: 'staging', ACCEPTANCE_PROFILE: 'direct-deepseek-usage-probe', SOURCE_RUN: '123', GITHUB_RUN_ATTEMPT: '1', ...overrides } } });
  assert.doesNotThrow(() => run({}));
  assert.throws(() => run({ DEPLOY_ENVIRONMENT: 'production' }), /staging-only/);
  assert.throws(() => run({ GITHUB_RUN_ATTEMPT: '2' }), /cannot be rerun/);
  assert.throws(() => run({ SOURCE_RUN: 'invalid' }), /source CI/);
});

test('operator-only critical proof and one global immutable intent precede the only provider request', () => {
  const proof = operatorJob.indexOf('node scripts/operator-usage-provenance.mjs');
  const prior = operatorJob.indexOf('Reject any previously sealed operator-probe intent');
  const intent = operatorJob.indexOf('name: v030-direct-deepseek-usage-probe-intent');
  const submit = operatorJob.indexOf('run: python scripts/deepseek-usage-probe.py');
  assert.ok(proof >= 0 && proof < prior && prior < intent && intent < submit);
  assert.equal((operatorJob.match(/run: python scripts\/deepseek-usage-probe.py/g) ?? []).length, 1);
  assert.ok(operatorJob.includes('artifacts?name=v030-direct-deepseek-usage-probe-intent'));
  assert.ok(operatorJob.includes('if: always()'));
});

test('usable core deployment does not pay for compact with an unavailable provider usage field', () => {
  assert.ok(deploymentJob.includes('--scope core --omit-compact --allow-model-calls --max-paid-submissions 7'));
});
