import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { createHash, randomUUID } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';
import OpenAI from 'openai';
import { archiveCompletedSession, assertCanonicalEvents, assertExecutionWorkspace, assertServingWorkerHealth, nativeAcceptanceModel, requireLiveDeploymentProvenance, tracedFetch, verifyWorkerIdentity } from './full-responses-sdk.mjs';
import * as policy from './paid-route-policy.mjs';

const origin = 'https://agents-staging.moesegfault.dev';
const expectedArtifact = Buffer.from('value\n42\n');
const hash = value => createHash('sha256').update(value).digest('hex');
const digest = value => hash(JSON.stringify(value));

export function codeRequest(agent, filename, memory = '1g') {
  assert.match(filename, /^code-[a-f0-9-]+\.csv$/);
  assert.ok(['1g', '4g'].includes(memory), 'actual_CF_memory_profile_required');
  return { model: agent, input: `Use Code Interpreter exactly once. With Python pathlib, write /mnt/data/${filename} with precisely the nine bytes b'value\\n42\\n', then print(42). In that same Python cell print('cgroup_memory_max=' + Path('/sys/fs/cgroup/memory.max').read_text().strip()). Do not use the network. After execution, reply briefly with the artifact link.`,
    tools: [{ type: 'code_interpreter', container: { type: 'auto', memory_limit: memory } }],
    include: ['code_interpreter_call.outputs'], max_tool_calls: 1,
    max_output_tokens: 512, reasoning: { effort: 'none' } };
}

export function summarizeCode(response, events, memory = '1g') {
  assert.equal(response.status, 'completed', 'hosted_code_response_not_completed');
  const calls = response.output.filter(item => item.type === 'code_interpreter_call');
  assert.equal(calls.length, 1, 'exactly_one_real_hosted_code_call_required');
  const item = calls[0];
  assert.equal(item.status, 'completed', 'hosted_code_call_not_completed');
  assert.match(item.container_id, /^cntr_[a-f0-9]{32}$/);
  assert.ok(typeof item.code === 'string' && item.code.length > 0, 'real_model_code_missing');
  const logs = item.outputs.filter(output => output.type === 'logs').map(output => output.logs).join('\n');
  assert.match(logs, /(?:^|\n)42\s*(?:\n|$)/, 'real_python_print_result_missing');
  const expectedMemoryBytes = { '1g': 1073741824, '4g': 4294967296 }[memory];
  assert.ok(expectedMemoryBytes, 'actual_CF_memory_profile_required');
  assert.ok(logs.split('\n').some(line => line.trim() === `cgroup_memory_max=${expectedMemoryBytes}`), 'actual_cgroup_memory_cap_proof_required');
  const phases = events.filter(event => event.item_id === item.id).map(event => event.type);
  const expected = ['response.code_interpreter_call.in_progress', 'response.code_interpreter_call_code.delta',
    'response.code_interpreter_call_code.done', 'response.code_interpreter_call.interpreting', 'response.code_interpreter_call.completed'];
  let prior = -1;
  for (const phase of expected) {
    const index = phases.indexOf(phase);
    assert.ok(index > prior, `hosted_code_phase_missing_or_unordered:${phase}`);
    prior = index;
  }
  return { itemId: item.id, containerId: item.container_id, codeSha256: hash(item.code), logsSha256: hash(logs), actualCgroupMemoryBytes: expectedMemoryBytes, phases };
}

