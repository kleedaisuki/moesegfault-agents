import assert from 'node:assert/strict';
import test from 'node:test';
import { verifyDeploymentEvidence } from './deploy-provenance.mjs';

test('image activation requires the normal verified deployment, not the old core-only daemon exception', () => {
  const value = fixture();
  value.workerRuntimeFeatures = ['web_search', 'file_search', 'code_interpreter', 'image_generation'];
  assert.deepEqual(verifyDeploymentEvidence(value).worker_runtime_features, value.workerRuntimeFeatures);
  const core = coreCompatibleFixture();
  core.workerRuntimeFeatures = ['image_generation'];
  assert.throws(() => verifyDeploymentEvidence(core), /images_require_normal_verified_deployment/);
});

test('Code staging requires the normal verified daemon and preserves its explicit runtime scope', () => {
  const value = fixture();
  value.workerRuntimeFeatures = ['web_search', 'file_search', 'code_interpreter'];
  const proof = verifyDeploymentEvidence(value);
  assert.deepEqual(proof.worker_runtime_features, value.workerRuntimeFeatures);
  assert.notEqual(proof.artifacts.container.status, 'core_compatible_disabled_runtime');
  const incompatible = coreCompatibleFixture();
  incompatible.workerRuntimeFeatures = value.workerRuntimeFeatures;
  assert.throws(() => verifyDeploymentEvidence(incompatible), /code_requires_verified_container/);
});

test('native FileSearch proof records both Worker services without promoting unverified compute', () => {
  const value = coreCompatibleFixture();
  value.workerRuntimeFeatures = ['web_search', 'file_search'];
  const proof = verifyDeploymentEvidence(value);
  assert.deepEqual(proof.worker_runtime_features, ['web_search', 'file_search']);
  assert.equal(proof.artifacts.container.source_sha, 'b1edf97746d3902373dbb05e224799f03d9a3a10');
  assert.equal(proof.staging_acceptance, 'not_executed');
  for (const features of [['file_search', 'file_search'], ['file_search', 'code_interpreter']]) {
    assert.throws(() => verifyDeploymentEvidence({ ...value, workerRuntimeFeatures: features }), /unverified_runtime_feature/);
  }
});

test('runtime feature proof retains the actual old daemon only for a bounded Worker search scope', () => {
  const value = coreCompatibleFixture();
  value.workerRuntimeFeatures = ['web_search'];
  const proof = verifyDeploymentEvidence(value);
  assert.deepEqual(proof.worker_runtime_features, ['web_search']);
  assert.equal(proof.artifacts.container.source_sha, 'b1edf97746d3902373dbb05e224799f03d9a3a10');
  assert.equal(proof.artifacts.container.status, 'core_compatible_disabled_runtime');
  assert.equal(proof.staging_acceptance, 'not_executed');
  for (const features of [['code_interpreter'], ['image_generation'], ['web_search', 'web_search']]) {
    assert.throws(() => verifyDeploymentEvidence({ ...value, workerRuntimeFeatures: features }), /unverified_runtime_feature/);
  }
  value.metadata['42'].jobs.jobs.find(job => job.name === 'worker').conclusion = 'failure';
  assert.throws(() => verifyDeploymentEvidence(value), /required_current_job_not_successful/);
});

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

test('composed selection remains distinct from the immutable compiled artifact proof', () => {
  const value = fixture();
  const selection = { source_sha: source, status: 'composed_accepted',
    receipts: [{ source_sha: base, run_id: '41', reference: 'docs/accepted-scoped-results.md' }], unresolved_checks: [] };
  value.catalog.push({ ...value.catalog[0], selection_base: selection });
  value.plan.components.worker.baseline.selection_base = selection;
  value.plan.required_remediations = [];
  value.plan.checks.worker = [];
  value.verification.worker.files['worker-journeys.json'] = { schema_version: 1, source_sha: source,
    job: 'worker-journeys', status: 'no_selected_tests', results: [] };
  value.expectedPlan = structuredClone(value.plan);
  const proof = verifyDeploymentEvidence(value);
  const worker = proof.baseline_evidence.find(item => item.component === 'worker');
  assert.equal(worker.source_sha, base);
  assert.equal(worker.job_conclusion, 'failure');
  assert.deepEqual(worker.selection_base, selection);
  assert.equal(proof.artifacts.worker.source_sha, source);
  assert.equal(proof.artifacts.worker.id, 88);
  const { hash, ...data } = value;
  const forged = structuredClone(data);
  forged.plan.components.worker.baseline.selection_base = {
    ...forged.plan.components.worker.baseline.selection_base, source_sha: 'd'.repeat(40),
  };
  forged.expectedPlan = structuredClone(forged.plan);
  assert.throws(() => verifyDeploymentEvidence({ ...forged, hash }), /baseline_not_sealed/);
});

test('same-source/dependency Worker artifact may be reused only after current failed-leaf repairs', () => {
  const proof = verifyDeploymentEvidence(fixture(true));
  assert.equal(proof.artifacts.worker.id, 77);
  assert.equal(proof.artifacts.worker.status, 'reused_identical_artifact');
});

