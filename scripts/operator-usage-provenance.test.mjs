import assert from 'node:assert/strict';
import test from 'node:test';
import fs from 'node:fs';
import path from 'node:path';
import { randomUUID } from 'node:crypto';
import { REQUIRED_CHECKS, SOURCE_FILES, verifyOperatorUsageEvidence, main } from './operator-usage-provenance.mjs';

const source = 'a'.repeat(40), historical = 'b'.repeat(40), repository = 'kleedaisuki/moesegfault-agents';
const fingerprints = Object.fromEntries(REQUIRED_CHECKS.map((id, index) => [id, String(index + 1).repeat(64)]));
const check = id => ({ id, argv: id.startsWith('pytest:') ? ['.cache/e2e-venv/bin/python', '-m', 'pytest', id.slice(7), '-ra'] : ['node', '--test', '--test-reporter=tap', id.slice(5)], env: {}, input_fingerprint: fingerprints[id], reason: 'new operator-only safety check' });
const artifact = (id, name, run, sha) => ({ id, name, size_in_bytes: 128, expired: false, expires_at: '2099-01-01T00:00:00Z', digest: 'sha256:' + 'e'.repeat(64), workflow_run: { id: run, head_sha: sha } });
function metadata(run, sha) {
  return { run: { id: run, head_sha: sha, status: 'in_progress', conclusion: null, repository: { full_name: repository }, head_repository: { full_name: repository }, path: '.github/workflows/ci.yml', event: 'workflow_dispatch', display_title: 'CI' }, jobs: { total_count: 3, jobs: ['changes', 'tooling', 'worker'].map(name => ({ name, head_sha: sha, run_id: run, status: name === 'worker' ? 'in_progress' : 'completed', conclusion: name === 'changes' ? 'success' : name === 'tooling' ? 'failure' : null })) }, artifacts: { total_count: 2, artifacts: [artifact(run + 100, 'incremental-plan', run, sha), artifact(run + 200, 'verification-tooling', run, sha)] } };
}
function fixture(reuse = false) {
  const selected = REQUIRED_CHECKS.map(check);
  const plan = { schema_version: 1, source_sha: source, policy: 'incremental-only', planned_checks_are_not_results: true, checks: { tooling: selected }, reused_checks: [], components: { container: { validation_scope: 'diagnostic_startup_only' } }, diagnostic_only_components: ['container'] };
  const result = item => ({ id: item.id, argv: item.argv, env: item.env, status: 'passed_current', input_fingerprint: item.input_fingerprint, executed_tests: 3 });
  const receipt = { schema_version: 1, source_sha: source, job: 'tooling', results: [...selected.map(result), { id: 'unrelated_failed_check', status: 'failed_current', executed_tests: 1 }] };
  const evidence = { source, sourceRun: '22', repository, plan, expectedPlan: structuredClone(plan), catalog: [], metadata: { '22': metadata(22, source) }, verification: { plan, files: { 'tooling.json': receipt }, file_hashes: { 'plan.json': 'd'.repeat(64), 'tooling.json': 'e'.repeat(64) } }, reusedVerification: {}, checkHash: (sha, item) => fingerprints[item.id], sourceHashes: Object.fromEntries(SOURCE_FILES.map(file => [file, 'f'.repeat(64)])), planFileSha256: 'd'.repeat(64) };
  if (!reuse) return evidence;
  const original = selected[0];
  const reused = { ...original, job: 'tooling', status: 'reused_verified', source_sha: historical, run_id: '21', artifact_name: 'verification-tooling', artifact_id: 221, receipt_path: 'tooling.json', executed_tests: 3 };
  plan.checks.tooling = selected.slice(1); plan.reused_checks = [reused];
  receipt.results = receipt.results.filter(row => row.id !== original.id);
  evidence.expectedPlan = structuredClone(plan);
  evidence.catalog = [{ source_sha: historical, run_id: '21', components: ['tooling'], receipt_artifact: 'verification-tooling', receipt_artifact_id: 221, verified_checks: [{ id: original.id, status: 'passed_current', receipt_path: 'tooling.json', input_fingerprint: original.input_fingerprint, executed_tests: 3 }] }];
  evidence.metadata['21'] = metadata(21, historical);
  const oldPlan = { source_sha: historical, checks: { tooling: [original] } };
  evidence.reusedVerification['21:verification-tooling'] = { plan: oldPlan, files: { 'tooling.json': { schema_version: 1, source_sha: historical, job: 'tooling', results: [result(original)] } }, file_hashes: { 'plan.json': '7'.repeat(64), 'tooling.json': '8'.repeat(64) } };
  return evidence;
}

