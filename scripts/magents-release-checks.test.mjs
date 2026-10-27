import assert from 'node:assert/strict';
import { mkdirSync, mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import test from 'node:test';
import { releaseChecks, runReleaseChecks } from './magents-release-checks.mjs';

const source = 'a'.repeat(40);
const content = readFileSync(new URL('../crates/magents/tests/space_responses.rs', import.meta.url), 'utf8');
const names = [
  'auxiliary_commands_preserve_native_requests_and_complete_compacted_windows',
  'native_conversation_crud_items_and_container_routes_preserve_json',
  'native_context_help_and_local_rejections_work_without_login',
  'failed_native_operations_report_http_failure_without_replay_or_body_leaks',
  'progressive_help_and_local_rejections_require_no_login',
];

test('native release increment selects only four new binary journeys and affected help on both targets', () => {
  for (const [target, platform] of [['x86_64-unknown-linux-gnu', 'linux'], ['x86_64-pc-windows-msvc', 'win32']]) {
    const plan = releaseChecks('0.3.0', target, source, content, platform);
    assert.equal(plan.source_sha, source);
    assert.equal(plan.target, target);
    assert.equal(plan.platform, platform);
    assert.equal(plan.profile, 'release');
    assert.deepEqual(plan.checks.rust.map(check => check.id), names.map(name => `rust:magents:${name}`));
    plan.checks.rust.forEach((check, index) => assert.deepEqual(check.argv, [
      'cargo', 'test', '--release', '--target', target, '--locked', '-p', 'magents',
      '--test', 'space_responses', names[index], '--', '--nocapture', '--exact',
    ]));
  }
});

test('native release increment fails closed for an undefined version platform source or missing actual test', () => {
  for (const version of ['0.2.0', '0.3.1', '0.4.0', undefined]) {
    assert.throws(() => releaseChecks(version, 'x86_64-unknown-linux-gnu', source, content), /defined only/);
  }
  assert.throws(() => releaseChecks('0.3.0', 'aarch64-apple-darwin', source, content), /target/);
  assert.throws(() => releaseChecks('0.3.0', 'x86_64-unknown-linux-gnu', 'branch', content), /source/);
  assert.throws(() => releaseChecks('0.3.0', 'x86_64-unknown-linux-gnu', source, content, 'win32'), /actual platform/);
  assert.throws(() => releaseChecks('0.3.0', 'x86_64-unknown-linux-gnu', source,
    content.replace(`fn ${names[0]}(`, 'fn renamed_case('), 'linux'), /Missing native release CLI case/);
});

test('native release runner retains exact per-platform source and argv and rejects zero-test success', () => {
  const temporary = resolve('.temp');
  mkdirSync(temporary, { recursive: true });
  const directory = mkdtempSync(join(temporary, 'magents-release-selection-'));
  try {
    const plan = releaseChecks('0.3.0', 'x86_64-pc-windows-msvc', source, content, 'win32');
    const calls = [];
    const execute = (command, argv) => {
      calls.push([command, ...argv]);
      return { status: 0, stdout: 'test result: ok. 1 passed; 0 failed; 21 filtered out', stderr: '' };
    };
    runReleaseChecks(plan, execute, directory);
    assert.deepEqual(calls, plan.checks.rust.map(check => check.argv));
    assert.deepEqual(JSON.parse(readFileSync(`${directory}/plan.json`, 'utf8')), plan);
    const receipt = JSON.parse(readFileSync(`${directory}/rust.json`, 'utf8'));
    assert.equal(receipt.source_sha, source);
    assert.equal(receipt.results.length, 5);
    assert.ok(receipt.results.every(result => result.status === 'passed_current' && result.executed_tests === 1));
    assert.throws(() => runReleaseChecks(plan,
      () => ({ status: 0, stdout: 'test result: ok. 0 passed; 0 failed; 22 filtered out', stderr: '' }), directory),
    /selected zero tests/);
    const failed = JSON.parse(readFileSync(`${directory}/rust.json`, 'utf8'));
    assert.ok(failed.results.every(result => result.status === 'failed_current' && result.executed_tests === 0));
  } finally {
    assert.equal(dirname(directory), temporary);
    rmSync(directory, { recursive: true, force: true });
  }
});

test('native release workflow retains tag source two-platform attach and byte checksum safeguards without full-suite replay', () => {
  const workflow = readFileSync(new URL('../.github/workflows/release-magents.yml', import.meta.url), 'utf8');
  assert.match(workflow, /run: node scripts\/magents-release-checks\.mjs/);
  assert.doesNotMatch(workflow, /run: cargo test\b/);
  assert.match(workflow, /name: native-checks-\$\{\{ env\.PACKAGE_VERSION \}\}-\$\{\{ matrix\.target \}\}/);
  assert.match(workflow, /path: \.temp\/magents-release-checks\/\$\{\{ matrix\.target \}\}\//);
  for (const target of ['x86_64-unknown-linux-gnu', 'x86_64-pc-windows-msvc']) assert.ok(workflow.includes(`target: ${target}`));
  assert.ok(workflow.includes('if tag and tag != f"v{version}":'));
  assert.ok(workflow.includes('if tagged != checked_out:'));
  assert.ok(workflow.includes('if actual != f"magents {version}":'));
  assert.ok(workflow.includes(String.raw`write_bytes(f"{checksum}  {archive.name}\n".encode("ascii"))`));
  const attach = workflow.slice(workflow.indexOf('  attach:'));
  assert.match(attach, /needs: package/);
  assert.doesNotMatch(attach, /if: always\(\)/);
  assert.match(attach, /pattern: magents-\*/);
  assert.ok(attach.includes('sha256sum --check *.sha256'));
  assert.ok(attach.includes('run: gh release upload "$RELEASE_TAG" dist/*'));
  assert.doesNotMatch(workflow, /--clobber|gh release create|git (?:push|tag)\b/);
});
