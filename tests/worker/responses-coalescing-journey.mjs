import assert from 'node:assert/strict';
import { mkdir, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import OpenAI from 'openai';
import { createResponseJourneyFixture } from './responses-journey.mjs';
import { assertCompletedResponse, assertStreamEvents } from './responses-sdk.mjs';

const origin = 'https://agents.test';
const encoder = new TextEncoder();
// More than 64KiB must traverse the ready stream: a tiny burst would conceal
// false observer drops after the bounded queue's first 32 frames.
const burstPiece = '雪🌸x'.repeat(64);
const burstCount = 144;
const burstDeltaFrames = 1 + burstCount / 2;
const burstText = 'S' + burstPiece.repeat(burstCount);
const usage = { prompt_tokens: 150, completion_tokens: 10, total_tokens: 160,
  prompt_tokens_details: { cached_tokens: 30 }, completion_tokens_details: { reasoning_tokens: 0 } };
const wireFrame = value => `data: ${JSON.stringify(value)}\n\n`;

export function createCoalescingProviderFixture() {
  const routes = new Map();
  return {
    register(name, actions) {
      const state = { baseUrl: `https://coalescing-${name}.example.com/v1`, model: `coalescing-${name}`,
        key: `synthetic-coalescing-${name}`, actions, calls: 0, failure: null, records: [], releases: [] };
      routes.set(state.baseUrl + '/chat/completions', state);
      return state;
    },
    async handle(request) {
      const state = routes.get(request.url);
      if (!state) return undefined;
      try {
        assert.equal(request.headers.get('authorization'), `Bearer ${state.key}`);
        const input = await request.json();
        assert.equal(input.model, state.model);
        assert.equal(input.stream, true);
        const action = state.actions[state.calls++];
        assert.ok(action, 'Reading/replaying a response must not dispatch another inference');
        const record = { kind: action.kind, emissions: [], cancelled: false };
        state.records.push(record);
        const timers = new Set();
        const schedule = (delay, callback) => {
          const timer = setTimeout(() => { timers.delete(timer); callback(); }, delay);
          timers.add(timer);
        };
        let closed = false;
        const body = new ReadableStream({
          start(controller) {
            const emit = value => {
              if (!closed) controller.enqueue(encoder.encode(wireFrame(value)));
            };
            const text = delta => {
              record.emissions.push({ text: delta, at: performance.now() });
              emit({ id: `coalescing-${state.calls}`, model: state.model,
                choices: [{ index: 0, delta: { content: delta }, finish_reason: null }] });
            };
            const complete = (calls = undefined) => {
              if (closed) return;
              record.completedAt = performance.now();
              emit({ id: `coalescing-${state.calls}`, model: state.model, usage,
                choices: [{ index: 0, delta: calls ? { tool_calls: calls } : {},
                  finish_reason: calls ? 'tool_calls' : 'stop' }] });
              controller.enqueue(encoder.encode('data: [DONE]\n\n'));
              closed = true;
              controller.close();
            };
            switch (action.kind) {
              case 'quiet':
              case 'chat':
                text('FIRST');
                schedule(1, () => text('SECOND'));
                schedule(3000, () => { text('THIRD'); complete(); });
                break;
              case 'burst':
                text('S');
                for (let index = 0; index < burstCount; index++) text(burstPiece);
                complete();
                break;
              case 'tool':
                text('TOOL_FIRST');
                schedule(1, () => text('TOOL_TAIL'));
                schedule(2, () => complete([{ index: 0, id: 'coalescing-clock', type: 'function',
                  function: { name: 'time', arguments: '{}' } }]));
                break;
              case 'terminal':
                text('TERMINAL_FIRST');
                schedule(1, () => text('TERMINAL_TAIL'));
                schedule(2, () => complete());
                break;
              case 'tool-final': {
                const result = input.messages.find(message => message.role === 'tool' && message.tool_call_id === 'coalescing-clock');
                assert.ok(result, 'The second model round must receive the actual clock result');
                assert.ok(Number.isSafeInteger(JSON.parse(result.content).unix_ms));
                text('TOOL_FINAL');
                complete();
                break;
              }
              case 'cancel':
                text('CANCEL_FIRST');
                schedule(1, () => text('CANCEL_TAIL'));
                state.releases.push(complete);
                break;
              default:
                throw new Error('Unknown coalescing test action');
            }
          },
          cancel() {
            record.cancelled = true;
            closed = true;
            for (const timer of timers) clearTimeout(timer);
            timers.clear();
          },
        });
        return new Response(body, { headers: { 'Content-Type': 'text/event-stream' } });
      } catch (error) {
        state.failure = error;
        throw error;
      }
    },
  };
}

async function bounded(promise, milliseconds = 8000) {
  let timer;
  try {
    return await Promise.race([promise, new Promise((_, reject) => {
      timer = setTimeout(() => reject(new Error('Coalescing observation exceeded its deadline')), milliseconds);
    })]);
  } finally { clearTimeout(timer); }
}

async function waitFor(predicate) {
  const deadline = Date.now() + 1000;
  while (!predicate()) {
    assert.ok(Date.now() < deadline, 'The controlled upstream did not emit its pending cancellation text');
    await new Promise(resolve => setTimeout(resolve, 1));
  }
}

async function collect(response, onEvent = undefined) {
  assert.equal(response.status, 200);
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  const events = [];
  const received = [];
  const frames = [];
  let pending = '';
  let bytes = 0;
  while (true) {
    const chunk = await bounded(reader.read());
    if (chunk.done) break;
    bytes += chunk.value.byteLength;
    pending += decoder.decode(chunk.value, { stream: true });
    let boundary;
    while ((boundary = pending.indexOf('\n\n')) >= 0) {
      const frame = pending.slice(0, boundary);
      pending = pending.slice(boundary + 2);
      const data = frame.split('\n').find(line => line.startsWith('data: '));
      if (!data || data === 'data: [DONE]') continue;
      const event = JSON.parse(data.slice(6));
      events.push(event);
      received.push(performance.now());
      frames.push(Buffer.byteLength(frame + '\n\n'));
      onEvent?.(event);
    }
  }
  assert.equal(pending.trim(), '', 'A stream cannot finish with a truncated frame');
  return { events, received, frames, bytes };
}

function textEvents(observation) {
  return observation.events.map((event, index) => ({ event, at: observation.received[index], bytes: observation.frames[index] }))
    .filter(value => value.event.type === 'response.output_text.delta');
}

function quietTimes(observation, record, chat = false) {
  const text = chat
    ? observation.events.map((event, index) => ({ event, at: observation.received[index] }))
      .filter(value => value.event.choices?.some(choice => choice.delta?.content))
      .map(value => ({ text: value.event.choices.map(choice => choice.delta?.content ?? '').join(''), at: value.at }))
    : textEvents(observation).map(value => ({ text: value.event.delta, at: value.at }));
  const first = text.find(value => value.text.includes('FIRST'));
  const second = text.find(value => value.text.includes('SECOND'));
  assert.ok(first && second, 'Both quiet-gap fragments must be delivered');
  const firstEmission = record.emissions.find(value => value.text === 'FIRST');
  const secondEmission = record.emissions.find(value => value.text === 'SECOND');
  const thirdEmission = record.emissions.find(value => value.text === 'THIRD');
  assert.ok(firstEmission && secondEmission && thirdEmission);
  return { first_added_ms: first.at - firstEmission.at, second_added_ms: second.at - secondEmission.at,
    quiet_gap_ms: thirdEmission.at - secondEmission.at, second_before_next_token_ms: thirdEmission.at - second.at };
}

async function persistReceipt(context, receipt) {
  const directory = fileURLToPath(new URL('../../.cache/perf/v3/', import.meta.url));
  await mkdir(directory, { recursive: true });
  await writeFile(new URL('../../.cache/perf/v3/responses-coalescing-actual.json', import.meta.url), JSON.stringify(receipt, null, 2) + '\n');
  context.diagnostic?.(JSON.stringify({ event: 'response.coalescing_actual_fixture', ...receipt }));
}

export async function runResponsesCoalescingJourney(context) {
  const providers = context.coalescingProviders;
  assert.ok(providers, 'The outer Worker must route native fixture responses through coalescingProviders');
  const scenario = await createResponseJourneyFixture({ ...context, providers }, 'actual', [
    { kind: 'quiet' }, { kind: 'burst' }, { kind: 'burst' }, { kind: 'terminal' }, { kind: 'tool' },
    { kind: 'tool-final' }, { kind: 'cancel' }, { kind: 'chat' },
  ]);
  const client = new OpenAI({ apiKey: scenario.principal.apiKey, baseURL: origin + '/v1', maxRetries: 0,
    timeout: 8000, fetch: (input, init) => context.mf.dispatchFetch(input, init) });
  let head;
  const receipt = { fixture: 'actual compiled Rust Worker with native streamed provider fixture', turns: [] };
  async function responseTurn(input, options = {}) {
    const raw = await bounded(context.mf.dispatchFetch(origin + '/v1/responses', {
      method: 'POST', headers: { Authorization: `Bearer ${scenario.principal.apiKey}`, 'Content-Type': 'application/json' },
      body: JSON.stringify({ model: scenario.agent.id, input, ...(head ? { previous_response_id: head } : {}), stream: true, ...options }),
    }));
    const observation = await collect(raw);
    scenario.observations.events.push(...observation.events);
    const created = observation.events.find(event => event.type === 'response.created')?.response;
    const canonical = created ? await client.responses.retrieve(created.id) : null;
    if (canonical) scenario.observations.responses.push(canonical);
    receipt.turns.push({ index: receipt.turns.length + 1, raw_body_bytes: observation.bytes,
      raw_event_count: observation.events.length, text_delta_frames: textEvents(observation).length,
      received_text_bytes: Buffer.byteLength(textEvents(observation).map(value => value.event.delta).join('')),
      observed_terminal_types: observation.events.map(event => event.type)
        .filter(type => ['response.completed', 'response.failed', 'response.incomplete', 'error'].includes(type)),
      event_types: [...new Set(observation.events.map(event => event.type))],
      canonical_status: canonical?.status ?? null,
      canonical_text_bytes: canonical ? Buffer.byteLength(canonical.output_text) : null });
    // A bounded observer may detach under an old uncoalesced burst. Retain its
    // actual wire receipt and one canonical read before asserting completeness;
    // this is recovery observation, never a replay of the create request.
    await persistReceipt(context, receipt);
    const terminal = assertStreamEvents(observation.events);
    head = terminal.id;
    scenario.observations.responses.push(terminal);
    const replay = client.responses.stream({ response_id: head });
    const replayEvents = [];
    replay.on('event', event => replayEvents.push(event));
    const stored = await replay.finalResponse();
    const exactText = textEvents(observation).map(value => value.event.delta).join('');
    assertCompletedResponse(stored, exactText);
    assertStreamEvents(replayEvents, exactText);
    // The Node SDK decorates parsed text parts with parsed:null, which is not wire state.
    const wireOutput = value => JSON.parse(JSON.stringify(value, (key, field) => key === 'parsed' ? undefined : field));
    assert.deepEqual(wireOutput(stored.output), terminal.output, 'Real SDK replay must preserve every output item and text boundary');
    assert.equal(stored.id, terminal.id);
    assert.equal(canonical.output_text, exactText, 'Canonical retrieval must match every disclosed UTF-8 byte');
    return { ...observation, terminal, stored, sessionId: raw.headers.get('x-agents-session') };
  }
  try {
    const quiet = await responseTurn('Deliver three quiet-gap fragments.', { stream_options: { include_obfuscation: false } });
    assert.equal(quiet.stored.output_text, 'FIRSTSECONDTHIRD');
    const quietTiming = quietTimes(quiet, scenario.state.records[0]);
    receipt.quiet = quietTiming;
    await persistReceipt(context, receipt);
    const padded = await responseTurn('Deliver the deterministic burst with default padding.');
    const unpadded = await responseTurn('Deliver the same deterministic burst without padding.', { stream_options: { include_obfuscation: false } });
    assert.equal(padded.stored.output_text, burstText);
    assert.equal(unpadded.stored.output_text, burstText);
    const defaultDeltas = textEvents(padded);
    const plainDeltas = textEvents(unpadded);
    assert.ok(defaultDeltas.every(value => typeof value.event.obfuscation === 'string'
      && value.event.obfuscation.length >= 16 && value.event.obfuscation.length <= 32));
    assert.ok(plainDeltas.every(value => !('obfuscation' in value.event)));
    receipt.quiet = quietTiming;
    receipt.burst = { upstream_fragments: burstCount + 1, text_bytes: Buffer.byteLength(burstText),
        default_delta_frames: defaultDeltas.length, no_padding_delta_frames: plainDeltas.length,
        default_delta_frame_bytes: defaultDeltas.reduce((sum, value) => sum + value.bytes, 0),
        no_padding_delta_frame_bytes: plainDeltas.reduce((sum, value) => sum + value.bytes, 0),
      default_body_bytes: padded.bytes, no_padding_body_bytes: unpadded.bytes, exact_text_preserved: true };
    await persistReceipt(context, receipt);
    // Native transport jitter is measured, not confused with the pure buffer's
    // exact 50ms deadline. This still distinguishes a timer from waiting 3s for
    // the next upstream token. Frame counts independently reject no coalescing.
    assert.ok(quietTiming.first_added_ms >= 0 && quietTiming.first_added_ms < 250, 'First text must not wait for the batching timer');
    assert.ok(quietTiming.second_added_ms >= 0 && quietTiming.second_added_ms < 250, 'Quiet pending text must flush without waiting for a later upstream token');
    assert.ok(quietTiming.second_before_next_token_ms > 2500);
    assert.ok(Buffer.byteLength(burstText) > 64 * 1024, 'The ready-stream stress must exceed the old observer queue capacity');
    // Timer-driven flushes may split a batch if workerd is descheduled for 50ms.
    // Exact UTF-8 text and SDK terminal/replay remain hard gates above; count the
    // real wire frames and require material reduction without inventing a schedule.
    for (const deltas of [defaultDeltas, plainDeltas]) {
      assert.ok(deltas.length >= burstDeltaFrames && deltas.length <= 86,
        `Expected 73..86 actual text frames for the 73,729-byte ready stream, received ${deltas.length}`);
    }
    assert.ok(padded.bytes > unpadded.bytes, 'Disabling random padding must reduce transferred body bytes for identical text');

    const terminalTail = await responseTurn('Flush a short pending tail before the provider terminal event.');
    assert.equal(terminalTail.stored.output_text, 'TERMINAL_FIRSTTERMINAL_TAIL');
    assert.equal(textEvents(terminalTail).map(value => value.event.delta).join(''), 'TERMINAL_FIRSTTERMINAL_TAIL');
    const terminalRecord = scenario.state.records.at(-1);
    const terminalAddedMs = terminalTail.received.at(-1) - terminalRecord.completedAt;
    const tailReceived = textEvents(terminalTail).find(value => value.event.delta.includes('TERMINAL_TAIL'));
    assert.ok(tailReceived);
    const tailAddedMs = tailReceived.at - terminalRecord.completedAt;
    receipt.terminal = { publication_added_ms: terminalAddedMs, tail_added_ms: tailAddedMs, exact_tail_preserved: true };
    await persistReceipt(context, receipt);
    // The terminal object also waits for durable finalization; measure batching
    // against the text tail, not unrelated storage publication latency. A tail
    // may already have timer-flushed just before provider completion.
    assert.ok(tailAddedMs >= -50 && tailAddedMs < 250, 'A terminal boundary must drain pending text without waiting for a batching timer');

    const tool = await responseTurn('Use the clock and finish after flushing each text item.');
    assert.equal(tool.stored.output_text, 'TOOL_FIRSTTOOL_TAILTOOL_FINAL');
    const tailIndex = tool.events.findIndex(event => event.type === 'response.output_text.delta' && event.delta.includes('TOOL_TAIL'));
    const firstDone = tool.events.findIndex(event => event.type === 'response.output_text.done' && event.text === 'TOOL_FIRSTTOOL_TAIL');
    const finalIndex = tool.events.findIndex(event => event.type === 'response.output_text.delta' && event.delta.includes('TOOL_FINAL'));
    assert.ok(tailIndex >= 0 && firstDone > tailIndex && finalIndex > firstDone,
      'Buffered text must finish its first item before post-tool text starts');
    assert.ok(finalIndex < tool.events.length - 1, 'Final-item text must be flushed before the terminal event');
    assert.ok(!tool.events.some(event => event.type.startsWith('response.function_call_arguments.')
      || (event.type === 'response.output_item.added' && event.item.type === 'function_call')),
    'Built-in server tools must never leak as caller-local function handoffs');
    const source = await context.request('GET', `/v1/sessions/${tool.sessionId}`, undefined, 200, scenario.principal.apiKey);
    const clockCall = source.messages.flatMap(message => message.tool_calls ?? [])
      .find(call => call.id === 'coalescing-clock' && call.function.name === 'time');
    assert.ok(clockCall, 'The source journal must retain the actual built-in clock invocation');
    const clockResult = source.messages.find(message => message.role === 'tool' && message.tool_call_id === clockCall.id);
    assert.ok(clockResult && Number.isSafeInteger(JSON.parse(clockResult.content).unix_ms),
      'The source journal must retain the actual clock result before final model text');

    const accepted = await scenario.invoke({ previous_response_id: head, input: 'Run until cancellation.', background: true });
    head = accepted.value.id;
    let signalFirst;
    const firstVisible = new Promise(resolve => { signalFirst = resolve; });
    const observer = await bounded(context.mf.dispatchFetch(`${origin}/v1/responses/${head}?stream=true`, {
      headers: { Authorization: `Bearer ${scenario.principal.apiKey}` },
    }));
    const cancellationWire = collect(observer, event => {
      if (event.type === 'response.output_text.delta') signalFirst();
    });
    await bounded(firstVisible);
    await waitFor(() => scenario.state.records.at(-1).emissions.some(value => value.text === 'CANCEL_TAIL'));
    await context.request('POST', `/v1/responses/${head}/cancel`, {}, 200, scenario.principal.apiKey);
    for (const release of scenario.state.releases) release();
    const cancelled = await bounded(cancellationWire);
    scenario.observations.events.push(...cancelled.events);
    assert.equal(cancelled.events.at(-1).type, 'error');
    assert.equal(cancelled.events.at(-1).code, 'response_cancelled');
    assert.equal(textEvents(cancelled).map(value => value.event.delta).join(''), 'CANCEL_FIRSTCANCEL_TAIL',
      'Cancellation must flush text already delivered by the upstream stream before closing');
    const cancelledResponse = await scenario.read(head);
    assert.equal(cancelledResponse.status, 'cancelled');

    const chatSession = await context.request('POST', `/v1/agents/${scenario.agent.id}/sessions`, {}, 201, scenario.principal.apiKey);
    const chat = await bounded(context.mf.dispatchFetch(origin + '/v1/chat/completions', {
      method: 'POST', headers: { Authorization: `Bearer ${scenario.principal.apiKey}`, 'Content-Type': 'application/json',
        'X-Agents-Session': chatSession.id },
      body: JSON.stringify({ model: scenario.agent.id, messages: [{ role: 'user', content: 'Keep the legacy immediate streaming path.' }], stream: true }),
    }));
    const legacy = await collect(chat);
    assert.equal(legacy.events.map(event => event.choices?.map(choice => choice.delta?.content ?? '').join('') ?? '').join(''), 'FIRSTSECONDTHIRD');
    assert.equal(legacy.events.filter(event => event.choices?.some(choice => choice.delta?.content)).length, 3,
      'Legacy Chat Completions must not inherit Responses text batching');
    receipt.chat = quietTimes(legacy, scenario.state.records.at(-1), true);
    assert.ok(receipt.chat.first_added_ms < 250 && receipt.chat.second_added_ms < 250);
    receipt.barriers = { tool_text_flushed: true, terminal_text_flushed: true, cancellation_text_flushed: true };
    await persistReceipt(context, receipt);
    assert.equal(scenario.state.calls, 8, 'Wire measurement, SDK replay and cancellation cannot start extra inference');
  } catch (error) { scenario.recordFailure(error); throw error; }
  finally {
    for (const release of scenario.state.releases) release();
    await scenario.finish();
  }
}
