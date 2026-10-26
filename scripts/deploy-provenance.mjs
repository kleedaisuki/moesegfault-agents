import { execFileSync } from 'node:child_process';
import { appendFileSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { pathToFileURL } from 'node:url';
import { parseArgs } from 'node:util';
import { deploymentRuntime } from './deploy-config.mjs';

const SHA = /^[a-f0-9]{40}$/;
const DIGEST = /^[a-f0-9]{64}$/;
const ID = /^[1-9][0-9]{0,14}$/;
const COMPONENTS = { worker: 'worker', native: 'rust', container: 'container', dashboard: 'dashboard', tooling: 'tooling' };
const ARTIFACTS = { worker: 'worker', container: 'container-image', dashboard: 'dashboard' };
const FILES = { rust: ['rust.json', 'native-build.json'], worker: ['worker-rust.json', 'worker-journeys.json'], dashboard: ['dashboard.json'], container: ['container.json'], tooling: ['tooling.json', 'current_sdk.json'] };
const CORE_CONTAINER = { source_sha: 'b1edf97746d3902373dbb05e224799f03d9a3a10', run_id: '40191955285' };
const fail = code => { throw Error(code); };
const need = (value, code) => { if (!value) fail(code); };
const canonical = value => Array.isArray(value) ? value.map(canonical) : value && typeof value === 'object' ? Object.fromEntries(Object.keys(value).sort().map(key => [key, canonical(value[key])])) : value;
const same = (left, right) => JSON.stringify(canonical(left)) === JSON.stringify(canonical(right));

function normalRun(bundle, runId, source, repository, requireSuccess, allowInProgress = false) {
  const run = bundle?.run;
  need(run && String(run.id) === String(runId) && run.head_sha === source && (run.status === 'completed' || allowInProgress && run.status === 'in_progress'), 'ci_identity_not_completed');
  need(run.repository?.full_name === repository && run.head_repository?.full_name === repository, 'untrusted_ci_repository');
  need(run.path === '.github/workflows/ci.yml' && ['push', 'workflow_dispatch'].includes(run.event) && !run.display_title?.startsWith('Worker diagnostics - '), 'not_normal_ci');
  need(!requireSuccess || run.conclusion === 'success', 'candidate_ci_not_successful');
  need(Array.isArray(bundle.jobs?.jobs) && bundle.jobs.total_count === bundle.jobs.jobs.length && bundle.jobs.total_count <= 100, 'incomplete_ci_jobs');
  return run;
}

function jobFor(bundle, name, source, runId) {
  const matches = bundle.jobs.jobs.filter(job => job.name === name);
  need(matches.length === 1, 'missing_or_duplicate_job');
  const job = matches[0];
  need(job.head_sha === source && String(job.run_id) === String(runId) && job.status === 'completed', 'job_source_not_completed');
  return job;
}

function artifactFor(bundle, name, runId, source, now) {
  const page = bundle?.artifacts;
  need(Array.isArray(page?.artifacts) && page.total_count === page.artifacts.length && page.total_count <= 100, 'incomplete_artifact_page');
  const matches = page.artifacts.filter(artifact => artifact.name === name);
  need(matches.length === 1, 'missing_or_duplicate_artifact');
  const artifact = matches[0];
  need(String(artifact.workflow_run?.id) === String(runId) && artifact.workflow_run?.head_sha === source, 'artifact_source_mismatch');
  need(artifact.expired === false && Date.parse(artifact.expires_at) > now && artifact.size_in_bytes > 0, 'artifact_expired_or_empty');
  return artifact;
}

function selectedFor(plan, file) {
  if (file === 'worker-rust.json') return plan.checks.worker.filter(check => check.argv[0] === 'cargo');
  if (file === 'worker-journeys.json') return plan.checks.worker.filter(check => check.argv[0] !== 'cargo');
  return plan.checks[file.replace('.json', '')];
}

function verifyReceipts(plan, verification, job) {
  const stored = verification[job];
  need(stored && same(stored.plan, plan), 'verification_plan_mismatch_or_missing');
  const executed = [];
  for (const file of FILES[job]) {
    const receipt = stored.files?.[file];
    need(receipt?.source_sha === plan.source_sha, 'current_receipt_missing_or_wrong_source');
    if (file === 'native-build.json') {
      const expected = plan.native_packages.length ? 'passed_current' : 'no_changed_native_packages';
      const argv = ['cargo', 'clippy', '--locked', ...plan.native_packages.flatMap(name => ['-p', name]), '--all-targets', '--', '-D', 'warnings'];
      need(receipt.status === expected && same(receipt.argv, argv), 'native_build_receipt_not_verified');
      continue;
    }
    const selected = selectedFor(plan, file);
    need(receipt.schema_version === 1 && receipt.job === file.replace('.json', '') && Array.isArray(receipt.results), 'invalid_result_receipt');
    need(receipt.results.length === selected.length, 'missing_or_extra_current_results');
    need(selected.length > 0 || receipt.status === 'no_selected_tests', 'unexecuted_checks_mislabeled_success');
    for (let index = 0; index < selected.length; index++) {
      const result = receipt.results[index], check = selected[index];
      need(result.id === check.id && same(result.argv, check.argv) && same(result.env, check.env), 'result_selector_mismatch');
      need(result.status === 'passed_current' && Number.isSafeInteger(result.executed_tests) && result.executed_tests > 0, 'current_check_not_executed_successfully');
      if (check.input_fingerprint != null) need(result.input_fingerprint === check.input_fingerprint, 'current_check_input_fingerprint_mismatch');
      executed.push(result.id);
    }
  }
  return executed;
}

export function requirePromotionScope(plan, coreContainer = false) {
  need(plan?.verification_scope == null || plan.verification_scope === 'incremental', 'research_only_verification_is_not_promotable');
  need(plan?.non_deployable !== true, 'research_only_verification_is_not_promotable');
  need(!(plan?.diagnostic_only_components ?? []).some(name => !coreContainer || name !== 'container'), 'diagnostic_only_component_is_not_promotable');
  for (const [name, item] of Object.entries(plan?.components ?? {})) {
    if (coreContainer && name === 'container') continue;
    need(item.validation_scope == null || item.validation_scope === 'incremental', 'diagnostic_only_component_is_not_promotable');
    need(!(item.deferred_checks?.length > 0), 'deferred_capability_checks_are_not_promotable');
  }
}

export function verifyReusedChecks({ plan, catalog, source, repository, metadata, reusedVerification, checkHash, allowInProgress = false }, now) {
  const verified = [];
  for (const check of plan.reused_checks ?? []) {
    need(check.status === 'reused_verified' && SHA.test(check.source_sha ?? '') && ID.test(String(check.run_id)) && DIGEST.test(check.input_fingerprint ?? '') && Number.isSafeInteger(check.executed_tests) && check.executed_tests > 0 && typeof checkHash === 'function', 'invalid_reused_check');
    const seal = catalog.find(base => base.source_sha === check.source_sha && String(base.run_id) === String(check.run_id) && base.receipt_artifact === check.artifact_name && (base.verified_checks ?? []).some(record => record.id === check.id && record.status === 'passed_current' && record.receipt_path === check.receipt_path && record.input_fingerprint === check.input_fingerprint && record.executed_tests === check.executed_tests && record.executed_tests > 0));
    need(seal, 'reused_check_not_sealed');
    need(checkHash(source, check) === check.input_fingerprint && checkHash(check.source_sha, check) === check.input_fingerprint, 'reused_check_inputs_changed');
    const old = metadata[String(check.run_id)];
    normalRun(old, check.run_id, check.source_sha, repository, false, allowInProgress);
    const job = check.job === 'current_sdk' ? 'tooling' : check.job;
    need(Object.hasOwn(FILES, job) && check.artifact_name === `verification-${job}` && FILES[job].includes(check.receipt_path) && check.receipt_path !== 'native-build.json', 'invalid_reused_receipt_location');
    jobFor(old, job, check.source_sha, check.run_id);
    const artifact = artifactFor(old, check.artifact_name, check.run_id, check.source_sha, now);
    if (check.artifact_id != null) need(artifact.id === check.artifact_id && seal.receipt_artifact_id === artifact.id, 'reused_receipt_artifact_changed');
    const stored = reusedVerification?.[`${check.run_id}:${check.artifact_name}`];
    need(stored?.plan?.source_sha === check.source_sha, 'historical_plan_missing');
    const originalChecks = selectedFor(stored.plan, check.receipt_path);
    need(Array.isArray(originalChecks), 'historical_selector_list_missing');
    const original = originalChecks.find(value => value.id === check.id);
    need(original && same(original.argv, check.argv) && same(original.env, check.env) && checkHash(check.source_sha, original) === check.input_fingerprint, 'historical_selector_or_inputs_changed');
    const receipt = stored.files?.[check.receipt_path];
    need(receipt?.source_sha === check.source_sha && receipt.job === check.receipt_path.replace('.json', ''), 'historical_result_source_mismatch');
    const results = receipt.results?.filter(result => result.id === check.id) ?? [];
    need(results.length === 1, 'historical_check_missing_or_duplicate');
    const result = results[0];
    need(result.status === 'passed_current' && result.executed_tests === check.executed_tests && result.executed_tests > 0 && same(result.argv, original.argv) && same(result.env, original.env), 'historical_check_did_not_pass');
    if (result.input_fingerprint != null) need(result.input_fingerprint === check.input_fingerprint, 'historical_result_fingerprint_changed');
    verified.push({ id: check.id, job, status: 'reused_verified', source_sha: check.source_sha, run_id: String(check.run_id), artifact_id: artifact.id, receipt_path: check.receipt_path, input_fingerprint: check.input_fingerprint, executed_tests: check.executed_tests });
  }
  return verified;
}

export function verifyDeploymentEvidence({ plan, expectedPlan, catalog, source, sourceRun, repository, metadata, verification, hash, checkHash, reusedVerification, coreContainer = false, workerRuntimeFeatures = [] }, now = Date.now()) {
  need(Array.isArray(workerRuntimeFeatures) && new Set(workerRuntimeFeatures).size === workerRuntimeFeatures.length && workerRuntimeFeatures.every(feature => ['web_search', 'file_search', 'code_interpreter', 'image_generation'].includes(feature)), 'unverified_runtime_feature');
  need(!workerRuntimeFeatures.includes('code_interpreter') || !coreContainer, 'unverified_runtime_feature:code_requires_verified_container');
  need(!workerRuntimeFeatures.includes('image_generation') || !coreContainer, 'unverified_runtime_feature:images_require_normal_verified_deployment');
  need(SHA.test(source ?? '') && ID.test(String(sourceRun)) && /^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/.test(repository ?? ''), 'invalid_candidate_identity');
  requirePromotionScope(plan, coreContainer);
  need(plan?.schema_version === 1 && plan.policy === 'incremental-only' && plan.planned_checks_are_not_results === true && plan.source_sha === source && same(plan, expectedPlan), 'candidate_plan_not_recomputed');
  const current = metadata[String(sourceRun)];
  normalRun(current, sourceRun, source, repository, !coreContainer);
  need(Array.isArray(plan.required_jobs) && new Set(plan.required_jobs).size === plan.required_jobs.length && plan.required_jobs.includes('changes'), 'invalid_required_jobs');
  const baselineEvidence = [], artifacts = {}, executed = [];
  const unresolved = new Set();
  const excludedRepairs = new Set(coreContainer ? (plan.components?.container?.baseline?.unresolved_checks ?? []).map(check => check.id) : []);
  for (const [component, name] of Object.entries(COMPONENTS)) {
    const item = plan.components?.[component];
    const compatible = coreContainer && component === 'container';
    const base = compatible ? { ...CORE_CONTAINER, job: 'container', fingerprint: hash(CORE_CONTAINER.source_sha, 'container') } : item?.baseline;
    need(item && ['build', 'reuse'].includes(item.action) && DIGEST.test(item.fingerprint ?? '') && item.fingerprint === hash(source, component), 'component_fingerprint_mismatch');
    need(SHA.test(base?.source_sha ?? '') && ID.test(String(base.run_id)) && base.job === name && base.fingerprint === hash(base.source_sha, component), 'baseline_fingerprint_mismatch');
    const seal = catalog.find(value => value.source_sha === base.source_sha && String(value.run_id) === String(base.run_id) && value.components.includes(component)
      && (!base.selection_base || same(value.selection_base, base.selection_base)));
    need(seal, 'baseline_not_sealed');
    need(seal.status == null || ['verified', 'partial_verified'].includes(seal.status), 'baseline_status_not_promotable');
    requirePromotionScope({ components: { [component]: seal } });
    if (!compatible) need(base.matches === (base.fingerprint === item.fingerprint) && (item.action === 'reuse') === base.matches, 'reuse_does_not_match_source_and_dependencies');
    const old = metadata[String(base.run_id)];
    normalRun(old, base.run_id, base.source_sha, repository, false);
    const baselineJob = jobFor(old, name, base.source_sha, base.run_id);
    if (seal.status === 'partial_verified') {
      const workerBuild = component === 'worker' && Array.isArray(seal.verified_steps) && seal.verified_steps.length > 0 && seal.verified_phases?.includes('wasm_build') && Number.isSafeInteger(seal.artifact_id);
      const toolingChecks = component === 'tooling' && seal.artifact_id == null && seal.receipt_artifact === 'verification-tooling' && seal.verified_checks?.length > 0 && seal.unresolved_checks?.length > 0;
      need(workerBuild || toolingChecks, 'partial_baseline_without_positive_proof');
      if (workerBuild) for (const step of seal.verified_steps) need(baselineJob.steps?.filter(value => value.name === step && value.conclusion === 'success').length === 1, 'baseline_phase_not_successful');
    } else need(baselineJob.conclusion === 'success', 'historical_component_job_not_successful');
    if (!compatible) for (const check of (seal.selection_base ?? seal).unresolved_checks ?? []) unresolved.add(check.id);
    baselineEvidence.push({ component, source_sha: base.source_sha, run_id: String(base.run_id), fingerprint: base.fingerprint, job_conclusion: baselineJob.conclusion, status: seal.status === 'partial_verified' ? component === 'tooling' ? 'partial_baseline_checks_only' : 'partial_baseline_phases_only' : 'passed_baseline_not_reexecuted',
      ...(seal.selection_base ? { selection_base: seal.selection_base } : {}) });
    if (!ARTIFACTS[component]) continue;
    const build = !compatible && item.action === 'build';
    const expectedRun = build ? String(sourceRun) : String(base.run_id);
    const expectedSource = build ? source : base.source_sha;
    const claimed = compatible ? { name: 'container-image', run_id: expectedRun, source_sha: expectedSource } : item.artifact;
    need(claimed?.name === ARTIFACTS[component] && String(claimed.run_id) === expectedRun && claimed.source_sha === expectedSource, 'artifact_plan_mismatch');
    const binary = artifactFor(metadata[expectedRun], claimed.name, expectedRun, expectedSource, now);
    if (item.action === 'reuse' && seal.artifact_id != null) need(binary.id === seal.artifact_id && claimed.artifact_id === seal.artifact_id, 'partial_artifact_identity_changed');
    artifacts[component] = { name: binary.name, id: binary.id, run_id: expectedRun, source_sha: expectedSource, digest: binary.digest ?? null, fingerprint: compatible ? base.fingerprint : item.fingerprint, status: compatible ? 'core_compatible_disabled_runtime' : build ? 'built_current' : 'reused_identical_artifact' };
  }
  for (const job of plan.required_jobs) {
    if (coreContainer && job === 'container') continue;
    need(job === 'changes' || Object.hasOwn(FILES, job), 'unknown_required_job');
    need(jobFor(current, job, source, sourceRun).conclusion === 'success', 'required_current_job_not_successful');
    if (job === 'changes') continue;
    artifactFor(current, `verification-${job}`, sourceRun, source, now);
    executed.push(...verifyReceipts(plan, verification, job));
  }
  const reused = verifyReusedChecks({ plan, catalog, source, repository, metadata, reusedVerification, checkHash }, now);
  for (const base of baselineEvidence.filter(value => value.status === 'partial_baseline_checks_only')) need(reused.some(check => check.job === 'tooling' && check.source_sha === base.source_sha && check.run_id === base.run_id), 'partial_baseline_without_verified_receipt');
  need(same([...unresolved].sort(), (plan.required_remediations ?? []).filter(id => !excludedRepairs.has(id)).sort()), 'missing_declared_remediation');
  for (const id of unresolved) need(executed.includes(id) || reused.some(check => check.id === id), 'unresolved_baseline_failure_not_repaired');
  const fingerprints = Object.fromEntries(Object.entries(plan.components).map(([name, item]) => [name, coreContainer && name === 'container' ? artifacts.container.fingerprint : item.fingerprint]));
  return { schema_version: 1, status: 'incremental_build_and_selected_checks_verified', source_sha: source, source_run: String(sourceRun), component_fingerprints: fingerprints, diagnostic_only_components: [], ...(workerRuntimeFeatures.length ? { worker_runtime_features: workerRuntimeFeatures } : {}), ...(coreContainer ? { runtime_scope: 'core-compatible/disabled-runtime', excluded_candidate_components: ['container'], source_ci_conclusion: current.run.conclusion, worker_runtime_features: workerRuntimeFeatures } : {}), artifacts, baseline_evidence: baselineEvidence, passed_current_checks: executed, reused_verified_checks: reused, staging_acceptance: 'not_executed', production_authorized: false };
}

async function main() {
  const { values } = parseArgs({ options: { plan: { type: 'string' }, run: { type: 'string' }, directory: { type: 'string', default: '.temp/deploy-verification' } } });
  const source = process.env.GITHUB_SHA, repository = process.env.GITHUB_REPOSITORY;
  const runtime = deploymentRuntime(process.env);
  const coreContainer = runtime.legacyContainerCompatible;
  need(SHA.test(source ?? '') && ID.test(values.run ?? ''), 'explicit_source_run_required');
  need(/^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/.test(repository ?? ''), 'invalid_ci_repository');
  const directory = path.resolve(values.directory), root = path.resolve('.');
  need(directory.startsWith(path.join(root, '.temp') + path.sep), 'verification_output_outside_project');
  const parse = file => JSON.parse(readFileSync(file, 'utf8').replace(/^\uFEFF/, ''));
  need(path.resolve(values.plan).startsWith(path.join(root, '.temp') + path.sep), 'plan_outside_project_temp');
  const plan = parse(values.plan), catalog = parse('scripts/verification-baselines.json');
  requirePromotionScope(plan, coreContainer);
  const module = await import('./incremental-checks.mjs');
  const containerScope = plan.components?.container?.validation_scope === 'diagnostic_startup_only' ? 'startup-1g' : 'incremental';
  const expectedPlan = module.plan(source, catalog, containerScope);
  for (const item of Object.values(expectedPlan.components)) if (item.action === 'build' && item.artifact) item.artifact.run_id = values.run;
  need(same(plan, expectedPlan), 'candidate_plan_not_recomputed');
  const api = suffix => JSON.parse(execFileSync('gh', ['api', `repos/${repository}/${suffix}`], { encoding: 'utf8', maxBuffer: 4 * 1024 * 1024 }));
  const metadata = {};
  const load = run => { need(ID.test(String(run)), 'invalid_metadata_run'); if (!metadata[String(run)]) metadata[String(run)] = { run: api(`actions/runs/${run}`), jobs: api(`actions/runs/${run}/jobs?filter=latest&per_page=100`), artifacts: api(`actions/runs/${run}/artifacts?per_page=100`) }; };
  load(values.run);
  normalRun(metadata[String(values.run)], values.run, source, repository, !coreContainer);
  for (const item of Object.values(plan.components ?? {})) load(item.baseline.run_id);
  if (coreContainer) load(CORE_CONTAINER.run_id);
  for (const check of plan.reused_checks ?? []) load(check.run_id);
  const verification = {};
  for (const job of plan.required_jobs ?? []) {
    if (job === 'changes') continue;
    if (coreContainer && job === 'container') continue;
    need(Object.hasOwn(FILES, job), 'unknown_current_verification_job');
    need(jobFor(metadata[String(values.run)], job, source, values.run).conclusion === 'success', 'required_current_job_not_successful');
    artifactFor(metadata[String(values.run)], `verification-${job}`, values.run, source, Date.now());
    const folder = path.join(directory, job);
    mkdirSync(folder, { recursive: true });
    execFileSync('gh', ['run', 'download', values.run, '--repo', repository, '--name', `verification-${job}`, '--dir', folder], { stdio: 'inherit' });
    verification[job] = { plan: parse(path.join(folder, 'plan.json')), files: Object.fromEntries(FILES[job].map(file => [file, parse(path.join(folder, file))])) };
  }
  const reusedVerification = {};
  for (const check of plan.reused_checks ?? []) {
    const key = `${check.run_id}:${check.artifact_name}`;
    need(/^verification-(rust|worker|dashboard|container|tooling)$/.test(check.artifact_name ?? ''), 'invalid_reused_artifact_name');
    if (!reusedVerification[key]) {
      const folder = path.join(directory, 'reused', `${check.run_id}-${check.artifact_name}`);
      mkdirSync(folder, { recursive: true });
      execFileSync('gh', ['run', 'download', String(check.run_id), '--repo', repository, '--name', check.artifact_name, '--dir', folder], { stdio: 'inherit' });
      reusedVerification[key] = { plan: parse(path.join(folder, 'plan.json')), files: {} };
    }
    need(Object.values(FILES).flat().includes(check.receipt_path), 'invalid_reused_receipt_path');
    reusedVerification[key].files[check.receipt_path] = parse(path.join(directory, 'reused', `${check.run_id}-${check.artifact_name}`, check.receipt_path));
  }
  const receipt = verifyDeploymentEvidence({ plan, expectedPlan, catalog, source, sourceRun: values.run, repository, metadata, verification, hash: module.fingerprint, checkHash: module.checkFingerprint, reusedVerification, coreContainer, workerRuntimeFeatures: runtime.workerFeatures });
  mkdirSync(directory, { recursive: true });
  writeFileSync(path.join(directory, 'deployment-provenance.json'), JSON.stringify(receipt, null, 2) + '\n', { flag: 'wx' });
  for (const [name, artifact] of Object.entries(receipt.artifacts)) if (process.env.GITHUB_OUTPUT) appendFileSync(process.env.GITHUB_OUTPUT, `${name}_artifact_run=${artifact.run_id}\n${name}_artifact_sha=${artifact.source_sha}\n`);
  process.stdout.write(JSON.stringify(receipt) + '\n');
}

if (process.argv[1] && import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href) Promise.resolve().then(main).catch(error => { process.stderr.write(`deployment_provenance_rejected:${error.message}\n`); process.exitCode = 1; });
