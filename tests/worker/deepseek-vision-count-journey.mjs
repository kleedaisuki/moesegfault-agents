import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { readFile, mkdir, writeFile } from 'node:fs/promises';
import OpenAI from 'openai';
import { createBrowserPrincipal } from './browser-principal.mjs';

const origin = 'https://agents.test';
const imageOrigin = 'https://vision-input.example.com';
const corpusUrl = new URL('../fixtures/deepseek-image-count.json', import.meta.url);

export function createVisionInputFixture() {
  const state = { requests: 0, redirects: 0, modelBodies: [] };
  return { state, async handle(request) {
    if (request.url === 'https://api.deepseek.com/responses') {
      assert.equal(request.headers.get('authorization'), 'Bearer synthetic-vision-counter-no-provider-call');
      const body = await request.json();
      state.modelBodies.push(body);
      const response = { id: 'controlled-vision-response', object: 'response', model: body.model, status: 'completed', error: null,
        output: [{ id: 'controlled-vision-message', type: 'message', role: 'assistant', status: 'completed',
          content: [{ type: 'output_text', text: 'A synthetic pixel.', annotations: [] }] }],
        usage: { input_tokens: 219, output_tokens: 4, total_tokens: 223, input_tokens_details: { cached_tokens: 0 }, output_tokens_details: { reasoning_tokens: 0 } } };
      return new Response(`event: response.created\ndata: ${JSON.stringify({ type: 'response.created', sequence_number: 0, response: { ...response, status: 'in_progress', output: [], usage: null } })}\n\n`
        + `event: response.completed\ndata: ${JSON.stringify({ type: 'response.completed', sequence_number: 1, response })}\n\n`, { headers: { 'Content-Type': 'text/event-stream' } });
    }
    if (!request.url.startsWith(imageOrigin + '/')) return undefined;
    state.requests++;
    assert.equal(request.headers.get('authorization'), null, 'Public image fetch must never carry provider/account credentials');
    assert.equal(request.headers.get('cookie'), null);
    if (request.url.endsWith('/redirect.gif')) {
      state.redirects++;
      return new Response(null, { status: 302, headers: { Location: 'http://127.0.0.1/private' } });
    }
    assert.equal(request.url, imageOrigin + '/small.gif');
    const corpus = JSON.parse(await readFile(corpusUrl, 'utf8'));
    return new Response(Buffer.from(corpus.assets.small_gif.base64, 'base64'), {
      headers: { 'Content-Type': 'image/gif', 'Content-Length': String(Buffer.from(corpus.assets.small_gif.base64, 'base64').length) },
    });
  } };
}

