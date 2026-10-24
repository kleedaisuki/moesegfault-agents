import assert from 'node:assert/strict';
import { mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { dirname, resolve } from 'node:path';
import test from 'node:test';
import { loadImageArtifact, validateImageReceipt, verifyImageArchive, writeImageReceipt } from './container-artifact.mjs';

const source = 'a'.repeat(40);
const descriptor = () => ({ image_id: `sha256:${'b'.repeat(64)}`, platform: 'linux/amd64' });

async function fixture(t) {
  const root = resolve('.temp');
  await mkdir(root, { recursive: true });
  const directory = await mkdtemp(resolve(root, 'container-artifact-'));
  t.after(async () => {
    assert.equal(dirname(directory), root);
    await rm(directory, { recursive: true, force: true });
  });
  // This exercises hashing and admission, not Docker's archive parser.
  await writeFile(resolve(directory, 'image.tar.gz'), Buffer.from('synthetic archive bytes'));
  await writeImageReceipt(directory, source, descriptor);
  return directory;
}

test('tested container receipt binds exact source, tag, platform, image ID and archive bytes', async (t) => {
  const directory = await fixture(t);
  const receipt = await verifyImageArchive(directory, source);
  assert.equal(receipt.source_sha, source);
  assert.equal(receipt.image_tag, `magents-workspace:${source}`);
  assert.deepEqual(Object.keys(receipt).sort(), ['archive', 'archive_sha256', 'image_id', 'image_tag', 'platform', 'schema_version', 'source_sha']);
  const calls = [];
  const loaded = await loadImageArtifact(directory, source, (...args) => calls.push(args), descriptor);
  assert.deepEqual(loaded, receipt);
  assert.deepEqual(calls, [['docker', ['image', 'load', '--quiet', '--input', resolve(directory, 'image.tar.gz')], { stdio: 'inherit' }]]);
});

test('wrong source, archive substitution and malformed receipts fail before Docker is invoked', async (t) => {
  const directory = await fixture(t);
  const receipt = JSON.parse(await readFile(resolve(directory, 'receipt.json'), 'utf8'));
  for (const [key, value] of Object.entries({
    schema_version: 2, source_sha: 'c'.repeat(40), image_tag: 'magents-workspace:acceptance',
    platform: 'linux/arm64', image_id: 'sha256:short', archive: '../image.tar.gz', archive_sha256: 'short',
  })) {
    assert.throws(() => validateImageReceipt({ ...receipt, [key]: value }, source));
  }
  assert.throws(() => validateImageReceipt({ ...receipt, unexpected: true }, source));
  let called = false;
  const docker = () => { called = true; };
  await assert.rejects(loadImageArtifact(directory, 'c'.repeat(40), docker, descriptor), /does not match/);
  await writeFile(resolve(directory, 'image.tar.gz'), 'substituted archive bytes');
  await assert.rejects(loadImageArtifact(directory, source, docker, descriptor), /checksum mismatch/);
  assert.equal(called, false);
});

test('loaded image identity must match the tested image and receipts are not overwritten', async (t) => {
  const directory = await fixture(t);
  await assert.rejects(loadImageArtifact(directory, source, () => {}, () => ({ ...descriptor(), image_id: `sha256:${'c'.repeat(64)}` })), /does not match the tested image/);
  await assert.rejects(writeImageReceipt(directory, source, descriptor), { code: 'EEXIST' });
  await assert.rejects(verifyImageArchive(directory, `${source}\n`), /Invalid container source SHA/);
});
