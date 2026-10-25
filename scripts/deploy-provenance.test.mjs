import assert from 'node:assert/strict';
import test from 'node:test';
import { verifyDeploymentEvidence } from './deploy-provenance.mjs';

const source = 'c'.repeat(40), base = 'b'.repeat(40), digest = 'a'.repeat(64), repository = 'kleedaisuki/moesegfault-agents';
const jobs = { worker: 'worker', native: 'rust', container: 'container', dashboard: 'dashboard', tooling: 'tooling' };
const nativeArgv = ['cargo', 'clippy', '--locked', '--all-targets', '--', '-D', 'warnings'];
const check = id => ({ id, argv: ['node', '--test', `${id}.mjs`], env: {}, reason: 'new behavior' });
function fixture(reuseWorker = false) {
  const hash = (sha, component) => sha === source && (component === 'tooling' || component === 'worker' && !reuseWorker) ? 'f'.repeat(64) : digest;
  const partial = { source_sha: base, run_id: '41', components: ['worker'], status: 'partial_verified', artifact_id: 77, verified_phases: ['wasm_build', 'rust_lib'], verified_steps: ['wasm', 'library'], unresolved_checks: [{ id: 'repair' }] };
  const catalog = [partial, { source_sha: base, run_id: '41', components: ['native', 'container', 'dashboard', 'tooling'], status: 'verified' }];
  const components = Object.fromEntries(Object.entries(jobs).map(([component, job]) => {
    const seal = component === 'worker' ? partial : catalog[1];
    const matches = hash(source, component) === digest;
    const name = { worker: 'worker', dashboard: 'dashboard', container: 'container-image' }[component];
    return [component, { fingerprint: hash(source, component), action: matches ? 'reuse' : 'build', baseline: { ...seal, job, fingerprint: digest, matches }, artifact: name ? { name, run_id: matches ? '41' : '42', source_sha: matches ? base : source, artifact_id: component === 'worker' && matches ? 77 : null } : null }];
  }));
  const plan = { schema_version: 1, source_sha: source, policy: 'incremental-only', planned_checks_are_not_results: true, required_jobs: ['changes', 'rust', 'worker', 'tooling'], required_remediations: ['repair'], components, native_packages: [], checks: { rust: [check('pricing')], worker: [check('repair')], tooling: [check('tooling-new')], current_sdk: [], container: [], dashboard: [] } };
  const artifact = (name, id, run, sha) => ({ name, id, expired: false, expires_at: '2099-01-01T00:00:00Z', size_in_bytes: 123, workflow_run: { id: run, head_sha: sha }, digest: 'sha256:' + 'd'.repeat(64) });
  const bundle = (runId, sha, conclusion) => ({ run: { id: runId, head_sha: sha, status: 'completed', conclusion, repository: { full_name: repository }, head_repository: { full_name: repository }, path: '.github/workflows/ci.yml', event: 'workflow_dispatch', display_title: 'CI' }, jobs: { total_count: 6, jobs: ['changes', 'rust', 'worker', 'dashboard', 'container', 'tooling'].map(name => ({ name, run_id: runId, head_sha: sha, status: 'completed', conclusion: 'success', steps: [] })) }, artifacts: { total_count: 0, artifacts: [] } });
  const previous = bundle(41, base, 'failure'), current = bundle(42, source, 'success');
  Object.assign(previous.jobs.jobs.find(job => job.name === 'worker'), { conclusion: 'failure', steps: [{ name: 'wasm', conclusion: 'success' }, { name: 'library', conclusion: 'success' }] });
  for (const name of ['dashboard', 'container']) current.jobs.jobs.find(job => job.name === name).conclusion = 'skipped';
  previous.artifacts.artifacts = [artifact('worker', 77, 41, base), artifact('dashboard', 78, 41, base), artifact('container-image', 79, 41, base)];
  current.artifacts.artifacts = [artifact('worker', 88, 42, source), ...['rust', 'worker', 'tooling'].map((name, index) => artifact(`verification-${name}`, 90 + index, 42, source))];
  previous.artifacts.total_count = previous.artifacts.artifacts.length;
  current.artifacts.total_count = current.artifacts.artifacts.length;
  const receipt = (name, selected) => ({ schema_version: 1, source_sha: source, job: name, ...(selected.length ? {} : { status: 'no_selected_tests' }), results: selected.map(item => ({ id: item.id, argv: item.argv, env: item.env, status: 'passed_current', executed_tests: 1 })) });
  const verification = {
    rust: { plan, files: { 'rust.json': receipt('rust', plan.checks.rust), 'native-build.json': { source_sha: source, argv: nativeArgv, status: 'no_changed_native_packages' } } },
    worker: { plan, files: { 'worker-rust.json': receipt('worker-rust', []), 'worker-journeys.json': receipt('worker-journeys', plan.checks.worker) } },
    tooling: { plan, files: { 'tooling.json': receipt('tooling', plan.checks.tooling), 'current_sdk.json': receipt('current_sdk', []) } },
  };
  return { plan, expectedPlan: structuredClone(plan), catalog, source, sourceRun: '42', repository, metadata: { '41': previous, '42': current }, verification, hash };
}

