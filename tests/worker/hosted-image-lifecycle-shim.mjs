import RustWorker, { Session as RustSession } from '../index.js';
export * from '../index.js';

// This fixture is absent from release modules. It observes real Rust state and
// advances only the controlled test clock; it never manufactures tool outcomes.
const NativeDate = Date;
const nativeNow = NativeDate.now.bind(NativeDate);
let offset = 0;
globalThis.Date = class extends NativeDate {
  constructor(...values) { super(...(values.length ? values : [nativeNow() + offset])); }
  static now() { return nativeNow() + offset; }
};

async function clock(request) {
  const next = (await request.json()).offset;
  if (!Number.isSafeInteger(next) || next < 0) throw Error('Invalid fixture clock');
  offset = next;
  return Response.json({ now: Date.now(), constructed_now: new Date().getTime() });
}

async function read(storage, key) {
  const index = await storage.get(key);
  if (!index) return null;
  const parts = await Promise.all(Array.from({ length: index.chunks }, (_, n) =>
    storage.get(`${key}:${index.generation}:${n}`)));
  return JSON.parse(new TextDecoder().decode(Uint8Array.from(parts.flat())));
}

export class Session extends RustSession {
  constructor(ctx, env) { super(ctx, env); this.fixtureStorage = ctx.storage; }
  async fetch(request) {
    const path = new URL(request.url).pathname;
    if (path === '/__test/hosted-image-clock') return clock(request);
    if (path !== '/__test/hosted-image-inspect') return super.fetch(request);
    const { responseId } = await request.json();
    const state = await read(this.fixtureStorage, 'session.v1');
    const retained = await read(this.fixtureStorage, `response:${responseId}`);
    const response = state?.run?.response?.id === responseId ? state.run.response : retained;
    if (!response) return Response.json({ found: false });
    const serialized = JSON.stringify(response);
    return Response.json({ found: true, status: response.status, store: response.request.store,
      canonical_bytes: new TextEncoder().encode(serialized).length,
      images: response.images ?? [], image_lifecycle: response.image_lifecycle ?? null,
      execution: response.execution,
      inline_final: response.output.some(item => item.type === 'image_generation_call' && typeof item.result === 'string'),
      inline_partial: response.events.some(event => typeof event.partial_image_b64 === 'string') });
  }
}

export default class extends RustWorker {
  async fetch(request) {
    const url = new URL(request.url);
    if (url.pathname === '/__test/hosted-image-clock') {
      return clock(request);
    }
    if (url.pathname === '/__test/hosted-image-sweep') {
      const pending = [];
      const ctx = { waitUntil(promise) { pending.push(promise); } };
      await RustWorker.prototype.scheduled.call({ env: this.env, ctx },
        { scheduledTime: Date.now(), cron: '* * * * *', noRetry() {} });
      await Promise.all(pending);
      return Response.json({ swept_at: Date.now() });
    }
    return super.fetch(request);
  }
}
