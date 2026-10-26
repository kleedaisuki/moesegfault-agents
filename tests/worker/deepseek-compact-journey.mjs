import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import OpenAI from 'openai';
import { createBrowserPrincipal } from './browser-principal.mjs';

const key = 'synthetic-purpose-specific-deepseek-key';
const policy = 'Pinned compact policy 雪.';
const summary = 'Preserved goal, custom tool result and correction 雪.';

function sse(events) {
  return new Response(events.map(event => `event: ${event.type}\ndata: ${JSON.stringify(event)}\n\n`).join(''),
    { headers: { 'Content-Type': 'text/event-stream' } });
}

export function createDeepseekCompactFixture() {
  const state = { enabled: false, compactions: [], generations: [], missingWrite: false, partial: false, thinking: false };
  return { state, async handle(request) {
    if (!state.enabled || new URL(request.url).hostname !== 'api.deepseek.com') return undefined;
    const body = await request.json();
    assert.equal(body.model, 'deepseek-flash');
    assert.equal(body.stream, true);
    if (new URL(request.url).pathname.endsWith('/responses')) {
      assert.equal(request.headers.get('authorization'), `Bearer ${key}`);
      state.generations.push(body);
      const response = { id: 'upstream-generation', object: 'response', model: body.model, status: 'completed', error: null,
        output: [{ id: 'upstream-message', type: 'message', role: 'assistant', status: 'completed',
          content: [{ type: 'output_text', text: 'Continued goal.', annotations: [], logprobs: [] }] }],
        usage: { input_tokens: 12, output_tokens: 3, total_tokens: 15,
          input_tokens_details: { cached_tokens: 2 }, output_tokens_details: { reasoning_tokens: 0 } } };
      return sse([{ type: 'response.created', sequence_number: 0, response: { ...response, status: 'in_progress', output: [], usage: null } },
        { type: 'response.completed', sequence_number: 1, response }]);
    }
    assert.equal(new URL(request.url).pathname, '/anthropic/v1/messages');
    assert.equal(request.headers.get('x-api-key'), key);
    assert.equal(request.headers.get('authorization'), null);
    assert.equal(body.thinking.type, 'disabled');
    assert.equal(body.tools, undefined);
    state.compactions.push(body);
    const usage = { input_tokens: 12, output_tokens: 0, cache_read_input_tokens: 2,
      ...(state.missingWrite ? {} : { cache_creation_input_tokens: 0 }) };
    const events = [{ type: 'message_start', message: { id: `compact-upstream-${state.compactions.length}`,
      type: 'message', role: 'assistant', model: body.model, content: [], stop_reason: null, usage } }];
    let index = 0;
    if (state.thinking) {
      events.push({ type: 'content_block_start', index, content_block: { type: 'thinking', thinking: 'synthetic' } },
        { type: 'content_block_stop', index });
      index++;
    }
    events.push({ type: 'content_block_start', index, content_block: { type: 'text', text: '' } },
      { type: 'content_block_delta', index, delta: { type: 'text_delta', text: summary } },
      { type: 'content_block_stop', index },
      { type: 'message_delta', delta: { stop_reason: state.partial ? 'max_tokens' : 'end_turn', stop_sequence: null }, usage: { output_tokens: 3 } },
      { type: 'message_stop' });
    return sse(events);
  } };
}

function strictCompaction(value) {
  const python = process.env.MAGENTS_CURRENT_SCHEMA_PYTHON ?? process.env.CURRENT_SCHEMA_PYTHON;
  assert.ok(python, 'Current SDK interpreter is required');
  // SDK 3.37 documents retained user messages but annotates output with an
  // assistant-only union. Keep the protocol roles and use its actual input type.
  const code = 'import json,sys\nfrom typing import Any, Union\nfrom openai.types.responses.compacted_response import CompactedResponse\nfrom openai.types.responses.response_input_message_item import ResponseInputMessageItem\nfrom openai.types.responses.response_compaction_item import ResponseCompactionItem\nclass CompactionWindow(CompactedResponse):\n output: list[Union[ResponseInputMessageItem, ResponseCompactionItem]]\nvalue: dict[str, Any] = json.load(sys.stdin)\nparsed: CompactionWindow = CompactionWindow.model_validate(value, strict=True)\nassert parsed.output[-1].type == "compaction"\nassert all(item.type == "message" and item.role == "user" for item in parsed.output[:-1])\nprint("strict_compaction_contract_pass")';
  assert.equal(execFileSync(python, ['-c', code], { input: JSON.stringify(value), encoding: 'utf8',
    stdio: ['pipe', 'pipe', 'pipe'], windowsHide: true }).trim(), 'strict_compaction_contract_pass');
}

