import assert from 'node:assert/strict';
import { fork, spawnSync } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { mkdir, mkdtemp, writeFile } from 'node:fs/promises';
import { once } from 'node:events';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

const root = fileURLToPath(new URL('../../', import.meta.url));
const childPath = fileURLToPath(new URL('./response-crash-child.mjs', import.meta.url));

function phaseProcess(phase, directory) {
  const child = fork(childPath, [phase, directory], {
    cwd: root, detached: process.platform !== 'win32', windowsHide: true,
    stdio: ['ignore', 'pipe', 'pipe', 'ipc'], env: process.env,
  });
  let output = '';
  for (const stream of [child.stdout, child.stderr]) stream.on('data', chunk => { output = (output + chunk.toString()).slice(-12000); });
  function message(kind) {
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error(`Crash fixture ${phase} did not report ${kind}`)), 25000);
      const cleanup = () => {
        clearTimeout(timer);
        child.off('message', onMessage);
        child.off('exit', onExit);
      };
      function onMessage(value) {
        if (value.type === kind) { cleanup(); resolve(value); }
        else if (value.type === 'failed') { cleanup(); reject(new Error(`${phase}: ${value.error.stack ?? value.error.message}`)); }
      }
      function onExit(code, signal) { cleanup(); reject(new Error(`Crash fixture ${phase} exited before ${kind}: ${code}/${signal}\n${output}`)); }
      child.on('message', onMessage);
      child.on('exit', onExit);
    });
  }
  return { child, message };
}

async function killTree(child) {
  if (child.exitCode !== null || child.signalCode !== null) return;
  assert.ok(Number.isSafeInteger(child.pid) && child.pid > 0);
  const exited = once(child, 'exit');
  if (process.platform === 'win32') {
    const killed = spawnSync('taskkill', ['/PID', String(child.pid), '/T', '/F'], { windowsHide: true, timeout: 8000, stdio: 'ignore' });
    assert.ok(killed.status === 0 || killed.status === 128, 'The owned crash-fixture process tree must be terminated');
  } else {
    // The detached child is a new process-group leader, so this cannot signal
    // the test runner or an unrelated workerd belonging to another fixture.
    try { process.kill(-child.pid, 'SIGKILL'); }
    catch (error) { if (error.code !== 'ESRCH') throw error; }
  }
  // Windows taskkill128/LinuxESRCH can mean the child exited between inspection
  // and signalling. Require its actual exit event, not merely the tool status.
  await boundedExit(exited);
}

async function boundedExit(exit) {
  let timer;
  try {
    await Promise.race([exit, new Promise((_, reject) => {
      timer = setTimeout(() => reject(new Error('Crash fixture process did not terminate')), 8000);
    })]);
  } finally { clearTimeout(timer); }
}

test('Responses hard process crash preserves disclosed sequence high-water without provider replay', { timeout: 60000 }, async t => {
  await mkdir(path.join(root, '.temp'), { recursive: true });
  const directory = await mkdtemp(path.join(root, '.temp/response-hard-crash-'));
  await writeFile(path.join(directory, 'config.json'), JSON.stringify({ encryptionKey: randomBytes(32).toString('base64url') }));
  const processes = [];
  t.after(async () => {
    for (const process of processes) await killTree(process.child);
  });
  const first = phaseProcess('start', directory);
  processes.push(first);
  const { receipt } = await first.message('disclosed');
  assert.ok(Number.isSafeInteger(receipt.sequence) && receipt.sequence >= 0);
  await writeFile(path.join(directory, 'receipt.json'), JSON.stringify(receipt));
  await killTree(first.child);
  const second = phaseProcess('recover', directory);
  processes.push(second);
  const { evidence } = await second.message('recovered');
  assert.ok(evidence.terminal_sequence > evidence.disclosed_sequence);
  assert.equal(evidence.provider_calls, 1);
  assert.equal(evidence.unresolved_completion_receipts, 1);
  if (second.child.exitCode === null && second.child.signalCode === null) {
    await boundedExit(once(second.child, 'exit'));
  }
  assert.equal(second.child.exitCode, 0, 'The recovered fixture must finish its normal runtime disposal');
  t.diagnostic(JSON.stringify({ event: 'response.hard_process_crash_fixture', ...evidence }));
});
