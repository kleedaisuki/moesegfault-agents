import assert from 'node:assert/strict';
import test from 'node:test';
import { main, programmaticRequest, programSource, summarizeProgrammatic } from './programmatic-sdk.mjs';

test('actual program operator bounds one forced program and two nested time calls', () => {
  const body = programmaticRequest('owned');
  assert.deepEqual(body.tools, [{ type: 'programmatic_tool_calling' }]);
  assert.deepEqual(body.tool_choice, { type: 'programmatic_tool_calling' });
  assert.equal(body.max_tool_calls, 3);
  assert.ok(body.input.includes(programSource));
  assert.equal(body.max_output_tokens, 512);
});

test('actual program evidence requires causal results and honest separate financial closure', () => {
  const caller = { type: 'program', caller_id: 'pc' };
  const response = { status: 'completed', output: [{ type: 'program', id: 'p', call_id: 'pc', code: programSource, fingerprint: 'opaque' },
    { type: 'program_output', id: 'po', call_id: 'pc', status: 'completed', result: JSON.stringify({ a: { utc: 'a', unix_ms: 1 }, b: { utc: 'b', unix_ms: 2 } }) },
    ...[0, 1].flatMap(i => [{ type: 'function_call', name: 'time', call_id: 'c' + i, arguments: '{}', caller },
      { type: 'function_call_output', call_id: 'c' + i, output: JSON.stringify({ utc: i ? 'b' : 'a', unix_ms: i + 1 }), caller }]),
    { type: 'message', content: [] }] };
  assert.equal(summarizeProgrammatic(response).completedTimeCalls, 2);
  assert.deepEqual(summarizeProgrammatic(response).expectedDynamicAllocationMicroUSD, { creation: 2000, request: 1, cpuSlot: 2 });
  assert.throws(() => summarizeProgrammatic({ ...response, output: response.output.slice(1) }), /exactly_one_program/);
  const forged = structuredClone(response); forged.output[2].caller.caller_id = 'forged';
  assert.throws(() => summarizeProgrammatic(forged));
});

test('actual program operator rejects missing paid intent before any network', async () => {
  await assert.rejects(main([], {}), /exact_program_assembly_source_required/);
  await assert.rejects(main(['--source', 'a'.repeat(40)], {}), /single_new_paid_program_gate_required/);
  await assert.rejects(main(['--source', 'a'.repeat(40), '--allow-paid-program'], {}), /owned_platform_key_missing/);
});
