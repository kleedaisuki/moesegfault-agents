import assert from 'node:assert/strict';
import OpenAI from 'openai';

const localTool = {
  type: 'function',
  name: 'local_echo',
  description: 'Run an account-local echo operation in the calling client.',
  strict: true,
  parameters: {
    type: 'object',
    properties: { value: { type: 'string' } },
    required: ['value'],
    additionalProperties: false,
  },
};

export function assertCompletedResponse(response, expectedText) {
  assert.equal(response.object, 'response');
  assert.equal(response.status, 'completed');
  assert.equal(typeof response.id, 'string');
  assert.ok(response.id.length > 0);
  assert.equal(response.error, null);
  assert.equal(response.incomplete_details, null);
  assert.ok(Array.isArray(response.output));
  assert.equal(typeof response.output_text, 'string', 'The real SDK must expose its output_text convenience property');
  if (expectedText !== undefined) assert.equal(response.output_text, expectedText);
  assertResponseUsage(response);
}

export function assertResponseUsage(response) {
  const usage = response.usage ?? response.agents_usage;
  assert.ok(usage, 'This completed billed fixture must retain its provider-reported token totals');
  for (const value of [usage.input_tokens, usage.output_tokens, usage.total_tokens, usage.input_tokens_details?.cached_tokens]) {
    assert.ok(Number.isSafeInteger(value));
    assert.ok(value >= 0);
  }
  assert.equal(usage.total_tokens, usage.input_tokens + usage.output_tokens);
  assert.ok(usage.input_tokens_details.cached_tokens <= usage.input_tokens);
  const reasoning = usage.output_tokens_details?.reasoning_tokens;
  const cacheWrite = usage.input_tokens_details?.cache_write_tokens;
  for (const [value, maximum] of [[reasoning, usage.output_tokens], [cacheWrite, usage.input_tokens]]) {
    assert.ok(value === null || (Number.isSafeInteger(value) && value >= 0 && value <= maximum),
      'Every known usage breakdown remains a bounded integer; unknown breakdowns are explicit null');
  }
  if (response.usage === null) {
    assert.ok(reasoning === null || cacheWrite === null,
      'Partial usage must identify a genuinely unknown breakdown without erasing known counts');
    assert.ok(response.agents_usage);
  } else {
    assert.ok(Number.isSafeInteger(reasoning), 'Standard Responses usage requires a concrete reasoning token count');
    assert.ok(Number.isSafeInteger(cacheWrite), 'Standard Responses usage requires a concrete cache-write token count');
  }
}

export function assertStreamEvents(events, expectedText) {
  assert.ok(events.length > 0, 'The SDK stream must contain semantic events');
  assert.equal(events[0].type, 'response.created');
  assert.ok(events.some(event => event.type === 'response.in_progress'));
  assert.ok(events.some(event => event.type === 'response.output_item.added'));
  assert.ok(events.some(event => event.type === 'response.content_part.added'));
  assert.ok(events.some(event => event.type === 'response.output_text.done'));
  assert.ok(events.some(event => event.type === 'response.content_part.done'));
  assert.ok(events.some(event => event.type === 'response.output_item.done'));
  assert.equal(events.at(-1).type, 'response.completed');
  let last = -1;
  for (const event of events) {
    assert.ok(Number.isSafeInteger(event.sequence_number));
    assert.ok(event.sequence_number > last, 'Response event sequence numbers must strictly increase');
    last = event.sequence_number;
  }
  const deltas = events.filter(event => event.type === 'response.output_text.delta');
  assert.ok(deltas.length > 0);
  const text = deltas.map(event => event.delta).join('');
  if (expectedText !== undefined) assert.equal(text, expectedText);
  const terminal = events.at(-1).response;
  assert.equal(terminal.status, 'completed');
  assert.equal(text, terminal.output.filter(item => item.type === 'message')
    .flatMap(item => item.content).filter(part => part.type === 'output_text').map(part => part.text).join(''));
  return terminal;
}

