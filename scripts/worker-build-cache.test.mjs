import assert from 'node:assert/strict';
import { mkdirSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { cacheKey, ensureTool, toolIdentity, VERSION } from './worker-build-cache.mjs';

const temp = () => {
  const parent = path.resolve('.temp/worker-build-cache-tests');
  mkdirSync(parent, { recursive: true });
  return mkdtempSync(path.join(parent, 'case-'));
};
const identity = { schema: 1, version: VERSION, os: 'Linux', arch: 'X64', image: 'ubuntu24', rust: 'rustc 1.98.1\nhost: x86_64-unknown-linux-gnu' };

function fakeInstall(root, { version = VERSION } = {}) {
  const calls = [];
  const run = (command, argv) => {
    calls.push({ command, argv });
    if (command === 'cargo') {
      mkdirSync(path.join(root, 'bin'), { recursive: true });
      writeFileSync(path.join(root, 'bin/worker-build'), 'controlled installed binary');
      return '';
    }
    assert.equal(command, path.join(root, 'bin/worker-build'));
    assert.deepEqual(argv, ['--version']);
    return version;
  };
  return { calls, run };
}

test('worker-build cache binds actual Rust host/compiler, runner image, architecture and tool version', () => {
  const actual = toolIdentity({ RUNNER_OS: 'Linux', RUNNER_ARCH: 'X64', ImageOS: 'ubuntu24' }, (command, argv) => {
    assert.equal(command, 'rustc'); assert.deepEqual(argv, ['-vV']); return identity.rust;
  });
  assert.deepEqual(actual, identity);
  for (const [field, value] of [['os', 'Windows'], ['arch', 'ARM64'], ['image', 'ubuntu26'], ['rust', 'rustc 1.99.0'], ['version', '0.8.8']]) {
    assert.notEqual(cacheKey(identity), cacheKey({ ...identity, [field]: value }));
  }
});

test('worker-build installs once, then validates checksum and executable version before reuse', () => {
  const root = temp(), fake = fakeInstall(root);
  assert.equal(ensureTool(root, identity, fake.run).status, 'installed');
  assert.deepEqual(fake.calls[0].argv, ['install', '--locked', '--force', '--root', root, 'worker-build', '--version', VERSION]);
  fake.calls.length = 0;
  assert.equal(ensureTool(root, identity, fake.run).status, 'validated_cache');
  assert.equal(fake.calls.length, 1);
  assert.deepEqual(fake.calls[0].argv, ['--version']);
});

test('worker-build repairs corrupt bytes, wrong provenance, malformed manifests and wrong executable versions', () => {
  for (const fault of ['bytes', 'identity', 'manifest', 'version']) {
    const root = temp(), fake = fakeInstall(root);
    ensureTool(root, identity, fake.run);
    if (fault === 'bytes') writeFileSync(path.join(root, 'bin/worker-build'), 'corrupt cache');
    if (fault === 'identity') {
      const manifest = JSON.parse(readFileSync(path.join(root, 'verified-tool.json'), 'utf8'));
      manifest.identity.arch = 'ARM64'; writeFileSync(path.join(root, 'verified-tool.json'), JSON.stringify(manifest));
    }
    if (fault === 'manifest') writeFileSync(path.join(root, 'verified-tool.json'), '{');
    let probes = 0;
    const run = (command, argv, options) => command !== 'cargo' && fault === 'version' && probes++ === 0 ? '0.8.6' : fake.run(command, argv, options);
    fake.calls.length = 0;
    assert.equal(ensureTool(root, identity, run).status, 'installed', fault);
    assert.equal(fake.calls.filter(call => call.command === 'cargo').length, 1, fault);
  }
});

test('CI caches only the dedicated verified tool and never substitutes it for Worker artifact provenance', () => {
  const workflow = readFileSync(new URL('../.github/workflows/ci.yml', import.meta.url), 'utf8');
  assert.match(workflow, /uses: actions\/cache@[a-f0-9]{40}/);
  assert.match(workflow, /path: \.cache\/toolchain\/worker-build\n\s+key: \$\{\{ steps\.worker_build_tool\.outputs\.key \}\}/);
  assert.doesNotMatch(workflow, /restore-keys:/);
  assert.match(workflow, /run: node scripts\/worker-build-cache\.mjs ensure\n\s+if: needs\.changes\.outputs\.worker_build == 'true'/);
  assert.ok(workflow.indexOf('worker-build-cache.mjs ensure') < workflow.indexOf('run: worker-build --release'));
  assert.ok(workflow.includes('Reuse the sealed identical Worker artifact'));
});
