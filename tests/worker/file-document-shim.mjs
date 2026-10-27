import RustWorker, { Session as RustSession, FileIngest as RustFileIngest } from '../index.js';
export * from '../index.js';

async function invoke(method, args) {
  const response = await fetch(`https://retrieval.fixture.test/${method}`, {
    method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(args),
  });
  if (!response.ok) throw Error('controlled_retrieval_service_failed');
  return response.json();
}

class Ai {
  async fetch(request) {
    if (request.url !== 'https://workers-binding.ai/to-everything/markdown/transformer') throw Error('unexpected_native_document_endpoint');
    return fetch('https://retrieval.fixture.test/native-convert', {method:request.method,headers:request.headers,body:await request.text()});
  }
  run(model, input) { return invoke('embed', [model, input]); }
  async toMarkdown(documents, options) {
    return invoke('convert', [await Promise.all(documents.map(async ({ name, blob }) => ({ name,
      bytes: Array.from(new Uint8Array(await blob.arrayBuffer())) }))), options]);
  }
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
    if (new URL(request.url).pathname === '/__test/retrieval-maintenance') {
      const pending = [];
      const ctx = { waitUntil(promise) { pending.push(promise); } };
      await RustWorker.prototype.scheduled.call({ env: bindings(this.env), ctx },
        { scheduledTime: Date.now(), cron: '* * * * *', noRetry() {} });
      await Promise.all(pending);
      return Response.json({ completed: true });
    }
    return RustWorker.prototype.fetch.call({ env: bindings(this.env), ctx: this.ctx }, request);
  }
  async scheduled(event) {
    return RustWorker.prototype.scheduled.call({ env: bindings(this.env), ctx: this.ctx }, event);
  }
}

function delayedDatabase(target, state, delay) {
  const originals=new WeakMap(), sources=new WeakMap();
  const statement=(value, sql)=> {
    const wrapped=new Proxy(value,{get(object,key){
      if(key==='bind') return (...args)=>statement(object.bind(...args),sql);
      const result=Reflect.get(object,key,object);return typeof result==='function'?result.bind(object):result;
    }}); originals.set(wrapped,value);sources.set(wrapped,sql);return wrapped;
  };
  return new Proxy(target,{get(object,key){
    if(key==='prepare')return sql=>statement(object.prepare(sql),sql);
    if(key==='batch')return async statements=>{
      const results=await object.batch(statements.map(item=>originals.get(item)??item));
      if(delay && !state.delays && statements.some(item=>(sources.get(item)??'').startsWith("UPDATE file_search_chunks SET vector_status='embedded'"))){
        state.delays++;state.knownVectorsCommitted=true;
        await new Promise(resolve=>setTimeout(resolve,delay));
      }
      return results;
    };
    const result=Reflect.get(object,key,object);return typeof result==='function'?result.bind(object):result;
  }});
}
export class FileIngest extends RustFileIngest {
  constructor(ctx,env){
    const state={delays:0,knownVectorsCommitted:false};
    super(ctx,{...bindings(env),DB:delayedDatabase(env.DB,state,Number(env.INGEST_ACK_DELAY_MS??0))});
    this.receiptState=state;
  }
  async fetch(request){
    if(new URL(request.url).pathname==='/__fixture/status')return Response.json(this.receiptState);
    return super.fetch(request);
  }
}
