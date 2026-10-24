import assert from 'node:assert/strict';
import { createBrowserPrincipal } from './browser-principal.mjs';

const summary = 'Preserved goal: continue after compaction. Unicode evidence: 雪🧪.';
// Opaque provider metadata crosses a real 256 KiB storage chunk boundary.
const signed = 'opaque-signature:' + 'x'.repeat(256 * 1024) + ':雪';
const thought = { type: 'thinking', thinking: 'Use time before continuing.', signature: signed };
const reasoning = [{ type: 'reasoning.encrypted', data: signed }];
const frame = value => `data: ${JSON.stringify(value)}\n\n`;

function anthropic(state, kind) {
  const initial = kind === 'tool';
  const resumed = kind === 'resumed';
  const blocks = initial ? [thought, { type: 'tool_use', id: state.toolId, name: 'time', input: {} }]
    : [{ type: 'text', text: resumed ? 'Resumed after compaction.' : 'First time turn completed.' }];
  const usage = resumed
    ? { input_tokens: 30, cache_creation_input_tokens: 2, cache_read_input_tokens: 8, output_tokens: 7 }
    : { input_tokens: 100, cache_creation_input_tokens: 20, cache_read_input_tokens: 30, output_tokens: initial ? 10 : 15 };
  return { id: `compact-${state.streams}`, type: 'message', role: 'assistant', model: state.model,
    content: blocks, stop_reason: initial ? 'tool_use' : 'end_turn', stop_sequence: null, usage };
}

function streamResponse(state, kind) {
  if (state.protocol === 'anthropic') {
    const message = anthropic(state, kind);
    const frames = [{ type: 'message_start', message: { ...message, content: [], stop_reason: null, usage: { ...message.usage, output_tokens: 0 } } }];
    for (const [index, block] of message.content.entries()) {
      frames.push({ type: 'content_block_start', index, content_block: block }, { type: 'content_block_stop', index });
    }
    frames.push({ type: 'message_delta', delta: { stop_reason: message.stop_reason, stop_sequence: null }, usage: { output_tokens: message.usage.output_tokens } }, { type: 'message_stop' });
    return new Response(frames.map(frame).join(''), { headers: { 'Content-Type': 'text/event-stream' } });
  }
  const initial = kind === 'tool';
  const resumed = kind === 'resumed';
  const delta = initial ? { role: 'assistant', reasoning_content: 'Opaque reasoning is preserved.', reasoning_details: reasoning,
    tool_calls: [{ index: 0, id: state.toolId, type: 'function', function: { name: 'time', arguments: '{}' } }] }
    : { role: 'assistant', content: resumed ? 'Resumed after compaction.' : 'First time turn completed.' };
  const usage = resumed ? { prompt_tokens: 40, completion_tokens: 7, total_tokens: 47, prompt_tokens_details: { cached_tokens: 8 }, cost: 7.123456789e-6 }
    : { prompt_tokens: 150, completion_tokens: initial ? 10 : 15, total_tokens: initial ? 160 : 165, prompt_tokens_details: { cached_tokens: 30 }, cost: 0.001234 };
  return new Response(frame({ id: `compact-${state.streams}`, model: state.model, choices: [{ index: 0, delta, finish_reason: initial ? 'tool_calls' : 'stop' }], usage }) + 'data: [DONE]\n\n', { headers: { 'Content-Type': 'text/event-stream' } });
}

function assertContinuation(state, input) {
  if (state.protocol === 'openai') {
    const assistant = input.messages.find(message => message.tool_calls?.some(call => call.id === state.toolId));
    assert.ok(assistant.reasoning_details[0].data === signed, 'Signed OpenAI continuation must survive storage byte boundaries');
    assert.equal(assistant.reasoning_content, 'Opaque reasoning is preserved.');
    assert.ok(input.messages.some(message => message.role === 'tool' && message.tool_call_id === state.toolId));
    return;
  }
  const assistant = input.messages.find(message => message.role === 'assistant' && message.content.some(block => block.type === 'tool_use'));
  assert.ok(assistant.content[0].signature === signed, 'Signed Anthropic continuation must survive storage byte boundaries');
  assert.ok(input.messages.some(message => message.role === 'user' && message.content.some(block => block.type === 'tool_result' && block.tool_use_id === state.toolId)));
}

