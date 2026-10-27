import assert from 'node:assert/strict';
import OpenAI from 'openai';
import { createResponseJourneyFixture } from './responses-journey.mjs';

const localTool = {
  type: 'function',
  name: 'policy_local_echo',
  strict: true,
  parameters: {
    type: 'object', properties: { value: { type: 'string' } },
    required: ['value'], additionalProperties: false,
  },
};
const resultSchema = {
  type: 'object', properties: { ok: { type: 'boolean' } },
  required: ['ok'], additionalProperties: false,
};
const resultFormat = { type: 'json_schema', name: 'policy_result', schema: resultSchema, strict: true };

function assertFailed(response, code) {
  assert.equal(response.object, 'response');
  assert.equal(response.status, 'failed', 'Provider policy violations cannot become successful completions');
  assert.equal(response.error?.code, 'server_error');
  assert.equal(response.agents_error_code, code);
  assert.equal(response.incomplete_details, null);
  assert.ok(!response.output.some(item => item.type === 'function_call'),
    'Invalid or unrequested calls must never be handed off for client execution');
}

async function assertSettledModelRounds(context, scenario, response, rounds) {
  assert.deepEqual(response.usage, {
    input_tokens: 150 * rounds, input_tokens_details: { cached_tokens: 30 * rounds },
    output_tokens: 10 * rounds, output_tokens_details: { reasoning_tokens: 0 }, total_tokens: 160 * rounds,
  }, 'A policy rejection must retain every known provider-reported round, including the rejected one');
  const rows = await context.db.prepare(`
    SELECT r.idempotency_key,r.state,CAST(l.quantity AS TEXT) AS quantity,l.unit,
      CAST(l.amount_microusd AS TEXT) AS amount,l.price_version
    FROM usage_ledger l JOIN usage_reservations r ON r.id=l.reservation_id
    WHERE r.account_id=?1 AND substr(r.idempotency_key,1,length(?2))=?2 AND l.resource='model'
    ORDER BY r.idempotency_key
  `).bind(scenario.principal.owner, `completion:${response.id}:round:`).all();
  assert.deepEqual(rows.results, Array.from({ length: rounds }, (_, index) => ({
    idempotency_key: `completion:${response.id}:round:${index}`,
    state: 'settled', quantity: '160', unit: 'tokens', amount: '0', price_version: 'byok',
  })), 'Observed inference must settle before malformed output is rejected; no replay or unbilled known round');
}

async function streamResponse(context, scenario, body) {
  const client = new OpenAI({
    apiKey: scenario.principal.apiKey, baseURL: 'https://agents.test/v1', maxRetries: 0, timeout: 8000,
    fetch: (input, init) => context.mf.dispatchFetch(input, init),
  });
  const { data: stream, response: raw } = await client.responses.create({
    model: scenario.agent.id, ...body, stream: true,
  }).withResponse();
  const sessionId = raw.headers.get('x-agents-session');
  assert.ok(sessionId);
  const events = [];
  for await (const event of stream) events.push(event);
  scenario.observations.events.push(...events);
  const terminal = events.filter(event => ['response.completed', 'response.failed', 'response.incomplete'].includes(event.type));
  assert.equal(terminal.length, 1, 'A policy decision must produce exactly one durable terminal event');
  assert.equal(events.at(-1), terminal[0]);
  assert.equal(events[0]?.type, 'response.created');
  for (let index = 1; index < events.length; index++) {
    assert.ok(events[index].sequence_number > events[index - 1].sequence_number);
  }
  const value = terminal[0].response;
  scenario.observations.responses.push(value);
  assert.deepEqual(await scenario.read(value.id), value, 'The original failed or completed response must be durably recoverable');
  return { value, events, sessionId };
}

async function runLocalPolicyFixture(context) {
  const scenario = await createResponseJourneyFixture(context, 'local-policy', [
    { text: 'The provider ignored the requested local function.', check(input) {
      assert.deepEqual(input.tool_choice, { type: 'function', function: { name: localTool.name } });
      assert.ok(input.tools.some(tool => tool.function.name === localTool.name));
    } },
    { calls: [{ id: 'invalid-local-arguments', name: localTool.name, arguments: { value: 42 } }], check(input) {
      const tool = input.tools.find(tool => tool.function.name === localTool.name);
      assert.equal(tool?.function.strict, true);
      assert.deepEqual(tool.function.parameters, localTool.parameters);
    } },
  ]);
  try {
    const forced = await scenario.invoke({
      input: 'Use the explicitly selected client function.', tools: [localTool],
      tool_choice: { type: 'function', name: localTool.name },
    });
    assertFailed(forced.value, 'provider_tool_choice_policy_violation');
    assert.deepEqual(await scenario.read(forced.value.id), forced.value);
    await assertSettledModelRounds(context, scenario, forced.value, 1);

    const invalid = await streamResponse(context, scenario, {
      input: 'Use the declared client function with a schema-valid string.', tools: [localTool],
    });
    assertFailed(invalid.value, 'invalid_function_arguments');
    assert.ok(!invalid.events.some(event => event.type.startsWith('response.function_call_arguments.')),
      'Invalid strict arguments cannot reach client function-call events');
    const journal = await context.request('GET', `/v1/sessions/${invalid.sessionId}`, undefined, 200, scenario.principal.apiKey);
    assert.ok(journal.messages.some(message => message.tool_calls?.some(call => call.id === 'invalid-local-arguments')),
      'The private source journal retains the provider result for diagnostics without publishing a callable output');
    await assertSettledModelRounds(context, scenario, invalid.value, 1);
    assert.equal(scenario.state.calls, 2, 'Neither policy rejection may retry accepted inference');
    context.diagnostic?.(JSON.stringify({ event: 'response.policy_fixture', scenario: 'local-policy',
      forced_selection_rejected: true, invalid_strict_arguments_rejected: true, settled_model_rounds: 2 }));
  } catch (error) { scenario.recordFailure(error); throw error; }
  finally { await scenario.finish(); }
}

