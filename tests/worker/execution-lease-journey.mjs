import assert from 'node:assert/strict';
import { createHash, randomUUID } from 'node:crypto';
import { createBrowserPrincipal } from './browser-principal.mjs';

const sha = value => createHash('sha256').update(value).digest('hex');
async function bounded(promise) {
  let timer;
  try { return await Promise.race([promise, new Promise((_, reject) => {
    timer = setTimeout(() => reject(new Error('Execution-lease terminal rejection did not arrive')), 8000);
  })]); } finally { clearTimeout(timer); }
}
async function receipt(namespace, operation) {
  const stub = namespace.get(namespace.idFromName(operation));
  return (await stub.fetch('https://fixture/__test/execution-receipt')).json();
}

export async function runExecutionLeaseJourney({ mf, db, encryptionKey, request, provider }) {
  const principal = await createBrowserPrincipal({ mf, db, encryptionKey, owner: 'execution-lease-owner' });
  await principal.browser('GET', '/v1/account');
  const credential = await principal.browser('POST', '/v1/provider-keys', {
    protocol: 'openai', base_url: 'https://auxiliary-provider.example.com/v1', api_key: 'synthetic-auxiliary-owned-key' }, 201);
  const agent = await request('POST', '/v1/agents', { name: 'Execution lease native',
    model: { provider: 'byok', credential_id: credential.id, protocol: 'responses', model: 'fixture-native-model',
      context_window: 8192, max_output_tokens: 64 }, tools: ['time'], workspace: false }, 201, principal.apiKey);
  const namespace = await mf.getDurableObjectNamespace('MODEL_OPERATIONS');
  const operation = `cmp_${sha('cold-lease-' + randomUUID())}`;
  const generation = randomUUID();
  const prepared = { owner: principal.owner, operation, request_hash: 'a'.repeat(64), agent: agent.id,
    model: { provider: 'byok', credential_id: credential.id, protocol: 'responses', model: 'fixture-native-model',
      context_window: 8192, max_output_tokens: 64 }, prefix_hash: 'b'.repeat(64), instructions: null,
    protected: [], messages: [{ role: 'user', content: 'Preserve cold-loss evidence.' }], retained: [],
    guidance: null, controls: {}, traceparent: null };
  const cold = { owner: principal.owner, operation, request_hash: prepared.request_hash,
    created_at_ms: Date.now() - 3 * 86400000, expires_at_ms: Date.now() - 2 * 86400000,
    phase: 'in_flight', admitted: true, execution_generation: generation, rejected_before_dispatch: false,
    error: null, duration_ms: null };
  await db.prepare("INSERT INTO usage_reservations(id,account_id,idempotency_key,request_hash,reserved_microusd,state,created_at,updated_at,execution_generation) VALUES(?,?,?,?,30,'reserved',1,1,?)")
    .bind(operation, principal.owner, `run:${operation}`, 'seeded-original-identity', generation).run();
  await db.prepare("INSERT INTO usage_reservations(id,account_id,idempotency_key,request_hash,reserved_microusd,state,created_at,updated_at) VALUES(?,?,?,?,20,'reserved',1,1)")
    .bind(`completion-${operation}`, principal.owner, `completion:${operation}:compact`, 'seeded-model-identity').run();
  for (const [event, resource] of [['compact.worker', 'worker'], ['compact.actor', 'durable_object']]) {
    await db.prepare("INSERT INTO resource_observations(run_id,account_id,session_id,event_key,resource,unit,quantity,price_version,observed_at,status) VALUES(?,?,NULL,?,?,'wall_ms',NULL,'fixture-pending-unknown',NULL,'pending')")
      .bind(operation, principal.owner, event, resource).run();
  }
  const stub = namespace.get(namespace.idFromName(operation));
  assert.equal((await stub.fetch('https://fixture/__test/execution-seed', { method: 'POST', body: JSON.stringify({ receipt: cold, prepared }) })).status, 200);
  const beforeCalls = provider.state.calls.length;
  const failed = await bounded(stub.fetch('https://model-operation/compact', { method: 'POST', body: JSON.stringify(prepared) }));
  const wire = await bounded(failed.text());
  assert.ok(wire.includes('compaction_outcome_unknown'));
  // The observer is notified before cleanup; wait only for the new evidence, not a task lifetime.
  const until = Date.now() + 5000;
  let state;
  do {
    state = await db.prepare('SELECT state FROM execution_leases WHERE reservation_id=?').bind(operation).first('state');
    if (state === 'no_local_execution') break;
    await new Promise(resolve => setTimeout(resolve, 10));
  } while (Date.now() < until);
  assert.equal(state, 'no_local_execution');
  assert.equal(await db.prepare('SELECT CAST(reserved_microusd AS TEXT) AS held FROM usage_accounts WHERE account_id=?').bind(principal.owner).first('held'), '50');
  assert.equal(await db.prepare('SELECT COUNT(*) AS n FROM usage_ledger l JOIN usage_reservations r ON r.id=l.reservation_id WHERE r.account_id=?').bind(principal.owner).first('n'), 0);
  assert.equal(await db.prepare("SELECT COUNT(*) AS n FROM resource_observations WHERE run_id=? AND status='pending' AND quantity IS NULL").bind(operation).first('n'), 2,
    'Cold loss must not fabricate zero-duration observations to clear funding');
  const recovered = await receipt(namespace, operation);
  assert.equal(recovered.phase, 'failed');
  assert.ok(recovered.expires_at_ms > Date.now() + 23 * 3600000, 'Retention begins at cold terminal recovery, not old creation');
  const replay = await stub.fetch('https://model-operation/compact', { method: 'POST', body: JSON.stringify(prepared) });
  assert.equal(replay.status, 200, 'Private observer transport carries its terminal status inside SSE');
  assert.ok((await bounded(replay.text())).includes('compaction_outcome_unknown'));
  assert.equal(provider.state.calls.length, beforeCalls, 'Unknown old request never redispatches');
  let releaseProvider;
  provider.state.pause = new Promise(resolve => { releaseProvider = resolve; });
  const nextPending = mf.dispatchFetch('https://agents.test/v1/responses/compact', {
    method: 'POST', headers: { Authorization: `Bearer ${principal.apiKey}`, 'Content-Type': 'application/json', 'Idempotency-Key': 'after-cold-lease' },
    body: JSON.stringify({ model: agent.id, input: 'New independent operation uses the freed Free slot.' }) });
  let next;
  try {
    const waitingUntil = Date.now() + 5000;
    while (provider.state.calls.length !== beforeCalls + 1) {
      assert.ok(Date.now() < waitingUntil, 'The controlled provider must become active before testing concurrency');
      await new Promise(resolve => setTimeout(resolve, 10));
    }
    const competing = await bounded(mf.dispatchFetch('https://agents.test/v1/responses/compact', {
      method: 'POST', headers: { Authorization: `Bearer ${principal.apiKey}`, 'Content-Type': 'application/json',
        'Idempotency-Key': 'competing-with-active-operation' },
      body: JSON.stringify({ model: agent.id, input: 'This concurrent operation must reject before provider dispatch.' }) }));
    assert.equal(competing.status, 429, await competing.clone().text());
    assert.equal((await competing.json()).error.code, 'run_limit_exceeded');
    const competingId = competing.headers.get('x-agents-operation');
    assert.ok(competingId);
    const competingReceipt = await receipt(namespace, competingId);
    assert.equal(competingReceipt.phase, 'failed');
    assert.equal(competingReceipt.rejected_before_dispatch, true);
    assert.equal(await db.prepare('SELECT COUNT(*) AS n FROM usage_reservations WHERE id=?').bind(competingId).first('n'), 0,
      'Capacity rejection cannot leave a new monetary hold');
    assert.equal(provider.state.calls.length, beforeCalls + 1, 'A live local operation retains its slot regardless of an unrelated unknown hold');
  } finally {
    releaseProvider();
    provider.state.pause = null;
    next = await bounded(nextPending);
  }
  assert.equal(next.status, 200, await next.clone().text());
  await bounded(next.text());
  assert.equal(provider.state.calls.length, beforeCalls + 1);
  const completedId = next.headers.get('x-agents-operation');
  const completedUntil = Date.now() + 5000;
  while ((await db.prepare('SELECT state FROM execution_leases WHERE reservation_id=?').bind(completedId).first('state')) !== 'no_local_execution') {
    assert.ok(Date.now() < completedUntil, 'Published success must complete its independent local-lease cleanup');
    await new Promise(resolve => setTimeout(resolve, 10));
  }

  const missingBody = JSON.stringify({ model: agent.id, input: 'Provider returns durable output without usage.' });
  provider.state.omitUsage = true;
  let unknownOperation;
  try {
    for (let retry = 0; retry < 2; retry++) {
      const unknown = await bounded(mf.dispatchFetch('https://agents.test/v1/responses/compact', {
        method: 'POST', headers: { Authorization: `Bearer ${principal.apiKey}`, 'Content-Type': 'application/json',
          'Idempotency-Key': 'known-result-missing-usage' }, body: missingBody }));
      assert.equal(unknown.status, 503, await unknown.clone().text());
      assert.equal((await unknown.json()).error.code, 'compaction_usage_unknown');
      unknownOperation = unknown.headers.get('x-agents-operation');
      const terminal = await receipt(namespace, unknownOperation);
      assert.equal(terminal.phase, 'failed');
      assert.equal(terminal.rejected_before_dispatch, false, 'Known provider I/O never uses unused-hold refund proof');
    }
  } finally { provider.state.omitUsage = false; }
  assert.equal(provider.state.calls.length, beforeCalls + 2, 'Known missing usage never replays its immutable provider result');
  const usageDeadline = Date.now() + 5000;
  let missingLease;
  do {
    missingLease = await db.prepare('SELECT state FROM execution_leases WHERE reservation_id=?').bind(unknownOperation).first('state');
    if (missingLease === 'no_local_execution') break;
    await new Promise(resolve => setTimeout(resolve, 10));
  } while (Date.now() < usageDeadline);
  assert.equal(missingLease, 'no_local_execution');
  assert.equal(await db.prepare('SELECT state FROM usage_reservations WHERE account_id=? AND idempotency_key=?')
    .bind(principal.owner, `completion:${unknownOperation}:compact`).first('state'), 'reserved',
    'Unknown provider usage keeps its original model hold even when platform BYOK amount is zero');

  for (const [stage, remaining] of [['admission', 0], ['completion', 20000]]) {
    const denied = await createBrowserPrincipal({ mf, db, encryptionKey, owner: `execution-denied-${stage}-owner` });
    await denied.browser('GET', '/v1/account');
    const deniedAgent = await request('POST', '/v1/agents', { name: 'Rejected compact', model: { provider: 'openrouter', model: 'test-model', context_window: 10000, max_output_tokens: 1000 }, tools: ['time'], workspace: false }, 201, denied.apiKey);
    const budget = await db.prepare('SELECT CAST(budget_microusd AS TEXT) AS budget FROM usage_accounts WHERE account_id=?').bind(denied.owner).first('budget');
    const held = (BigInt(budget) - BigInt(remaining)).toString();
    await db.prepare("INSERT INTO usage_reservations(id,account_id,idempotency_key,request_hash,reserved_microusd,state,created_at,updated_at) VALUES(?,?,?,?,CAST(? AS INTEGER),'reserved',1,1)")
      .bind(`denied-untouched-hold-${stage}`, denied.owner, 'financial:unknown', 'unknown-not-a-run', held).run();
    const deniedBody = JSON.stringify({ model: deniedAgent.id, input: 'Reject before model dispatch.' });
    for (let retry = 0; retry < 2; retry++) {
      const rejected = await bounded(mf.dispatchFetch('https://agents.test/v1/responses/compact', { method: 'POST', headers: {
        Authorization: `Bearer ${denied.apiKey}`, 'Content-Type': 'application/json', 'Idempotency-Key': 'denied-permanent' }, body: deniedBody }));
      assert.equal(rejected.status, 402, await rejected.clone().text());
      assert.equal((await bounded(rejected.json())).error.code, 'insufficient_credits');
      const rejectedReceipt = await receipt(namespace, rejected.headers.get('x-agents-operation'));
      assert.equal(rejectedReceipt.phase, 'failed');
      assert.equal(rejectedReceipt.rejected_before_dispatch, true);
    }
    const deadline = Date.now() + 5000;
    let actual;
    do {
      actual = await db.prepare('SELECT CAST(reserved_microusd AS TEXT) AS held FROM usage_accounts WHERE account_id=?').bind(denied.owner).first('held');
      if (actual === held) break;
      await new Promise(resolve => setTimeout(resolve, 10));
    } while (Date.now() < deadline);
    assert.equal(actual, held, 'Only the proven-unused new run hold is refunded; unrelated unknown hold stays');
    assert.equal(await db.prepare("SELECT COUNT(*) AS n FROM execution_leases WHERE account_id=? AND state='active'").bind(denied.owner).first('n'), 0);
  }
}
