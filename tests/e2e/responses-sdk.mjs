import assert from 'node:assert/strict';
import { createHash, randomBytes } from 'node:crypto';
import OpenAI from 'openai';
import { assertAvailableDirectDeepSeek, createPinnedSession, verifyServingSource } from './paid-route-policy.mjs';
import { assertCompletedResponse, assertStreamEvents } from '../worker/responses-sdk.mjs';

const origin = process.env.MAGENTS_TEST_API_ORIGIN?.replace(/\/$/, '');
const apiKey = process.env.MAGENTS_TEST_API_KEY;
assert.ok(origin && apiKey, 'The deployed Node SDK journey requires an explicit origin and platform API key');
assert.equal(process.env.MAGENTS_TEST_INFERENCE, '1', 'Real inference requires MAGENTS_TEST_INFERENCE=1');
assert.equal(new URL(origin).hostname, 'agents-staging.moesegfault.dev', 'Billed Node SDK acceptance is staging-only');
const headers = { Authorization: `Bearer ${apiKey}` };
const seed = process.env.MAGENTS_TEST_TRACE_SEED;
if (seed) headers.traceparent = `00-${createHash('sha256').update(seed).digest('hex').slice(0, 32)}-${randomBytes(8).toString('hex')}-00`;

async function request(method, endpoint, body, accepted = [200]) {
  const response = await fetch(origin + endpoint, {
    method,
    headers: { ...headers, ...(body === undefined ? {} : { 'Content-Type': 'application/json' }) },
    ...(body === undefined ? {} : { body: JSON.stringify(body) }),
    signal: AbortSignal.timeout(30000),
  });
  assert.ok(accepted.includes(response.status), `${method} ${endpoint}: HTTP ${response.status}`);
  if (response.status === 404) { await response.body?.cancel(); return null; }
  const text = await response.text();
  return text ? JSON.parse(text) : null;
}

async function cleanup(agentId, sessionIds) {
  const failures = [];
  try {
    const listing = await request('GET', `/v1/agents/${agentId}/sessions`);
    for (const session of listing.data) sessionIds.add(session.id);
  } catch (error) { failures.push({ phase: 'discover_sessions', error: error.name }); }
  for (const id of sessionIds) {
    const deadline = Date.now() + 480000;
    let closed = false;
    while (Date.now() < deadline) {
      try {
        const state = await request('GET', `/v1/sessions/${id}`, undefined, [200, 404]);
        if (state === null || (state.status === 'idle' || state.status === 'closed') && !state.run_id) {
          await request('DELETE', `/v1/sessions/${id}`, undefined, [200, 404]);
          closed = true;
          break;
        }
      } catch (error) {
        if (Date.now() + 2000 >= deadline) failures.push({ phase: 'close_session', error: error.name });
      }
      await new Promise(resolve => setTimeout(resolve, 2000));
    }
    if (!closed) failures.push({ phase: 'close_session', deadline_exceeded: true });
  }
  try { await request('DELETE', `/v1/agents/${agentId}`, undefined, [200, 404]); }
  catch (error) { failures.push({ phase: 'delete_agent', error: error.name }); }
  assert.deepEqual(failures, [], 'The deployed Node SDK fixture must fully clean up its owned resources');
}

async function main() {
  const sessionIds = new Set();
  const expectedBuild = process.env.MAGENTS_TEST_EXPECTED_BUILD;
  assert.match(expectedBuild ?? '', /^[a-f0-9]{40}$/, 'An exact approved deployed source is required before paid execution');
  const health = await request('GET', '/healthz');
  assert.equal(health.build_id, expectedBuild, 'The served source changed before paid execution');
  const routeProof = verifyServingSource(expectedBuild);
  const registered = assertAvailableDirectDeepSeek(await request('GET', '/v1/models'));
  const agent = await request('POST', '/v1/agents', {
    name: `node-sdk-${randomBytes(6).toString('hex')}`,
    model: { ...registered, max_output_tokens: 1024 },
    instructions: 'Use the time tool for current-time requests. Reply briefly and report its actual result.',
    tools: ['time'],
  }, [201]);
  try {
    const pinned = await createPinnedSession(request, agent, sessionIds);
    const client = new OpenAI({ apiKey, baseURL: origin + '/v1', maxRetries: 0, timeout: 180000, defaultHeaders: headers });
    const { data: stream, response: raw } = await client.responses.create({
      model: agent.id,
      input: 'Call the time tool once and report the current UTC time briefly.',
      stream: true,
    }, { headers: { 'X-Agents-Session': pinned.id } }).withResponse();
    const sessionId = raw.headers.get('x-agents-session');
    assert.equal(sessionId, pinned.id, 'The paid request must bind the exact preflighted Session');
    sessionIds.add(sessionId);
    const events = [];
    for await (const event of stream) events.push(event);
    const terminal = assertStreamEvents(events);
    const stored = await client.responses.retrieve(terminal.id);
    assertCompletedResponse(stored);
    assert.equal(stored.id, terminal.id);
    assert.ok(stored.output_text.length > 0);
    assert.deepEqual(stored.output, terminal.output);
    assert.deepEqual(stored.usage, terminal.usage);
    assert.deepEqual(stored.agents_usage, terminal.agents_usage);
    const session = await request('GET', `/v1/sessions/${sessionId}`);
    assert.ok(session.messages.some(message => message.role === 'tool'));
    assert.ok(session.messages.some(message => message.tool_calls?.some(call => call.function.name === 'time')));
    const cursor = events.find(event => event.type === 'response.output_text.delta').sequence_number;
    const replay = [];
    for await (const event of await client.responses.retrieve(stored.id, { stream: true, starting_after: cursor })) replay.push(event);
    assert.deepEqual(replay, events.filter(event => event.sequence_number > cursor));
    console.log(JSON.stringify({ journey: 'deployed-node-sdk-responses', result: 'passed', semantic_events: events.length, replay_events: replay.length, route: routeProof }));
  } finally {
    await cleanup(agent.id, sessionIds);
  }
}

main().catch(error => {
  console.error(JSON.stringify({ journey: 'deployed-node-sdk-responses', result: 'failed', error: error.name, ...(Number.isInteger(error.status) ? { status: error.status } : {}) }));
  process.exitCode = 1;
});