test('operator scope accepts three positive checks despite unrelated failed job/check and running CI', () => {
  const proof = verifyOperatorUsageEvidence(fixture());
  assert.equal(proof.status, 'critical_operator_checks_verified');
  assert.equal(proof.scope, 'operator_usage_helper_only');
  assert.equal(proof.ci_status, 'in_progress');
  assert.equal(proof.ci_conclusion, null);
  assert.equal(proof.checks.length, 3);
  assert.ok(proof.checks.every(row => row.executed_tests > 0 && row.job_conclusion === 'failure'));
  assert.equal(proof.platform_acceptance, 'not_established');
  assert.equal(proof.paid_execution_authorized, false);
  assert.equal(proof.production_authorized, false);
  assert.equal(proof.helper_source.source_sha, source);
  assert.equal(proof.workflow_source.path, '.github/workflows/deploy.yml');
});

test('operator scope preserves completed failed overall CI without relabeling it passed', () => {
  const evidence = fixture(); Object.assign(evidence.metadata['22'].run, { status: 'completed', conclusion: 'failure' });
  const proof = verifyOperatorUsageEvidence(evidence);
  assert.equal(proof.ci_conclusion, 'failure');
  assert.equal(proof.ci_status, 'completed');
  assert.ok(proof.checks.every(check => check.status === 'passed_current'));
});

for (const [name, mutate] of [
  ['wrong pushed source', value => { value.metadata['22'].run.head_sha = historical; }],
  ['foreign repository', value => { value.metadata['22'].run.head_repository.full_name = 'attacker/fork'; }],
  ['diagnostic run', value => { value.metadata['22'].run.display_title = 'Worker diagnostics - artifact'; }],
  ['missing plan artifact', value => { value.metadata['22'].artifacts.artifacts.shift(); value.metadata['22'].artifacts.total_count--; }],
  ['plan not recomputed', value => { value.expectedPlan.source_sha = historical; }],
  ['changes job still running', value => { value.metadata['22'].jobs.jobs[0].status = 'in_progress'; }],
  ['own job still running', value => { value.metadata['22'].jobs.jobs[1].status = 'in_progress'; }],
  ['missing critical selected ID', value => { value.plan.checks.tooling.pop(); value.expectedPlan = structuredClone(value.plan); }],
  ['missing current result', value => { value.verification.files['tooling.json'].results.shift(); }],
  ['duplicate current result', value => { value.verification.files['tooling.json'].results.push(value.verification.files['tooling.json'].results[0]); }],
  ['zero actual tests', value => { value.verification.files['tooling.json'].results[0].executed_tests = 0; }],
  ['failed critical result', value => { value.verification.files['tooling.json'].results[0].status = 'failed_current'; }],
  ['selector argv drift', value => { value.verification.files['tooling.json'].results[0].argv = ['node', '--version']; }],
  ['selector env drift', value => { value.verification.files['tooling.json'].results[0].env = { UNPLANNED: '1' }; }],
  ['missing result fingerprint', value => { delete value.verification.files['tooling.json'].results[0].input_fingerprint; }],
  ['expired positive artifact', value => { value.metadata['22'].artifacts.artifacts[1].expired = true; }],
  ['artifact source drift', value => { value.metadata['22'].artifacts.artifacts[1].workflow_run.head_sha = historical; }],
  ['missing exact helper hash', value => { delete value.sourceHashes['scripts/deepseek-usage-probe.py']; }],
  ['missing exact workflow hash', value => { delete value.sourceHashes['.github/workflows/deploy.yml']; }],
  ['missing downloaded receipt hash', value => { delete value.verification.file_hashes['tooling.json']; }],
]) test(`operator scope rejects ${name}`, () => { const evidence = fixture(); mutate(evidence); assert.throws(() => verifyOperatorUsageEvidence(evidence)); });

test('operator reuses genuine sealed check with matching old/current inputs and original positive receipt', () => {
  const proof = verifyOperatorUsageEvidence(fixture(true));
  assert.equal(proof.checks[0].status, 'reused_verified');
  assert.equal(proof.checks[0].source_sha, historical);
  assert.equal(proof.checks[0].run_id, '21');
  assert.equal(proof.checks[0].executed_tests, 3);
  assert.equal(proof.checks[0].job_conclusion, 'failure');
  assert.equal(proof.ci_status, 'in_progress');
  assert.equal(proof.production_authorized, false);
});