test('incremental gate preserves partial baseline and skipped-job truth while verifying new checks', () => {
  const proof = verifyDeploymentEvidence(fixture());
  assert.equal(proof.artifacts.container.source_sha, base);
  assert.equal(proof.artifacts.container.status, 'reused_identical_artifact');
  assert.equal(proof.artifacts.worker.source_sha, source);
  assert.equal(proof.baseline_evidence.find(value => value.component === 'worker').job_conclusion, 'failure');
  assert.equal(proof.baseline_evidence.find(value => value.component === 'worker').status, 'partial_baseline_phases_only');
  assert.deepEqual(proof.passed_current_checks, ['pricing', 'repair', 'tooling-new']);
  assert.equal(proof.staging_acceptance, 'not_executed');
  assert.equal(proof.production_authorized, false);
});

test('same-source/dependency Worker artifact may be reused only after current failed-leaf repairs', () => {
  const proof = verifyDeploymentEvidence(fixture(true));
  assert.equal(proof.artifacts.worker.id, 77);
  assert.equal(proof.artifacts.worker.status, 'reused_identical_artifact');
});

for (const [name, change] of [
  ['candidate failure', value => { value.metadata['42'].run.conclusion = 'failure'; }],
  ['foreign repository', value => { value.metadata['41'].run.head_repository.full_name = 'attacker/fork'; }],
  ['diagnostic baseline', value => { value.metadata['41'].run.display_title = 'Worker diagnostics - artifact'; }],
  ['skipped current remediation job', value => { value.metadata['42'].jobs.jobs.find(job => job.name === 'worker').conclusion = 'skipped'; }],
  ['failed historical build phase', value => { value.metadata['41'].jobs.jobs.find(job => job.name === 'worker').steps[0].conclusion = 'failure'; }],
  ['unsealed baseline', value => { value.catalog = []; }],
  ['missing current receipt', value => { delete value.verification.worker.files['worker-journeys.json']; }],
  ['zero executed tests', value => { value.verification.worker.files['worker-journeys.json'].results[0].executed_tests = 0; }],
  ['old result called current', value => { value.verification.worker.files['worker-journeys.json'].source_sha = base; }],
  ['baseline result relabeled', value => { value.verification.worker.files['worker-journeys.json'].results[0].status = 'passed_baseline'; }],
  ['expired reused image', value => { value.metadata['41'].artifacts.artifacts.find(item => item.name === 'container-image').expired = true; }],
  ['wrong artifact source', value => { value.metadata['42'].artifacts.artifacts.find(item => item.name === 'worker').workflow_run.head_sha = base; }],
  ['missing artifact page', value => { value.metadata['42'].artifacts.total_count++; }],
  ['unrecomputed selection', value => { value.plan.policy = 'run-everything'; }],
]) test(`incremental gate rejects ${name}`, () => { const value = fixture(); change(value); assert.throws(() => verifyDeploymentEvidence(value)); });

test('changed result arguments cannot impersonate the selected repair', () => {
  const value = fixture();
  value.verification.worker.files['worker-journeys.json'].results[0].argv = ['node', '--version'];
  assert.throws(() => verifyDeploymentEvidence(value), /selector/);
});

test('a declared partial artifact ID cannot be substituted by another same-name archive', () => {
  const value = fixture(true);
  value.metadata['41'].artifacts.artifacts.find(item => item.name === 'worker').id = 999;
  assert.throws(() => verifyDeploymentEvidence(value), /artifact_identity/);
});

test('removing a failed-leaf check never seals the partial baseline as complete', () => {
  const value = fixture();
  value.plan.checks.worker = [];
  value.expectedPlan = structuredClone(value.plan);
  Object.assign(value.verification.worker.files['worker-journeys.json'], { results: [], status: 'no_selected_tests' });
  assert.throws(() => verifyDeploymentEvidence(value), /not_repaired/);
});

test('diagnostic increment cannot promote a green 1g-startup-only container', () => {
  const value = fixture();
  value.plan.diagnostic_only_components = ['container'];
  value.expectedPlan = structuredClone(value.plan);
  assert.throws(() => verifyDeploymentEvidence(value), /diagnostic_only_component_is_not_promotable/);
});

test('diagnostic increment cannot hide its scope or deferred memory and network cases', () => {
  for (const patch of [{ validation_scope: 'diagnostic_startup_only' }, { validation_scope: 'incremental', deferred_checks: [{ id: 'memory4g' }, { id: 'network' }] }]) {
    const value = fixture();
    Object.assign(value.plan.components.container, patch);
    value.expectedPlan = structuredClone(value.plan);
    assert.throws(() => verifyDeploymentEvidence(value), /not_promotable/);
  }
});

