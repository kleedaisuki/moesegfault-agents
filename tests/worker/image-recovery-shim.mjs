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
    const roots = keys.filter(key => !key.slice('hosted:image:'.length).includes(':'));
    const operations = [];
    for (const key of roots) operations.push({ key, value: await read(this.fixtureStorage, key) });
    return Response.json({ fault: this.fixtureFault, operations });
  }
}
export default RustWorker;
