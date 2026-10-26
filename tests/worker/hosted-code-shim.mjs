import RustWorker, { Session as RustSession } from '../index.js';
export * from '../index.js';
export { RustWorker as default };

// Test-only supervisor boundary. All harness state, leases, Files and events
// remain the compiled Rust implementation; real Python is tested in Docker.
export class Session extends RustSession {
  constructor(ctx, env) {
    let running = false;
    const container = {
      get running() { return running; },
      start() { running = true; },
      async destroy() { running = false; },
      async setInactivityTimeout() {},
      getTcpPort() {
        return { fetch(request) {
          const target = new URL(request.url);
          return fetch(new Request(`https://code-supervisor.fixture${target.pathname}${target.search}`, request));
        } };
      },
    };
    const state = new Proxy(ctx, { get(target, field) {
      if (field === 'container') return container;
      const value = Reflect.get(target, field, target);
      return typeof value === 'function' ? value.bind(target) : value;
    } });
    super(state, env);
    this.fixtureStorage = ctx.storage;
    this.fixtureContainer = container;
  }
  async fetch(request) {
    if (new URL(request.url).pathname !== '/__test/code-inspect') return super.fetch(request);
    const entries = await this.fixtureStorage.list({ prefix: 'hosted:code:' });
    const operations = [];
    for (const [key, header] of entries) {
      if (!header?.chunks || key.endsWith(':outcome')) continue;
      const chunks = await Promise.all(Array.from({ length: header.chunks }, (_, n) => this.fixtureStorage.get(`${key}:${header.generation}:${n}`)));
      const operation = JSON.parse(new TextDecoder().decode(Uint8Array.from(chunks.flat())));
      operations.push({ prepared: operation.prepared, submitted: operation.submitted, context: operation.context,
        failure: operation.failure, has_result: operation.result !== null });
    }
    return Response.json({ running: this.fixtureContainer.running, operations,
      boot_present: Boolean(await this.fixtureStorage.get('workspace:boot')),
      lease_present: Boolean(await this.fixtureStorage.get('workspace:lease')) });
  }
}