function options(argv, environment, root) {
  const { values } = parseArgs({ args: argv, options: { source: { type: 'string' }, output: { type: 'string' },
    'current-python': { type: 'string' }, memory: { type: 'string', default: '1g' }, 'allow-model-call': { type: 'boolean', default: false } }, strict: true });
  assert.match(values.source ?? '', /^[a-f0-9]{40}$/, 'exact_deployed_assembly_source_required');
  assert.equal(values['allow-model-call'], true, 'one_new_model_submission_must_be_explicit');
  assert.ok(['1g', '4g'].includes(values.memory), 'actual_CF_memory_profile_required');
  assert.ok(environment.MAGENTS_TEST_API_KEY, 'owned_platform_key_missing');
  assert.match(environment.MAGENTS_TEST_TRACE_SEED ?? '', /^[1-9][0-9]*:[1-9][0-9]*$/);
  const output = path.resolve(values.output ?? '.temp/stage-delivery/hosted-code-receipt.json');
  assert.ok(output.startsWith(path.join(root, '.temp') + path.sep), 'receipt_must_stay_in_project_temp');
  assert.equal(fs.existsSync(output), false, 'existing_intent_requires_reconciliation_not_replay');
  const python = path.resolve(values['current-python'] ?? '.cache/current-sdk-venv/Scripts/python.exe');
  assert.ok(python.startsWith(path.join(root, '.cache') + path.sep) && fs.existsSync(python), 'project_current_python_required');
  return { source: values.source, output, python, memory: values.memory };
}

async function verifyArtifact(client, filename) {
  const matches = [];
  for await (const file of client.files.list({ limit: 100 })) {
    if (file.filename === filename) matches.push(file);
  }
  assert.equal(matches.length, 1, 'one_new_published_artifact_required');
  const file = matches[0];
  const bytes = Buffer.from(await (await client.files.content(file.id)).arrayBuffer());
  assert.equal(bytes.length, expectedArtifact.length, 'published_artifact_byte_count');
  assert.equal(hash(bytes), hash(expectedArtifact), 'published_artifact_digest');
  return { fileId: file.id, bytes: bytes.length, sha256: hash(bytes) };
}

export async function countCodeBeforeAdmission(client, request, agent, body) {
  const route = `/v1/agents/${agent}/sessions`;
  const before = await request('GET', route);
  assert.deepEqual(before.data, [], 'new_Code_agent_must_not_have_a_preselected_profile');
  const count = await client.responses.inputTokens.count(body);
  assert.equal(count.object, 'response.input_tokens');
  assert.ok(Number.isSafeInteger(count.input_tokens) && count.input_tokens > 0);
  assert.deepEqual(await request('GET', route), before, 'count_must_not_allocate_or_pin_a_Session');
  return { inputTokens: count.input_tokens, noSessionAdmitted: true, noPhysicalProfilePreselected: true };
}

export async function verifyCodeResourceLifetime(client, request, receipt, save) {
  const cid = receipt.checks.code.containerId;
  await archiveCompletedSession(request, receipt.resources.sessions[0]);
  const retainedContainer = await client.containers.retrieve(cid);
  assert.equal(retainedContainer.id, cid); assert.equal(retainedContainer.status, 'running');
  receipt.checks.harnessArchivePreservesOwnedCode = { sessionClosed: true, containerStillRunning: true }; save();
  await client.containers.delete(cid);
  await assert.rejects(client.containers.retrieve(cid), error => error.status === 404);
  receipt.releasedContainers.push(cid); receipt.checks.explicitContainerRelease = true; save();
  const retained = Buffer.from(await (await client.files.content(receipt.checks.artifact.fileId)).arrayBuffer());
  assert.equal(hash(retained), hash(expectedArtifact));
  receipt.checks.artifactSurvivesComputeRelease = true;
}

async function cleanup(client, request, receipt, save) {
  const failures = [];
  if (receipt.pending) {
    receipt.cleanup = { failures: [{ kind: 'ambiguous_response_requires_original_id_reconciliation' }], automaticReplay: false };
    save();
    return;
  }
  for (const cid of receipt.resources.containers.filter(cid => !receipt.releasedContainers.includes(cid))) {
    try {
      await client.containers.delete(cid);
      await assert.rejects(client.containers.retrieve(cid), error => error.status === 404);
      receipt.releasedContainers.push(cid); save();
    } catch (error) { failures.push({ kind: 'container_release', id: cid, status: error.status ?? null }); }
  }
  for (const session of receipt.resources.sessions) {
    try { await archiveCompletedSession(request, session); }
    catch (error) { failures.push({ kind: 'session_archive', id: session, status: error.status ?? null }); }
  }
  if (!failures.length && receipt.resources.agent) {
    try { await request('DELETE', `/v1/agents/${receipt.resources.agent}`, undefined, [200, 204, 404]); }
    catch (error) { failures.push({ kind: 'agent_delete', status: error.status ?? null }); }
  }
  receipt.cleanup = { failures, containersReleased: receipt.releasedContainers, canonicalResponsesAndArtifactsRetained: true };
  save();
}

