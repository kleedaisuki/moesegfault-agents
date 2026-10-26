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
    const path = new URL(request.url).pathname;
    if (path === '/__test/code-drop') {
      const stored = await this.fixtureStorage.get('workspace:lease');
      const lease = typeof stored === 'string' ? JSON.parse(stored) : stored;
      await this.fixtureContainer.destroy();
      return Response.json({ lost: true, lease_id: lease?.id ?? null });
    }
    if (path !== '/__test/code-inspect') return super.fetch(request);
    const entries = await this.fixtureStorage.list({ prefix: 'hosted:code:' });
    const operations = [];
    for (const [key, header] of entries) {
      if (!header?.chunks || key.endsWith(':outcome')) continue;
      const chunks = await Promise.all(Array.from({ length: header.chunks }, (_, n) => this.fixtureStorage.get(`${key}:${header.generation}:${n}`)));
      const operation = JSON.parse(new TextDecoder().decode(Uint8Array.from(chunks.flat())));
      operations.push({ prepared: operation.prepared, submitted: operation.submitted, context: operation.context,
        failure: operation.failure, has_result: operation.result !== null });
    }
    let borrowedReceipts = 0, publishedBorrowed = 0, borrowedOutcomeBytes = 0;
    for (const [key, header] of await this.fixtureStorage.list({ prefix: 'code-borrowed:' })) {
      if (!header?.chunks || key.endsWith(':outcome')) continue;
      const chunks = await Promise.all(Array.from({ length: header.chunks }, (_, n) => this.fixtureStorage.get(`${key}:${header.generation}:${n}`)));
      const receipt = JSON.parse(new TextDecoder().decode(Uint8Array.from(chunks.flat())));
      borrowedReceipts++;
      if (receipt.stage === 'published') publishedBorrowed++;
      borrowedOutcomeBytes += (await this.fixtureStorage.get(`${key}:outcome`))?.bytes ?? 0;
    }
    return Response.json({ running: this.fixtureContainer.running, operations,
      borrowed_receipts: borrowedReceipts, published_borrowed_receipts: publishedBorrowed, borrowed_outcome_bytes: borrowedOutcomeBytes,
      boot_present: Boolean(await this.fixtureStorage.get('workspace:boot')),
      lease_present: Boolean(await this.fixtureStorage.get('workspace:lease')) });
  }
}