function checkStructuredRequest(input, callId) {
  assert.deepEqual(input.response_format, { type: 'json_schema', json_schema: {
    name: resultFormat.name, schema: resultSchema, strict: true,
  } });
  if (callId) {
    assert.equal(input.tool_choice, 'auto', 'A forced initial built-in call must permit a final answer after its result');
    const result = input.messages.find(message => message.role === 'tool' && message.tool_call_id === callId);
    assert.ok(result, 'The next model round must consume the actual built-in tool result');
    const clock = JSON.parse(result.content);
    assert.ok(Number.isSafeInteger(clock.unix_ms));
    assert.ok(Math.abs(clock.unix_ms - Date.now()) < 10000);
  } else {
    assert.deepEqual(input.tool_choice, { type: 'function', function: { name: 'time' } });
  }
}

async function runStructuredPolicyFixture(context) {
  const rejectedRationale = 'Intermediate non-JSON rationale for the rejected result.';
  const acceptedRationale = 'Intermediate non-JSON rationale for the accepted result.';
  const invalidFinal = '{"ok":"not-a-boolean"}';
  const scenario = await createResponseJourneyFixture(context, 'structured-policy', [
    { text: rejectedRationale, calls: [{ id: 'structured-time-invalid', name: 'time' }],
      check(input) { checkStructuredRequest(input); } },
    { text: invalidFinal, check(input) { checkStructuredRequest(input, 'structured-time-invalid'); } },
    { text: acceptedRationale, calls: [{ id: 'structured-time-valid', name: 'time' }],
      check(input) { checkStructuredRequest(input); } },
    { text: '{"ok":true}', check(input) { checkStructuredRequest(input, 'structured-time-valid'); } },
  ]);
  try {
    const body = { input: 'Call time once, then return only the requested JSON result.',
      text: { format: resultFormat }, tool_choice: { type: 'function', name: 'time' } };
    const rejected = await streamResponse(context, scenario, body);
    assertFailed(rejected.value, 'invalid_structured_output');
    assert.deepEqual(rejected.value.output.filter(item => item.type === 'message'), [],
      'Schema-invalid final data must not be accepted as a structured output message');
    assert.ok(!rejected.events.some(event => event.type === 'response.output_text.delta'),
      'Neither intermediate rationale nor a schema-invalid final may be disclosed as structured text deltas');
    const rejectedSource = await context.request('GET', `/v1/sessions/${rejected.sessionId}`, undefined, 200, scenario.principal.apiKey);
    assert.ok(rejectedSource.messages.some(message => message.content === rejectedRationale));
    assert.ok(rejectedSource.messages.some(message => message.content === invalidFinal));
    assert.equal(rejectedSource.messages.filter(message => message.role === 'tool' && message.tool_call_id === 'structured-time-invalid').length, 1);
    await assertSettledModelRounds(context, scenario, rejected.value, 2);

    const accepted = await streamResponse(context, scenario, body);
    assert.equal(accepted.value.status, 'completed');
    assert.equal(accepted.value.error, null);
    const text = accepted.events.filter(event => event.type === 'response.output_text.delta').map(event => event.delta).join('');
    assert.deepEqual(JSON.parse(text), { ok: true }, 'Successful structured text must remain directly JSON.parse-able after a real tool loop');
    const committedText = accepted.value.output.filter(item => item.type === 'message').flatMap(item => item.content)
      .filter(part => part.type === 'output_text').map(part => part.text).join('');
    assert.equal(committedText, text);
    assert.ok(!JSON.stringify(accepted.events).includes(acceptedRationale), 'Intermediate tool rationale cannot contaminate structured output');
    const acceptedSource = await context.request('GET', `/v1/sessions/${accepted.sessionId}`, undefined, 200, scenario.principal.apiKey);
    assert.ok(acceptedSource.messages.some(message => message.content === acceptedRationale));
    assert.equal(acceptedSource.messages.filter(message => message.role === 'tool' && message.tool_call_id === 'structured-time-valid').length, 1);
    await assertSettledModelRounds(context, scenario, accepted.value, 2);
    assert.equal(scenario.state.calls, 4, 'Structured validation must not replay a tool or model round');
    context.diagnostic?.(JSON.stringify({ event: 'response.policy_fixture', scenario: 'structured-policy',
      invalid_final_rejected: true, valid_final_parseable: true, settled_model_rounds: 4 }));
  } catch (error) { scenario.recordFailure(error); throw error; }
  finally { await scenario.finish(); }
}

export async function runResponsesPolicyJourney(context) {
  await runLocalPolicyFixture(context);
  await runStructuredPolicyFixture(context);
}
