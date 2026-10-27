import assert from 'node:assert/strict';
import OpenAI from 'openai';
import { createResponseJourneyFixture } from './responses-journey.mjs';

const delay = milliseconds => new Promise(resolve => setTimeout(resolve, milliseconds));

function gate() {
  let release;
  const wait = new Promise(resolve => { release = resolve; });
  return { wait, release };
}

async function waitUntil(predicate, milliseconds = 8000) {
  const deadline = Date.now() + milliseconds;
  while (!await predicate()) {
    assert.ok(Date.now() < deadline, 'Chat recovery did not reach its expected observation boundary');
    await delay(10);
  }
}

async function bounded(promise, milliseconds = 10000) {
  let timer;
  try {
    return await Promise.race([promise, new Promise((_, reject) => {
      timer = setTimeout(() => reject(new Error('Chat recovery observation deadline exceeded')), milliseconds);
    })]);
  } finally { clearTimeout(timer); }
}

export function createChatRecoveryControl() {
  const cases = new Map();
  return {
    summary() { return [...cases].map(([caseId, state]) => ({ caseId, ...state })); },
    register(caseId, mode) {
      assert.ok(!cases.has(caseId));
      const state = { mode, runPosts: 0, reads: 0, cuts: 0, statuses: [], readRunIds: [], readStreams: [],
        subscriptions: 0, heartbeats: 0, subscriptionTerminals: 0, runId: null };
      cases.set(caseId, state);
      return state;
    },
    async handle(request) {
      if (request.url !== 'https://chat-recovery-control.test/') return undefined;
      const input = await request.json();
      const state = cases.get(input.caseId);
      assert.ok(state, 'Only the explicit test case may select an internal fault');
      if (input.operation === 'open') { state.runPosts++; state.runId = input.runId; }
      else if (input.operation === 'read') { state.reads++; state.readRunIds.push(input.runId); state.readStreams.push(input.stream); }
      else if (input.operation === 'read-result') {
        state.statuses.push(input.status);
        if (input.status === 200 && input.contentType?.includes('text/event-stream') && input.marker === 'chat') state.subscriptions++;
      }
      else if (input.operation === 'subscription-heartbeat') state.heartbeats++;
      else if (input.operation === 'subscription-terminal') state.subscriptionTerminals++;
      else if (input.operation === 'cut') state.cuts++;
      else assert.fail('Unknown Chat recovery test operation');
      return Response.json({ mode: state.mode });
    },
  };
}

function consume(stream) {
  const observed = { text: '', terminal: null, stopCount: 0, error: null, finished: false };
  observed.done = (async () => {
    try {
      for await (const chunk of stream) {
        for (const choice of chunk.choices ?? []) {
          if (typeof choice.delta?.content === 'string') observed.text += choice.delta.content;
          if (choice.finish_reason != null) { assert.equal(choice.finish_reason, 'stop'); observed.stopCount++; observed.terminal = chunk; }
        }
      }
    } catch (error) { observed.error = error; }
    observed.finished = true;
    return observed;
  })();
  return observed;
}

