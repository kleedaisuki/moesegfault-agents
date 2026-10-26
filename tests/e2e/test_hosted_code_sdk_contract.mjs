import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import test from 'node:test';
import { codeRequest, main, summarizeCode, verifyCodeResourceLifetime } from './hosted-code-sdk.mjs';

const cid = `cntr_${'a'.repeat(32)}`;
const phases = ['response.code_interpreter_call.in_progress', 'response.code_interpreter_call_code.delta',
  'response.code_interpreter_call_code.done', 'response.code_interpreter_call.interpreting', 'response.code_interpreter_call.completed'];
const events = phases.map(type => ({ type, item_id: 'ci-owned' }));
const response = { status: 'completed', output: [{ id: 'ci-owned', type: 'code_interpreter_call', status: 'completed',
  container_id: cid, code: 'print(42)', outputs: [{ type: 'logs', logs: '42\ncgroup_memory_max=1073741824\n' }] }] };

test('new Code acceptance bounds one native capability to the explicitly selected real CF profile', () => {
  const body = codeRequest('owned-agent', 'code-1234-abcd.csv');
  assert.equal(body.max_tool_calls, 1);
  assert.deepEqual(body.tools, [{ type: 'code_interpreter', container: { type: 'auto', memory_limit: '1g' } }]);
  assert.equal(codeRequest('owned-agent', 'code-1234-abcd.csv', '4g').tools[0].container.memory_limit, '4g');
  assert.throws(() => codeRequest('owned-agent', 'code-1234-abcd.csv', '16g'));
  assert.deepEqual(body.include, ['code_interpreter_call.outputs']);
  assert.equal(body.reasoning.effort, 'none');
  assert.equal(body.max_output_tokens, 512);
  assert.match(body.input, /value\\n42\\n/);
  assert.throws(() => codeRequest('owned-agent', '../outside.csv'));
});

test('Code evidence requires real typed output, phases and CID while retaining only hashes', () => {
  const result = summarizeCode(response, events);
  assert.equal(result.containerId, cid);
  assert.equal(result.codeSha256, createHash('sha256').update('print(42)').digest('hex'));
  assert.equal(Object.hasOwn(result, 'code'), false);
  assert.equal(Object.hasOwn(result, 'logs'), false);
  assert.equal(result.actualCgroupMemoryBytes, 1073741824);
  assert.throws(() => summarizeCode(response, events, '4g'), /actual_cgroup_memory_cap_proof_required/);
  const fourGiB = { ...response, output: [{ ...response.output[0], outputs: [{ type: 'logs', logs: '42\ncgroup_memory_max=4294967296\n' }] }] };
  assert.equal(summarizeCode(fourGiB, events, '4g').actualCgroupMemoryBytes, 4294967296);
  for (const output of [[], [...response.output, ...response.output], [{ ...response.output[0], status: 'failed' }],
    [{ ...response.output[0], container_id: 'cntr_fake' }], [{ ...response.output[0], outputs: [] }]]) {
    assert.throws(() => summarizeCode({ ...response, output }, events));
  }
  assert.throws(() => summarizeCode(response, events.slice(1)));
  assert.throws(() => summarizeCode(response, [...events].reverse()));
});

test('live Code entrypoint rejects unbounded invocation before any network or key use', async () => {
  await assert.rejects(main([], {}), /exact_deployed_assembly_source_required/);
  await assert.rejects(main(['--source', 'a'.repeat(40)], {}), /one_new_model_submission_must_be_explicit/);
  await assert.rejects(main(['--source', 'a'.repeat(40), '--allow-model-call'], {}), /owned_platform_key_missing/);
});

test('Code lifecycle acceptance closes only the harness before independent CID release and retained File read', async () => {
  const order = [];
  let sessionClosed = false, containerDeleted = false;
  const client = { containers: {
    retrieve: async id => {
      order.push('container:get'); assert.equal(sessionClosed, true); assert.equal(id, cid);
      if (containerDeleted) throw Object.assign(Error('not_found'), { status: 404 });
      return { id, status: 'running' };
    },
    delete: async id => { order.push('container:delete'); assert.equal(id, cid); containerDeleted = true; },
  }, files: { content: async id => {
    order.push('file:read'); assert.equal(id, 'file-owned'); assert.equal(containerDeleted, true);
    return new Response('value\n42\n');
  } } };
  const request = async method => {
    order.push(`session:${method}`);
    if (method === 'DELETE') sessionClosed = true;
    return { status: sessionClosed ? 'closed' : 'idle', run_id: null };
  };
  const receipt = { resources: { sessions: ['session-owned'] }, releasedContainers: [],
    checks: { code: { containerId: cid }, artifact: { fileId: 'file-owned' } } };
  await verifyCodeResourceLifetime(client, request, receipt, () => {});
  assert.deepEqual(order, ['session:GET', 'session:DELETE', 'session:GET', 'container:get', 'container:delete', 'container:get', 'file:read']);
  assert.deepEqual(receipt.releasedContainers, [cid]);
  assert.equal(receipt.checks.artifactSurvivesComputeRelease, true);
});
