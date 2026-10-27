import assert from 'node:assert/strict';
import { createNativeSemanticsScenario } from './native-semantics-journey.mjs';

async function until(probe, description) {
  const end = Date.now() + 15000;
  while (Date.now() < end) {
    const value = await probe();
    if (value) return value;
    await new Promise(resolve => setTimeout(resolve, 10));
  }
  throw new Error(description);
}

export async function runCompactReceiptObserveJourney(context) {
  let release;
  const gate = { promise: new Promise(resolve => { release = resolve; }), resolve: () => release() };
  const scenario = await createNativeSemanticsScenario(context, 'compact-receipt-observe', [{
    gate, output: [{ type: 'message', id: 'summary-source', role: 'assistant', status: 'completed',
      content: [{ type: 'output_text', text: 'Preserved frozen source goal.', annotations: [], logprobs: [] }] }],
    check: body => assert.ok(JSON.stringify(body.input).includes('Frozen compact receipt source')),
  }]);
  const key = 'one-new-compact-receipt';
  const body = { model: scenario.agent.id, input: [{ role: 'user', content: [{ type: 'input_file',
    file_url: 'https://compact-source.example.com/note.txt', filename: 'note.txt' }] }], instructions: 'Summarize the frozen goal.' };
  const compact = (value = body, bearer = scenario.principal.apiKey) => context.mf.dispatchFetch('https://agents.test/v1/responses/compact', {
    method: 'POST', headers: { Authorization: 'Bearer ' + bearer, 'Content-Type': 'application/json', 'Idempotency-Key': key },
    body: JSON.stringify(value),
  });
  const original = compact();
  await until(() => scenario.state.calls === 1, 'Original compaction did not enter the controlled provider');
  assert.equal(context.compactSource.fetches, 1);
  const operation = (await context.db.prepare("SELECT id FROM usage_reservations WHERE account_id=? AND idempotency_key LIKE 'run:cmp_%'")
    .bind(scenario.principal.owner).first()).id;
  const namespace = await context.mf.getDurableObjectNamespace('MODEL_OPERATIONS');
  const actor = () => namespace.get(namespace.idFromName(operation));
  const status = async () => (await actor().fetch('https://fixture/__compact/status')).json();
  context.compactSource.available = false;
  await context.request('DELETE', `/v1/agents/${scenario.agent.id}`, undefined, 200, scenario.principal.apiKey);
  const pendingFollower = compact();
  const conflict = await compact({ ...body, instructions: 'Different but valid request body.' });
  assert.equal(conflict.status, 409);
  assert.equal((await conflict.json()).error.code, 'idempotency_conflict');
  gate.resolve();
  await until(async () => { const value = await status(); return value.knownBlocked && value.receipt.phase === 'known' && value; },
    'The original known provider result was not durably fenced');
  const knownFollower = compact();
  await actor().fetch('https://fixture/__compact/release', { method: 'POST' });
  const completed = await until(async () => (await status()).receipt.phase === 'complete', 'Known accounting recovery did not finish');
  assert.equal(completed, true);
  const responses = await Promise.all([original, pendingFollower, knownFollower]);
  const values = [];
  for (const response of responses) {
    assert.equal(response.status, 200, await response.clone().text());
    assert.equal(response.headers.get('x-agents-operation'), operation);
    assert.equal(response.headers.get('idempotency-key'), key);
    values.push(await response.json());
  }
  assert.equal(values[0].object, 'response.compaction');
  assert.deepEqual(values[1], values[0]); assert.deepEqual(values[2], values[0]);
  await context.mf.unsafeEvictDurableObject('', 'ModelOperation', { name: operation });
  const replay = await compact();
  assert.equal(replay.status, 200);
  assert.deepEqual(await replay.json(), values[0], 'Completed replay reads original immutable output without the deleted Agent/source');
  assert.equal(context.compactSource.fetches, 1, 'No retry can re-fetch the unavailable original source');
  const saved = (await status()).receipt;
  const foreign = await actor().fetch('https://model-operation/observe', { method: 'POST',
    body: JSON.stringify({ owner: 'not-the-owner', operation, request_hash: saved.request_hash }) });
  assert.equal(foreign.status, 409, 'Internal lookup still verifies the bound owner');
  await actor().fetch('https://fixture/__compact/expire', { method: 'POST' });
  await context.mf.unsafeEvictDurableObject('', 'ModelOperation', { name: operation });
  const expired = await compact();
  assert.equal(expired.status, 410);
  assert.equal((await expired.json()).error.code, 'compaction_expired');
  const expiredAgain = await compact();
  assert.equal(expiredAgain.status, 410);
  const expiredConflict = await compact({ ...body, instructions: 'Different but valid request body.' });
  assert.equal(expiredConflict.status, 409);
  assert.equal((await status()).receipt.phase, 'expired');
  const ledger = (await context.db.prepare(`SELECT r.state,l.price_version FROM usage_ledger l
    JOIN usage_reservations r ON r.id=l.reservation_id WHERE r.account_id=? AND l.resource='model'`)
    .bind(scenario.principal.owner).all()).results;
  assert.equal(ledger.length, 1);
  assert.deepEqual(ledger[0], { state: 'settled', price_version: 'byok' });
  assert.equal(scenario.state.calls, 1);
  assert.equal(context.compactSource.fetches, 1);
  return { provider_calls: 1, original_remote_fetches: 1, pending_and_known_followers_reused: true,
    completed_after_agent_delete_and_source_loss: true, cold_exact_replay: true, owner_binding_checked: true,
    different_valid_body_conflicts: true, expired_replay_410: true, model_ledger_rows: 1, external_network_calls: 0 };
}