test('partial Worker build does not claim an unsuccessful historical library compile passed', () => {
  const value = fixture(true);
  value.catalog[0].verified_phases = ['wasm_build'];
  value.catalog[0].verified_steps = ['wasm'];
  value.metadata['41'].jobs.jobs.find(job => job.name === 'worker').steps[1].conclusion = 'failure';
  const proof = verifyDeploymentEvidence(value);
  assert.equal(proof.artifacts.worker.id, 77);
  assert.equal(proof.baseline_evidence.find(item => item.component === 'worker').job_conclusion, 'failure');
  value.metadata['42'].jobs.jobs.find(job => job.name === 'worker').conclusion = 'failure';
  assert.throws(() => verifyDeploymentEvidence(value), /required_current_job_not_successful/);
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

function partialToolingFixture() {
  const value = fixture(), input = 'e'.repeat(64), original = check('tooling-old');
  const record = { id: original.id, status: 'passed_current', input_fingerprint: input, executed_tests: 2, receipt_path: 'tooling.json' };
  const seal = { source_sha: base, run_id: '41', components: ['tooling'], status: 'partial_verified', receipt_artifact: 'verification-tooling', receipt_artifact_id: 179, verified_checks: [record], unresolved_checks: [{ id: 'tooling-new' }] };
  value.catalog[1].components = value.catalog[1].components.filter(component => component !== 'tooling');
  value.catalog.push(seal);
  value.plan.components.tooling.baseline = { ...seal, job: 'tooling', fingerprint: digest, matches: false };
  value.plan.required_remediations.push('tooling-new');
  value.plan.reused_checks = [{ ...original, ...record, source_sha: base, run_id: '41', job: 'tooling', artifact_name: 'verification-tooling', artifact_id: 179, status: 'reused_verified' }];
  value.expectedPlan = structuredClone(value.plan);
  value.metadata['41'].jobs.jobs.find(job => job.name === 'tooling').conclusion = 'failure';
  value.metadata['41'].artifacts.artifacts.push({ name: 'verification-tooling', id: 179, expired: false, expires_at: '2099-01-01T00:00:00Z', size_in_bytes: 123, workflow_run: { id: 41, head_sha: base } });
  value.metadata['41'].artifacts.total_count++;
  value.checkHash = () => input;
  value.reusedVerification = { '41:verification-tooling': { plan: { source_sha: base, checks: { tooling: [original], current_sdk: [] } }, files: { 'tooling.json': { source_sha: base, job: 'tooling', results: [{ ...original, status: 'passed_current', executed_tests: 2 }] } } } };
  return value;
}

test('partial Tooling job reuses actual positive selectors without a successful job or build claim', () => {
  const proof = verifyDeploymentEvidence(partialToolingFixture());
  const baseline = proof.baseline_evidence.find(item => item.component === 'tooling');
  assert.equal(baseline.status, 'partial_baseline_checks_only');
  assert.equal(baseline.job_conclusion, 'failure');
  assert.equal(proof.reused_verified_checks[0].job, 'tooling');
  assert.equal(proof.reused_verified_checks[0].executed_tests, 2);
  assert.equal(proof.passed_current_checks.includes('tooling-old'), false);
  assert.equal(proof.passed_current_checks.includes('tooling-new'), true);
  assert.equal(Object.hasOwn(proof.artifacts, 'tooling'), false);
});

test('partial Tooling job cannot use missing or failed historical receipts or waive its current repair', () => {
  for (const change of [
    value => { value.plan.reused_checks = []; value.expectedPlan = structuredClone(value.plan); },
    value => { value.reusedVerification['41:verification-tooling'].files['tooling.json'].results[0].status = 'failed_current'; },
    value => { value.verification.tooling.files['tooling.json'].results[0].status = 'failed_current'; },
  ]) {
    const value = partialToolingFixture(); change(value);
    assert.throws(() => verifyDeploymentEvidence(value));
  }
});

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

test('research-only CI success cannot be promoted as an app component assembly', () => {
  const value = fixture();
  Object.assign(value.plan, { verification_scope: 'operator-usage', non_deployable: true, app_validation: 'not_executed' });
  value.expectedPlan = structuredClone(value.plan);
  assert.throws(() => verifyDeploymentEvidence(value), /research_only_verification_is_not_promotable/);
});

function coreCompatibleFixture() {
  const value = fixture();
  const oldSource = 'b1edf97746d3902373dbb05e224799f03d9a3a10', oldRun = '40191955285';
  const old = structuredClone(value.metadata['41']);
  Object.assign(old.run, { id: Number(oldRun), head_sha: oldSource });
  for (const job of old.jobs.jobs) Object.assign(job, { run_id: Number(oldRun), head_sha: oldSource });
  for (const artifact of old.artifacts.artifacts) Object.assign(artifact.workflow_run, { id: Number(oldRun), head_sha: oldSource });
  value.metadata[oldRun] = old;
  value.catalog.push({ source_sha: oldSource, run_id: oldRun, components: ['container'], status: 'verified' });
  const originalHash = value.hash;
  value.hash = (sha, component) => sha === source && component === 'container' ? '9'.repeat(64) : originalHash(sha, component);
  Object.assign(value.plan.components.container, { fingerprint: '9'.repeat(64), action: 'build', validation_scope: 'diagnostic_startup_only', deferred_checks: [{ id: 'new-interpreter' }], artifact: { name: 'container-image', source_sha: source, run_id: '42' } });
  value.plan.diagnostic_only_components = ['container'];
  value.plan.required_jobs.push('container');
  value.expectedPlan = structuredClone(value.plan);
  value.metadata['42'].run.conclusion = 'failure';
  value.metadata['42'].jobs.jobs.find(job => job.name === 'container').conclusion = 'failure';
  value.coreContainer = true;
  return value;
}

test('core-compatible staging sends the actual verified old daemon instead of a failed disabled runtime', () => {
  const proof = verifyDeploymentEvidence(coreCompatibleFixture());
  assert.equal(proof.artifacts.container.source_sha, 'b1edf97746d3902373dbb05e224799f03d9a3a10');
  assert.equal(proof.artifacts.container.run_id, '40191955285');
  assert.equal(proof.artifacts.container.status, 'core_compatible_disabled_runtime');
  assert.equal(proof.artifacts.container.fingerprint, digest);
  assert.equal(proof.component_fingerprints.container, digest);
  assert.equal(proof.runtime_scope, 'core-compatible/disabled-runtime');
  assert.equal(proof.source_ci_conclusion, 'failure');
  assert.deepEqual(proof.excluded_candidate_components, ['container']);
});

test('core-compatible staging cannot waive core failures or promote an unverified daemon', () => {
  for (const change of [
    value => { value.metadata['42'].jobs.jobs.find(job => job.name === 'worker').conclusion = 'failure'; },
    value => { value.metadata['40191955285'].jobs.jobs.find(job => job.name === 'container').conclusion = 'failure'; },
    value => { value.metadata['40191955285'].artifacts.artifacts.find(artifact => artifact.name === 'container-image').expired = true; },
    value => { value.plan.diagnostic_only_components.push('worker'); },
    value => { value.plan.verification_scope = 'operator-usage'; },
  ]) {
    const value = coreCompatibleFixture(); change(value);
    value.expectedPlan = structuredClone(value.plan);
    assert.throws(() => verifyDeploymentEvidence(value));
  }
  const ordinary = coreCompatibleFixture(); ordinary.coreContainer = false;
  assert.throws(() => verifyDeploymentEvidence(ordinary), /not_promotable/);
});

test('core-compatible staging excludes only pending checks for its disabled candidate container', () => {
  const value = coreCompatibleFixture();
  const pending = [{ id: 'interpreter-memory' }, { id: 'interpreter-network' }];
  value.catalog.at(-1).unresolved_checks = pending;
  value.plan.components.container.baseline.unresolved_checks = pending;
  value.plan.required_remediations.push(...pending.map(check => check.id));
  value.expectedPlan = structuredClone(value.plan);
  assert.equal(verifyDeploymentEvidence(value).runtime_scope, 'core-compatible/disabled-runtime');
  value.plan.required_remediations.push('unproven-core-repair');
  value.expectedPlan = structuredClone(value.plan);
  assert.throws(() => verifyDeploymentEvidence(value), /missing_declared_remediation/);
});

test('image-only runtime still requires a normal verified daemon for preserved cleanup apps', async () => {
  const { deploymentRuntime } = await import('./deploy-config.mjs');
  const runtime = deploymentRuntime({ DEPLOY_ENVIRONMENT: 'staging', RUNTIME_FEATURES: 'web_search,file_search,image_generation' });
  assert.equal(runtime.legacyContainerCompatible, false);
  const value = fixture(); value.workerRuntimeFeatures = runtime.workerFeatures;
  assert.deepEqual(verifyDeploymentEvidence(value).worker_runtime_features, runtime.workerFeatures);
  value.coreContainer = true;
  assert.throws(() => verifyDeploymentEvidence(value), /images_require_normal_verified_deployment/);
});

test('native MCP and programmatic activation reaches the shared normal deployment proof', async () => {
  const { deploymentRuntime } = await import('./deploy-config.mjs');
  const runtime = deploymentRuntime({ DEPLOY_ENVIRONMENT: 'staging', RUNTIME_FEATURES: 'web_search,file_search,code_interpreter,mcp,programmatic_tool_calling' });
  const value = fixture(); value.workerRuntimeFeatures = runtime.workerFeatures;
  const proof = verifyDeploymentEvidence(value);
  assert.deepEqual(proof.worker_runtime_features, runtime.workerFeatures);
  assert.equal(proof.staging_acceptance, 'not_executed');
  assert.equal(proof.production_authorized, false);
  assert.throws(() => verifyDeploymentEvidence({ ...value, workerRuntimeFeatures: [...runtime.workerFeatures, 'unknown_future_executor'] }), /unverified_runtime_feature/);
});
