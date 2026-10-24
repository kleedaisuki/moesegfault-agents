import assert from 'node:assert/strict';
import test from 'node:test';
import { measureReadFlow } from './v3-dataflow.mjs';

function fixture({ corrupted = false, changed = false } = {}) {
  const source = Array.from({ length: 128 }, (_, sequence) => ({ role: sequence % 2 ? 'tool' : 'assistant', content: `line-${sequence} ${'x'.repeat(4096)}` }));
  const calls = [];
  const fetcher = async (url, init) => {
    calls.push(url.pathname);
    assert.equal(init.headers.Authorization, 'Bearer fixture-only');
    const send = (value, headers = {}) => new Response(JSON.stringify(value), { headers: { 'Content-Type': 'application/json', ...headers } });
    if (url.pathname === '/healthz') return send({ status: 'ok', build_id: 'a'.repeat(40) });
    if (url.pathname.endsWith('/view')) {
      if (init.headers['If-None-Match'] && !changed) return new Response(null, { status: 304, headers: { ETag: '"view-1"' } });
      return send({ status: 'idle', message_count: source.length }, { ETag: '"view-1"' });
    }
    if (url.pathname.endsWith('/messages')) {
      const end = Number(url.searchParams.get('before') ?? source.length);
      const start = Math.max(0, end - 50);
      return send({
        data: source.slice(start, end).map((message, offset) => ({ sequence: start + offset, message: corrupted ? { ...message, content: 'changed' } : message })),
        start, end, high_water: source.length, before_cursor: String(start), after_cursor: String(end), has_older: start > 0, has_newer: false,
      });
    }
    return send({ status: 'idle', messages: source });
  };
  return { fetcher, calls };
}

test('read-flow probe compares actual bodies and reconstructs every original source entry', async () => {
  const { fetcher, calls } = fixture();
  const result = await measureReadFlow({ origin: 'http://127.0.0.1:8080', session: 'fixture-session', token: 'fixture-only', polls: 10, fetcher });
  assert.ok(result.body_reduction_percent > 95);
  assert.equal(result.message_count, 128);
  assert.equal(result.recent_messages, 50);
  assert.equal(result.older_history_requests, 2);
  assert.ok(result.complete_source_reconstruction);
  assert.equal(calls.filter(path => path === '/v1/sessions/fixture-session').length, 1);
  assert.ok(!JSON.stringify(result).includes('fixture-session'));
});

test('read-flow probe refuses changed history and never treats a non-304 as unchanged', async () => {
  for (const options of [{ corrupted: true }, { changed: true }]) {
    await assert.rejects(measureReadFlow({ origin: 'http://localhost:8080', session: 's', token: 'fixture-only', fetcher: fixture(options).fetcher }));
  }
});

test('read-flow probe does not implicitly benchmark production or run without explicit identity', async () => {
  await assert.rejects(measureReadFlow({ origin: 'https://agents.moesegfault.dev', session: 's', token: 'fixture-only' }), /staging or a local fixture/);
  await assert.rejects(measureReadFlow({ origin: 'http://localhost:8080', session: '', token: 'fixture-only' }), /Supply a session/);
});

test('probe errors do not include untrusted payloads and enforce the response-byte budget', async () => {
  const invalid = () => Promise.resolve(new Response('private-source-fragment'));
  await assert.rejects(measureReadFlow({ origin: 'http://localhost', session: 's', token: 'fixture-only', fetcher: invalid }), error => {
    assert.equal(error.message, 'Read-flow response is not valid JSON');
    return true;
  });
  let cancelled = false;
  const oversized = () => Promise.resolve(new Response(new ReadableStream({
    start(controller) { controller.enqueue(new Uint8Array(16 * 1024 * 1024 + 1)); },
    cancel() { cancelled = true; },
  })));
  await assert.rejects(measureReadFlow({ origin: 'http://localhost', session: 's', token: 'fixture-only', fetcher: oversized }), /16 MiB probe budget/);
  assert.ok(cancelled);
});