for (const [name, mutate] of [
  ['unsealed reuse', value => { value.catalog = []; }],
  ['changed reused inputs', value => { value.checkHash = (sha, item) => sha === source ? '0'.repeat(64) : fingerprints[item.id]; }],
  ['missing original receipt', value => { delete value.reusedVerification['21:verification-tooling'].files['tooling.json']; }],
  ['zero historical tests', value => { value.reusedVerification['21:verification-tooling'].files['tooling.json'].results[0].executed_tests = 0; }],
  ['historical argv drift', value => { value.reusedVerification['21:verification-tooling'].plan.checks.tooling[0].argv = ['node', '--version']; }],
  ['historical artifact substitution', value => { value.metadata['21'].artifacts.artifacts[1].id = 999; }],
  ['historical job running', value => { value.metadata['21'].jobs.jobs[1].status = 'in_progress'; }],
  ['historical result wrong present fingerprint', value => { value.reusedVerification['21:verification-tooling'].files['tooling.json'].results[0].input_fingerprint = '0'.repeat(64); }],
]) test(`operator scope rejects ${name}`, () => { const evidence = fixture(true); mutate(evidence); assert.throws(() => verifyOperatorUsageEvidence(evidence)); });

test('operator CLI mocked metadata/download path emits only scoped proof without app or provider execution', async () => {
  const evidence = fixture(), commands = [];
  const root = process.cwd(), owned = path.resolve('.temp');
  const directory = path.join(owned, 'operator-verifier-tests-' + randomUUID());
  assert.ok(directory.startsWith(owned + path.sep));
  const execute = (command, argv, options) => {
    commands.push({ command, argv });
    if (command === 'git' && argv[0] === 'rev-parse') return source + '\n';
    if (command === 'git' && argv[0] === 'show') return fs.readFileSync(path.join(root, argv[1].slice(41)));
    assert.equal(command, 'gh');
    if (argv[0] === 'api') {
      const route = argv[1];
      if (route.endsWith('/jobs?filter=latest&per_page=100')) return JSON.stringify(evidence.metadata['22'].jobs);
      if (route.endsWith('/artifacts?per_page=100')) return JSON.stringify(evidence.metadata['22'].artifacts);
      assert.ok(route.endsWith('/actions/runs/22')); return JSON.stringify(evidence.metadata['22'].run);
    }
    assert.deepEqual(argv.slice(0, 2), ['run', 'download']);
    const name = argv[argv.indexOf('--name') + 1], folder = argv[argv.indexOf('--dir') + 1];
    fs.writeFileSync(path.join(folder, 'plan.json'), JSON.stringify(evidence.plan));
    if (name === 'verification-tooling') fs.writeFileSync(path.join(folder, 'tooling.json'), JSON.stringify(evidence.verification.files['tooling.json']));
    else assert.equal(name, 'incremental-plan');
    return '';
  };
  try {
    await main(['--run', '22', '--directory', directory], { GITHUB_SHA: source, GITHUB_REPOSITORY: repository }, execute, async () => ({ plan: () => structuredClone(evidence.expectedPlan), checkFingerprint: evidence.checkHash }));
    const proof = JSON.parse(fs.readFileSync(path.join(directory, 'operator-usage-provenance.json'), 'utf8'));
    assert.equal(proof.scope, 'operator_usage_helper_only');
    assert.equal(proof.paid_execution_authorized, false);
    assert.equal(proof.platform_acceptance, 'not_established');
    assert.equal(proof.checks.length, 3);
    assert.ok(commands.every(item => ['git', 'gh'].includes(item.command)));
    assert.ok(commands.filter(item => item.command === 'gh').every(item => item.argv[0] === 'api' || item.argv[0] === 'run' && item.argv[1] === 'download'));
    assert.equal(commands.filter(item => item.command === 'gh' && item.argv[0] === 'run').length, 2);
    assert.equal(Object.keys(proof.source_files).length, SOURCE_FILES.length);
  } finally { assert.ok(directory.startsWith(owned + path.sep)); fs.rmSync(directory, { recursive: true, force: true }); }
});

test('operator legacy missing fingerprint uses recomputed original plan but wrong present fingerprint fails', () => {
  const evidence = fixture(true);
  const row = evidence.reusedVerification['21:verification-tooling'].files['tooling.json'].results[0];
  delete row.input_fingerprint;
  const proof = verifyOperatorUsageEvidence(evidence);
  assert.equal(proof.checks[0].status, 'reused_verified');
  assert.equal(proof.checks[0].input_fingerprint, fingerprints[REQUIRED_CHECKS[0]]);
  assert.equal(proof.checks[0].executed_tests, 3);
  row.input_fingerprint = '0'.repeat(64);
  assert.throws(() => verifyOperatorUsageEvidence(evidence), /fingerprint/);
});
