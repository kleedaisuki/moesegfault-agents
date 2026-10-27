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
import { seedreamProfile } from '../../scripts/deploy-config.mjs';

const origin = 'https://agents-staging.moesegfault.dev';
const hash = value => createHash('sha256').update(value).digest('hex');
const digest = value => hash(JSON.stringify(value));

export function imageRequest(agent) {
  return { model: agent, input: 'Use image_generation exactly once to draw a tiny turquoise robot watering a sunflower, flat illustration, pale lavender background, no text. Then answer in one short sentence. Do not use any other tool.',
    tools: [{ type: 'image_generation', size: '1024x1024' }], max_tool_calls: 1, max_output_tokens: 256, reasoning: { effort: 'none' } };
}

export function summarizeImage(response, events) {
  assert.equal(response.status, 'completed', 'actual_image_response_not_completed');
  const calls = response.output.filter(item => item.type === 'image_generation_call');
  assert.equal(calls.length, 1, 'exactly_one_real_image_call_required');
  const item = calls[0];
  assert.equal(item.status, 'completed', 'image_service_not_completed');
  assert.ok(typeof item.result === 'string' && item.result.length > 64, 'native_base64_image_missing');
  const bytes = Buffer.from(item.result, 'base64');
  assert.equal(bytes.toString('base64'), item.result, 'native_result_must_be_exact_base64_not_url_or_data_url');
  assert.ok(bytes.length <= 16 * 1024 * 1024, 'decoded_image_size_limit');
  const png = bytes.subarray(0, 8).equals(Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]));
  const jpeg = bytes[0] === 255 && bytes[1] === 216 && bytes[2] === 255;
  const webp = bytes.toString('ascii', 0, 4) === 'RIFF' && bytes.toString('ascii', 8, 12) === 'WEBP';
  assert.ok(png || jpeg || webp, 'actual_supported_raster_required');
  const phases = events.filter(event => event.item_id === item.id && event.type.startsWith('response.image_generation_call.')).map(event => event.type);
  assert.deepEqual(phases, ['response.image_generation_call.in_progress', 'response.image_generation_call.completed']);
  const published = events.find(event => event.type === 'response.output_item.done' && event.item?.id === item.id);
  assert.equal(published?.item.result, item.result, 'SSE_done_image_differs_from_terminal');
  assert.ok(!JSON.stringify(response).includes('__agents_image_generation'), 'private_image_alias_leaked');
  return { itemId: item.id, bytes: bytes.length, sha256: hash(bytes), format: png ? 'png' : jpeg ? 'jpeg' : 'webp', phases };
}

function configuration(argv, environment, root) {
  const { values } = parseArgs({ args: argv, options: { source: { type: 'string' }, output: { type: 'string' },
    'current-python': { type: 'string' }, 'allow-paid-image': { type: 'boolean', default: false } }, strict: true });
  assert.match(values.source ?? '', /^[a-f0-9]{40}$/, 'exact_image_assembly_source_required');
  assert.equal(values['allow-paid-image'], true, 'single_new_paid_image_gate_required');
  assert.ok(environment.MAGENTS_TEST_API_KEY, 'owned_platform_key_missing');
  assert.match(environment.MAGENTS_TEST_TRACE_SEED ?? '', /^[1-9][0-9]*:[1-9][0-9]*$/);
  const output = path.resolve(values.output ?? '.temp/stage-delivery/hosted-image-receipt.json');
  assert.ok(output.startsWith(path.join(root, '.temp') + path.sep), 'receipt_must_stay_in_project_temp');
  assert.equal(fs.existsSync(output), false, 'existing_image_intent_requires_reconciliation_not_replay');
  const python = path.resolve(values['current-python'] ?? '.cache/current-sdk-venv/Scripts/python.exe');
  assert.ok(python.startsWith(path.join(root, '.cache') + path.sep) && fs.existsSync(python), 'project_current_python_required');
  return { source: values.source, output, python };
}

async function cleanup(request, receipt, save) {
  const failures = [];
  if (receipt.pending) {
    receipt.cleanup = { failures: [{ kind: 'ambiguous_response_reconcile_original_id_only' }], automaticReplay: false }; save(); return;
  }
  for (const file of receipt.resources.files) {
    try { await request('DELETE', `/v1/files/${file}`, undefined, [200, 404]); }
    catch (error) { failures.push({ kind: 'file_delete', id: file, status: error.status ?? null }); }
  }
  for (const session of receipt.resources.sessions) {
    try { await archiveCompletedSession(request, session); }
    catch (error) { failures.push({ kind: 'session_archive', id: session, status: error.status ?? null }); }
  }
  if (!failures.length && receipt.resources.agent) {
    try { await request('DELETE', `/v1/agents/${receipt.resources.agent}`, undefined, [200, 204, 404]); }
    catch (error) { failures.push({ kind: 'agent_delete', status: error.status ?? null }); }
  }
  receipt.cleanup = { failures, canonicalResponsesRetained: true }; save();
}

