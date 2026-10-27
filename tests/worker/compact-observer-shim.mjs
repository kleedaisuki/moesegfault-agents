import { ModelOperation as RustModelOperation } from '../index.js';
export * from '../index.js';
export { default } from '../index.js';

function stateWithKnownFence(ctx, fault) {
  const storage = new Proxy(ctx.storage, { get(target, name) {
    if (name === 'put') return async (key, value, ...rest) => {
      if (key === 'compact.receipt.v1' && value.phase === 'complete' && fault.holdComplete) {
        fault.knownBlocked = true;
        throw new Error('Fixture blocks complete publication after the original known result');
      }
      return target.put(key, value, ...rest);
    };
    const value = Reflect.get(target, name, target);
    return typeof value === 'function' ? value.bind(target) : value;
  } });
  return new Proxy(ctx, { get(target, name) {
    if (name === 'storage') return storage;
    const value = Reflect.get(target, name, target);
    return typeof value === 'function' ? value.bind(target) : value;
  } });
}

// Fixture-only controls around the actual Rust actor, never a public business API.
export class ModelOperation extends RustModelOperation {
  constructor(ctx, env) {
    const fault = { holdComplete: true, knownBlocked: false };
    super(stateWithKnownFence(ctx, fault), env);
    this.storage = ctx.storage;
    this.fault = fault;
  }
  async fetch(request) {
    const path = new URL(request.url).pathname;
    if (path === '/__compact/status') return Response.json({ receipt: await this.storage.get('compact.receipt.v1'), ...this.fault });
    if (path === '/__compact/release') { this.fault.holdComplete = false; return Response.json({ released: true }); }
    if (path === '/__compact/expire') {
      const receipt = await this.storage.get('compact.receipt.v1');
      if (receipt.phase !== 'complete') throw new Error('Only the genuinely completed original receipt may expire');
      receipt.expires_at_ms = Date.now() - 1;
      await this.storage.put('compact.receipt.v1', receipt);
      return Response.json({ expired: true });
    }
    return super.fetch(request);
  }
}
