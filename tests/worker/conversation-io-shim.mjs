import { ConversationState as RustConversationState } from '../index.js';
export * from '../index.js';
export { default } from '../index.js';

function database(target, state) {
  const statement = (value, sql) => new Proxy(value, { get(object, key) {
    if (key === 'constructor') return object.constructor;
    if (key === 'bind') return (...args) => statement(object.bind(...args), sql);
    if (key === 'all') return async (...args) => {
      if (state.armed && sql.startsWith('WITH desired AS MATERIALIZED')) {
        state.armed = false; state.entered = true; state.delays++;
        await new Promise(resolve => setTimeout(resolve, 33000));
      }
      return object.all(...args);
    };
    const result = Reflect.get(object, key, object);
    return typeof result === 'function' ? result.bind(object) : result;
  } });
  return new Proxy(target, { get(object, key) {
    if (key === 'constructor') return object.constructor;
    if (key === 'prepare') return sql => statement(object.prepare(sql), sql);
    const result = Reflect.get(object, key, object);
    return typeof result === 'function' ? result.bind(object) : result;
  } });
}

// A real >30s asynchronous D1 boundary in the actual Rust actor; fixture only.
export class ConversationState extends RustConversationState {
  constructor(ctx, env) {
    const state = { armed: false, entered: false, delays: 0, platformBlocks: 0 };
    const wrappedContext = new Proxy(ctx, { get(object, key) {
      if (key === 'constructor') return object.constructor;
      if (key === 'blockConcurrencyWhile') return (...args) => { state.platformBlocks++; return object.blockConcurrencyWhile(...args); };
      const result = Reflect.get(object, key, object);
      return typeof result === 'function' ? result.bind(object) : result;
    } });
    const wrappedEnv = new Proxy(env, { get(object, key) { return key === 'DB' ? database(object.DB, state) : Reflect.get(object, key, object); } });
    super(wrappedContext, wrappedEnv);
    this.ioState = state;
  }
  async fetch(request) {
    const path = new URL(request.url).pathname;
    if (path === '/__io/arm') { this.ioState.armed = true; return Response.json(this.ioState); }
    if (path === '/__io/status') return Response.json(this.ioState);
    return super.fetch(request);
  }
}
