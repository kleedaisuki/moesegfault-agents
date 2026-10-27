import { ConversationState as RustConversationState } from '../index.js';
export * from '../index.js';
export { default } from '../index.js';

function database(target, state) {
  const originals = new WeakMap();
  const sqls = new WeakMap();
  const count = async statements => {
    if (!state.armed) return;
    state.rpcs++;
    const sources = statements.map(item => sqls.get(item) ?? '');
    const business = sources.filter(sql => !/resource_observations|usage_ledger|usage_reservations/.test(sql));
    state.businessStatements += business.length;
    if (sources.length > 1) state.batches++;
    state.delayMs += state.rttMs;
    await new Promise(resolve => setTimeout(resolve, state.rttMs));
  };
  const statement = (value, sql) => {
    const wrapped = new Proxy(value, { get(object, key) {
      if (key === 'constructor') return object.constructor;
      if (key === 'bind') return (...args) => statement(object.bind(...args), sql);
      if (['run','all','first','raw'].includes(key)) return async (...args) => {
        await count([wrapped]);
        return object[key](...args);
      };
      const result = Reflect.get(object, key, object);
      return typeof result === 'function' ? result.bind(object) : result;
    } });
    originals.set(wrapped, value); sqls.set(wrapped, sql);
    return wrapped;
  };
  return new Proxy(target, { get(object, key) {
    if (key === 'constructor') return object.constructor;
    if (key === 'prepare') return sql => statement(object.prepare(sql), sql);
    if (key === 'batch') return async statements => {
      await count(statements);
      return object.batch(statements.map(item => originals.get(item) ?? item));
    };
    const result = Reflect.get(object, key, object);
    return typeof result === 'function' ? result.bind(object) : result;
  } });
}

export class ConversationState extends RustConversationState {
  constructor(ctx, env) {
    const state = { armed:false, rttMs:20, rpcs:0, batches:0, businessStatements:0, delayMs:0 };
    super(ctx, new Proxy(env, { get(object, key) { return key === 'DB' ? database(object.DB, state) : Reflect.get(object, key, object); } }));
    this.performanceState = state;
  }
  async fetch(request) {
    const path = new URL(request.url).pathname;
    if (path === '/__performance/arm') { Object.assign(this.performanceState, {armed:true,rpcs:0,batches:0,businessStatements:0,delayMs:0}); return Response.json(this.performanceState); }
    if (path === '/__performance/status') return Response.json(this.performanceState);
    return super.fetch(request);
  }
}