export async function runDeepseekCompactJourney({ mf, encryptionKey, request, provider, configure, budgetOnly = false }) {
  const contextWindow = budgetOnly ? 1_000_000 : 8192;
  const registry = [{ provider: 'deepseek', protocol: 'responses', model: 'deepseek-flash', context_window: contextWindow, max_output_tokens: 64 }];
  const prices = { 'deepseek/deepseek-flash': { input_microusd_per_million: 150000,
    cached_input_microusd_per_million: 3000, output_microusd_per_million: 600000, revision: 'compaction-fixture' } };
  const db = await configure({ MODEL_REGISTRY_JSON: JSON.stringify(registry), MODEL_PRICING_JSON: JSON.stringify(prices), DEEPSEEK_API_KEY: key });
  provider.state.enabled = true;
  try {
    const principal = await createBrowserPrincipal({ mf, db, encryptionKey, owner: budgetOnly ? 'deepseek-compaction-budget-owner' : 'deepseek-compaction-owner' });
    await principal.browser('GET', '/v1/account');
    if (budgetOnly) await db.prepare('UPDATE usage_accounts SET budget_microusd=100000 WHERE account_id=?').bind(principal.owner).run();
    const agent = await request('POST', '/v1/agents', { name: 'Explicit compaction transport',
      model: { provider: 'deepseek', protocol: 'responses', model: 'deepseek-flash', context_window: contextWindow, max_output_tokens: 64 },
      instructions: policy, tools: ['time'], workspace: false }, 201, principal.apiKey);
    const sdk = new OpenAI({ apiKey: principal.apiKey, baseURL: 'https://agents.test/v1', maxRetries: 0,
      fetch: (input, init) => mf.dispatchFetch(input, init) });
    const body = { model: agent.id, input: [{ role: 'user', content: 'Preserve this goal and correction.' }], instructions: 'Use concise bullets.' };
    const options = { headers: { 'Idempotency-Key': 'deepseek-compact-once' } };
    const raw = await sdk.responses.compact(body, options).asResponse();
    assert.equal(raw.status, 200);
    const window = await raw.json();
    strictCompaction(window);
    if (budgetOnly) {
      assert.equal(window.object, 'response.compaction');
      const reserved = await db.prepare("SELECT MAX(reserved_microusd) AS amount FROM usage_reservations WHERE account_id=? AND idempotency_key LIKE 'completion:%'").bind(principal.owner).first('amount');
      assert.ok(reserved > 0 && reserved < 5000, 'A small prompt must not reserve the entire one-million-token model window');
      assert.equal(await db.prepare("SELECT COUNT(*) AS n FROM usage_reservations WHERE account_id=? AND state='reserved'").bind(principal.owner).first('n'), 0);
      return;
    }
    assert.deepEqual(window.usage, { input_tokens: 14, input_tokens_details: { cached_tokens: 2, cache_write_tokens: 0 },
      output_tokens: 3, output_tokens_details: { reasoning_tokens: 0 }, total_tokens: 17 });
    assert.match(window.output.at(-1).encrypted_content, /^mcmp1\./);
    assert.deepEqual(await sdk.responses.compact(body, options), window);
    assert.equal(provider.state.compactions.length, 1, 'Same-key compaction must not buy a second inference');
    const context = provider.state.compactions[0].messages[0].content[0].text;
    assert.ok(context.includes(policy));
    assert.ok(context.includes('Preserve this goal and correction.'));
    assert.equal(await db.prepare('SELECT COUNT(*) AS n FROM sessions WHERE account_id=?').bind(principal.owner).first('n'), 0);
    const count = await sdk.responses.inputTokens.count({ model: agent.id, input: window.output });
    assert.ok(count.input_tokens > 0);
    const generation = await sdk.responses.create({ model: agent.id, input: [...window.output, { role: 'user', content: 'Continue.' }] }).withResponse();
    const resumed = generation.data;
    assert.equal(resumed.status, 'completed');
    assert.equal(resumed.usage, null, 'Native generation still keeps its unreported cache-write unknown');
    assert.equal(provider.state.generations.length, 1);
    assert.ok(JSON.stringify(provider.state.generations[0].input).includes(summary));
    assert.ok(JSON.stringify(provider.state.generations[0].input).includes(policy));
    const session = generation.response.headers.get('X-Agents-Session');
    assert.ok(session);
    await request('DELETE', `/v1/sessions/${session}`, undefined, 200, principal.apiKey);
    assert.equal((await request('GET', `/v1/sessions/${session}`, undefined, 200, principal.apiKey)).status, 'closed');
    await assert.rejects(sdk.responses.compact({ ...body, input: 'Different.' }, options), error => error.status === 409);
    for (const mode of ['missingWrite', 'thinking', 'partial']) {
      provider.state[mode] = true;
      const calls = provider.state.compactions.length;
      const failureOptions = { headers: { 'Idempotency-Key': `compact-${mode}` } };
      await assert.rejects(sdk.responses.compact(body, failureOptions), error => error.status === 503);
      await assert.rejects(sdk.responses.compact(body, failureOptions), error => error.status === 503);
      assert.equal(provider.state.compactions.length, calls + 1, `${mode} must not trigger a hidden repair generation`);
      provider.state[mode] = false;
    }
    assert.equal(await db.prepare("SELECT COUNT(*) AS n FROM execution_leases WHERE account_id=? AND state='active'").bind(principal.owner).first('n'), 0);
    assert.equal(await db.prepare("SELECT COUNT(*) AS n FROM usage_reservations WHERE account_id=? AND state='reserved'").bind(principal.owner).first('n'), 0);
  } finally {
    provider.state.enabled = false;
    await configure(null);
  }
}