test('diagnostic baseline cannot masquerade as a fully verified reusable image', () => {
  const value = fixture();
  value.catalog[1].validation_scope = 'diagnostic_startup_only';
  assert.throws(() => verifyDeploymentEvidence(value), /diagnostic_only_component_is_not_promotable/);
});

test('incremental component fingerprints are explicit for cross-source feature evidence reuse', () => {
  const proof = verifyDeploymentEvidence(fixture());
  assert.deepEqual(Object.keys(proof.component_fingerprints).sort(), ['container', 'dashboard', 'native', 'tooling', 'worker']);
  assert.deepEqual(proof.diagnostic_only_components, []);
  assert.equal(proof.component_fingerprints.worker, 'f'.repeat(64));
});

function reusedFixture() {
  const value = fixture();
  const historical = 'd'.repeat(40), input = 'e'.repeat(64);
  const original = check('repair');
  const reused = { ...original, status: 'reused_verified', source_sha: historical, run_id: '43', job: 'worker', artifact_name: 'verification-worker', artifact_id: 179, receipt_path: 'worker-journeys.json', input_fingerprint: input, executed_tests: 2 };
  value.plan.checks.worker = [];
  value.plan.reused_checks = [reused];
  value.expectedPlan = structuredClone(value.plan);
  Object.assign(value.verification.worker.files['worker-journeys.json'], { results: [], status: 'no_selected_tests' });
  value.catalog.push({ source_sha: historical, run_id: '43', components: [], receipt_artifact: 'verification-worker', receipt_artifact_id: 179, verified_checks: [{ id: 'repair', status: 'passed_current', input_fingerprint: input, executed_tests: 2, receipt_path: 'worker-journeys.json' }] });
  const old = structuredClone(value.metadata['41']);
  Object.assign(old.run, { id: 43, head_sha: historical });
  for (const job of old.jobs.jobs) Object.assign(job, { run_id: 43, head_sha: historical });
  old.artifacts = { total_count: 1, artifacts: [{ name: 'verification-worker', id: 179, expired: false, expires_at: '2099-01-01T00:00:00Z', size_in_bytes: 123, workflow_run: { id: 43, head_sha: historical } }] };
  value.metadata['43'] = old;
  value.checkHash = () => input;
  value.reusedVerification = { '43:verification-worker': { plan: { source_sha: historical, checks: { worker: [original] } }, files: { 'worker-journeys.json': { source_sha: historical, job: 'worker-journeys', results: [{ ...original, status: 'passed_current', executed_tests: 2 }] } } } };
  return value;
}

test('bounded reused check proof satisfies an already-fixed leaf without relabeling it current', () => {
  const proof = verifyDeploymentEvidence(reusedFixture());
  assert.equal(proof.passed_current_checks.includes('repair'), false);
  assert.equal(proof.reused_verified_checks[0].id, 'repair');
  assert.equal(proof.reused_verified_checks[0].status, 'reused_verified');
  assert.equal(proof.reused_verified_checks[0].source_sha, 'd'.repeat(40));
  assert.equal(proof.reused_verified_checks[0].executed_tests, 2);
});

for (const [name, change] of [
  ['failed historical leaf', value => { value.reusedVerification['43:verification-worker'].files['worker-journeys.json'].results[0].status = 'failed_current'; }],
  ['changed input group', value => { value.checkHash = sha => sha === source ? 'f'.repeat(64) : 'e'.repeat(64); }],
  ['unsealed positive claim', value => { value.catalog.pop(); }],
  ['substituted receipt artifact', value => { value.metadata['43'].artifacts.artifacts[0].id = 999; }],
  ['selector drift', value => { value.reusedVerification['43:verification-worker'].plan.checks.worker[0].argv = ['node', '--version']; }],
  ['false current status', value => { value.plan.reused_checks[0].status = 'passed_current'; value.expectedPlan = structuredClone(value.plan); }],
  ['legacy receipt fingerprint drift', value => { value.reusedVerification['43:verification-worker'].files['worker-journeys.json'].results[0].input_fingerprint = 'f'.repeat(64); }],
]) test(`bounded reused check rejects ${name}`, () => { const value = reusedFixture(); change(value); assert.throws(() => verifyDeploymentEvidence(value)); });

test('sealed check status must explicitly attest an original pass, not only a positive test count', () => {
  assert.equal(verifyDeploymentEvidence(reusedFixture()).reused_verified_checks.length, 1);
  const value = reusedFixture();
  value.catalog.at(-1).verified_checks[0].status = 'failed_current';
  assert.throws(() => verifyDeploymentEvidence(value), /not_sealed/);
});
