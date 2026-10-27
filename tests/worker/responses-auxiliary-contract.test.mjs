import assert from 'node:assert/strict';
import test from 'node:test';
import OpenAI from 'openai';

// This isolates SDK shape/route behavior. The compiled Rust Worker and its
// financial/stateful acceptance are exercised separately by the HTTP journeys.
test('SDK inputTokens.count sends a count-only request without creating a response', async () => {
  const seen = [];
  const input = [{ role: 'user', content: [{ type: 'input_text', text: 'Count 雪 precisely.' }] }];
  const client = new OpenAI({
    apiKey: 'fixture-only', baseURL: 'https://fixture.invalid/v1', maxRetries: 0,
    fetch: async (url, init) => {
      seen.push({ method: init.method, path: new URL(url).pathname, body: JSON.parse(init.body) });
      return Response.json({ object: 'response.input_tokens', input_tokens: 42 });
    },
  });
  const counted = await client.responses.inputTokens.count({ model: 'agent_owned', input });
  assert.deepEqual(counted, { object: 'response.input_tokens', input_tokens: 42 });
  assert.deepEqual(seen, [{ method: 'POST', path: '/v1/responses/input_tokens', body: { model: 'agent_owned', input } }]);
});

test('SDK compact returns a reusable window rather than a response-chain identity', async () => {
  const user = {
    id: 'msg_user', type: 'message', role: 'user', status: 'completed',
    content: [{ type: 'input_text', text: 'Original goal 雪.' }],
  };
  const opaque = { id: 'cmp_owned', type: 'compaction', encrypted_content: 'mcmp1.fixture-opaque' };
  const compacted = {
    id: 'resp_compaction', object: 'response.compaction', created_at: 1792920000,
    output: [user, opaque],
    usage: { input_tokens: 42, input_tokens_details: { cached_tokens: 10, cache_write_tokens: 4 },
      output_tokens: 5, output_tokens_details: { reasoning_tokens: 1 }, total_tokens: 47 },
  };
  const seen = [];
  const client = new OpenAI({
    apiKey: 'fixture-only', baseURL: 'https://fixture.invalid/v1', maxRetries: 0,
    fetch: async (url, init) => {
      const request = { method: init.method, path: new URL(url).pathname, body: JSON.parse(init.body) };
      seen.push(request);
      if (request.path.endsWith('/compact')) return Response.json(compacted);
      return Response.json({
        id: 'resp_next', object: 'response', created_at: 1792920001, model: 'agent_owned',
        status: 'completed', error: null, incomplete_details: null, metadata: {}, output: [],
        parallel_tool_calls: true, tools: [], tool_choice: 'auto', instructions: null,
        usage: compacted.usage,
      });
    },
  });
  const window = await client.responses.compact({ model: 'agent_owned', previous_response_id: 'resp_previous', instructions: 'Preserve the fixed goal.' });
  assert.deepEqual(window, compacted);
  const nextInput = [...window.output, { role: 'user', content: 'Continue.' }];
  await client.responses.create({ model: 'agent_owned', input: nextInput, store: false });
  assert.deepEqual(seen.map(value => value.path), ['/v1/responses/compact', '/v1/responses']);
  assert.deepEqual(seen[1].body.input, nextInput);
  assert.equal(seen[1].body.previous_response_id, undefined, 'A compacted window is passed as input, not used as previous_response_id');
});
