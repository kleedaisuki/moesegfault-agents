import { ConversationState as RustConversationState } from '../index.js';
export * from '../index.js';
export { default } from '../index.js';

// Fixture-only fault injection around the actual compiled Rust actor. Neither
// the released bundle nor public API imports this module or its control routes.
function database(target, fault) {
  function statement(value, sql) {
    return new Proxy(value, {
      get(object, key) {
        if (key === 'constructor') return object.constructor;
        if (key === 'bind') return (...args) => statement(object.bind(...args), sql);
        if (key === 'all') return async (...args) => {
          if (fault.commit && sql.startsWith("UPDATE conversation_capacity_objects SET state='retained'")) {
            fault.commit = false;
            throw new Error('Synthetic lost quota-commit database acknowledgment');
          }
          return object.all(...args);
        };
        const result = Reflect.get(object, key, object);
        return typeof result === 'function' ? result.bind(object) : result;
      },
    });
  }
  return new Proxy(target, {
    get(object, key) {
      if (key === 'constructor') return object.constructor;
      if (key === 'prepare') return sql => statement(object.prepare(sql), sql);
      const result = Reflect.get(object, key, object);
      return typeof result === 'function' ? result.bind(object) : result;
    },
  });
}

function context(target, fault) {
  const storage = new Proxy(target.storage, {
    get(object, key) {
      if (key === 'constructor') return object.constructor;
      if (key === 'put') return async (...args) => {
        const result = await object.put(...args);
        if (fault.header && args[0] === 'conversation.v1') {
          fault.header = false;
          throw new Error('Synthetic lost canonical header acknowledgment');
        }
        return result;
      };
      const result = Reflect.get(object, key, object);
      return typeof result === 'function' ? result.bind(object) : result;
    },
  });
  return new Proxy(target, {
    get(object, key) {
      if (key === 'constructor') return object.constructor;
      if (key === 'storage') return storage;
      const result = Reflect.get(object, key, object);
      return typeof result === 'function' ? result.bind(object) : result;
    },
  });
}

async function stored(storage, key) {
  const index = await storage.get(key);
  if (!index) return null;
  const chunks = await Promise.all(Array.from({ length: index.chunks }, (_, offset) =>
    storage.get(`${key}:${index.generation}:${offset}`)));
  const bytes = new Uint8Array(chunks.reduce((length, chunk) => length + chunk.length, 0));
  let offset = 0;
  for (const chunk of chunks) { bytes.set(chunk, offset); offset += chunk.length; }
  return JSON.parse(new TextDecoder().decode(bytes));
}

export class ConversationState extends RustConversationState {
  constructor(ctx, env) {
    const fault = { commit: false, header: false };
    const wrapped = new Proxy(env, {
      get(object, key) { return key === 'DB' ? database(object.DB, fault) : Reflect.get(object, key, object); },
    });
    super(context(ctx, fault), wrapped);
    this.fixtureFault = fault;
    this.fixtureStorage = ctx.storage;
  }

  async fetch(request) {
    const path = new URL(request.url).pathname;
    if (path === '/__quota/fault') {
      const { phase } = await request.json();
      if (!['commit', 'header'].includes(phase)) throw new Error('Unknown fixture fault');
      this.fixtureFault[phase] = true;
      return Response.json({ armed: phase });
    }
    if (path === '/__quota/state') return Response.json({
      record: await stored(this.fixtureStorage, 'conversation.v1'),
      intent: await stored(this.fixtureStorage, 'conversation.quota-publication.v1'),
    });
    return super.fetch(request);
  }
}
