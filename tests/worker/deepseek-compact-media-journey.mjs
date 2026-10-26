import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import OpenAI from 'openai';
import { createBrowserPrincipal } from './browser-principal.mjs';

const png = 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=';
const gif = 'R0lGODlhAQABAIAAAAAAAP///ywAAAAAAQABAAACAUwAOw==';
const secret = 'synthetic-purpose-specific-deepseek-key';

export async function runDeepseekCompactMediaJourney({ mf, encryptionKey, request, provider, configure }) {
  const registry = ['deepseek-flash', 'deepseek-v4-pro'].map(model => ({
    provider: 'deepseek', protocol: 'responses', model, context_window: 1_000_000, max_output_tokens: 64,
  }));
  const price = { input_microusd_per_million: 150000, cached_input_microusd_per_million: 3000,
    output_microusd_per_million: 600000, revision: 'compact-media-fixture' };
  const db = await configure({ MODEL_REGISTRY_JSON: JSON.stringify(registry),
    MODEL_PRICING_JSON: JSON.stringify(Object.fromEntries(registry.map(({ model }) => [`deepseek/${model}`, price]))),
    DEEPSEEK_API_KEY: secret });
  provider.state.enabled = true;
  try {
    const principal = await createBrowserPrincipal({ mf, db, encryptionKey, owner: 'deepseek-compact-media-owner' });
    await principal.browser('GET', '/v1/account');
    const createAgent = model => request('POST', '/v1/agents', { name: 'Visual compaction',
      model: { provider: 'deepseek', protocol: 'responses', model, context_window: 1_000_000, max_output_tokens: 64 },
      instructions: 'Keep the visual decision and fixed policy 雪.', tools: [], workspace: false }, 201, principal.apiKey);
    const agent = await createAgent('deepseek-flash');
    const pro = await createAgent('deepseek-v4-pro');
    const sdk = new OpenAI({ apiKey: principal.apiKey, baseURL: 'https://agents.test/v1', maxRetries: 0,
      fetch: (input, init) => mf.dispatchFetch(input, init) });
    // This in-process fetch transport has no HTTP stack to supply the multipart
    // Content-Length. Use the existing explicit-wire fixture, not another SDK
    // upload acceptance test, for this journey's owned-image prerequisite.
    const form = new FormData();
    form.set('purpose', 'user_data');
    form.set('file', new File([Buffer.from(gif, 'base64')], 'source.gif', { type: 'image/gif' }));
    const encoded = new Request('https://agents.test/v1/files', { method: 'POST', body: form });
    const bytes = Buffer.from(await encoded.arrayBuffer());
    const uploaded = await mf.dispatchFetch(encoded.url, { method: 'POST', headers: {
      Authorization: `Bearer ${principal.apiKey}`, 'Content-Type': encoded.headers.get('content-type'), 'Content-Length': String(bytes.length),
    }, body: bytes });
    assert.equal(uploaded.status, 200);
    const upload = await uploaded.json();
    const body = { model: agent.id, input: [
      { role: 'developer', content: 'Preserve the correction: the right pixel is selected.' },
      { role: 'user', content: [
        { type: 'input_text', text: 'Remember both source images and the attached decision.' },
        { type: 'input_image', file_id: upload.id, detail: 'original' },
        { type: 'input_image', image_url: `data:image/png;base64,${png}`, detail: 'low' },
        { type: 'input_file', filename: 'decision.txt', file_data: `data:text/plain;base64,${Buffer.from('Selected right pixel 雪.').toString('base64')}` },
      ] },
    ] };
    const calls = provider.state.compactions.length;
    const key = { headers: { 'Idempotency-Key': 'visual-compact-once' } };
    const value = await sdk.responses.compact(body, key);
    assert.equal(value.object, 'response.compaction');
    assert.equal(value.output[0].role, 'user');
    assert.equal(value.output[0].content[1].image_url, `data:image/gif;base64,${gif}`);
    assert.equal(value.output[0].content[2].detail, 'low');
    assert.equal(value.output.at(-1).type, 'compaction');
    const wire = provider.state.compactions[calls];
    assert.ok(wire, 'Exactly one synthetic compact must reach the actual Rust Messages serializer');
    assert.equal(provider.state.compactions.length, calls + 1);
    const parts = wire.messages[0].content;
    assert.equal(parts.filter(part => part.type === 'image').length, 2);
    assert.equal(parts.find(part => part.type === 'image').source.media_type, 'image/gif');
    assert.equal(parts.find(part => part.type === 'image').source.data, gif);
    assert.ok(parts[0].text.includes('Selected right pixel 雪.'));
    assert.ok(parts[0].text.includes('source_image'));
    assert.ok(!parts[0].text.includes(gif), 'Base64 image data must not be paid as JSON text tokens');
    assert.deepEqual(await sdk.responses.compact(body, key), value);
    assert.equal(provider.state.compactions.length, calls + 1);
    const python = process.env.MAGENTS_CURRENT_SCHEMA_PYTHON ?? process.env.CURRENT_SCHEMA_PYTHON;
    assert.ok(python);
    // Preserve the actual user roles; SDK 3.37's assistant-only output annotation
    // contradicts its documented compact window. Reuse its input-item schema.
    const code = 'import json,sys\nfrom typing import Union\nfrom openai.types.responses.compacted_response import CompactedResponse\nfrom openai.types.responses.response_input_message_item import ResponseInputMessageItem\nfrom openai.types.responses.response_compaction_item import ResponseCompactionItem\nclass Window(CompactedResponse):\n output: list[Union[ResponseInputMessageItem, ResponseCompactionItem]]\nWindow.model_validate(json.load(sys.stdin), strict=True)';
    execFileSync(python, ['-c', code], { input: JSON.stringify(value), stdio: ['pipe', 'pipe', 'pipe'], windowsHide: true });
    await request('DELETE', `/v1/files/${upload.id}`, undefined, 200, principal.apiKey);
    const count = await sdk.responses.inputTokens.count({ model: agent.id, input: value.output });
    assert.ok(count.input_tokens >= 368);
    const resumed = await sdk.responses.create({ model: agent.id, input: [...value.output, { role: 'user', content: 'Continue the selected decision.' }] }).withResponse();
    assert.equal(resumed.data.status, 'completed');
    const input = provider.state.generations.at(-1).input;
    assert.ok(JSON.stringify(input).includes('Preserve the correction: the right pixel is selected.'));
    assert.ok(JSON.stringify(input).includes(`data:image/gif;base64,${gif}`));
    const session = resumed.response.headers.get('X-Agents-Session');
    assert.ok(session);
    await request('DELETE', `/v1/sessions/${session}`, undefined, 200, principal.apiKey);
    const before = provider.state.compactions.length;
    await assert.rejects(sdk.responses.compact({ model: pro.id, input: [{ role: 'user', content: [{ type: 'input_image', image_url: `data:image/png;base64,${png}` }] }] },
      { headers: { 'Idempotency-Key': 'pro-vision-not-a-text-placeholder' } }), error => error.status === 422);
    assert.equal(provider.state.compactions.length, before);
    await request('DELETE', `/v1/agents/${pro.id}`, undefined, 200, principal.apiKey);
    const credential = await principal.browser('POST', '/v1/provider-keys', {
      protocol: 'openai', base_url: 'https://api.deepseek.com', api_key: secret,
    }, 201);
    const byok = await request('POST', '/v1/agents', { name: 'BYOK visual compact',
      model: { provider: 'byok', credential_id: credential.id, protocol: 'responses', model: 'deepseek-flash',
        context_window: 1_000_000, max_output_tokens: 64 }, tools: [], workspace: false }, 201, principal.apiKey);
    const byokWindow = await sdk.responses.compact({ model: byok.id, input: [{ role: 'user',
      content: [{ type: 'input_image', image_url: `data:image/png;base64,${png}` }] }] },
      { headers: { 'Idempotency-Key': 'byok-purpose-compactor' } });
    assert.equal(byokWindow.object, 'response.compaction');
    assert.equal(provider.state.compactions.length, before + 1);
    assert.equal(provider.state.compactions.at(-1).messages[0].content.filter(part => part.type === 'image').length, 1);
    assert.equal(await db.prepare("SELECT COUNT(*) AS n FROM usage_reservations WHERE account_id=? AND state='reserved'").bind(principal.owner).first('n'), 0);
    assert.equal(await db.prepare("SELECT COUNT(*) AS n FROM execution_leases WHERE account_id=? AND state='active'").bind(principal.owner).first('n'), 0);
  } finally {
    provider.state.enabled = false;
    await configure(null);
  }
}