export async function runResponsesSdkJourney({ apiKey, baseURL, agentId, fetch }) {
  const client = new OpenAI({ apiKey, baseURL, fetch, maxRetries: 0, timeout: 8000 });
  const { data: first, response: raw } = await client.responses.create({
    model: agentId,
    input: 'Return the first Node SDK fixture answer.',
    instructions: 'Keep this request-specific instruction distinct from the server-managed prefix.',
    metadata: { verification: 'node-sdk' },
  }).withResponse();
  const sessionId = raw.headers.get('x-agents-session');
  assert.ok(sessionId, 'A standard SDK raw response must retain the durable session recovery handle');
  assertCompletedResponse(first, 'Node SDK first.');
  assert.equal(first.model, agentId);
  assert.equal(first.metadata.verification, 'node-sdk');
  assert.deepEqual(await client.responses.retrieve(first.id), first);

  const events = [];
  const stream = client.responses.stream({
    model: agentId,
    previous_response_id: first.id,
    input: 'Return the streamed Node SDK fixture answer.',
  });
  stream.on('event', event => events.push(event));
  const second = await stream.finalResponse();
  const terminal = assertStreamEvents(events, 'Node SDK streamed.');
  assertCompletedResponse(second, 'Node SDK streamed.');
  assert.equal(second.id, terminal.id);
  assert.equal(second.previous_response_id, first.id);
  assert.notEqual(second.id, first.id);
  assert.deepEqual(await client.responses.retrieve(first.id), first, 'Continuing must not mutate the old response snapshot');

  const cursor = events.find(event => event.type === 'response.output_text.delta').sequence_number;
  const replay = [];
  for await (const event of await client.responses.retrieve(second.id, { stream: true, starting_after: cursor })) replay.push(event);
  assert.deepEqual(replay, events.filter(event => event.sequence_number > cursor), 'SDK replay must exclude every event at or before starting_after');
  const replayStream = client.responses.stream({ response_id: second.id });
  assertCompletedResponse(await replayStream.finalResponse(), 'Node SDK streamed.');

  const calls = await client.responses.create({
    model: agentId,
    previous_response_id: second.id,
    input: 'Call local_echo with value sdk-local-value.',
    tools: [localTool],
    tool_choice: { type: 'function', name: 'local_echo' },
  });
  assertCompletedResponse(calls, '');
  const pending = calls.output.filter(item => item.type === 'function_call');
  assert.equal(pending.length, 1);
  assert.equal(pending[0].name, 'local_echo');
  assert.equal(pending[0].call_id, 'node-local-call');
  assert.deepEqual(JSON.parse(pending[0].arguments), { value: 'sdk-local-value' });
  const localResult = 'sdk-local-result';
  const final = await client.responses.create({
    model: agentId,
    previous_response_id: calls.id,
    input: [{ type: 'function_call_output', call_id: pending[0].call_id, output: localResult }],
    tools: [localTool],
  });
  assertCompletedResponse(final, 'Node SDK local tool complete.');
  assert.equal(final.previous_response_id, calls.id);
  const inputItems = await client.responses.inputItems.list(final.id, { limit: 1, order: 'asc' });
  assert.ok(Array.isArray(inputItems.data));
  assert.equal(inputItems.data.length, 1);
  assert.equal(inputItems.data[0].type, 'function_call_output');
  assert.equal(inputItems.data[0].call_id, pending[0].call_id);
  assert.equal(inputItems.data[0].output, localResult);

  const originals = [first, second, calls, final];
  async function retrieveCanonical(id) {
    const response = await (fetch ?? globalThis.fetch)(`${baseURL.replace(/\/$/, '')}/responses/${encodeURIComponent(id)}`, {
      method: 'GET', headers: { Authorization: `Bearer ${apiKey}` }, signal: AbortSignal.timeout(8000),
    });
    assert.equal(response.status, 200, 'Canonical response snapshot must be read successfully');
    return response.json();
  }
  // Stream helpers enrich SDK objects with parsed/output_parsed convenience
  // fields. Compare full HTTP representations on both sides, without stripping.
  const canonicalBeforeFork = await Promise.all(originals.map(response => retrieveCanonical(response.id)));
  const { data: fork, response: forkRaw } = await client.responses.create({
    model: agentId,
    previous_response_id: first.id,
    input: 'Create an independent Node SDK historical child.',
  }).withResponse();
  assertCompletedResponse(fork, 'Node SDK historical branch.');
  assert.equal(fork.previous_response_id, first.id);
  const branchSessionId = forkRaw.headers.get('x-agents-session');
  assert.ok(branchSessionId);
  assert.notEqual(branchSessionId, sessionId, 'Historical SDK continuation needs its own execution session');
  assert.ok(!originals.some(response => response.id === fork.id));
  assert.deepEqual(await Promise.all(originals.map(response => retrieveCanonical(response.id))), canonicalBeforeFork,
    'Historical branching cannot alter any field of any prior canonical HTTP response in the original chain');
  assert.deepEqual(await client.responses.retrieve(fork.id), fork);
  return { sessionId, branchSessionId, responseIds: [...originals.map(response => response.id), fork.id], events };
}