export async function runDeepSeekVisionCountJourney({ mf, db, encryptionKey, request, diagnostic, visionInputs }) {
  const principal = await createBrowserPrincipal({ mf, db, encryptionKey, owner: 'deepseek-vision-counter-owner' });
  await principal.browser('GET', '/v1/account');
  const credential = await principal.browser('POST', '/v1/provider-keys', {
    protocol: 'openai', base_url: 'https://api.deepseek.com', api_key: 'synthetic-vision-counter-no-provider-call',
  }, 201);
  const model = name => ({ provider: 'byok', credential_id: credential.id, protocol: 'responses', model: name,
    context_window: 1048576, max_output_tokens: 512 });
  const agent = await request('POST', '/v1/agents', { name: 'Vision exact counter', instructions: 'Counter vision fixed instructions.',
    tools: [], skills: [], workspace: false, model: model('deepseek-flash') }, 201, principal.apiKey);
  const corpus = JSON.parse(await readFile(corpusUrl, 'utf8'));
  const sdk = new OpenAI({ apiKey: principal.apiKey, baseURL: origin + '/v1', maxRetries: 0, timeout: 30000,
    fetch: (input, init) => mf.dispatchFetch(input, init) });
  const samples = [];
  let file;
  async function invoke(body, label, expectedCount, status = 200) {
    const start = performance.now();
    if (status === 200) {
      const value = await sdk.responses.inputTokens.count({ ...body, model: agent.id });
      assert.equal(value.object, 'response.input_tokens');
      assert.equal(value.input_tokens, expectedCount, label);
      samples.push({ label, input_tokens: value.input_tokens, wall_ms: performance.now() - start });
      return value;
    }
    const response = await mf.dispatchFetch(origin + '/v1/responses/input_tokens', {
      method: 'POST', headers: { Authorization: `Bearer ${principal.apiKey}`, 'Content-Type': 'application/json', 'CF-Connecting-IP': '192.0.2.82' },
      body: JSON.stringify({ ...body, model: agent.id }),
    });
    const value = await response.json();
    assert.equal(response.status, status, `${label}: ${JSON.stringify(value)}`);
    if (status === 200) {
      assert.equal(value.object, 'response.input_tokens');
      assert.equal(value.input_tokens, expectedCount, label);
      samples.push({ label, input_tokens: value.input_tokens, wall_ms: performance.now() - start });
    }
    return value;
  }
  try {
    const form = new FormData();
    form.set('purpose', 'user_data');
    form.set('file', new Blob([Buffer.from(corpus.assets.small_png.base64, 'base64')], { type: 'image/png' }), 'vision-small.png');
    const encoded = new Request(origin + '/v1/files', { method: 'POST', body: form });
    const bytes = Buffer.from(await encoded.arrayBuffer());
    const upload = await mf.dispatchFetch(origin + '/v1/files', { method: 'POST', headers: {
      Authorization: `Bearer ${principal.apiKey}`, 'Content-Type': encoded.headers.get('content-type'), 'Content-Length': String(bytes.length),
    }, body: bytes });
    assert.equal(upload.status, 200);
    file = await upload.json();
    const fileSet = await request('GET', '/v1/files', undefined, 200, principal.apiKey);
    for (const fixture of corpus.fixtures) await invoke(fixture.body, fixture.name, fixture.public_input_tokens);
    const fromFile = structuredClone(corpus.fixtures[0].body);
    fromFile.input[0].content[1] = { type: 'input_image', file_id: file.id, image_url: null, detail: 'original' };
    await invoke(fromFile, 'owned-file-original', corpus.fixtures[0].public_input_tokens);
    const fromHttps = structuredClone(corpus.fixtures[1].body);
    fromHttps.input[0].content[1].image_url = imageOrigin + '/small.gif';
    const reads = visionInputs.state.requests;
    await invoke(fromHttps, 'frozen-https-gif', corpus.fixtures[1].public_input_tokens);
    assert.equal(visionInputs.state.requests, reads + 1, 'Preparation fetches exactly once; counter never refetches');
    const publicReads = visionInputs.state.requests;
    const privateSource = structuredClone(fromHttps);
    privateSource.input[0].content[1].image_url = 'https://127.0.0.1/private';
    await invoke(privateSource, 'private-url-refused-before-fetch', undefined, 400);
    assert.equal(visionInputs.state.requests, publicReads);
    fromHttps.input[0].content[1].image_url = imageOrigin + '/redirect.gif';
    await invoke(fromHttps, 'redirect-refused', undefined, 400);
    assert.equal(visionInputs.state.redirects, 1);
    const oversized = structuredClone(corpus.fixtures[1].body);
    const badGif = Buffer.from(corpus.assets.small_gif.base64, 'base64');
    badGif.fill(255, 6, 10);
    oversized.input[0].content[1].image_url = 'data:image/gif;base64,' + badGif.toString('base64');
    await invoke(oversized, 'oversized-gif-refused-before-decode', undefined, 400);
    await request('PATCH', `/v1/agents/${agent.id}`, { model: model('deepseek-v4-pro') }, 200, principal.apiKey);
    const unsupported = await invoke(corpus.fixtures[0].body, 'pro-genuine-no-vision', undefined, 422);
    assert.equal(unsupported.error.code, 'token_count_model_vision_unsupported');
    const after = await request('GET', '/v1/files', undefined, 200, principal.apiKey);
    assert.deepEqual(after.data.map(value => value.id), fileSet.data.map(value => value.id), 'Read-only counts never create hidden Files');
    assert.equal((await db.prepare('SELECT COUNT(*) AS count FROM sessions WHERE account_id=?').bind(principal.owner).first()).count, 0);
    assert.equal((await db.prepare("SELECT COUNT(*) AS count FROM usage_ledger l JOIN usage_reservations r ON r.id=l.reservation_id WHERE r.account_id=? AND l.resource='model'").bind(principal.owner).first()).count, 0);
    assert.equal(visionInputs.state.modelBodies.length, 0, 'No inference during count or rejected images');
    await request('PATCH', `/v1/agents/${agent.id}`, { model: model('deepseek-flash') }, 200, principal.apiKey);
    const createBody = structuredClone(corpus.fixtures[1].body);
    createBody.input[0].content[1].image_url = imageOrigin + '/small.gif';
    const beforeCreate = visionInputs.state.requests;
    const created = await sdk.responses.create({ ...createBody, model: agent.id, max_output_tokens: 64 });
    assert.equal(created.status, 'completed');
    assert.equal(created.output_text, 'A synthetic pixel.');
    assert.equal(visionInputs.state.requests, beforeCreate + 1, 'Create freezes once through the same preparation path');
    assert.equal(visionInputs.state.modelBodies.length, 1);
    const sent = visionInputs.state.modelBodies[0];
    const media = sent.input.flatMap(item => item.content ?? []).filter(part => part.type === 'input_image');
    assert.equal(media.length, 1);
    assert.equal(media[0].image_url, `data:image/gif;base64,${corpus.assets.small_gif.base64}`);
    assert.equal(media[0].detail, 'auto');
    assert.equal(created.usage.input_tokens, samples.find(sample => sample.label === 'frozen-https-gif').input_tokens);
    assert.ok(process.env.MAGENTS_CURRENT_SCHEMA_PYTHON, 'Strict current SDK validation must not be optional');
    const strict = spawnSync(process.env.MAGENTS_CURRENT_SCHEMA_PYTHON, ['-c',
      'import json,sys; from openai.types.responses import Response; Response.model_validate(json.load(sys.stdin), strict=True)'],
    { input: JSON.stringify(created), encoding: 'utf8' });
    assert.equal(strict.status, 0, strict.stderr);
    await sdk.responses.delete(created.id);
    const report = { schema: 1, source: process.env.MAGENTS_WORKER_SOURCE_SHA ?? process.env.GITHUB_SHA ?? 'local-uncommitted', actual_worker: true,
      paid_model_calls: 0, controlled_model_calls: 1, media_fetches: visionInputs.state.requests, samples, renderer_sha256: corpus.renderer_sha256 };
    await mkdir('.cache/perf/v3', { recursive: true });
    await writeFile('.cache/perf/v3/deepseek-vision-count-workerd.json', JSON.stringify(report, null, 2) + '\n');
    diagnostic?.(`DeepSeek vision counter receipt: ${JSON.stringify(report)}`);
  } finally {
    if (file?.id) await request('DELETE', `/v1/files/${file.id}`, undefined, 200, principal.apiKey);
    await request('DELETE', `/v1/agents/${agent.id}`, undefined, 200, principal.apiKey);
  }
}
