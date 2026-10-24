import assert from 'node:assert/strict';
import { createBrowserPrincipal } from './browser-principal.mjs';

const baseUrl = 'https://observer.example.com/v1';
const finalText = 'The durable observer job completed.';
const userKey = 'synthetic-observer-projection-user-key';
const toolId = 'observer-projection-time';

export function createObserverProviderFixture() {
  const state = { calls: 0, failure: null };
  return {
    state,
    async handle(request) {
      if (request.url !== `${baseUrl}/chat/completions`) return undefined;
      try {
        state.calls++;
        assert.ok(state.calls <= 2, 'Finalization recovery must never repeat model execution');
        assert.ok(request.headers.get('authorization') === `Bearer ${userKey}`);
        const input = await request.json();
        assert.equal(input.stream, true);
        if (state.calls === 2) assert.ok(input.messages.some(message => message.role === 'tool' && message.tool_call_id === toolId));
        const delta = state.calls === 1 ? { role: 'assistant', tool_calls: [{ index: 0, id: toolId, type: 'function', function: { name: 'time', arguments: '{}' } }] }
          : { role: 'assistant', content: finalText };
        return new Response(`data: ${JSON.stringify({ id: `observer-model-${state.calls}`, model: 'observer-custom-model', choices: [{ index: 0, delta, finish_reason: state.calls === 1 ? 'tool_calls' : 'stop' }], usage: { prompt_tokens: 100, completion_tokens: 10, total_tokens: 110, cost: 0.00001 } })}\n\ndata: [DONE]\n\n`, { headers: { 'Content-Type': 'text/event-stream' } });
      } catch (error) { state.failure = error; throw error; }
    },
  };
}

const pause = ms => new Promise(resolve => setTimeout(resolve, ms));

export async function runProjectionFaultJourney({ mf, db, encryptionKey, request, provider, diagnostic }) {
  const principal = await createBrowserPrincipal({ mf, db, encryptionKey, owner: 'observer-projection-owner' });
  const credential = await principal.browser('POST', '/v1/provider-keys', { protocol: 'openai', base_url: baseUrl, api_key: userKey }, 201);
  const agent = await request('POST', '/v1/agents', { name: 'Terminal projection fault', model: { provider: 'byok', credential_id: credential.id, model: 'observer-custom-model', context_window: 8192, max_output_tokens: 512 }, tools: ['time'], workspace: false }, 201, principal.apiKey);
  const session = await request('POST', `/v1/agents/${agent.id}/sessions`, {}, 201, principal.apiKey);
  await db.prepare('CREATE TABLE fixture_projection_fault(session_id TEXT PRIMARY KEY)').run();
  await db.prepare('INSERT INTO fixture_projection_fault(session_id) VALUES(?1)').bind(session.id).run();
  await db.prepare("CREATE TRIGGER fixture_fail_terminal_projection BEFORE UPDATE ON sessions WHEN EXISTS(SELECT 1 FROM fixture_projection_fault WHERE session_id=NEW.id) AND json_extract(OLD.config_snapshot_json,'$.status')='running' AND json_extract(NEW.config_snapshot_json,'$.status')='idle' BEGIN SELECT RAISE(ABORT,'fixture_terminal_projection_fault'); END").run();
  let reader;
  let wire = '';
  let eof = false;
  let readError;
  try {
    const response = await mf.dispatchFetch('https://agents.test/v1/chat/completions', {
      method: 'POST', headers: { Authorization: `Bearer ${principal.apiKey}`, 'Content-Type': 'application/json', 'X-Agents-Session': session.id },
      body: JSON.stringify({ model: agent.id, messages: [{ role: 'user', content: 'Call time and complete despite a transient index projection failure.' }], stream: true }),
    });
    assert.equal(response.status, 200);
    reader = response.body.getReader();
    const decoder = new TextDecoder();
    const reading = (async () => {
      try {
        while (true) {
          const value = await reader.read();
          if (value.done) { eof = true; break; }
          wire += decoder.decode(value.value, { stream: true });
        }
        wire += decoder.decode();
      } catch (error) { readError = error.name; }
    })();
    // Keep the fault enabled long enough to observe finalization, not just provider text.
    await pause(6000);
    if (provider.state.failure) throw provider.state.failure;
    const canonical = await request('GET', `/v1/sessions/${session.id}`, undefined, 200, principal.apiKey);
    const projection = await db.prepare("SELECT json_extract(config_snapshot_json,'$.status') AS status FROM sessions WHERE id=?1").bind(session.id).first();
    const settlementSql = "SELECT idempotency_key,state,CAST(settled_microusd AS TEXT) AS settled FROM usage_reservations WHERE session_id=?1 AND (idempotency_key LIKE 'run:%' OR idempotency_key LIKE 'completion:%') ORDER BY idempotency_key";
    const reservations = await db.prepare(settlementSql).bind(session.id).all();
    assert.ok(canonical.messages.some(message => message.role === 'assistant' && message.content === finalText), 'Canonical source history must already contain the completed answer');
    assert.equal(projection.status, 'running', 'The deliberately failing noncanonical projection must remain stale');
    assert.equal(canonical.projection_pending, true, 'Canonical history remains readable while projection repair is pending');
    assert.equal(reservations.results.length, 3);
    assert.ok(reservations.results.every(row => row.state === 'settled'), 'Observer repair must not bypass or repeat financial settlement');
    assert.equal(provider.state.calls, 2);
    diagnostic(JSON.stringify({ phase: 'terminal_projection_fault', canonical_status: canonical.status,
      canonical_run_pending: canonical.run_id !== null, projection_status: projection.status, settled_reservations: reservations.results.length,
      provider_calls: provider.state.calls, observer_eof: eof, observer_error_kind: readError ?? null,
      observer_done: wire.includes('data: [DONE]'), observer_stop: wire.includes('"finish_reason":"stop"') }));
    assert.ok(wire.includes('"finish_reason":"stop"'), 'Canonical completion must publish its terminal event while the derived D1 projection is unavailable');
    assert.ok(wire.trimEnd().endsWith('data: [DONE]'));
    assert.equal(eof, true);
    await db.prepare('DELETE FROM fixture_projection_fault WHERE session_id=?1').bind(session.id).run();
    const deadline = Date.now() + 12000;
    let repaired;
    do {
      repaired = await db.prepare("SELECT json_extract(config_snapshot_json,'$.status') AS status FROM sessions WHERE id=?1").bind(session.id).first();
      if (repaired.status === 'idle') break;
      await pause(100);
    } while (Date.now() < deadline);
    assert.equal(repaired.status, 'idle', 'The alarm must repair D1 without a public read or mutation triggering reconciliation');
    assert.equal(provider.state.calls, 2, 'Reconciliation must never repeat the model or tool');
    assert.deepEqual((await db.prepare(settlementSql).bind(session.id).all()).results, reservations.results);
    const recovered = await request('GET', `/v1/sessions/${session.id}`, undefined, 200, principal.apiKey);
    assert.equal(recovered.projection_pending, false);
    assert.deepEqual(recovered.messages, canonical.messages);
    await request('DELETE', `/v1/sessions/${session.id}`, undefined, 200, principal.apiKey);
  } finally {
    await db.prepare('DROP TRIGGER IF EXISTS fixture_fail_terminal_projection').run();
    await db.prepare('DROP TABLE IF EXISTS fixture_projection_fault').run();
    if (reader && !eof) await Promise.race([reader.cancel().catch(() => {}), pause(250)]);
  }
}
