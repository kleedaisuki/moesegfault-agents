import assert from 'node:assert/strict';
import test from 'node:test';

import { diagnosticReceipt, diagnosticRunId } from './worker-diagnostic.mjs';

const repository = 'owner/repository';
const sourceRun = 40116721317;
const testSource = 'c'.repeat(40);
const run = {
  id: sourceRun, repository: { full_name: repository }, head_repository: { full_name: repository },
  path: '.github/workflows/ci.yml', event: 'workflow_dispatch', status: 'completed',
  conclusion: 'failure', head_sha: 'a'.repeat(40),
};
const artifact = {
  id: 12569535719, name: 'worker', expired: false, size_in_bytes: 2048,
  digest: 'sha256:' + 'b'.repeat(64), workflow_run: { id: sourceRun, head_sha: run.head_sha },
};
const artifacts = { artifacts: [artifact] };
const receipt = (changes = {}, files = artifacts) =>
  diagnosticReceipt({ ...run, ...changes }, files, repository, sourceRun, testSource);

test('diagnostic artifact provenance records distinct trusted binary and fixture sources', () => {
  assert.equal(diagnosticRunId(String(sourceRun)), sourceRun);
  assert.equal(receipt().diagnostic_only, true);
  assert.equal(receipt().worker_source_sha, run.head_sha);
  assert.equal(receipt().test_source_sha, testSource);
  assert.equal(receipt({ conclusion: 'success', event: 'push' }).source_conclusion, 'success');
});

test('diagnostic source rejects untrusted events, incomplete builds and mismatched identity', () => {
  for (const change of [
    { id: sourceRun + 1 }, { repository: { full_name: 'foreign/repository' } },
    { head_repository: { full_name: 'fork/repository' } }, { path: '.github/workflows/deploy.yml' },
    { display_title: 'Worker diagnostics - 40116721317' },
    { event: 'pull_request' }, { event: 'pull_request_target' }, { event: 'release' },
    { status: 'in_progress' }, { conclusion: 'cancelled' }, { head_sha: '../worker' },
  ]) assert.throws(() => receipt(change));
  for (const value of ['', '0', '-1', '1/../2', '1e3', '0012', '9999999999999999']) {
    assert.throws(() => diagnosticRunId(value));
  }
  assert.throws(() => diagnosticReceipt(run, artifacts, repository, sourceRun, 'invalid'));
});

test('diagnostic artifacts must be unique, retained and bound to the original CI source', () => {
  for (const change of [
    { expired: true }, { id: 1.5 }, { id: -1 }, { size_in_bytes: 0 }, { size_in_bytes: undefined },
    { workflow_run: { id: sourceRun + 1, head_sha: run.head_sha } },
    { workflow_run: { id: sourceRun, head_sha: 'd'.repeat(40) } }, { digest: 'unknown' },
  ]) assert.throws(() => receipt({}, { artifacts: [{ ...artifact, ...change }] }));
  for (const files of [[], [artifact, artifact], [{ ...artifact, name: 'worker-diagnostic-1' }]]) {
    assert.throws(() => receipt({}, { artifacts: files }));
  }
});
