import assert from 'node:assert/strict';
import { createResponseJourneyFixture } from './responses-journey.mjs';

// This runs against the real Rust Worker/DO using its encrypted BYOK route.
// The provider is intentionally disobedient after the server allowance expires.
export async function runResponsesToolBudgetJourney(context) {
  const clientTool = { type: 'function', name: 'caller_local', parameters: { type: 'object' } };
  const scenario = await createResponseJourneyFixture(context, 'tool-budget', [
    { calls: [{ id: 'first', name: 'time' }, { id: 'overflow', name: 'time' }] },
    { calls: [{ id: 'disobedient', name: 'time' }], check(input) {
      assert.ok(!input.tools.some(tool => tool.function?.name === 'time'));
      assert.ok(input.tools.some(tool => tool.function?.name === 'caller_local'));
      const overflow = input.messages.find(message => message.tool_call_id === 'overflow');
      assert.equal(JSON.parse(overflow.content).error.code, 'max_tool_calls_exceeded');
    } },
    { text: 'Completed without additional server effects.', check(input) {
      assert.ok(!input.tools.some(tool => tool.function?.name === 'time'));
      const denied = input.messages.find(message => message.tool_call_id === 'disobedient');
      assert.equal(JSON.parse(denied.content).error.code, 'max_tool_calls_exceeded');
    } },
    { calls: [{ id: 'first', name: 'time' }], check(input) {
      assert.ok(input.tools.some(tool => tool.function?.name === 'time'),
        'A new Response gets its own allowance, not the previous Response counter');
    } },
    { text: 'The next Response has one fresh server invocation.' },
  ]);
  try {
    const first = await scenario.invoke({
      input: 'Call the clock more than once.', max_tool_calls: 1, tools: [clientTool],
    });
    assert.equal(first.value.status, 'completed');
    assert.equal(first.value.incomplete_details, null);
    assert.equal(scenario.state.calls, 3);
    const sessionId = first.headers.get('x-agents-session');
    const session = await context.request('GET', `/v1/sessions/${sessionId}`, undefined, 200,
      scenario.principal.apiKey);
    const outputs = session.messages.filter(message => message.role === 'tool');
    assert.equal(outputs.length, 3);
    assert.ok(!JSON.parse(outputs.find(message => message.tool_call_id === 'first').content).error);
    for (const id of ['overflow', 'disobedient']) {
      assert.equal(JSON.parse(outputs.find(message => message.tool_call_id === id).content).error.code,
        'max_tool_calls_exceeded');
    }
    await context.mf.unsafeEvictDurableObject('', 'Session', {
      name: `${scenario.principal.owner}:${sessionId}`,
    });
    assert.deepEqual(await scenario.read(first.value.id), first.value);
    assert.equal(scenario.state.calls, 3, 'Retrieval does not spend another tool or inference slot');
    const second = await scenario.invoke({
      previous_response_id: first.value.id, input: 'One fresh clock call.',
      max_tool_calls: 1, tools: [clientTool],
    });
    assert.equal(second.value.status, 'completed');
    assert.equal(scenario.state.calls, 5);
  } catch (error) {
    scenario.recordFailure(error);
    throw error;
  } finally {
    await scenario.finish();
  }
}
