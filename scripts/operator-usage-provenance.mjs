import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { mkdirSync, readFileSync, realpathSync, statSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { pathToFileURL } from 'node:url';
import { parseArgs } from 'node:util';
import { verifyReusedChecks } from './deploy-provenance.mjs';

const SHA = /^[a-f0-9]{40}$/;
const HASH = /^[a-f0-9]{64}$/;
const ID = /^[1-9][0-9]{0,14}$/;
const REPOSITORY = 'kleedaisuki/moesegfault-agents';
export const REQUIRED_CHECKS = [
  'pytest:tests/e2e/test_deepseek_usage_probe.py',
  'node:scripts/deploy-workflow.test.mjs',
  'node:scripts/operator-usage-provenance.test.mjs',
];
export const SOURCE_FILES = [
  'scripts/deepseek-usage-probe.py', '.github/workflows/deploy.yml',
  'scripts/operator-usage-provenance.mjs', 'scripts/incremental-checks.mjs',
  'scripts/deploy-provenance.mjs', 'scripts/verification-baselines.json',
  'tests/e2e/test_deepseek_usage_probe.py', 'scripts/deploy-workflow.test.mjs',
  'scripts/operator-usage-provenance.test.mjs',
];
const need = (value, code) => { if (!value) throw Error(code); };
const canonical = value => Array.isArray(value) ? value.map(canonical) : value && typeof value === 'object' ? Object.fromEntries(Object.keys(value).sort().map(key => [key, canonical(value[key])])) : value;
const same = (a, b) => JSON.stringify(canonical(a)) === JSON.stringify(canonical(b));
const hashBytes = bytes => createHash('sha256').update(bytes).digest('hex');
const hashJson = value => hashBytes(JSON.stringify(value));

function normalRun(bundle, runId, source, repository) {
  const run = bundle?.run;
  need(run && String(run.id) === String(runId) && run.head_sha === source, 'operator_ci_identity_mismatch');
  need(['queued', 'in_progress', 'completed'].includes(run.status), 'operator_ci_status_invalid');
  need(run.repository?.full_name === repository && run.head_repository?.full_name === repository, 'operator_ci_repository_untrusted');
  need(run.path === '.github/workflows/ci.yml' && ['push', 'workflow_dispatch'].includes(run.event) && !run.display_title?.startsWith('Worker diagnostics - '), 'operator_requires_normal_ci');
  need(Array.isArray(bundle.jobs?.jobs) && bundle.jobs.total_count === bundle.jobs.jobs.length && bundle.jobs.total_count <= 100, 'operator_ci_job_page_incomplete');
  return run;
}

function completedJob(bundle, name, source, runId) {
  const jobs = bundle.jobs.jobs.filter(job => job.name === name);
  need(jobs.length === 1 && jobs[0].head_sha === source && String(jobs[0].run_id) === String(runId) && jobs[0].status === 'completed', 'operator_own_job_not_completed');
  return jobs[0];
}

function artifactFor(bundle, name, source, runId, now) {
  const page = bundle?.artifacts;
  need(Array.isArray(page?.artifacts) && page.total_count === page.artifacts.length && page.total_count <= 100, 'operator_artifact_page_incomplete');
  const matches = page.artifacts.filter(artifact => artifact.name === name);
  need(matches.length === 1, 'operator_artifact_missing_or_duplicate');
  const artifact = matches[0];
  need(Number.isSafeInteger(artifact.id) && artifact.id > 0 && String(artifact.workflow_run?.id) === String(runId) && artifact.workflow_run?.head_sha === source, 'operator_artifact_source_mismatch');
  need(artifact.expired === false && Date.parse(artifact.expires_at) > now && Number.isSafeInteger(artifact.size_in_bytes) && artifact.size_in_bytes > 0 && artifact.size_in_bytes <= 16 * 1024 * 1024, 'operator_artifact_expired_or_unbounded');
  return artifact;
}

function checkDescriptor(check) {
  need(check && REQUIRED_CHECKS.includes(check.id) && Array.isArray(check.argv) && check.env && typeof check.env === 'object' && !Array.isArray(check.env), 'operator_selector_missing');
  need(Object.keys(check.env).length === 0, 'operator_check_has_unexpected_environment');
  const file = check.id.slice(check.id.indexOf(':') + 1);
  need(check.argv.includes(file), 'operator_selector_wrong_file');
  need(check.id.startsWith('node:') ? check.argv[0] === 'node' && check.argv.includes('--test') : check.argv.includes('pytest') && check.argv.includes('-m'), 'operator_selector_wrong_runner');
  need(HASH.test(check.input_fingerprint ?? ''), 'operator_selector_fingerprint_missing');
}

function currentCheck(plan, receipt, check, source, checkHash) {
  checkDescriptor(check);
  need(receipt?.schema_version === 1 && receipt.source_sha === source && receipt.job === 'tooling' && Array.isArray(receipt.results), 'operator_current_receipt_wrong_source');
  need(checkHash(source, check) === check.input_fingerprint, 'operator_current_inputs_changed');
  const matches = receipt.results.filter(result => result.id === check.id);
  need(matches.length === 1, 'operator_current_result_missing_or_duplicate');
  const result = matches[0];
  need(same(result.argv, check.argv) && same(result.env, check.env), 'operator_current_selector_changed');
  need(result.status === 'passed_current' && Number.isSafeInteger(result.executed_tests) && result.executed_tests > 0, 'operator_current_check_not_positive');
  need(result.input_fingerprint === check.input_fingerprint, 'operator_current_result_fingerprint_changed');
  return { id: check.id, status: 'passed_current', source_sha: source, argv: check.argv, env: check.env, executed_tests: result.executed_tests, input_fingerprint: check.input_fingerprint };
}

export function verifyOperatorUsageEvidence({ source, sourceRun, repository, plan, expectedPlan, catalog, metadata, verification, reusedVerification = {}, checkHash, sourceHashes, planFileSha256 }, now = Date.now()) {
  need(SHA.test(source ?? '') && ID.test(String(sourceRun)) && repository === REPOSITORY, 'operator_source_identity_required');
  need(HASH.test(planFileSha256 ?? ''), 'operator_plan_file_hash_required');
  need(typeof checkHash === 'function', 'operator_check_hash_function_required');
  need(plan?.schema_version === 1 && plan.source_sha === source && plan.policy === 'incremental-only' && plan.planned_checks_are_not_results === true && same(plan, expectedPlan), 'operator_plan_not_recomputed');
  need(SOURCE_FILES.every(file => HASH.test(sourceHashes?.[file] ?? '')) && Object.keys(sourceHashes).length === SOURCE_FILES.length, 'operator_exact_source_hashes_required');
  const bundle = metadata[String(sourceRun)];
  const run = normalRun(bundle, sourceRun, source, repository);
  const changes = completedJob(bundle, 'changes', source, sourceRun);
  need(changes.conclusion === 'success', 'operator_current_plan_job_not_successful');
  const planArtifact = artifactFor(bundle, 'incremental-plan', source, sourceRun, now);
  const current = [], reused = [];
  for (const id of REQUIRED_CHECKS) {
    const pending = (plan.checks?.tooling ?? []).filter(check => check.id === id);
    const retained = (plan.reused_checks ?? []).filter(check => check.id === id);
    need(pending.length + retained.length === 1, 'operator_required_check_missing_or_ambiguous');
    if (retained.length) { checkDescriptor(retained[0]); reused.push(retained[0]); continue; }
    const job = completedJob(bundle, 'tooling', source, sourceRun);
    const artifact = artifactFor(bundle, 'verification-tooling', source, sourceRun, now);
    need(verification && same(verification.plan, plan), 'operator_downloaded_plan_mismatch');
    need(HASH.test(verification.file_hashes?.['tooling.json'] ?? '') && HASH.test(verification.file_hashes?.['plan.json'] ?? ''), 'operator_current_file_hashes_missing');
    const proof = currentCheck(plan, verification.files?.['tooling.json'], pending[0], source, checkHash);
    current.push({ ...proof, run_id: String(sourceRun), artifact_id: artifact.id, job_status: job.status, job_conclusion: job.conclusion, receipt_sha256: verification.file_hashes['tooling.json'], plan_sha256: verification.file_hashes['plan.json'] });
  }
  const retained = verifyReusedChecks({ plan: { ...plan, reused_checks: reused }, catalog, source, repository, metadata, reusedVerification, checkHash, allowInProgress: true }, now);
  for (const check of retained) {
    const selected = reused.find(value => value.id === check.id);
    const receipt = reusedVerification[`${check.run_id}:${selected.artifact_name}`]?.files?.[check.receipt_path];
    need(receipt?.schema_version === 1, 'operator_historical_receipt_schema_invalid');
    const row = receipt.results.find(value => value.id === check.id);
    need(!Object.hasOwn(row, 'input_fingerprint') || row.input_fingerprint === check.input_fingerprint, 'operator_historical_result_fingerprint_changed');
    check.argv = selected.argv; check.env = selected.env;
    const historical = completedJob(metadata[check.run_id], 'tooling', check.source_sha, check.run_id);
    check.job_status = historical.status; check.job_conclusion = historical.conclusion;
    const files = reusedVerification[`${check.run_id}:${selected.artifact_name}`].file_hashes;
    need(HASH.test(files?.[check.receipt_path] ?? '') && HASH.test(files?.['plan.json'] ?? ''), 'operator_historical_file_hashes_missing');
    check.receipt_sha256 = files[check.receipt_path]; check.plan_sha256 = files['plan.json'];
  }
  const checks = REQUIRED_CHECKS.map(id => [...current, ...retained].find(check => check.id === id));
  return { schema_version: 1, scope: 'operator_usage_helper_only', status: 'critical_operator_checks_verified', source_sha: source, source_run: String(sourceRun), ci_status: run.status, ci_conclusion: run.conclusion ?? null, plan_artifact_id: planArtifact.id, plan_sha256: planFileSha256, plan_json_sha256: hashJson(plan), source_files: sourceHashes, helper_source: { source_sha: source, path: 'scripts/deepseek-usage-probe.py', sha256: sourceHashes['scripts/deepseek-usage-probe.py'] }, workflow_source: { source_sha: source, path: '.github/workflows/deploy.yml', sha256: sourceHashes['.github/workflows/deploy.yml'] }, checks, platform_acceptance: 'not_established', paid_execution_authorized: false, production_authorized: false };
}

function readJson(file) {
  need(statSync(file).size <= 4 * 1024 * 1024, 'operator_json_size_exceeded');
  return JSON.parse(readFileSync(file, 'utf8').replace(/^\uFEFF/, ''));
}

export async function main(argv = process.argv.slice(2), environment = process.env, execute = execFileSync, loadEngine = () => import('./incremental-checks.mjs')) {
  const { values } = parseArgs({ args: argv, options: { run: { type: 'string' }, directory: { type: 'string', default: '.temp/operator-probe-verification' } } });
  const source = environment.GITHUB_SHA, repository = environment.GITHUB_REPOSITORY;
  need(SHA.test(source ?? '') && ID.test(values.run ?? '') && repository === REPOSITORY, 'operator_pushed_source_and_run_required');
  const root = realpathSync('.'), directory = path.resolve(values.directory);
  need(directory.startsWith(path.join(root, '.temp') + path.sep), 'operator_output_outside_project_temp');
  mkdirSync(directory, { recursive: true });
  need(realpathSync(directory).startsWith(realpathSync(path.join(root, '.temp')) + path.sep), 'operator_output_realpath_escape');
  const api = suffix => JSON.parse(execute('gh', ['api', `repos/${repository}/${suffix}`], { encoding: 'utf8', windowsHide: true, timeout: 60000, maxBuffer: 4 * 1024 * 1024 }));
  const metadata = {};
  const load = run => { need(ID.test(String(run)), 'operator_metadata_run_invalid'); if (!metadata[String(run)]) metadata[String(run)] = { run: api(`actions/runs/${run}`), jobs: api(`actions/runs/${run}/jobs?filter=latest&per_page=100`), artifacts: api(`actions/runs/${run}/artifacts?per_page=100`) }; return metadata[String(run)]; };
  const download = (run, name, expectedSource) => {
    const bundle = load(run); normalRun(bundle, run, expectedSource, repository);
    const artifact = artifactFor(bundle, name, expectedSource, run, Date.now());
    const folder = path.join(directory, `${run}-${artifact.id}`);
    mkdirSync(folder, { recursive: false });
    execute('gh', ['run', 'download', String(run), '--repo', repository, '--name', name, '--dir', folder], { encoding: 'utf8', windowsHide: true, timeout: 60000, maxBuffer: 1024 * 1024 });
    return folder;
  };
  const current = load(values.run); normalRun(current, values.run, source, repository);
  need(completedJob(current, 'changes', source, values.run).conclusion === 'success', 'operator_plan_job_incomplete');
  const planFolder = download(values.run, 'incremental-plan', source);
  const plan = readJson(path.join(planFolder, 'plan.json'));
  const gitBytes = file => execute('git', ['show', `${source}:${file}`], { maxBuffer: 2 * 1024 * 1024, timeout: 10000, windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'] });
  need(execute('git', ['rev-parse', 'HEAD'], { encoding: 'utf8', windowsHide: true, timeout: 10000 }).trim() === source, 'operator_checkout_is_not_pushed_source');
  const sourceHashes = Object.fromEntries(SOURCE_FILES.map(file => {
    const committed = hashBytes(gitBytes(file));
    need(hashBytes(readFileSync(path.join(root, file))) === committed, 'operator_runtime_source_differs_from_commit');
    return [file, committed];
  }));
  const catalog = JSON.parse(gitBytes('scripts/verification-baselines.json').toString('utf8').replace(/^\uFEFF/, ''));
  const engine = await loadEngine();
  const scope = plan.components?.container?.validation_scope === 'diagnostic_startup_only' ? 'startup-1g' : 'incremental';
  const expectedPlan = engine.plan(source, catalog, scope);
  for (const item of Object.values(expectedPlan.components)) if (item.action === 'build' && item.artifact) item.artifact.run_id = values.run;
  const receiptFiles = folder => {
    const files = ['plan.json', 'tooling.json'];
    const stored = Object.fromEntries(files.map(file => {
      const target = realpathSync(path.join(folder, file));
      need(target.startsWith(realpathSync(folder) + path.sep), 'operator_downloaded_receipt_escape');
      return [file, readJson(target)];
    }));
    return { plan: stored['plan.json'], files: { 'tooling.json': stored['tooling.json'] }, file_hashes: Object.fromEntries(files.map(file => [file, hashBytes(readFileSync(path.join(folder, file)))])) };
  };
  const hasCurrent = (plan.checks?.tooling ?? []).some(check => REQUIRED_CHECKS.includes(check.id));
  const verification = hasCurrent ? receiptFiles(download(values.run, 'verification-tooling', source)) : null;
  const reusedVerification = {};
  for (const check of plan.reused_checks ?? []) {
    if (!REQUIRED_CHECKS.includes(check.id)) continue;
    need(check.artifact_name === 'verification-tooling' && check.receipt_path === 'tooling.json', 'operator_reused_receipt_location_invalid');
    const key = `${check.run_id}:${check.artifact_name}`;
    if (!reusedVerification[key]) reusedVerification[key] = receiptFiles(download(check.run_id, check.artifact_name, check.source_sha));
  }
  const receipt = verifyOperatorUsageEvidence({ source, sourceRun: values.run, repository, plan, expectedPlan, catalog, metadata, verification, reusedVerification, checkHash: engine.checkFingerprint, sourceHashes, planFileSha256: hashBytes(readFileSync(path.join(planFolder, 'plan.json'))) });
  writeFileSync(path.join(directory, 'operator-usage-provenance.json'), JSON.stringify(receipt, null, 2) + '\n', { flag: 'wx' });
  process.stdout.write(JSON.stringify(receipt) + '\n');
}

if (process.argv[1] && import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href) main().catch(() => { process.stderr.write('operator_usage_provenance_rejected\n'); process.exitCode = 1; });
