import assert from 'node:assert/strict';
import { test } from 'node:test';
import { readFileSync, mkdirSync, mkdtempSync } from 'node:fs';
import { baselineFor, changedRustTests, fingerprint, runChecks, selections, executedTests, containerSelection, checkInputPaths, checkFingerprint, reuseChecks, rustTestArguments } from './incremental-checks.mjs';
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

test('pytest temporary files remain under the project rather than the runner system tmp', () => {
  const selected = selections(source, baseline, ['tests/e2e/test_deepseek_usage_probe.py']);
  const argv = selected.tooling[0].argv;
  assert.ok(argv.some(argument => /^--basetemp=\.temp\/pytest-increment-[a-f0-9]{16}$/.test(argument)));
  assert.ok(argv.includes('cache_dir=.cache/pytest'));
});
test('passed historical fork and blob journeys are not replayed by a broad implementation path mapping', () => {
  const selected = selections(source, baseline, ['tests/worker/workspace-blob-journey.mjs', 'tests/worker/responses-journey.mjs']);
  assert.equal(selected.worker.length, 0);
});

test('independent checks finish after one failure and preserve both actual receipts', () => {
  const plan = {source_sha:source, checks:{tooling:[
    {id:'node:fail',argv:['node','--test','failure.test.mjs'],env:{},reason:'New failure'},
    {id:'python:pass',argv:['python3','-m','unittest'],env:{},reason:'Independent increment'},
  ]}};
  let calls=0;
  assert.throws(()=>runChecks(plan,'tooling',()=>++calls===1
    ? {status:1,stdout:'# pass 0\n# fail 1\n',stderr:''}
    : {status:0,stdout:'',stderr:'Ran 1 test in 0.01s\nOK'}, directory),/Incremental checks failed/);
  const rows=JSON.parse(readFileSync(`${directory}/tooling.json`,'utf8')).results;
  assert.equal(calls,2);
  assert.deepEqual(rows.map(row=>row.status),['failed_current','passed_current']);
  assert.deepEqual(rows.map(row=>row.executed_tests),[1,1]);
});
test('a compiler failure stops before recompiling the same broken crate', () => {
  const plan={source_sha:source,checks:{worker:[
    {id:'cargo:first',argv:['cargo','test','first'],env:{},reason:'New native increment'},
    {id:'cargo:second',argv:['cargo','test','second'],env:{},reason:'Same crate'},
  ]}};
  let calls=0;
  assert.throws(()=>runChecks(plan,'worker-rust',()=>{calls++;return{status:1,stdout:'',stderr:'error: could not compile agents-worker'};},directory),/Compilation failed/);
  assert.equal(calls,1);
});

test('startup diagnostic is one finite case and cannot silently imply full capability acceptance', () => {
  const network={id:'network:new',argv:['python','-m','pytest','network.py']};
  const selected=containerSelection([network],'startup-1g');
  assert.equal(selected.checks.length,1);
  assert.ok(selected.checks[0].argv.includes('tests/e2e/test_workspace_interpreter_memory.py::test_physical_memory_guard_precedes_kernel_and_execution_id_acceptance[1g]'));
  assert.equal(selected.validation_scope,'diagnostic_startup_only');
  assert.equal(selected.deferred_checks.length,3);
  assert.ok(selected.deferred_checks[0].argv.some(arg=>arg.endsWith('[4g]')));
  assert.ok(selected.deferred_checks[1].argv.some(arg=>arg.endsWith('[None]')));
  assert.deepEqual(containerSelection([network],'incremental').checks,[network]);
  assert.throws(()=>containerSelection([],'startup-1g; echo unsafe'),/Invalid finite/);
});

