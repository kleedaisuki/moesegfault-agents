import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { createReadStream } from 'node:fs';
import { lstat, readFile, writeFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

const archiveName = 'image.tar.gz';
const receiptName = 'receipt.json';
const sourcePattern = /^[a-f0-9]{40}$/;
const digestPattern = /^[a-f0-9]{64}$/;
const receiptFields = ['archive', 'archive_sha256', 'image_id', 'image_tag', 'platform', 'schema_version', 'source_sha'];

function sourceTag(source) {
  if (!sourcePattern.test(source ?? '')) throw new Error('Invalid container source SHA');
  return `magents-workspace:${source}`;
}

export function validateImageReceipt(receipt, source) {
  const tag = sourceTag(source);
  if (!receipt || typeof receipt !== 'object' || Array.isArray(receipt)
      || Object.keys(receipt).sort().join() !== receiptFields.join()
      || receipt.schema_version !== 1 || receipt.source_sha !== source
      || receipt.image_tag !== tag || receipt.platform !== 'linux/amd64'
      || !/^sha256:[a-f0-9]{64}$/.test(receipt.image_id ?? '')
      || receipt.archive !== archiveName || !digestPattern.test(receipt.archive_sha256 ?? '')) {
    throw new Error('Container artifact receipt does not match this source and platform');
  }
  return receipt;
}

async function archiveHash(directory) {
  const archive = resolve(directory, archiveName);
  const info = await lstat(archive);
  if (!info.isFile() || info.size === 0) throw new Error('Container image archive must be a nonempty regular file');
  const hash = createHash('sha256');
  for await (const chunk of createReadStream(archive)) hash.update(chunk);
  return hash.digest('hex');
}

function dockerImage(tag) {
  const [image_id, platform, ...extra] = execFileSync('docker', [
    'image', 'inspect', '--format', '{{.Id}}\n{{.Os}}/{{.Architecture}}', tag,
  ], { encoding: 'utf8' }).trim().split(/\r?\n/);
  if (extra.length) throw new Error('Docker returned an unexpected image descriptor');
  return { image_id, platform };
}

export async function writeImageReceipt(directory, source, inspectImage = dockerImage) {
  const image_tag = sourceTag(source);
  const receipt = validateImageReceipt({
    schema_version: 1, source_sha: source, image_tag,
    ...inspectImage(image_tag), archive: archiveName, archive_sha256: await archiveHash(directory),
  }, source);
  await writeFile(resolve(directory, receiptName), `${JSON.stringify(receipt, null, 2)}\n`, { flag: 'wx' });
  return receipt;
}

export async function verifyImageArchive(directory, source) {
  sourceTag(source);
  const receiptPath = resolve(directory, receiptName);
  const info = await lstat(receiptPath);
  if (!info.isFile() || info.size > 16384) throw new Error('Invalid container artifact receipt file');
  const receipt = validateImageReceipt(JSON.parse(await readFile(receiptPath, 'utf8')), source);
  if (await archiveHash(directory) !== receipt.archive_sha256) throw new Error('Container image archive checksum mismatch');
  return receipt;
}

export async function loadImageArtifact(directory, source, docker = execFileSync, inspectImage = dockerImage) {
  const receipt = await verifyImageArchive(directory, source);
  docker('docker', ['image', 'load', '--quiet', '--input', resolve(directory, archiveName)], { stdio: 'inherit' });
  const loaded = inspectImage(receipt.image_tag);
  if (loaded.image_id !== receipt.image_id || loaded.platform !== receipt.platform) {
    throw new Error('Loaded container does not match the tested image');
  }
  return receipt;
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  const [operation, directory] = process.argv.slice(2);
  if (!directory || !['create', 'load'].includes(operation)) throw new Error('Usage: container-artifact.mjs create|load DIRECTORY');
  const receipt = operation === 'create'
    ? await writeImageReceipt(directory, process.env.GITHUB_SHA)
    : await loadImageArtifact(directory, process.env.GITHUB_SHA);
  console.log(JSON.stringify(receipt));
}
