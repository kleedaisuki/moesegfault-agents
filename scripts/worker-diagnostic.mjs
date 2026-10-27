import { execFileSync } from 'node:child_process';
import { appendFileSync, mkdirSync, writeFileSync } from 'node:fs';
import { pathToFileURL } from 'node:url';

const sha = /^[a-f0-9]{40}$/;

export function diagnosticRunId(value) {
  if (!/^[1-9][0-9]{0,14}$/.test(value ?? '')) throw Error('Invalid diagnostic source run ID');
  return Number(value);
}

export function diagnosticReceipt(run, artifacts, repository, sourceRun, testSource) {
  if (!Number.isSafeInteger(sourceRun) || sourceRun <= 0 || !sha.test(testSource ?? '')
    || run.id !== sourceRun || run.repository?.full_name !== repository
    || run.head_repository?.full_name !== repository
    || run.path !== '.github/workflows/ci.yml'
    || run.display_title?.startsWith('Worker diagnostics - ')
    || !['push', 'workflow_dispatch'].includes(run.event)
    || run.status !== 'completed' || !['success', 'failure'].includes(run.conclusion)
    || !sha.test(run.head_sha ?? '')) {
    throw Error('Source must be a completed trusted same-repository CI run');
  }
  const matches = artifacts.artifacts?.filter(artifact => artifact.name === 'worker');
  if (matches?.length !== 1) throw Error('Expected one Worker artifact');
  const artifact = matches[0];
  if (artifact.expired || !Number.isSafeInteger(artifact.id) || artifact.id <= 0
    || !Number.isSafeInteger(artifact.size_in_bytes) || artifact.size_in_bytes <= 0
    || artifact.workflow_run?.id !== run.id || artifact.workflow_run?.head_sha !== run.head_sha
    || !/^sha256:[a-f0-9]{64}$/.test(artifact.digest ?? '')) {
    throw Error('Worker artifact identity or availability is invalid');
  }
  // Fixture changes can target a retained older binary; neither source is promoted.
  return {
    schema_version: 1,
    diagnostic_only: true,
    source_run: run.id,
    source_conclusion: run.conclusion,
    worker_source_sha: run.head_sha,
    test_source_sha: testSource,
    worker_artifact_id: artifact.id,
    worker_artifact_digest: artifact.digest,
  };
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const sourceRun = diagnosticRunId(process.env.SOURCE_RUN);
  const repository = process.env.GITHUB_REPOSITORY;
  if (!/^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/.test(repository ?? '')) throw Error('Invalid repository');
  const api = path => JSON.parse(execFileSync('gh', ['api', `repos/${repository}/${path}`], {
    encoding: 'utf8', maxBuffer: 2 * 1024 * 1024,
  }));
  const receipt = diagnosticReceipt(api(`actions/runs/${sourceRun}`),
    api(`actions/runs/${sourceRun}/artifacts?per_page=100`), repository, sourceRun, process.env.GITHUB_SHA);
  mkdirSync('.temp/worker-diagnostic', { recursive: true });
  writeFileSync('.temp/worker-diagnostic/provenance.json', JSON.stringify(receipt, null, 2) + '\n');
  appendFileSync(process.env.GITHUB_STEP_SUMMARY, [
    'Diagnostic only.',
    'Worker source: ' + receipt.worker_source_sha,
    'Fixture source: ' + receipt.test_source_sha,
    'This job cannot authorize deployment.',
    '',
  ].join('\n'));
}
