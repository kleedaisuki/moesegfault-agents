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
  }
}
