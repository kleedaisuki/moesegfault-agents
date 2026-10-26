import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { assertKnownModelReceipts, collectNativeEvents, createNativeSemanticsScenario } from './native-semantics-journey.mjs';

const summary = 'This summary is safe to disclose.';
const rawReasoning = 'INTEGRITY_PRIVATE_REASONING_NEVER_PUBLIC';
const rawCiphertext = 'INTEGRITY_VENDOR_CIPHERTEXT_NEVER_PUBLIC';
const reasoning = { type: 'reasoning', id: 'vendor-integrity-reasoning',
  summary: [{ type: 'summary_text', text: summary }], encrypted_content: rawCiphertext,
  content: [{ type: 'reasoning_text', text: rawReasoning }] };
const message = text => ({ type: 'message', id: `vendor-message-${text.length}`, role: 'assistant', status: 'completed',
  content: [{ type: 'output_text', text, annotations: [], logprobs: [] }] });

function assertPrivate(value) {
  const wire = JSON.stringify(value);
  for (const marker of [rawReasoning, rawCiphertext]) assert.ok(!wire.includes(marker));
}

function changedToken(token) {
  const position = Math.floor(token.length / 2);
  return token.slice(0, position) + (token[position] === 'A' ? 'B' : 'A') + token.slice(position + 1);
}

