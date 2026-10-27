import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { appendFileSync, lstatSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { pathToFileURL } from 'node:url';

export const VERSION = '0.8.7';
const SCHEMA = 1;
const hash = bytes => createHash('sha256').update(bytes).digest('hex');

export function toolIdentity(env = process.env, run = execFileSync) {
  return { schema: SCHEMA, version: VERSION, os: env.RUNNER_OS ?? process.platform,
    arch: env.RUNNER_ARCH ?? process.arch, image: env.ImageOS ?? process.platform,
    rust: run('rustc', ['-vV'], { encoding: 'utf8' }).trim() };
}

export function cacheKey(identity) {
  return `worker-build-v${SCHEMA}-${VERSION}-${hash(JSON.stringify(identity))}`;
}

export function ensureTool(root, identity, run = execFileSync) {
  const binary = path.join(root, 'bin', identity.os === 'Windows' ? 'worker-build.exe' : 'worker-build');
  const manifest = path.join(root, 'verified-tool.json');
  const version = () => run(binary, ['--version'], { encoding: 'utf8', timeout: 10000 }).trim();
  try {
    const saved = JSON.parse(readFileSync(manifest, 'utf8'));
    if (lstatSync(binary).isFile() && JSON.stringify(saved.identity) === JSON.stringify(identity)
        && saved.sha256 === hash(readFileSync(binary)) && version() === VERSION) {
      return { status: 'validated_cache', binary };
    }
  } catch {
    // A cache is an optimization, not build authority. Repair missing/corrupt
    // entries from the locked registry package instead of skipping verification.
  }
  run('cargo', ['install', '--locked', '--force', '--root', root, 'worker-build', '--version', VERSION], { stdio: 'inherit' });
  if (!lstatSync(binary).isFile() || version() !== VERSION) throw Error('Installed worker-build identity mismatch');
  writeFileSync(manifest, `${JSON.stringify({ identity, sha256: hash(readFileSync(binary)) })}\n`);
  return { status: 'installed', binary };
}

function main(command) {
  const root = path.resolve('.cache/toolchain/worker-build');
  const identity = toolIdentity();
  if (command === 'key') {
    if (!process.env.GITHUB_OUTPUT) throw Error('GITHUB_OUTPUT is required');
    appendFileSync(process.env.GITHUB_OUTPUT, `key=${cacheKey(identity)}\n`);
    return;
  }
  if (command !== 'ensure') throw Error('Expected key or ensure');
  if (!process.env.GITHUB_PATH) throw Error('GITHUB_PATH is required');
  mkdirSync(root, { recursive: true });
  const result = ensureTool(root, identity);
  appendFileSync(process.env.GITHUB_PATH, `${path.dirname(result.binary)}\n`);
  console.log(`worker-build ${VERSION}: ${result.status}`);
}

if (process.argv[1] && import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href) main(process.argv[2]);
