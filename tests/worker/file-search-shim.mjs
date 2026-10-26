import RustWorker, { Session as RustSession } from '../index.js';
export * from '../index.js';

async function invoke(method, args) {
  const response = await fetch(`https://retrieval.fixture.test/${method}`, {
    method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(args),
  });
  if (!response.ok) throw Error('controlled_retrieval_service_failed');
  return response.json();
}

class Ai {
  run(model, input) { return invoke('embed', [model, input]); }
}

function bindings(env) {
  return { ...env,
    AI: new Ai(),
    VECTORIZE: Object.fromEntries(['query', 'upsert', 'deleteByIds', 'getByIds', 'describe']
      .map(method => [method, (...args) => invoke(method, args)])),
  };
}

export class Session extends RustSession {
  constructor(ctx, env) { super(ctx, bindings(env)); }
}

export default class extends RustWorker {
  async fetch(request) {
    return RustWorker.prototype.fetch.call({ env: bindings(this.env), ctx: this.ctx }, request);
  }
  async scheduled(event) {
    return RustWorker.prototype.scheduled.call({ env: bindings(this.env), ctx: this.ctx }, event);
  }
}