export async function runSealedNativeContextJourney(context) {
  const firstMessage = message('The retained native message.');
  const scenario = await createNativeSemanticsScenario(context, 'sealed-context', [
    { output: [reasoning, firstMessage] },
    { output: [message('Stateless reasoning replay completed.')], check(body) {
      const items = body.input.filter(item => item.type === 'reasoning');
      assert.equal(items.length, 1);
      assert.equal(items[0].id, reasoning.id);
      assert.equal(items[0].encrypted_content, rawCiphertext);
      assert.equal(items[0].content[0].text, rawReasoning);
      assert.deepEqual(items[0].summary, reasoning.summary, 'Caller edits cannot rewrite authoritative sealed reasoning');
      assert.ok(!JSON.stringify(body).includes('CALLER_REWRITTEN_SUMMARY'));
    } },
    { output: [message('Owned item-reference replay completed.')], check(body) {
      const items = body.input.filter(item => item.type === 'message' && item.role === 'assistant');
      assert.equal(items.length, 1);
      assert.equal(items[0].id, firstMessage.id);
      assert.deepEqual(items[0].content, firstMessage.content);
    } },
  ]);
  const source = await scenario.invoke({ input: 'Produce a retained reasoning and message pair.',
    include: ['reasoning.encrypted_content'] });
  // This scenario deliberately retains three independent live Sessions. Give
  // only this synthetic principal the existing Go concurrency entitlement.
  const now = Math.floor(Date.now() / 1000);
  const subscriptions = [{ id: 'synthetic-native-sealed-go', product_id: 'agents', plan_id: 'agents-go',
    status: 'active', current_period_start: now - 10, current_period_end: now + 3600,
    entitlements: ['agents.access', 'agents.tier.go'] }];
  await scenario.db.prepare("INSERT INTO subscription_snapshots(account_id,product_id,subscription_json,fetched_at,valid_until) VALUES(?,'agents',?,?,?) ON CONFLICT(account_id,product_id) DO UPDATE SET subscription_json=excluded.subscription_json,fetched_at=excluded.fetched_at,valid_until=excluded.valid_until")
    .bind(scenario.principal.owner, JSON.stringify(subscriptions), now, now + 300).run();
  const sealed = source.value.output.find(item => item.type === 'reasoning');
  const publicMessage = source.value.output.find(item => item.type === 'message');
  assert.ok(sealed.encrypted_content.startsWith('mnri1.'));
  assert.equal(sealed.content, undefined);
  assertPrivate(source.value);
  const replayUrl = `https://agents.test/v1/responses/${source.value.id}?stream=true&include%5B%5D=reasoning.encrypted_content`;
  const headers = { Authorization: `Bearer ${scenario.principal.apiKey}` };
  const wire = await (await scenario.mf.dispatchFetch(replayUrl, { headers })).text();
  const events = await collectNativeEvents(new Response(wire));
  const checked = spawnSync(process.env.MAGENTS_CURRENT_SCHEMA_PYTHON,
    ['tests/e2e/current_responses_sdk.py', 'response-wire', '--input', '-'],
    { input: JSON.stringify({ response: source.value, events }), encoding: 'utf8', maxBuffer: 1024 * 1024 });
  assert.equal(checked.status, 0, checked.stderr || checked.stdout);
  assert.equal(await (await scenario.mf.dispatchFetch(replayUrl, { headers })).text(), wire,
    'A sealed item and its original event bytes survive repeated GET without resealing');
  assertPrivate(events);
  assert.equal(scenario.state.calls, 1);
  scenario.state.count = body => {
    const items = body.input.filter(item => item.type === 'reasoning');
    if (items.length) {
      assert.equal(items.length, 1);
      assert.equal(items[0].id, reasoning.id);
      assert.equal(items[0].encrypted_content, rawCiphertext);
      assert.equal(items[0].content[0].text, rawReasoning);
    } else {
      const messages = body.input.filter(item => item.type === 'message' && item.role === 'assistant');
      assert.equal(messages.length, 1);
      assert.deepEqual(messages[0], firstMessage);
    }
    assert.ok(!JSON.stringify(body).includes('mnri1.'), 'The counter receives resolved native bytes, never a public proxy');
    return 120;
  };
  for (const input of [[sealed], [{ type: 'item_reference', id: publicMessage.id }]]) {
    const counted = await scenario.client.responses.inputTokens.count({ model: scenario.agent.id, input });
    assert.equal(counted.input_tokens, 120);
  }
  assert.equal(scenario.state.countCalls, 2);
  assert.equal(scenario.state.calls, 1, 'Read-only native counts do not buy generation');
  const replay = await scenario.invoke({ input: [{ ...sealed,
    summary: [{ type: 'summary_text', text: 'CALLER_REWRITTEN_SUMMARY' }] },
  { role: 'user', content: 'Use the sealed native context in a new response.' }] });
  assert.equal(replay.value.previous_response_id, null);
  assert.equal(replay.value.status, 'completed');
  assertPrivate(replay.value);
  const referenced = await scenario.invoke({ input: [{ type: 'item_reference', id: publicMessage.id },
    { role: 'user', content: 'Use the owner-scoped retained message.' }] });
  assert.equal(referenced.value.previous_response_id, null);
  assert.equal(referenced.value.status, 'completed');
  assertPrivate(referenced.value);
  const negativeInputs = [
    [{ ...sealed, encrypted_content: changedToken(sealed.encrypted_content) }],
    [{ ...sealed, id: publicMessage.id }],
    [{ ...reasoning, content: undefined }],
    [{ type: 'item_reference', id: 'item_unknown_foreign_retained_source_0_0' }],
  ];
  for (const input of negativeInputs) {
    const rejected = await scenario.invoke({ input }, 400);
    assert.ok(rejected.value.error);
    assertPrivate(rejected.value);
    assert.equal(scenario.state.calls, 3, 'Unowned, tampered and raw native contexts cannot buy inference');
    await assert.rejects(scenario.client.responses.inputTokens.count({ model: scenario.agent.id, input }), error => {
      assert.equal(error.status, 400);
      return true;
    });
    assert.equal(scenario.state.countCalls, 2, 'Invalid native contexts must fail before the provider counter');
  }
  const foreign = await createNativeSemanticsScenario(context, 'sealed-foreign', [],
    { baseUrl: scenario.state.baseUrl, model: scenario.state.model });
  await assert.rejects(foreign.client.responses.inputTokens.count({ model: foreign.agent.id, input: [sealed] }), error => {
    assert.equal(error.status, 400);
    return true;
  });
  assert.equal(foreign.state.calls, 0);
  assert.equal(foreign.state.countCalls, 0, 'A foreign owner cannot send the sealed source to any provider counter');
  await assertKnownModelReceipts(scenario, [source.value.id, replay.value.id, referenced.value.id]);
  return { provider_calls: 3, opaque_owner_bound_replay: true, stateless_item_reference: true,
    client_summary_not_authoritative: true, forged_context_rejected_before_dispatch: true,
    provider_counter_calls: 2, counter_received_authoritative_native_items: true,
    foreign_or_tampered_context_rejected_before_count: true };
}

