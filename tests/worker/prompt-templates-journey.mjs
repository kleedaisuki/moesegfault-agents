import assert from 'node:assert/strict';
import { createBrowserPrincipal } from './browser-principal.mjs';

export const promptRegistry = JSON.stringify({ format: 1, prompts: [{
  owner: 'prompt-templates-owner', id: 'pmpt_support', current: '2', versions: {
    '1': { variables: { customer: 'text' }, instructions: 'Template v1 for {{customer}}',
      input: [{ role: 'user', content: [{ type: 'input_text', text: 'Version one: {{customer}}' }] }] },
    '2': { variables: { customer: 'text' }, instructions: 'Template v2 for {{customer}}',
      input: [{ role: 'user', content: [{ type: 'variable', name: 'customer' }] }] },
  },
}] });

export function createPromptProviderFixture() {
  const calls = [];
  return {
    calls,
    async handle(request) {
      if (new URL(request.url).origin !== 'https://prompts.example.com') return undefined;
      assert.equal(new URL(request.url).pathname, '/chat/completions');
      assert.equal(request.headers.get('authorization'), 'Bearer prompt-test-key');
      const body = await request.json();
      assert.equal(body.stream, true);
      assert.equal(body.model, 'prompt-fixture');
      assert.equal(body.prompt, undefined, 'Local registry IDs must never reach an upstream account');
      calls.push(body);
      return new Response(`data: ${JSON.stringify({ id: `prompt-${calls.length}`, model: body.model,
        choices: [{ index: 0, delta: { content: 'Template response.' }, finish_reason: 'stop' }],
        usage: { prompt_tokens: 100, completion_tokens: 4, total_tokens: 104 },
      })}\n\ndata: [DONE]\n\n`, { headers: { 'Content-Type': 'text/event-stream' } });
    },
  };
}

export async function runPromptTemplatesJourney({ mf, db, encryptionKey, request, provider, replaceRegistry }) {
  const principal = await createBrowserPrincipal({ mf, db, encryptionKey, owner: 'prompt-templates-owner' });
  const key = await principal.browser('POST', '/v1/provider-keys', {
    protocol: 'openai', base_url: 'https://prompts.example.com', api_key: 'prompt-test-key',
  }, 201);
  const agentBody = { name: 'Prompt compatibility', instructions: 'Protected pinned instructions.',
    tools: [], skills: [], workspace: false, compaction_percent: 80,
    model: { provider: 'byok', protocol: 'openai', credential_id: key.id,
      model: 'prompt-fixture', context_window: 8192, max_output_tokens: 256 } };
  const agent = await request('POST', '/v1/agents', agentBody, 201, principal.apiKey);
  async function create(body, identity, expected = 200, bearer = principal.apiKey) {
    const response = await mf.dispatchFetch('https://agents.test/v1/responses', {
      method: 'POST', headers: { Authorization: `Bearer ${bearer}`, 'Content-Type': 'application/json',
        'Idempotency-Key': identity, 'CF-Connecting-IP': '192.0.2.89' }, body: JSON.stringify(body),
    });
    const text = await response.text();
    assert.equal(response.status, expected, text);
    return JSON.parse(text);
  }
  const body = { model: agent.id, input: 'Current task.',
    prompt: { id: 'pmpt_support', variables: { customer: { type: 'input_text', text: 'Klee {{literal}}' } } } };
  const first = await create(body, 'prompt-current-version');
  assert.equal(first.status, 'completed');
  assert.equal(first.prompt.version, '2');
  assert.equal(first.prompt.variables.customer.text, 'Klee {{literal}}');
  assert.equal(provider.calls.length, 1);
  assert.match(provider.calls[0].messages[0].content, /Protected pinned instructions\./);
  assert.match(provider.calls[0].messages[0].content, /Template v2 for Klee \{\{literal\}\}/);
  assert.deepEqual(provider.calls[0].messages.filter(message => message.role === 'user').map(message => message.content),
    ['Klee {{literal}}', 'Current task.']);
  const replay = await create(body, 'prompt-current-version');
  assert.equal(replay.id, first.id);
  assert.equal(replay.prompt.version, '2');
  assert.equal(provider.calls.length, 1, 'Idempotent replay cannot render or execute a new version');
  const explicit = await create({ ...body, previous_response_id: first.id,
    prompt: { ...body.prompt, version: '1' } }, 'prompt-explicit-version');
  assert.equal(explicit.prompt.version, '1');
  assert.equal(provider.calls.length, 2);
  assert.match(provider.calls[1].messages[0].content, /Template v1 for Klee \{\{literal\}\}/);
  const next = { ...body, previous_response_id: explicit.id };
  const missing = await create({ ...next, prompt: { id: 'pmpt_support', variables: {} } }, 'prompt-missing-variable', 400);
  assert.equal(missing.error.code, 'invalid_prompt_variables');
  const unknown = await create({ ...next, prompt: { ...body.prompt, version: 'not-published' } }, 'prompt-missing-version', 404);
  assert.equal(unknown.error.code, 'prompt_not_found');
  assert.equal(provider.calls.length, 2);
  const other = await createBrowserPrincipal({ mf, db, encryptionKey, owner: 'prompt-templates-other-owner' });
  const otherKey = await other.browser('POST', '/v1/provider-keys', {
    protocol: 'openai', base_url: 'https://prompts.example.com', api_key: 'prompt-test-key',
  }, 201);
  const otherAgent = await request('POST', '/v1/agents', {
    ...agentBody, model: { ...agentBody.model, credential_id: otherKey.id },
  }, 201, other.apiKey);
  const isolated = await create({ ...body, model: otherAgent.id }, 'prompt-cross-owner', 404, other.apiKey);
  assert.equal(isolated.error.code, 'prompt_not_found');
  assert.equal(provider.calls.length, 2, 'A prompt ID does not authorize another account to render it');
  const changed = JSON.parse(promptRegistry);
  changed.prompts[0].current = '1';
  delete changed.prompts[0].versions['2'];
  await replaceRegistry(JSON.stringify(changed));
  try {
    const coldReplay = await create(body, 'prompt-current-version');
    assert.equal(coldReplay.id, first.id);
    assert.equal(coldReplay.prompt.version, '2', 'Cold replay owns its version even after that registry version disappears');
    assert.equal(provider.calls.length, 2);
  } finally {
    await replaceRegistry(promptRegistry);
  }
  const route = await db.prepare('SELECT session_id FROM response_routes WHERE id=? AND account_id=?')
    .bind(first.id, principal.owner).first();
  assert.ok(route?.session_id);
  const pinned = await db.prepare('SELECT config_snapshot_json FROM sessions WHERE id=?')
    .bind(route.session_id).first();
  assert.ok(pinned);
  assert.equal(JSON.parse(pinned.config_snapshot_json).pinned.instructions, agentBody.instructions);
  return { provider_calls: provider.calls.length, current_version: first.prompt.version,
    explicit_version: explicit.prompt.version, replay_id: replay.id, literal_variable: true };
}
