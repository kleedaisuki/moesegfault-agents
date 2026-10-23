// Platform contract only: this fixture does not exercise the compiled Rust transport.
import assert from 'node:assert/strict';
import { mkdir, mkdtemp } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { Miniflare, convertV4MiniflareOptions } from 'miniflare';

const root = fileURLToPath(new URL('../../', import.meta.url));

test('workerd native service-binding Request abort covers headers and body', { timeout: 10000 }, async t => {
  await mkdir(path.join(root, '.temp'), { recursive: true });
  const temp = await mkdtemp(path.join(root, '.temp/transport-contract-'));
  process.env.TMP = process.env.TEMP = process.env.TMPDIR = temp;
  const mf = new Miniflare(convertV4MiniflareOptions({
    modules: true,
    compatibilityDate: '2026-10-23',
    resourcePersistencePath: path.join(temp, 'storage'),
    script: `export default { async fetch(request, env) {
      const url = new URL(request.url);
      const mode = url.pathname;
      const controller = new AbortController();
      const started = Date.now();
      let phase = 'headers';
      const timer = setTimeout(() => controller.abort(), 40);
      try {
        const options = url.searchParams.has('drop_signal') ? {} : { signal: controller.signal };
        const upstream = new Request('https://workspace.test' + mode, options);
        const response = await env.WORKSPACE.fetch(upstream);
        phase = 'body';
        const text = await response.text();
        return Response.json({ outcome: 'completed', text });
      } catch (error) {
        return Response.json({ outcome: 'aborted', phase, name: error.name, elapsed_ms: Date.now() - started });
      } finally { clearTimeout(timer); }
    } };`,
    serviceBindings: {
      WORKSPACE: async request => {
        const mode = new URL(request.url).pathname;
        if (mode === '/headers') {
          await new Promise(resolve => setTimeout(resolve, 1200));
          return new Response('too late');
        }
        if (mode === '/body') {
          let timer;
          return new Response(new ReadableStream({
            start(controller) {
              controller.enqueue(new TextEncoder().encode('partial'));
              timer = setTimeout(() => controller.close(), 1200);
            },
            cancel() { clearTimeout(timer); },
          }));
        }
        return new Response('ready');
      },
    },
  }));
  t.after(() => mf.dispose());
  for (const phase of ['headers', 'body']) {
    await t.test(`abort stalled ${phase} before the upstream finishes`, async () => {
      const response = await mf.dispatchFetch(`https://transport.test/${phase}`);
      const result = await response.json();
      assert.equal(result.outcome, 'aborted');
      assert.equal(result.phase, phase);
      assert.equal(result.name, 'AbortError');
      assert.ok(result.elapsed_ms < 800, JSON.stringify(result));
    });
  }
  await t.test('completed responses remain usable', async () => {
    const response = await mf.dispatchFetch('https://transport.test/success');
    assert.deepEqual(await response.json(), { outcome: 'completed', text: 'ready' });
  });
  await t.test('negative control: dropping the Request signal defeats the deadline', async () => {
    const response = await mf.dispatchFetch('https://transport.test/body?drop_signal=1');
    assert.deepEqual(await response.json(), { outcome: 'completed', text: 'partial' });
  });
});
