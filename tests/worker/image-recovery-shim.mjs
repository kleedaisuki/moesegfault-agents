import RustWorker, { Session as RustSession } from '../index.js';
export * from '../index.js';

async function read(storage, key) {
  const index = await storage.get(key);
  if (!index) return null;
  const parts = await Promise.all(Array.from({ length: index.chunks }, (_, n) => storage.get(`${key}:${index.generation}:${n}`)));
  return JSON.parse(new TextDecoder().decode(Uint8Array.from(parts.flat())));
}

function bucket(target, fault, storage) {
  return new Proxy(target, { get(object, key) {
    if (key === 'constructor') return object.constructor;
    if (key === 'put') return async (...args) => {
      fault.injected ||= !!await storage.get('__test.image-recovery.injected');
      const name = String(args[0]);
      const raw = name.startsWith('provider-response-receipts/');
      const artifact = name.startsWith('owner-blobs/') && name.includes('/image/');
      if (raw) {
        const value = typeof args[1] === 'string' ? args[1] : new TextDecoder().decode(args[1]);
        fault.receiptWrites++;
        fault.encrypted = !value.includes('b64_json') && !value.includes('gen-img-controlled');
        fault.receiptBytes = value.length;
      }
      if (!fault.injected && fault.mode === 'image_put' && artifact) {
        fault.injected = true;
        await storage.put('__test.image-recovery.injected', true);
        throw Error('Controlled image CAS failed before creating the object');
      }
      const result = await object.put(...args);
      if (!fault.injected && fault.mode === 'receipt_ack' && raw) {
        fault.injected = true;
        await storage.put('__test.image-recovery.injected', true);
        throw Error('Controlled raw response PUT succeeded but its acknowledgement was lost');
      }
      return result;
    };
    const result = Reflect.get(object, key, object);
    return typeof result === 'function' ? result.bind(object) : result;
  } });
}

export class Session extends RustSession {
  constructor(ctx, env) {
    const fault = { mode: env.IMAGE_RECOVERY_FAULT ?? '', injected: false, receiptWrites: 0, receiptBytes: 0, encrypted: false };
    const wrapped = new Proxy(env, { get(object, key) {
      return key === 'WORKSPACES' ? bucket(object.WORKSPACES, fault, ctx.storage) : Reflect.get(object, key, object);
    } });
    super(ctx, wrapped);
    this.fixtureStorage = ctx.storage;
    this.fixtureFault = fault;
  }
  async fetch(request) {
    if (new URL(request.url).pathname !== '/__test/image-recovery') return super.fetch(request);
    const keys = [...(await this.fixtureStorage.list({ prefix: 'hosted:image:' })).keys()];
    const operations = [];
    for (const key of keys) {
      const index = await this.fixtureStorage.get(key);
      if (typeof index?.generation !== 'string' || !Number.isSafeInteger(index.chunks)) continue;
      operations.push({ key, value: await read(this.fixtureStorage, key) });
    }
    return Response.json({ fault: this.fixtureFault, operations });
  }
}
const NativeDate = Date;
let clockOffset = 0;
globalThis.Date = class extends NativeDate {
  constructor(...values) { super(...(values.length ? values : [NativeDate.now() + clockOffset])); }
  static now() { return NativeDate.now() + clockOffset; }
};

function database(target, fault) {
  function statement(object, sql, values = []) {
    return new Proxy(object, { get(value, key) {
      if (key === 'constructor') return value.constructor;
      if (key === 'bind') return (...args) => statement(value.bind(...args), sql, args);
      if (key === 'run') return async (...args) => {
        const result = await value.run(...args);
        if (fault.reserve && sql.startsWith('INSERT INTO usage_reservations ') && String(values[0]).startsWith('receipt-gc:')) {
          fault.reserve = false; fault.injected = true; fault.reservation = values[0];
          throw Error('Controlled GC reservation committed before its acknowledgement was lost');
        }
        return result;
      };
      const result = Reflect.get(value, key, value);
      return typeof result === 'function' ? result.bind(value) : result;
    } });
  }
  return new Proxy(target, { get(object, key) {
    if (key === 'constructor') return object.constructor;
    if (key === 'prepare') return sql => statement(object.prepare(sql), sql);
    const result = Reflect.get(object, key, object);
    return typeof result === 'function' ? result.bind(object) : result;
  } });
}

export default class extends RustWorker {
  constructor(ctx, env) {
    const fault = { reserve: false, injected: false, reservation: null };
    const wrapped = new Proxy(env, { get(object, key) {
      return key === 'DB' ? database(object.DB, fault) : Reflect.get(object, key, object);
    } });
    super(ctx, wrapped); this.fixtureGc = fault;
  }
  async fetch(request) {
    if (new URL(request.url).pathname !== '/__test/image-recovery-sweep') return super.fetch(request);
    const body = await request.json();
    if (!Number.isSafeInteger(body.offset) || body.offset < 0) throw Error('Invalid fixture clock');
    clockOffset = body.offset; this.fixtureGc.reserve = body.reserve_ack === true;
    const pending = []; const ctx = { waitUntil(promise) { pending.push(promise); } };
    await RustWorker.prototype.scheduled.call({ env: this.env, ctx }, { scheduledTime: Date.now(), cron: '* * * * *', noRetry() {} });
    await Promise.all(pending);
    return Response.json({ fault: this.fixtureGc, now: Date.now() });
  }
}
