import assert from 'node:assert/strict';
import { test } from 'node:test';
import { readFileSync, mkdirSync, mkdtempSync } from 'node:fs';
import { baselineFor, changedRustTests, fingerprint, runChecks, selections, executedTests } from './incremental-checks.mjs';
mkdirSync('.temp', { recursive: true });
const directory = mkdtempSync('.temp/incremental-checks-test-');
const source = 'a'.repeat(40), baseline = 'b'.repeat(40);
const catalog = [{ source_sha: baseline, run_id: '12345', components: ['worker'] }];

test('reuse requires identical complete component fingerprint, not merely a green label', () => {
  assert.equal(baselineFor(source, 'worker', catalog, () => 'same').matches, true);
  assert.equal(baselineFor(source, 'worker', catalog, sha => sha).matches, false);
  assert.throws(() => baselineFor(source, 'dashboard', catalog), /No sealed baseline/);
});
test('fingerprints bind modes, paths and exact dependency blobs', () => {
  const calls = [];
  const digest = fingerprint(source, 'worker', (...args) => { calls.push(args); return '100644 blob abc\tCargo.lock'; });
  assert.match(digest, /^[a-f0-9]{64}$/);
  assert.ok(calls[0].includes('Cargo.lock'));
  assert.ok(calls[0].includes('third_party'));
  assert.notEqual(digest, fingerprint(source, 'worker', () => '100755 blob abc\tCargo.lock'));
  assert.throws(() => fingerprint('HEAD; echo bad', 'worker'), /Invalid/);
});
test('a fixture literal repair selects its own test, not the old test module', () => {
  const text = '#[test]\nfn already_passed() {\n}\n#[test]\nfn repaired_literal() {\n let value = 1;\n}\n';
  assert.deepEqual(changedRustTests(text, [[6, 7]]), ['repaired_literal']);
});
test('property-test function syntax retains targeted selection', () => {
  assert.deepEqual(changedRustTests('proptest! {\n #[test]\n fn queue_capacity(input in any::<u8>()) {\n }\n}', [[1, 6]]), ['queue_capacity']);
});
test('new standalone Worker contracts and assets are selected without the legacy HTTP suite', () => {
  const selected = selections(source, baseline, ['tests/worker/native-semantics.test.mjs', 'tests/test_deepseek_tokenizer_assets.py']);
  assert.deepEqual(selected.worker.filter(check => !check.env.MAGENTS_WORKER_CASE).map(check => check.argv), [['node', '--test', '--test-reporter=tap', 'tests/worker/native-semantics.test.mjs']]);
  assert.equal(selected.worker.filter(check => check.env.MAGENTS_WORKER_CASE).length, 1);
  assert.equal(selected.rust[0].argv.at(-2), 'test_deepseek_tokenizer_assets.py');
});
test('new auxiliary and tokenizer increments select exact public journeys', () => {
  const selected = selections(source, baseline, ['tests/worker/auxiliary-journey.mjs', 'tests/worker/deepseek-tokenizer-journey.mjs']);
  assert.deepEqual(selected.worker.filter(check => check.env.MAGENTS_WORKER_CASE).map(check => check.env.MAGENTS_WORKER_CASE).sort(), ['DeepSeek exact count-only tokenizer cold warm and profile replacement', 'Responses auxiliary compact and count']);
});
test('changed current SDK contracts stay isolated from the legacy Python SDK', () => {
  const selected = selections(source, baseline, ['tests/e2e/test_current_responses_sdk_contract.py']);
  assert.equal(selected.tooling.length, 0);
  assert.equal(selected.current_sdk[0].argv[0], '.cache/e2e-current-venv/bin/python');
});
test('a zero-test Rust filter is a failed receipt, never a successful gate', () => {
  const plan = { source_sha: source, checks: { worker: [{ id: 'rust:empty', argv: ['cargo', 'test', 'wrong'], env: {}, reason: 'Negative test' }] } };
  assert.throws(() => runChecks(plan, 'worker-rust', () => ({ status: 0, stdout: 'test result: ok. 0 passed; 0 failed;\n', stderr: '' }), directory), /zero tests/);
  const receipt = JSON.parse(readFileSync(`${directory}/worker-rust.json`, 'utf8'));
  assert.equal(receipt.results[0].status, 'failed_current');
});
test('a successful targeted check records actual current result separately from its plan', () => {
  const plan = { source_sha: source, checks: { worker: [{ id: 'rust:one', argv: ['cargo', 'test', 'one'], env: {}, reason: 'New behavior' }] } };
  runChecks(plan, 'worker-rust', () => ({ status: 0, stdout: 'test result: ok. 1 passed; 0 failed;\n', stderr: '' }), directory);
  const receipt = JSON.parse(readFileSync(`${directory}/worker-rust.json`, 'utf8'));
  assert.equal(receipt.source_sha, source);
  assert.equal(receipt.results[0].status, 'passed_current');
});
test('workflow does not quietly retain blanket regression commands', () => {
  const workflow = readFileSync('.github/workflows/ci.yml', 'utf8');
  assert.doesNotMatch(workflow, /cargo test --locked --workspace|npm test\n|FORCE_ALL/);
  assert.match(workflow, /incremental-checks\.mjs run worker-journeys/);
  assert.match(workflow, /incremental-plan/);
  // The old diagnostic workflow remains visibly non-promoting and independent.
  assert.match(workflow, /Fixture revisions may target a retained older binary/);
});

test('zero unittest / Node / pytest selections are never counted as execution', () => {
  assert.equal(executedTests(['python3', '-m', 'unittest'], 'Ran 0 tests in 0.000s\nOK'), 0);
  assert.equal(executedTests(['node', '--test'], '# tests 0\n# pass 0\n'), 0);
  assert.equal(executedTests(['python', '-m', 'pytest'], 'no tests ran in 0.02s'), 0);
  assert.equal(executedTests(['python3', '-m', 'unittest'], 'Ran 4 tests in 0.003s\nOK'), 4);
  assert.equal(executedTests(['node', '--test'], '# tests 3\n# pass 2\n# skipped 1\n'), 2);
  assert.equal(executedTests(['python', '-m', 'pytest'], '================= 15 passed in 0.03s ================='), 15);
});
