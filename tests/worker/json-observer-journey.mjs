import assert from 'node:assert/strict';
import { createResponseJourneyFixture } from './responses-journey.mjs';

const delay = milliseconds => new Promise(resolve => setTimeout(resolve, milliseconds));

function gate() {
  let release;
  const wait = new Promise(resolve => { release = resolve; });
  return { wait, release };
}

async function bounded(promise, milliseconds = 8000) {
  let timer;
  try {
    return await Promise.race([promise, new Promise((_, reject) => {
      timer = setTimeout(() => reject(new Error('JSON observer exceeded its observation deadline')), milliseconds);
    })]);
  } finally { clearTimeout(timer); }
}

async function waitUntil(predicate) {
  const deadline = Date.now() + 8000;
  while (!await predicate()) {
    assert.ok(Date.now() < deadline, 'JSON observer did not reach the expected durable boundary');
    await delay(10);
  }
}

export async function runJsonObserverJourney(context) {
  const quietMilliseconds = process.env.MAGENTS_JSON_OBSERVER_FAST_PROBE === '1' ? 60 : 16050;
  const gates = Array.from({ length: 6 }, gate);
  const scenario = await createResponseJourneyFixture(context, 'json-observer', [
    { text: 'Quiet Chat JSON terminal.', wait: gates[0].wait },
    { text: 'Quiet Responses JSON terminal.', wait: gates[1].wait },
    { error: 503, wait: gates[2].wait },
    { error: 503, wait: gates[3].wait },
    { text: 'A late result cannot undo cancellation.', wait: gates[4].wait },
    { text: 'The disconnected observer did not cancel execution.', wait: gates[5].wait },
  ]);
  const bearer = scenario.principal.apiKey;
  const ownedRequest = (method, endpoint, body, expected = 200) => context.request(method, endpoint, body, expected, bearer);

  function start(endpoint, body, signal) {
    let headersReceived = false;
    const response = context.mf.dispatchFetch(`https://agents.test${endpoint}`, {
      method: 'POST', headers: { Authorization: `Bearer ${bearer}`, 'Content-Type': 'application/json' },
      body: JSON.stringify({ model: scenario.agent.id, ...body }), ...(signal ? { signal } : {}),
    }).then(value => { headersReceived = true; return value; });
    response.catch(() => {});
    return { response, headersReceived: () => headersReceived };
  }

  async function pendingHeaders(observer, calls, quietMilliseconds = 60) {
    await waitUntil(() => scenario.state.calls === calls);
    await delay(quietMilliseconds);
    assert.equal(observer.headersReceived(), false, 'Public JSON must not commit headers or expose internal heartbeats before a terminal outcome');
  }

  async function json(observer, expected = 200) {
    const response = await bounded(observer.response);
    assert.equal(response.status, expected);
    assert.match(response.headers.get('content-type') ?? '', /^application\/json(?:;|$)/i);
    assert.equal(response.headers.get('x-agents-internal-observer'), null, 'Private transport framing must not escape through public headers');
    const text = await bounded(response.text());
    assert.equal(text[0], '{', 'Public JSON must not begin with a whitespace/comment heartbeat');
    assert.ok(!text.includes(': keepalive\n') && !text.includes('event: terminal'), 'Internal SSE framing must remain private');
    return { response, value: JSON.parse(text) };
  }

  async function routeForLatestAcceptedRun() {
    const route = await context.db.prepare('SELECT id,session_id FROM response_routes WHERE account_id=?1 AND agent_id=?2 ORDER BY created_at DESC LIMIT 1')
      .bind(scenario.principal.owner, scenario.agent.id).first();
    assert.ok(route?.id && route.session_id, 'Routing evidence locates the already accepted operation; it does not authorize replay');
    return route;
  }

  try {
    const quietChat = start('/v1/chat/completions', { messages: [{ role: 'user', content: 'Return the quiet Chat terminal.' }] });
    await pendingHeaders(quietChat, 1, quietMilliseconds);
    gates[0].release();
    const chat = await json(quietChat);
    assert.equal(chat.value.choices[0].message.content, 'Quiet Chat JSON terminal.');
    assert.equal(chat.value.choices[0].finish_reason, 'stop');
    const chatSession = chat.response.headers.get('x-agents-session');
    assert.ok(chatSession);
    const chatState = await ownedRequest('GET', `/v1/sessions/${chatSession}`);
    assert.equal(chatState.run_id, null);
    assert.equal(chatState.messages.at(-1).content, chat.value.choices[0].message.content);
    await ownedRequest('DELETE', `/v1/sessions/${chatSession}`);

    const quietResponse = start('/v1/responses', { input: 'Return the quiet Responses terminal.' });
    await pendingHeaders(quietResponse, 2);
    gates[1].release();
    const response = await json(quietResponse);
    assert.equal(response.value.status, 'completed');
    assert.equal(response.value.output.filter(item => item.type === 'message')
      .flatMap(item => item.content).filter(part => part.type === 'output_text').map(part => part.text).join(''),
    'Quiet Responses JSON terminal.');
    assert.deepEqual(await scenario.read(response.value.id), response.value);
    await ownedRequest('DELETE', `/v1/sessions/${response.response.headers.get('x-agents-session')}`);

    const failedChat = start('/v1/chat/completions', { messages: [{ role: 'user', content: 'Return an explicit provider failure.' }] });
    await pendingHeaders(failedChat, 3);
    const errorSessions = await ownedRequest('GET', `/v1/agents/${scenario.agent.id}/sessions`);
    const running = [];
    for (const row of errorSessions.data) {
      const state = await ownedRequest('GET', `/v1/sessions/${row.id}`);
      if (state.run_id) running.push(state);
    }
    assert.equal(running.length, 1);
    scenario.expectUncertain(running[0].run_id);
    gates[2].release();
    const chatError = await json(failedChat, 502);
    assert.equal(chatError.value.error.code, 'provider_error');
    assert.equal(chatError.value.choices, undefined, 'A delayed error is not a successful HTTP 200 completion');
    assert.equal(chatError.response.headers.get('x-agents-session'), running[0].id);
    await ownedRequest('DELETE', `/v1/sessions/${running[0].id}`);

    const failedResponse = start('/v1/responses', { input: 'Retain the failed canonical response.' });
    await pendingHeaders(failedResponse, 4);
    gates[3].release();
    const responseError = await json(failedResponse);
    assert.equal(responseError.value.status, 'failed');
    assert.ok(responseError.value.error?.code);
    scenario.expectUncertain(responseError.value.id);
    assert.deepEqual(await scenario.read(responseError.value.id), responseError.value);
    await ownedRequest('DELETE', `/v1/sessions/${responseError.response.headers.get('x-agents-session')}`);

    const cancelled = await scenario.invoke({ input: 'Wait for explicit cancellation.', background: true });
    await waitUntil(() => scenario.state.calls === 5);
    const cancelSession = cancelled.headers.get('x-agents-session');
    const before = await ownedRequest('GET', `/v1/sessions/${cancelSession}`);
    await ownedRequest('POST', `/v1/responses/${cancelled.value.id}/cancel`, {});
    gates[4].release();
    await waitUntil(async () => (await scenario.read(cancelled.value.id)).status === 'cancelled');
    const cancelTerminal = await scenario.read(cancelled.value.id);
    assert.equal(cancelTerminal.status, 'cancelled');
    const after = await ownedRequest('GET', `/v1/sessions/${cancelSession}`);
    assert.equal(after.input_revision, before.input_revision);
    await ownedRequest('DELETE', `/v1/sessions/${cancelSession}`);

    const controller = new AbortController();
    const disconnected = start('/v1/responses', { input: 'Complete once even if the observer disconnects.' }, controller.signal);
    await pendingHeaders(disconnected, 6);
    const disconnectRoute = await routeForLatestAcceptedRun();
    controller.abort();
    await assert.rejects(() => bounded(disconnected.response), 'An explicitly disconnected HTTP observer cannot receive a later JSON response');
    gates[5].release();
    await waitUntil(async () => (await scenario.read(disconnectRoute.id)).status === 'completed');
    const retained = await scenario.read(disconnectRoute.id);
    await ownedRequest('DELETE', `/v1/sessions/${disconnectRoute.session_id}`);
    assert.deepEqual(await scenario.read(disconnectRoute.id), retained, 'Archival must not change the detached response or replay execution');
    assert.equal(scenario.state.calls, 6, 'Errors, cancellation, disconnection and retrieval cannot dispatch replacement inference');
    context.diagnostic?.(JSON.stringify({ event: 'json.observer_transport_fixture',
      quiet_observation_window_ms: quietMilliseconds, provider_calls: scenario.state.calls,
      public_headers_held: true, chat_failure_status: chatError.response.status,
      responses_failed_status: responseError.value.status, cancelled_status: cancelTerminal.status,
      disconnected_canonical_status: retained.status, internal_header_exposed: false }));
  } catch (error) { scenario.recordFailure(error); throw error; }
  finally { for (const pending of gates) pending.release(); await scenario.finish(); }
}