async function summaryResponse(state) {
  if (state.protocol === 'openai') {
    // Real native I/O crosses the second stream's heartbeat while the summary is awaited.
    await new Promise(resolve => setTimeout(resolve, 16000));
    const value = { id: 'compact-json-summary', model: state.model, choices: [{ message: { role: 'assistant', content: summary, reasoning_details: [{ type: 'reasoning.encrypted', data: signed }] }, finish_reason: 'stop' }],
      usage: { prompt_tokens: 150, completion_tokens: 9, total_tokens: 159, prompt_tokens_details: { cached_tokens: 30 }, cost: 'RAW_DECIMAL' } };
    const body = JSON.stringify(value).replace('"cost":"RAW_DECIMAL"', '"cost":0.00005472727272727273123456789');
    assert.ok(Buffer.byteLength(body) > 256 * 1024 && Buffer.byteLength(body) < 16 * 1024 * 1024);
    return new Response(body, { headers: { 'Content-Type': 'application/json' } });
  }
  return Response.json({ id: 'compact-json-summary', model: state.model, type: 'message', role: 'assistant', content: [thought, { type: 'text', text: summary }], stop_reason: 'end_turn', stop_sequence: null,
    usage: { input_tokens: 100, cache_creation_input_tokens: 20, cache_read_input_tokens: 30, output_tokens: 9 } });
}

export function createCompactionProviderFixture() {
  const routes = new Map();
  return {
    register(protocol) {
      const baseUrl = `https://compact.${protocol}.example.com/v1`;
      const state = { protocol, baseUrl, model: `compact-custom-${protocol}`, key: `synthetic-compact-${protocol}-key`, toolId: `compact-time-${protocol}`, streams: 0, summaries: 0, failure: null };
      routes.set(baseUrl + (protocol === 'anthropic' ? '/messages' : '/chat/completions'), state);
      return state;
    },
    async handle(request) {
      const state = routes.get(request.url);
      if (!state) return undefined;
      try {
        const input = await request.json();
        assert.equal(input.model, state.model);
        if (state.protocol === 'openai') assert.ok(request.headers.get('authorization') === `Bearer ${state.key}`);
        else {
          assert.ok(request.headers.get('x-api-key') === state.key);
          assert.equal(request.headers.get('authorization'), null);
          assert.equal(request.headers.get('anthropic-version'), '2023-06-01');
        }
        assert.equal(input.max_tokens, 512);
        if (input.stream === false) {
          state.summaries++;
          assert.equal(state.summaries, 1);
          assert.equal(input.tools, undefined, 'Compaction must not dispatch tools');
          const instruction = state.protocol === 'openai' ? input.messages[0].content : input.system[0].text;
          assert.ok(instruction.startsWith('Summarize this agent session'));
          assertContinuation(state, input);
          return await summaryResponse(state);
        }
        assert.equal(input.stream, true);
        state.streams++;
        assert.ok(state.streams <= 3);
        if (state.streams === 2) assertContinuation(state, input);
        if (state.streams === 3) {
          assert.equal(state.summaries, 1);
          const texts = state.protocol === 'openai' ? input.messages.map(message => message.content) : input.messages.flatMap(message => message.content).map(block => block.text);
          assert.ok(texts.includes(`Earlier session context (summary):\n${summary}`), 'The post-compaction request must consume the actual JSON summary');
        }
        return streamResponse(state, state.streams === 1 ? 'tool' : state.streams === 2 ? 'initial-final' : 'resumed');
      } catch (error) { state.failure = error; throw error; }
    },
  };
}

async function bounded(value, timeout = 8000) {
  let timer;
  try { return await Promise.race([value, new Promise((_, reject) => { timer = setTimeout(() => reject(new Error('Compaction integration observer exceeded its deadline')), timeout); })]); }
  finally { clearTimeout(timer); }
}

function assertToolGroups(messages) {
  const pending = new Set();
  for (const message of messages) {
    if (message.role === 'tool') { assert.ok(pending.delete(message.tool_call_id)); continue; }
    assert.equal(pending.size, 0);
    for (const call of message.tool_calls ?? []) pending.add(call.id);
  }
  assert.equal(pending.size, 0);
}