test('per-check reuse binds the exact recipe and explicit shared implementation inputs', () => {
  const check={id:'node:tests/e2e/paid-route-policy.test.mjs',argv:['node','--test','tests/e2e/paid-route-policy.test.mjs'],env:{}};
  let paths=[];
  const hash=checkFingerprint(source,check,(...args)=>{paths=args;return '100644 blob abc\towned';});
  assert.ok(paths.includes('tests/e2e/paid-route-policy.mjs'));
  assert.ok(paths.includes('crates/agents-worker/src/provider_config.rs'));
  assert.ok(paths.includes('package-lock.json'));
  assert.ok(!paths.includes('scripts/incremental-checks.mjs'));
  assert.notEqual(hash,checkFingerprint(source,{...check,env:{NEW_BEHAVIOR:'1'}},()=> '100644 blob abc\towned'));
  assert.notEqual(hash,checkFingerprint(source,check,()=> '100644 blob changed\towned'));
});
test('pure provider checks include wire dependencies but not an unrelated Session implementation', () => {
  const check={id:'rust:agents-worker:provider_roundtrip',source_file:'crates/agents-worker/src/provider_responses.rs',argv:['cargo','test','-p','agents-worker','provider_roundtrip'],env:{}};
  const paths=checkInputPaths(source,check,()=>['crates/agents-worker/src/provider_responses.rs','crates/agents-worker/src/responses_wire.rs','crates/agents-worker/src/session.rs'].join('\n'));
  assert.ok(paths.includes('crates/agents-worker/src/responses_wire.rs'));
  assert.ok(!paths.includes('crates/agents-worker/src/session.rs'));
  assert.ok(paths.includes('Cargo.lock'));
});
test('a verified unchanged check becomes reused_verified rather than passed_current', () => {
  const check={id:'node:new',argv:['node','--test','new.test.mjs'],env:{}};
  const seal={source_sha:baseline,run_id:'12345',components:[],receipt_artifact:'verification-tooling',receipt_artifact_id:42,verified_checks:[{id:check.id,status:'passed_current',input_fingerprint:'same',receipt_path:'tooling.json',executed_tests:2}]};
  const result=reuseChecks(source,[check],[seal],()=> 'same');
  assert.equal(result.checks.length,0);
  assert.equal(result.reused_checks[0].status,'reused_verified');
  assert.equal(result.reused_checks[0].source_sha,baseline);
  assert.equal(result.reused_checks[0].receipt_path,'tooling.json');
  assert.equal(result.reused_checks[0].executed_tests,2);
  assert.equal(reuseChecks(source,[check],[seal],()=> 'changed').checks.length,1);
});
test('failed and zero-test historical receipts never authorize check reuse', () => {
  const check={id:'node:new',argv:['node','--test','new.test.mjs'],env:{}};
  for(const proof of [{status:'failed_current',executed_tests:1},{status:'passed_current',executed_tests:0}]) {
    const seal={source_sha:baseline,run_id:'12345',components:[],verified_checks:[{id:check.id,input_fingerprint:'same',...proof}]};
    assert.equal(reuseChecks(source,[check],[seal],()=> 'same').checks.length,1);
  }
});
test('failed check receipts preserve a bounded tail even when process output cannot flush', () => {
  const plan={source_sha:source,checks:{tooling:[{id:'node:tail',argv:['node','--test','tail.test.mjs'],env:{},reason:'Actual diagnosis'}]}};
  assert.throws(()=>runChecks(plan,'tooling',()=>({status:1,stdout:'# fail 1\n',stderr:'Compiler diagnostic tail'}),directory),/failed/);
  const row=JSON.parse(readFileSync(`${directory}/tooling.json`,'utf8')).results[0];
  assert.equal(row.failure.stderr_tail,'Compiler diagnostic tail');
  assert.equal(row.executed_tests,1);
});

test('new browser specs use focused Playwright rather than an empty Vitest match', () => {
  const selected=selections(source,baseline,['apps/dashboard/e2e/agent-routing.spec.ts','apps/dashboard/src/agent-routing.test.tsx']);
  assert.equal(selected.dashboard.length,2);
  const browser=selected.dashboard.find(check=>check.argv.includes('test:browser'));
  assert.ok(browser.argv.includes('e2e/agent-routing.spec.ts'));
  assert.equal(executedTests(browser.argv,'  2 passed (1.5s)'),2);
  const unit=selected.dashboard.find(check=>!check.argv.includes('test:browser'));
  assert.ok(unit.argv.includes('src/agent-routing.test.tsx'));
  assert.equal(executedTests(unit.argv,'Tests 15 passed'),15);
});

test('existing Agents edits select only the three changed assertions and the new routing group', () => {
  const selected=selections(source,baseline,['apps/dashboard/src/AgentRouting.test.tsx','apps/dashboard/src/Agents.test.tsx']);
  assert.equal(selected.dashboard.length,2);
  for(const check of selected.dashboard) {
    assert.ok(check.argv.includes('-t'));
    assert.match(check.argv.at(-1),/independent agent service routing/);
    assert.match(check.argv.at(-1),/preserves existing custom BYOK model limits/);
  }
});

test('Rust unit selection compiles only its declared target, not unrelated old integration benchmarks', () => {
  const lib=rustTestArguments('agents-worker','namespace_identity','crates/agents-worker/src/auxiliary_context.rs');
  assert.ok(lib.includes('--lib'));
  assert.ok(!lib.includes('--test'));
  assert.deepEqual(rustTestArguments('magents','session_header','crates/magents/tests/space_responses.rs').slice(5,7),['--test','space_responses']);
  assert.deepEqual(rustTestArguments('workspace-daemon','memory','crates/workspace-daemon/src/interpreter.rs').slice(5,7),['--bin','workspace-daemon']);
});

test('ignored benchmarks are not converted into automatic zero-test or expensive runs', () => {
  const text='#[test]\n#[ignore]\nfn expensive_benchmark() {}\n#[ignore = "manual"]\n#[test]\nfn second_benchmark() {}\n#[test]\nfn actual_increment() {}\n';
  assert.deepEqual(changedRustTests(text,[[1,20]]),['actual_increment']);
});
test('a built exact Worker can run actual journeys despite independent unit assertion failures', () => {
  const workflow=readFileSync('.github/workflows/ci.yml','utf8');
  const worker=workflow.slice(workflow.indexOf('\n  worker:'),workflow.indexOf('\n  container:'));
  assert.ok(worker.indexOf('node scripts/bundle-worker.mjs')<worker.indexOf('Compile changed Worker tests'));
  assert.match(worker,/worker-journeys\n\s+if: \$\{\{ !cancelled\(\) && \(steps\.worker_bundle\.outcome == 'success' \|\| steps\.worker_reuse\.outcome == 'success'\)/);
  assert.match(worker,/name: worker\n/);
});
