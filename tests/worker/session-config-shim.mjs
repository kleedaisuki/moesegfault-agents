import { Session as FixtureSession } from './workspace-blob-shim.mjs';
export * from './execution-lease-shim.mjs';
export { default } from './execution-lease-shim.mjs';

async function read(storage, key) {
  const index = await storage.get(key);
  if (!index) throw Error('Fixture requires actual initialized Rust state');
  const parts = await Promise.all(Array.from({ length: index.chunks }, (_, i) => storage.get(`${key}:${index.generation}:${i}`)));
  return JSON.parse(new TextDecoder().decode(Uint8Array.from(parts.flat())));
}

async function write(storage, key, value) {
  const bytes = new TextEncoder().encode(JSON.stringify(value));
  const generation = `fixture-${crypto.randomUUID()}`;
  const size = 256 * 1024;
  const chunks = Math.ceil(bytes.length / size);
  for (let i = 0; i < chunks; i++) await storage.put(`${key}:${generation}:${i}`, Array.from(bytes.subarray(i * size, (i + 1) * size)));
  await storage.put(key, { generation, chunks, bytes: bytes.length });
}

// This fixture module is absent from release bundles. Only the issued-recovery
// case supplies a stopped container; any attempt to start/contact it fails.
export class Session extends FixtureSession {
  constructor(ctx, env) {
    const attempts = [];
    const stopped = { running: false, start() { attempts.push('start'); throw Error('Unexpected new container start'); },
      getTcpPort() { attempts.push('connect'); throw Error('Unexpected new container request'); },
      async setInactivityTimeout() {}, async destroy() { attempts.push('destroy'); } };
    const state = env.SESSION_CONFIG_ISSUED_FIXTURE === '1' ? new Proxy(ctx, { get(target, field) {
      if (field === 'container') return stopped;
      const value = Reflect.get(target, field, target);
      return typeof value === 'function' && field !== 'constructor' ? value.bind(target) : value;
    } }) : ctx;
    super(state, env);
    this.fixtureAttempts = attempts;
  }

  async fetch(request) {
    const path = new URL(request.url).pathname;
    if (path === '/__test/configuration-issued-inspect') {
      const state = await read(this.fixtureStorage, 'session.v1');
      const fences = Object.fromEntries(await this.fixtureStorage.list({ prefix: 'workspace:dispatch:' }));
      return Response.json({ state, fences, boot: await this.fixtureStorage.get('workspace:boot'), attempts: this.fixtureAttempts });
    }
    if (path !== '/__test/configuration-issued-seed') return super.fetch(request);
    const { run_id, call_id, execution_id, boot } = await request.json();
    const state = await read(this.fixtureStorage, 'session.v1');
    if (state.run || state.journal_count || !state.record.pinned.workspace || !state.record.pinned.tools.some(tool => tool.name === 'bash'))
      throw Error('Issued recovery fixture requires its own empty, authorized Bash Session');
    const message = { role: 'assistant', content: '', tool_calls: [{ id: call_id, type: 'function',
      function: { name: 'bash', arguments: '{"command":"printf must-not-replay"}' } }] };
    state.messages.push(message);
    state.record.status = 'running';
    state.journal_count = 1;
    state.run = { id: run_id, round: 1, admitted: false,
      total: { prompt_tokens: 0, completion_tokens: 0, total_tokens: 0, cached_tokens: 0, reported: false },
      pending_completion: null, started_at_ms: Date.now() };
    const digest = Array.from(new Uint8Array(await crypto.subtle.digest('SHA-256', new TextEncoder().encode(execution_id))))
      .map(byte => byte.toString(16).padStart(2, '0')).join('');
    await this.fixtureStorage.put(`workspace:dispatch:${digest}`, boot);
    await this.fixtureStorage.put('workspace:boot', boot);
    await write(this.fixtureStorage, 'journal:0', message);
    await this.fixtureStorage.put('journal:count', 1);
    await write(this.fixtureStorage, 'session.v1', state);
    return Response.json({ seeded: true, execution_id, fence: `workspace:dispatch:${digest}` });
  }
}