export async function runCompactionJourney({ mf, db, encryptionKey, request, providers, protocol }) {
  const principal = await createBrowserPrincipal({ mf, db, encryptionKey, owner: `compaction-${protocol}-owner` });
  const state = providers.register(protocol);
  const credential = await principal.browser('POST', '/v1/provider-keys', { protocol, base_url: state.baseUrl, api_key: state.key }, 201);
  const agent = await request('POST', '/v1/agents', { name: `Compact ${protocol}`, model: { provider: 'byok', credential_id: credential.id, model: state.model, context_window: 8192, max_output_tokens: 512 }, tools: ['time'], compaction_percent: 1, workspace: false }, 201, principal.apiKey);
  const session = await request('POST', `/v1/agents/${agent.id}/sessions`, {}, 201, principal.apiKey);
  async function turn(content, timeout = 8000) {
    const response = await bounded(mf.dispatchFetch('https://agents.test/v1/chat/completions', { method: 'POST', headers: { Authorization: `Bearer ${principal.apiKey}`, 'Content-Type': 'application/json', 'X-Agents-Session': session.id }, body: JSON.stringify({ model: agent.id, messages: [{ role: 'user', content }], stream: true }) }));
    assert.equal(response.status, 200);
    const wire = await bounded(response.text(), timeout);
    if (state.failure) throw state.failure;
    const chunks = wire.split('\n\n').filter(frame => frame.startsWith('data: ') && frame !== 'data: [DONE]').map(frame => JSON.parse(frame.slice(6)));
    assert.ok(wire.trimEnd().endsWith('data: [DONE]'), 'The actual observer must receive terminal DONE before EOF');
    assert.ok(chunks.some(chunk => chunk.choices?.some(choice => choice.finish_reason === 'stop')));
    return { wire, chunks, usage: chunks.find(chunk => chunk.usage)?.usage };
  }
  await turn('Use time, then complete the first turn.');
  const before = await request('GET', `/v1/sessions/${session.id}`, undefined, 200, principal.apiKey);
  assert.equal(before.compact_count, 0);
  assert.equal(before.messages.length, 4);
  assertToolGroups(before.messages);
  const continued = await turn('Continue the preserved goal after compaction.', 32000);
  assert.equal(state.summaries, 1);
  assert.equal(state.streams, 3);
  assert.ok(continued.wire.includes('Resumed after compaction.'));
  if (protocol === 'openai') assert.ok(continued.wire.includes(': keepalive\n\n'), 'A real 16-second JSON summary wait must keep the second observer alive');
  assert.deepEqual(continued.usage, { prompt_tokens: 190, completion_tokens: 16, total_tokens: 206, prompt_tokens_details: { cached_tokens: 38 } });
  const after = await request('GET', `/v1/sessions/${session.id}`, undefined, 200, principal.apiKey);
  assert.equal(after.compact_count, 1);
  assert.deepEqual(after.pinned, before.pinned);
  assert.deepEqual(after.messages.slice(0, before.messages.length), before.messages);
  assert.equal(after.messages.length, 6);
  assertToolGroups(after.messages);
  await mf.unsafeEvictDurableObject('', 'Session', { name: `${principal.owner}:${session.id}` });
  const recovered = await request('GET', `/v1/sessions/${session.id}`, undefined, 200, principal.apiKey);
  assert.equal(recovered.compact_count, 1);
  assert.deepEqual(recovered.messages, after.messages);
  const costs = await db.prepare("SELECT CAST(l.quantity AS TEXT) AS quantity,CAST(l.amount_microusd AS TEXT) AS amount FROM usage_ledger l JOIN usage_reservations r ON r.id=l.reservation_id WHERE r.account_id=?1 AND l.resource='model'").bind(principal.owner).all();
  assert.deepEqual(costs.results.map(row => row.quantity).sort(), ['159', '160', '165', '47'].sort());
  assert.ok(costs.results.every(row => row.amount === '0'));
  await request('DELETE', `/v1/sessions/${session.id}`, undefined, 200, principal.apiKey);
  assert.equal(await db.prepare('SELECT CAST(reserved_microusd AS TEXT) AS held FROM usage_accounts WHERE account_id=?1').bind(principal.owner).first('held'), '0');
}