export async function main(argv = process.argv.slice(2), environment = process.env) {
  const root = path.resolve('.'), config = options(argv, environment, root);
  assertExecutionWorkspace(root, config.source, environment);
  const provenance = requireLiveDeploymentProvenance(environment, config.source, root);
  const manifest = JSON.parse(fs.readFileSync(path.resolve(root, provenance.receipt), 'utf8'));
  assert.notEqual(manifest.artifacts.container.status, 'core_compatible_disabled_runtime', 'actual_code_daemon_artifact_required');
  verifyWorkerIdentity(provenance, root, policy.verifyServingSource);
  const transport = tracedFetch(fetch, environment.MAGENTS_TEST_TRACE_SEED);
  const request = async (method, route, body, expected = [200]) => {
    const response = await transport(origin + route, { method, headers: { Authorization: `Bearer ${environment.MAGENTS_TEST_API_KEY}`,
      ...(body === undefined ? {} : { 'Content-Type': 'application/json' }) },
    ...(body === undefined ? {} : { body: JSON.stringify(body) }), signal: AbortSignal.timeout(30000) });
    assert.ok(expected.includes(response.status), `platform_status:${method}:${response.status}`);
    const bytes = await response.text();
    return bytes ? JSON.parse(bytes) : null;
  };
  const client = new OpenAI({ apiKey: environment.MAGENTS_TEST_API_KEY, baseURL: origin + '/v1',
    maxRetries: 0, timeout: 300000, fetch: transport });
  const receipt = { execution: 'new_live_hosted_code_increment', source: config.source, provenance,
    scriptSha256: hash(fs.readFileSync(fileURLToPath(import.meta.url))), traceSeed: environment.MAGENTS_TEST_TRACE_SEED,
    startedAt: new Date().toISOString(), phase: 'health', run: randomUUID(), submissions: 0, pending: null,
    resources: { agent: null, sessions: [], responses: [], containers: [], files: [] }, releasedContainers: [], checks: {}, cleanup: {},
    status: 'running', accounting: 'session_and_run_scoped_postflight_required',
    memory: config.memory, notSelected: ['natural_idle_clock_wait', 'accepted_POST_replay', 'old_core_eight_calls'] };
  fs.mkdirSync(path.dirname(config.output), { recursive: true });
  fs.writeFileSync(config.output, JSON.stringify(receipt, null, 2), { flag: 'wx' });
  const save = () => fs.writeFileSync(config.output, JSON.stringify(receipt, null, 2));
  try {
    assertServingWorkerHealth(await request('GET', '/healthz'), provenance);
    const approved = policy.assertAvailableDirectDeepSeek(await request('GET', '/v1/models'));
    receipt.phase = 'agent_create_intent'; save();
    const agent = await request('POST', '/v1/agents', { name: `hosted-code-${receipt.run}`, model: { ...nativeAcceptanceModel(approved), context_window: 8192, max_output_tokens: 512 },
      instructions: 'Use the declared Code Interpreter for the requested computation. Never claim execution without a tool result.',
      workspace: false, tools: [], skills: [], compaction_percent: 80 }, [201]);
    receipt.resources.agent = agent.id; save();
    policy.assertDirectDeepSeekModel(agent.model);
    policy.assertDirectDeepSeekBounds(agent.model, approved);
    const filename = `code-${receipt.run}.csv`, body = codeRequest(agent.id, filename, config.memory);
    receipt.phase = 'hosted_admission_without_model'; save();
    receipt.checks.prepaidHostedAdmission = await countCodeBeforeAdmission(client, request, agent.id, body);
    receipt.phase = 'single_response_intent'; receipt.submissions = 1;
    receipt.pending = { idempotencyKey: `hosted-code-${receipt.run}`, bodySha256: digest(body), session: null }; save();
    const streamed = await client.responses.create({ ...body, stream: true }, { headers: {
      'Idempotency-Key': receipt.pending.idempotencyKey } }).withResponse();
    const responseId = streamed.response.headers.get('x-agents-response');
    if (responseId) receipt.resources.responses.push(responseId);
    const sessionId = streamed.response.headers.get('x-agents-session');
    assert.ok(typeof sessionId === 'string' && sessionId.length > 0, 'actual_profile_Session_header_required');
    receipt.resources.sessions.push(sessionId); receipt.pending.session = sessionId; save();
    const events = [];
    for await (const event of streamed.data) {
      events.push(event);
      if (event.type === 'response.created' && !receipt.resources.responses.includes(event.response.id)) receipt.resources.responses.push(event.response.id);
      if (event.item?.type === 'code_interpreter_call' && /^cntr_[a-f0-9]{32}$/.test(event.item.container_id ?? '') && !receipt.resources.containers.includes(event.item.container_id)) receipt.resources.containers.push(event.item.container_id);
      if (['response.completed', 'response.failed', 'response.incomplete'].includes(event.type) && receipt.resources.responses.includes(event.response?.id)) receipt.pending = null;
      save();
    }
    const response = assertCanonicalEvents(events);
    receipt.pending = null; receipt.phase = 'canonical_terminal'; save();
    assert.equal(response.id, responseId, 'canonical_response_header_identity');
    receipt.checks.code = summarizeCode(response, events, config.memory);
    const cid = receipt.checks.code.containerId;
    if (!receipt.resources.containers.includes(cid)) receipt.resources.containers.push(cid);
    receipt.checks.response = { id: response.id, sha256: digest(response), events: events.length,
      inputTokens: response.usage?.input_tokens ?? null, outputTokens: response.usage?.output_tokens ?? null };
    policy.assertDirectDeepSeekPin(await request('GET', `/v1/sessions/${sessionId}`), agent.id);
    const container = await client.containers.retrieve(cid);
    assert.equal(container.id, cid); assert.equal(container.status, 'running'); assert.equal(container.memory_limit, config.memory);
    assert.deepEqual(container.expires_after, { anchor: 'last_active_at', minutes: 20 });
    receipt.checks.container = { id: cid, status: container.status, memory: container.memory_limit, expiresAfter: container.expires_after };
    receipt.checks.artifact = await verifyArtifact(client, filename);
    receipt.resources.files.push(receipt.checks.artifact.fileId); save();
    const typed = JSON.parse(execFileSync(config.python, [path.join(root, 'tests/e2e/current_responses_sdk.py'), 'response-wire', '--input', '-'], {
      cwd: root, input: JSON.stringify({ response, events }), encoding: 'utf8', maxBuffer: 1024 * 1024,
      timeout: 30000, windowsHide: true, stdio: ['pipe', 'pipe', 'pipe'] }));
    assert.equal(typed.passed, true, 'actual_code_wire_current_sdk_failed');
    receipt.checks.currentPythonWire = { sdk: typed.sdk, actualPayloads: true, newModelRuns: 0 }; save();
    await verifyCodeResourceLifetime(client, request, receipt, save);
    receipt.status = 'live_code_behavior_passed_financial_postflight_pending'; save();
  } catch (error) {
    receipt.status = 'failed'; receipt.failure = { name: error.name, status: error.status ?? null }; save();
    throw error;
  } finally {
    await cleanup(client, request, receipt, save);
    receipt.endedAt = new Date().toISOString(); save();
    if (receipt.cleanup.failures.length) process.exitCode = 1;
  }
  process.stdout.write(JSON.stringify({ status: receipt.status, submissions: receipt.submissions, receipt: config.output }) + '\n');
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main().catch(error => { process.stderr.write(JSON.stringify({ result: 'failed', name: error.name, status: error.status ?? null }) + '\n'); process.exitCode = 1; });
}
