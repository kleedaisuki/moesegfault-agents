import assert from 'node:assert/strict';
import test from 'node:test';
import { createNativeSemanticsProvider } from './native-semantics-journey.mjs';

test('native fixture emits byte-fragmented Unicode semantic events before releasing its terminal latch', async () => {
  let release;
  const gate = { promise: new Promise(resolve => { release = resolve; }) };
  gate.resolve = release;
  const providers = createNativeSemanticsProvider();
  const text = '雪🙂 boundaries';
  const state = providers.register('fixture-selfcheck', [{ gate, output: [{ type: 'message', id: 'provider-id',
    role: 'assistant', status: 'completed', content: [{ type: 'output_text', text, annotations: [], logprobs: [] }] }] }]);
  const response = await providers.handle(new Request(`${state.baseUrl}/responses`, { method: 'POST',
    headers: { Authorization: `Bearer ${state.key}` }, body: JSON.stringify({ model: state.model, stream: true, store: false }) }));
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let wire = '';
  let chunks = 0;
  try {
    while (!wire.includes('response.output_text.delta')) {
      const next = await reader.read();
      assert.equal(next.done, false);
      chunks++;
      wire += decoder.decode(next.value, { stream: true });
    }
    assert.equal(state.terminalSent, false);
    release();
    while (true) {
      const next = await reader.read();
      if (next.done) break;
      chunks++;
      wire += decoder.decode(next.value, { stream: true });
    }
    wire += decoder.decode();
  } finally { release(); await reader.cancel(); }
  const events = wire.split('\n\n').filter(Boolean).map(frame => JSON.parse(frame.split('\n').find(line => line.startsWith('data: ')).slice(6)));
  assert.ok(chunks > 100, 'Transport deliberately exercises non-frame and non-UTF8 boundaries');
  assert.equal(events.filter(event => event.type === 'response.output_text.delta').map(event => event.delta).join(''), text);
  assert.equal(events.at(-1).type, 'response.completed');
  assert.equal(events.at(-1).response.usage.input_tokens_details.cache_write_tokens, 0);
  assert.equal(state.calls, 1);
});

test('native fixture rejects an unplanned second provider dispatch instead of hiding execution replay', async () => {
  const providers = createNativeSemanticsProvider();
  const state = providers.register('no-replay', [{ output: [] }]);
  const invoke = () => providers.handle(new Request(`${state.baseUrl}/responses`, { method: 'POST',
    headers: { Authorization: `Bearer ${state.key}` }, body: JSON.stringify({ model: state.model, stream: true, store: false }) }));
  await (await invoke()).text();
  await assert.rejects(invoke(), /unplanned\/replayed inference/);
  assert.ok(state.failure);
});
