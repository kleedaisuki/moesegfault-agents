import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';
import { runInNewContext } from 'node:vm';

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
  assert.equal((deploy.match(/run-id: \$\{\{ inputs\.source_run \}\}/g) ?? []).length, 3);
  assert.match(deploy, /cancel-in-progress: false/);
});

test('compiled Worker cases use strict SDK validation and keep performance artifacts out of release builds', async () => {
  const ci = await workflow('ci');
  assert.match(ci, /run: npm run test:worker\n\s+env:\n\s+MAGENTS_SCHEMA_PYTHON: \.cache\/e2e-venv\/bin\/python/);
  assert.match(ci, /name: performance-v3\n\s+path: \.cache\/perf\/v3\/\*\.json\n\s+if-no-files-found: warn/);
  assert.match(ci, /name: worker\n\s+path: crates\/agents-worker\/build\n/);
  assert.match(ci, /node --test scripts\/perf\/v3-\*\.test\.mjs/);
});

test('container is saved only after its real journey and deployed without a rebuild fallback', async () => {
  const ci = await workflow('ci');
  const deploy = await workflow('deploy');
  const testStep = ci.indexOf('pytest tests/e2e/test_workspace_daemon.py');
  const saveStep = ci.indexOf('name: Save the exact tested container for promotion');
  assert.ok(testStep >= 0 && saveStep > testStep);
  assert.match(ci, /docker image save .* \| gzip -n -1 > \.temp\/container-release\/image\.tar\.gz/);
  assert.match(ci, /name: container-image\n\s+path: \.temp\/container-release\/\n\s+compression-level: 0\n\s+if-no-files-found: error\n\s+retention-days: 7/);
  assert.match(deploy, /node scripts\/container-artifact\.mjs load \.temp\/container-release/);
  assert.ok(deploy.indexOf('name: Verify and load the exact tested container') < deploy.indexOf('name: Publish tested container'));
  assert.doesNotMatch(deploy, /docker\/(setup-buildx|build-push)-action|docker (?:image )?build/);
});

async function runReleaseGate(runOverrides = {}, changeJobs = jobs => jobs, changePage = page => page) {
  const source = 'a'.repeat(40);
  const repository = 'example/agents';
  const run = {
    id: 123, repository: { full_name: repository }, head_repository: { full_name: repository },
    status: 'completed', head_sha: source, conclusion: 'success', path: '.github/workflows/ci.yml',
    event: 'workflow_dispatch', display_title: `CI - ${source}`, ...runOverrides,
  };
  const jobs = changeJobs(['changes', 'rust', 'worker', 'dashboard', 'container', 'tooling'].map(name => ({
    name, run_id: 123, head_sha: source, status: 'completed', conclusion: 'success',
  })));
  const calls = [];
  const deploy = await workflow('deploy');
  const block = deploy.match(/node --input-type=module <<'NODE'\n([\s\S]*?)\n\s+NODE/);
  assert.ok(block, 'release eligibility must be executable before downloading artifacts');
  const code = block[1].replace(/^\s*import \{ execFileSync \} from 'node:child_process';\n/, '');
  runInNewContext(code, {
    process: { env: { GITHUB_REPOSITORY: repository, GITHUB_SHA: source, SOURCE_RUN: '123' } },
    execFileSync(command, args, options) {
      assert.equal(command, 'gh');
      assert.equal(options.maxBuffer, 2 * 1024 * 1024);
      calls.push(args[1]);
      if (args[1] === `repos/${repository}/actions/runs/123`) return JSON.stringify(run);
      assert.equal(args[1], `repos/${repository}/actions/runs/123/jobs?filter=latest&per_page=100`);
      return JSON.stringify(changePage({ total_count: jobs.length, jobs }));
    },
  });
  return calls;
}

test('release gate permits full successful exact-source CI with one bounded jobs query', async () => {
  assert.equal((await runReleaseGate()).length, 2);
  assert.equal((await runReleaseGate({ event: 'push' })).length, 2);
  assert.equal((await runReleaseGate({}, jobs => [...jobs, {
    name: 'diagnostic', run_id: 123, head_sha: 'a'.repeat(40), status: 'completed', conclusion: 'skipped',
  }])).length, 2);
});

test('release gate rejects diagnostic, partial, foreign and non-successful CI runs', async () => {
  for (const change of [
    { display_title: 'Worker diagnostics - 123' }, { event: 'pull_request' }, { event: 'workflow_run' },
    { head_sha: 'b'.repeat(40) }, { status: 'in_progress' }, { conclusion: 'failure' },
    { id: 456 }, { path: '.github/workflows/diagnose-worker.yml' },
    { repository: { full_name: 'foreign/agents' } }, { head_repository: { full_name: 'foreign/agents' } },
  ]) await assert.rejects(runReleaseGate(change), /not approved/);
  for (const name of ['changes', 'rust', 'worker', 'dashboard', 'container', 'tooling']) {
    await assert.rejects(runReleaseGate({}, jobs => jobs.filter(job => job.name !== name)), /not successful/);
    for (const conclusion of ['skipped', 'failure', 'cancelled', null]) {
      await assert.rejects(runReleaseGate({}, jobs => jobs.map(job => job.name === name ? { ...job, conclusion } : job)), /not successful/);
    }
  }
});

test('release gate rejects duplicate or inconsistent job identity and truncated pagination', async () => {
  await assert.rejects(runReleaseGate({}, jobs => [...jobs, jobs[0]]), /not successful/);
  for (const change of [{ run_id: 456 }, { head_sha: 'b'.repeat(40) }, { status: 'in_progress' }]) {
    await assert.rejects(runReleaseGate({}, jobs => jobs.map(job => ({ ...job, ...change }))), /not successful/);
  }
  for (const total_count of [101, 7, null]) {
    await assert.rejects(runReleaseGate({}, jobs => jobs, page => ({ ...page, total_count })), /incomplete/);
  }
});

test('artifact diagnostics use isolated CI mode and cannot publish release artifacts', async () => {
  const ci = await workflow('ci');
  assert.match(ci, /diagnostic_run:\n\s+description: Reuse a completed CI Worker artifact/);
  assert.match(ci, /changes:\n\s+if: \$\{\{ !inputs\.diagnostic_run \}\}/);
  assert.match(ci, /cancel-in-progress: \$\{\{ !inputs\.diagnostic_run \}\}/);
  assert.match(ci, /Worker diagnostics - /);
  const diagnostic = ci.slice(ci.indexOf('  diagnostic:'));
  assert.ok(diagnostic.startsWith('  diagnostic:'), 'diagnostic job must be separately named');
  assert.doesNotMatch(diagnostic, /secrets\.|cargo |docker |wrangler /);
  const upload = diagnostic.slice(diagnostic.indexOf('uses: actions/upload-artifact@'));
  assert.ok(upload.startsWith('uses: actions/upload-artifact@'));
  assert.match(upload, /name: worker-diagnostic-/);
  assert.doesNotMatch(upload, /name: (?:worker|dashboard|container-image)\n/);
});