export async function main(argv = process.argv.slice(2), environment = process.env) {
  const root = path.resolve('.'), config = configuration(argv, environment, root);
  assertExecutionWorkspace(root, config.source, environment);
  const provenance = requireLiveDeploymentProvenance(environment, config.source, root);
  verifyWorkerIdentity(provenance, root, policy.verifyServingSource);
  const selectedConfig = execFileSync('git', ['show', `${config.source}:scripts/deploy-config.mjs`], {
    cwd: root, encoding: 'utf8', maxBuffer: 256 * 1024, windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'] });
  assert.equal(fs.readFileSync(path.join(root, 'scripts/deploy-config.mjs'), 'utf8').replaceAll('\r\n', '\n'),
    selectedConfig.replaceAll('\r\n', '\n'), 'exact_deployed_image_profile_required');
  const deployment = JSON.parse(fs.readFileSync(path.resolve(environment.MAGENTS_TEST_DEPLOYMENT_PROVENANCE), 'utf8'));
  assert.ok(deployment.worker_runtime_features?.includes('image_generation'), 'actual_deployment_must_enable_images');
  const profile = seedreamProfile.profiles[0];
  assert.equal(profile.model, 'bytedance-seed/seedream-5-0-flash');
  assert.equal(profile.reservation_microusd, 18000); assert.deepEqual(profile.asset_hosts, []);
  const transport = tracedFetch(fetch, environment.MAGENTS_TEST_TRACE_SEED);
  const request = async (method, route, body, expected = [200]) => {
    const response = await transport(origin + route, { method, headers: { Authorization: `Bearer ${environment.MAGENTS_TEST_API_KEY}`,
      ...(body === undefined ? {} : { 'Content-Type': 'application/json' }) },
      ...(body === undefined ? {} : { body: JSON.stringify(body) }), signal: AbortSignal.timeout(30000) });
    assert.ok(expected.includes(response.status), `platform_status:${method}:${response.status}`);
    const bytes = await response.text(); return bytes ? JSON.parse(bytes) : null;
  };
  const client = new OpenAI({ apiKey: environment.MAGENTS_TEST_API_KEY, baseURL: origin + '/v1', maxRetries: 0, timeout: 300000, fetch: transport });
  const receipt = { execution: 'new_live_platform_seedream_image_increment', source: config.source, provenance,
    price: { model: profile.model, imageMicroUSD: profile.reservation_microusd, revision: profile.price_revision, profileSha256: digest(profile) },
    scriptSha256: hash(fs.readFileSync(fileURLToPath(import.meta.url))), traceSeed: environment.MAGENTS_TEST_TRACE_SEED,
    startedAt: new Date().toISOString(), run: randomUUID(), phase: 'health', status: 'running', submissions: 0, pending: null,
    resources: { agent: null, sessions: [], responses: [], files: [] }, checks: {}, cleanup: {},
    accounting: 'actual_one_image_18000microusd_plus_direct_deepseek_and_resource_postflight_required',
    notSelected: ['direct_provider_paid_probe', 'Code', 'Search', 'accepted_POST_replay', 'controlled_image_six_cases', 'raw_image_persistence'] };
  fs.mkdirSync(path.dirname(config.output), { recursive: true });
  fs.writeFileSync(config.output, JSON.stringify(receipt, null, 2), { flag: 'wx' });
  const save = () => fs.writeFileSync(config.output, JSON.stringify(receipt, null, 2));
  try {
    assertServingWorkerHealth(await request('GET', '/healthz'), provenance);
    const approved = policy.assertAvailableDirectDeepSeek(await request('GET', '/v1/models'));
    receipt.phase = 'agent_create_intent'; save();
    const agent = await request('POST', '/v1/agents', { name: `hosted-image-${receipt.run}`, model: nativeAcceptanceModel(approved),
      instructions: 'Use the declared image_generation exactly once when asked. Do not create a second image. After its successful result, answer briefly. Never claim a tool executed without its result.',
      workspace: false, tools: [], skills: [], tool_services: { image_generation: { provider: 'platform' } }, compaction_percent: 80 }, [201]);
    receipt.resources.agent = agent.id; save();
    const initial = await policy.createPinnedSession(request, agent, { add: id => { receipt.resources.sessions.push(id); save(); } });
    const body = imageRequest(agent.id);
    receipt.submissions = 1; receipt.phase = 'single_image_response_intent';
    receipt.pending = { idempotencyKey: `hosted-image-${receipt.run}`, session: initial.id, bodySha256: digest(body) }; save();
    const result = await client.responses.create({ ...body, stream: true }, { headers: {
      'X-Agents-Session': initial.id, 'Idempotency-Key': receipt.pending.idempotencyKey } }).withResponse();
    const responseId = result.response.headers.get('x-agents-response');
    if (responseId) receipt.resources.responses.push(responseId);
    assert.equal(result.response.headers.get('x-agents-session'), initial.id); save();
    const events = [];
    for await (const event of result.data) {
      events.push(event);
      if (event.type === 'response.created' && !receipt.resources.responses.includes(event.response.id)) { receipt.resources.responses.push(event.response.id); save(); }
      if (['response.completed', 'response.failed', 'response.incomplete'].includes(event.type) && receipt.resources.responses.includes(event.response?.id)) {
        receipt.pending = null;
        receipt.checks.terminal = { type: event.type, responseId: event.response.id, status: event.response.status,
          errorCode: event.response.agents_error_code ?? event.response.error?.code ?? null,
          output: event.response.output?.map(item => ({ type: item.type, id: item.id, status: item.status })) };
        save();
      }
    }
    const response = assertCanonicalEvents(events);
    assert.equal(response.id, responseId);
    receipt.phase = 'canonical_image_terminal'; receipt.checks.image = summarizeImage(response, events); save();
    policy.assertDirectDeepSeekPin(await request('GET', `/v1/sessions/${initial.id}`), agent.id);
    const typed = JSON.parse(execFileSync(config.python, [path.join(root, 'tests/e2e/current_responses_sdk.py'), 'response-wire', '--input', '-'], {
      cwd: root, input: JSON.stringify({ response, events }), encoding: 'utf8', maxBuffer: 1024 * 1024,
      timeout: 30000, windowsHide: true, stdio: ['pipe', 'pipe', 'pipe'] }));
    assert.equal(typed.passed, true);
    receipt.checks.currentPythonWire = { sdk: typed.sdk, actualPayloads: true, newModelRuns: 0 }; save();
    const retrieved = await client.responses.retrieve(response.id);
    assert.deepEqual(retrieved.output, response.output);
    const replay = [];
    for await (const event of await client.responses.retrieve(response.id, { stream: true })) replay.push(event);
    assert.deepEqual(replay, events, 'GET_event_replay_changed_image_canonical_wire');
    receipt.checks.canonicalRetrieval = { jsonOutputSha256: digest(retrieved.output), getEventsSha256: digest(replay), originalEvents: events.length, acceptedPostReplay: false }; save();
    const files = await request('GET', '/v1/files');
    const linked = files.data.filter(file => file.filename?.startsWith(receipt.checks.image.itemId));
    assert.equal(linked.length, 1, 'actual_image_files_link_required');
    const file = linked[0]; receipt.resources.files.push(file.id); save();
    const content = await client.files.content(file.id);
    const bytes = Buffer.from(await content.arrayBuffer());
    assert.equal(hash(bytes), receipt.checks.image.sha256, 'Files_content_differs_from_native_image');
    await request('DELETE', `/v1/files/${file.id}`);
    receipt.resources.files = receipt.resources.files.filter(id => id !== file.id);
    const afterDelete = await client.responses.retrieve(response.id);
    assert.deepEqual(afterDelete.output, response.output, 'Files_delete_invalidated_canonical_image');
    receipt.checks.files = { id: file.id, bytes: bytes.length, sha256: hash(bytes), deleted: true, canonicalImageRetained: true }; save();
    receipt.status = 'live_image_behavior_passed_financial_postflight_pending'; save();
  } catch (error) {
    receipt.status = 'failed'; receipt.failure = { phase: receipt.phase, name: error.name, code: error.code ?? null,
      status: error.status ?? null, requestId: error.requestID ?? null,
      message: error.message?.split('\n')[0].slice(0, 500), operator: error.operator ?? null }; save(); throw error;
  } finally {
    await cleanup(request, receipt, save); receipt.endedAt = new Date().toISOString(); save();
    if (receipt.cleanup.failures.length) process.exitCode = 1;
  }
  process.stdout.write(JSON.stringify({ status: receipt.status, submissions: receipt.submissions, receipt: config.output }) + '\n');
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main().catch(error => { process.stderr.write(JSON.stringify({ result: 'failed', name: error.name, status: error.status ?? null }) + '\n'); process.exitCode = 1; });
}
