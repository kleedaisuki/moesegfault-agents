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
    const state = { armed: false, entered: false, delays: 0, platformBlocks: 0, loseReserveAck: false, ackLost: false };
    const wrappedContext = new Proxy(ctx, { get(object, key) {
      if (key === 'constructor') return object.constructor;
      if (key === 'blockConcurrencyWhile') return (...args) => { state.platformBlocks++; return object.blockConcurrencyWhile(...args); };
      const result = Reflect.get(object, key, object);
      return typeof result === 'function' ? result.bind(object) : result;
    } });
    const wrappedEnv = new Proxy(env, { get(object, key) { return key === 'DB' ? database(object.DB, state) : Reflect.get(object, key, object); } });
    super(wrappedContext, wrappedEnv);
    this.ioState = state;
    this.fixtureStorage = ctx.storage;
  }
  async fetch(request) {
    const path = new URL(request.url).pathname;
    if (path === '/__io/arm') { this.ioState.armed = true; return Response.json(this.ioState); }
    if (path === '/__io/status') return Response.json(this.ioState);
    if (path === '/__io/lose-reserve') { this.ioState.loseReserveAck = true; return Response.json(this.ioState); }
    if (path === '/__io/expire-pending') {
      const index = await this.fixtureStorage.get('conversation.v1');
      if (index.chunks !== 1) throw new Error('This bounded fixture header must occupy one chunk');
      const key = `conversation.v1:${index.generation}:0`;
      const record = JSON.parse(new TextDecoder().decode(new Uint8Array(await this.fixtureStorage.get(key))));
      if (record.active_activated || !record.pending_expires_at_ms) throw new Error('Cannot expire active/unknown work');
      record.pending_expires_at_ms = Date.now() - 1;
      const bytes = new TextEncoder().encode(JSON.stringify(record));
      await this.fixtureStorage.put(key, Array.from(bytes));
      await this.fixtureStorage.put('conversation.v1', { ...index, bytes: bytes.length });
      return Response.json({ expired: true });
    }
    const response = await super.fetch(request);
    if (path === '/responses/reserve' && this.ioState.loseReserveAck && response.status === 200) {
      this.ioState.loseReserveAck = false; this.ioState.ackLost = true;
      await response.arrayBuffer();
      throw new Error('Fixture loses the successful reserve acknowledgment before Session acceptance');
    }
    return response;
  }
}