export async function runChatRecoveryJourney(context) {
  const gates = Array.from({ length: 6 }, gate);
  const actions = [
    { text: '原始 雪🙂 before-output.', wait: gates[0].wait },
    { text: 'Later independent run.' },
    { text: '工具前言雪🙂', calls: [{ id: 'chat-recovery-time', name: 'time' }] },
    { text: ' 完成 Ω🙂.', wait: gates[1].wait },
    { text: 'Terminal before DONE.' },
    { text: 'prefix 雪🙂 mismatch.', wait: gates[2].wait },
    { text: 'An overloaded observer cannot restart this run.', wait: gates[3].wait },
    { error: 503, wait: gates[4].wait },
    { text: 'Caller abort leaves the original execution alive.', wait: gates[5].wait },
  ];
  const scenario = await createResponseJourneyFixture(context, 'chat-recovery', actions,
    { context_window: 32768, validate_response_schema: false });
  const control = context.chatRecoveryControl;
  const principal = scenario.principal;
  const owned = (method, endpoint, body, expected = 200) => context.request(method, endpoint, body, expected, principal.apiKey);

  async function start(caseId, mode, sessionId) {
    const state = control.register(caseId, mode);
    let raw;
    const sdk = new OpenAI({ apiKey: principal.apiKey, baseURL: 'https://agents.test/v1', maxRetries: 0,
      fetch: async (input, init) => { raw = await context.mf.dispatchFetch(input, init); return raw; } });
    const stream = await sdk.chat.completions.create({ model: scenario.agent.id,
      messages: [{ role: 'user', content: 'Execute the next bounded controlled Chat recovery action.' }], stream: true,
    }, { headers: { 'x-worker-test-chat-recovery': caseId, ...(sessionId ? { 'x-agents-session': sessionId } : {}) } });
    const id = raw.headers.get('x-agents-session');
    assert.ok(id);
    return { state, sessionId: id, stream, observed: consume(stream) };
  }

  async function receipt(sessionId, runId, expected = 200) {
    const namespace = await context.mf.getDurableObjectNamespace('SESSIONS');
    const stub = namespace.get(namespace.idFromName(`${principal.owner}:${sessionId}`));
    const response = await stub.fetch(`https://session.internal/chat-runs/${runId}`);
    assert.equal(response.status, expected);
    return response.json();
  }

  async function follow(sessionId, runId) {
    const namespace = await context.mf.getDurableObjectNamespace('SESSIONS');
    const stub = namespace.get(namespace.idFromName(`${principal.owner}:${sessionId}`));
    const response = await stub.fetch(`https://session.internal/chat-runs/${runId}?stream=true`);
    assert.equal(response.status, 200);
    if (!response.headers.get('content-type')?.includes('text/event-stream')) return response.json();
    assert.equal(response.headers.get('x-agents-internal-observer'), 'chat');
    const read = bounded(response.text());
    // Start draining immediately; completion may race the next subscription.
    read.catch(() => {});
    return { live: true, value: read.then(text => {
      const payloads = text.split('\n\n').flatMap(block => block.split('\n').filter(line => line.startsWith('data: ')))
        .map(line => line.slice(6)).filter(data => data !== '[DONE]').map(data => JSON.parse(data));
      assert.equal(payloads.length, 1, 'Each committed follower receives one terminal envelope');
      assert.equal(payloads[0].status, 200);
      return payloads[0].body;
    }) };
  }

  async function complete(observation, text) {
    const result = await bounded(observation.observed.done);
    assert.equal(result.error, null, result.error?.message);
    assert.equal(result.text, text);
    assert.equal(result.stopCount, 1, 'A repaired observation has exactly one original terminal stop');
    assert.ok(result.terminal?.usage);
    const original = await receipt(observation.sessionId, observation.state.runId);
    assert.equal(original.run_id, observation.state.runId);
    assert.equal(original.status, 'completed');
    assert.equal(original.text, text);
    assert.deepEqual(result.terminal, original.terminal, 'Recovered usage and created timestamp must be the exact original terminal');
    assert.ok(observation.state.readRunIds.every(id => id === observation.state.runId));
    assert.equal(observation.state.runPosts, 1, 'Recovery cannot POST the run again');
    return original;
  }

  try {
    const before = await start('before-output', 'before');
    await waitUntil(() => before.state.reads > 0 || before.observed.finished);
    assert.equal(before.observed.finished, false, 'Internal observation loss must not truncate the public stream before its original terminal stop');
    assert.equal(before.state.readStreams[0], true, 'Recovery must attach to the exact active run, not poll its task duration');
    await waitUntil(() => before.state.subscriptions === 1 || before.observed.finished);
    assert.equal(before.state.subscriptions, 1, 'An active run exposes one native terminal subscription');
    const pending = await owned('GET', `/v1/sessions/${before.sessionId}`);
    assert.deepEqual(await receipt(before.sessionId, before.state.runId, 202),
      { run_id: before.state.runId, status: 'in_progress' }, 'Plain reads retain their pending introspection contract');
    const missing = await receipt(before.sessionId, `run_${'f'.repeat(32)}`, 404);
    assert.equal(missing.error.code, 'chat_observation_unavailable');
    await delay(16050);
    await waitUntil(() => before.state.heartbeats > 0, 2500);
    assert.equal(before.observed.finished, false, 'A healthy pending subscription cannot complete before its original run');
    assert.equal(before.state.reads, 1, 'Healthy attached waiting across a native keepalive performs no repeated polling');
    assert.equal(before.state.subscriptions, 1);
    assert.equal(before.state.subscriptionTerminals, 0);
    const peer = await follow(before.sessionId, before.state.runId);
    assert.equal(peer.live, true, 'A second active follower registers without replacing the existing observer');
    gates[0].release();
    const racing = [follow(before.sessionId, before.state.runId), follow(before.sessionId, before.state.runId)];
    const immutable = await complete(before, actions[0].text);
    assert.deepEqual(await peer.value, immutable);
    for (const connection of await Promise.all(racing)) {
      assert.deepEqual(connection.live ? await connection.value : connection, immutable,
        'Finish-versus-subscribe races must return the same committed result, never lose publication');
    }
    assert.equal(before.state.reads, 1);
    assert.equal(before.state.subscriptionTerminals, 1);
    const after = await owned('GET', `/v1/sessions/${before.sessionId}`);
    assert.equal(after.input_revision, pending.input_revision);
    assert.equal(after.last_input_at_ms, pending.last_input_at_ms);
    assert.deepEqual(after.pinned, pending.pinned);
    const later = await start('later-normal', 'pass', before.sessionId);
    await complete(later, actions[1].text);
    assert.equal(later.state.reads, 0, 'An ordinary completed stream performs no recovery reads');
    const fixedClock = await owned('GET', `/v1/sessions/${before.sessionId}`);
    assert.deepEqual(await receipt(before.sessionId, before.state.runId), immutable, 'A later run cannot replace the original receipt');
    const completedFollower = await follow(before.sessionId, before.state.runId);
    assert.equal(completedFollower.live, undefined);
    assert.deepEqual(completedFollower, immutable, 'An already committed observation remains an immediate JSON read');
    await owned('DELETE', `/v1/sessions/${before.sessionId}`);
    await context.mf.unsafeEvictDurableObject('', 'Session', { name: `${principal.owner}:${before.sessionId}` });
    assert.deepEqual(await receipt(before.sessionId, before.state.runId), immutable, 'Archive/cold reconstruction retains the same exact receipt');
    const closed = await owned('GET', `/v1/sessions/${before.sessionId}`);
    assert.equal(closed.input_revision, fixedClock.input_revision);
    assert.equal(closed.last_input_at_ms, fixedClock.last_input_at_ms);
    await owned('GET', `/v1/sessions/${before.sessionId}/chat-runs/${before.state.runId}`, undefined, 404);

    const prefix = await start('unicode-tool-prefix', 'prefix-error');
    await waitUntil(() => prefix.observed.text === actions[2].text && prefix.state.reads > 0 && scenario.state.calls === 4);
    gates[1].release();
    await complete(prefix, actions[2].text + actions[3].text);
    const toolState = await owned('GET', `/v1/sessions/${prefix.sessionId}`);
    assert.equal(toolState.messages.filter(message => message.role === 'tool' && message.tool_call_id === 'chat-recovery-time').length, 1);
    assert.equal(scenario.state.requests[3].messages.filter(message => message.role === 'tool' && message.tool_call_id === 'chat-recovery-time').length, 1);
    const modelRows = await context.db.prepare("SELECT COUNT(*) AS count FROM usage_ledger l JOIN usage_reservations r ON r.id=l.reservation_id WHERE r.session_id=?1 AND l.resource='model'").bind(prefix.sessionId).first('count');
    assert.equal(modelRows, 2, 'Each of the two original provider rounds settles once despite internal transport failure');
    await owned('DELETE', `/v1/sessions/${prefix.sessionId}`);

    const terminal = await start('terminal-no-done', 'terminal');
    await complete(terminal, actions[4].text);
    assert.equal(terminal.state.reads, 0, 'A received terminal needs only DONE, never a result lookup');
    await owned('DELETE', `/v1/sessions/${terminal.sessionId}`);

    const mismatch = await start('prefix-mismatch', 'mismatch');
    gates[2].release();
    const wrong = await bounded(mismatch.observed.done);
    assert.equal(wrong.error?.code, 'chat_observation_mismatch');
    assert.equal(wrong.stopCount, 0);
    assert.equal(wrong.terminal, null);
    assert.equal(wrong.text, actions[5].text);
    assert.ok(mismatch.state.reads > 0);
    await owned('DELETE', `/v1/sessions/${mismatch.sessionId}`);

    const overloaded = await start('overloaded-observation', 'overload');
    await waitUntil(() => overloaded.state.reads > 0);
    const overflow = await bounded(overloaded.observed.done);
    assert.equal(overflow.error?.code, 'chat_observation_overloaded');
    assert.equal(overflow.stopCount, 0);
    assert.equal(overloaded.state.reads, 1, 'An overloaded observation must not amplify load');
    gates[3].release();
    await waitUntil(async () => !(await owned('GET', `/v1/sessions/${overloaded.sessionId}`)).run_id);
    await owned('DELETE', `/v1/sessions/${overloaded.sessionId}`);

    const failed = await start('unknown-provider-outcome', 'unknown');
    await waitUntil(() => failed.state.reads > 0);
    scenario.expectUncertain(failed.state.runId);
    gates[4].release();
    const unknown = await bounded(failed.observed.done);
    assert.equal(unknown.error?.code, 'provider_http_error');
    assert.equal(unknown.stopCount, 0);
    assert.equal(unknown.terminal, null);
    const failedReceipt = await receipt(failed.sessionId, failed.state.runId);
    assert.equal(failedReceipt.status, 'failed');
    assert.equal(failedReceipt.completion, null);
    assert.ok(failedReceipt.terminal.error);
    await owned('DELETE', `/v1/sessions/${failed.sessionId}`);

    const abortState = control.register('caller-abort', 'abort');
    const controller = new AbortController();
    const response = await context.mf.dispatchFetch('https://agents.test/v1/chat/completions', {
      method: 'POST', signal: controller.signal,
      headers: { Authorization: `Bearer ${principal.apiKey}`, 'Content-Type': 'application/json', 'x-worker-test-chat-recovery': 'caller-abort' },
      body: JSON.stringify({ model: scenario.agent.id, stream: true, messages: [{ role: 'user', content: 'Bounded caller-abort observation.' }] }),
    });
    const abortSession = response.headers.get('x-agents-session');
    const reader = response.body.getReader();
    const pendingRead = reader.read().catch(() => undefined);
    await waitUntil(() => abortState.reads > 0);
    controller.abort();
    await reader.cancel().catch(() => {});
    await pendingRead;
    await delay(100);
    const readsAfterAbort = abortState.reads;
    await delay(900);
    assert.equal(abortState.reads, readsAfterAbort, 'Caller abort drops recovery read/timer rather than continuing a background repair');
    gates[5].release();
    await waitUntil(async () => !(await owned('GET', `/v1/sessions/${abortSession}`)).run_id);
    assert.equal((await receipt(abortSession, abortState.runId)).text, actions[8].text);
    assert.equal(abortState.runPosts, 1);
    await owned('DELETE', `/v1/sessions/${abortSession}`);

    assert.equal(scenario.state.calls, actions.length, 'Every controlled provider action executes exactly once');
    context.diagnostic?.(JSON.stringify({ event: 'chat.observation_recovery_fixture', provider_calls: scenario.state.calls,
      before_output_reads: before.state.reads, unicode_prefix_reads: prefix.state.reads,
      healthy_subscription_opens: before.state.subscriptions, healthy_keepalives: before.state.heartbeats,
      normal_reads: later.state.reads, terminal_reads: terminal.state.reads, overload_reads: overloaded.state.reads,
      exact_text_and_terminal: true, immutable_archived_receipt: true, caller_abort_stops_reads: true }));
  } catch (error) {
    context.diagnostic?.(JSON.stringify({ event: 'chat.observation_recovery_failure', error: error.name,
      provider_calls: scenario.state.calls, observations: control.summary() }));
    scenario.recordFailure(error);
    throw error;
  }
  finally {
    for (const waiting of gates) waiting.release();
    await scenario.finish();
  }
}
